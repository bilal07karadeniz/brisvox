//! Pipelines: compositions of processors, started and stopped as a unit.
//!
//! A [`Pipeline`] is a list of stages. A stage is a processor, a nested pipeline, or a parallel
//! block of branch pipelines. Nested pipelines are wired inline, so nesting is free (no extra
//! hop, no extra task) and a nested pipeline can go anywhere a processor can. Every processor gets
//! its own inbox and task ([`run_processor`]); neighbours are linked in both directions. A
//! parallel block adds a fan-out and a merge unit around its branches (`pipeline/parallel.rs`).
//!
//! Lifecycle of a [`RunningPipeline`]:
//! - [`Pipeline::start`] spawns the tasks and sends `SystemFrame::Start` downstream.
//! - [`RunningPipeline::stop`] sends `SystemFrame::Stop`; each processor drains its queue in
//!   order before passing `Stop` on, so everything already sent is processed. Bounded by a
//!   timeout, after which the pipeline is cancelled.
//! - [`RunningPipeline::cancel`] stops every task immediately.
//! - [`RunningPipeline::wait`] supervises: the first processor error or panic cancels the rest
//!   and is returned.
//! - [`RunningPipeline::stats`] reads, without interrupting the tasks, each stage's queue depth,
//!   overflow-policy drops and stale-epoch drops (a brief per-inbox lock per call: poll at 1 Hz
//!   or slower).

mod parallel;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::frame::SystemFrame;
use crate::processor::{Direction, Processor, ProcessorContext, run_processor};
use crate::queue::{FrameSender, Inbox, InboxProbe, QueueConfig, QueueStats};

use self::parallel::{Ledger, MAX_BRANCHES, run_fanout, run_merge};

type TaskFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type SpawnFn = Box<dyn FnOnce(Inbox, ProcessorContext, CancellationToken) -> TaskFuture + Send>;

/// A type-erased processor: boxed once when added, so frames are never boxed.
struct ProcessorStage {
    name: &'static str,
    queue: QueueConfig,
    spawn: SpawnFn,
}

enum Stage {
    Processor(ProcessorStage),
    Pipeline(Pipeline),
    Parallel(Vec<Pipeline>),
}

/// A builder for a chain of processors, nested pipelines and parallel blocks.
pub struct Pipeline {
    name: &'static str,
    queue: QueueConfig,
    stages: Vec<Stage>,
}

impl Pipeline {
    /// An empty pipeline. Processors added with [`Self::then`] get [`QueueConfig::DEFAULT`].
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            queue: QueueConfig::DEFAULT,
            stages: Vec::new(),
        }
    }

    /// Inbox configuration for processors added afterwards with [`Self::then`] (and for the
    /// fan-out/merge units of parallel blocks).
    #[must_use]
    pub fn with_queue_config(mut self, queue: QueueConfig) -> Self {
        self.queue = queue;
        self
    }

    /// Appends a processor with this pipeline's inbox configuration.
    #[must_use]
    pub fn then<P: Processor>(self, processor: P) -> Self {
        let queue = self.queue;
        self.then_with_queue(processor, queue)
    }

    /// Appends a processor with its own inbox configuration (e.g.
    /// [`OverflowPolicies::REALTIME_INPUT`](crate::queue::OverflowPolicies::REALTIME_INPUT) for
    /// the stage after the input transport).
    #[must_use]
    pub fn then_with_queue<P: Processor>(mut self, processor: P, queue: QueueConfig) -> Self {
        let name = processor.name();
        let spawn: SpawnFn = Box::new(move |inbox, ctx, cancel| {
            Box::pin(run_processor(processor, inbox, ctx, cancel))
        });
        self.stages
            .push(Stage::Processor(ProcessorStage { name, queue, spawn }));
        self
    }

    /// Appends a nested pipeline. Its processors run inline, exactly as if added one by one.
    #[must_use]
    pub fn then_pipeline(mut self, pipeline: Pipeline) -> Self {
        self.stages.push(Stage::Pipeline(pipeline));
        self
    }

    /// Appends a parallel block: every frame entering it goes to every branch, and the branches'
    /// outputs are merged (semantics in `docs/ARCHITECTURE.md`, "Parallel branches").
    #[must_use]
    pub fn then_parallel(mut self, branches: impl IntoIterator<Item = Pipeline>) -> Self {
        self.stages
            .push(Stage::Parallel(branches.into_iter().collect()));
        self
    }

    /// The pipeline's name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Number of user processors, including those in nested pipelines and branches (fan-out
    /// and merge units not counted).
    pub fn processor_count(&self) -> usize {
        self.stages
            .iter()
            .map(|stage| match stage {
                Stage::Processor(_) => 1,
                Stage::Pipeline(p) => p.processor_count(),
                Stage::Parallel(branches) => branches.iter().map(Pipeline::processor_count).sum(),
            })
            .sum()
    }

    /// Spawns every processor and sends `SystemFrame::Start` downstream.
    ///
    /// `links` connects the pipeline's edges: frames leaving the last processor downstream go
    /// to `links.downstream`, frames leaving the first processor upstream go to
    /// `links.upstream` (dropped where `None`). `cancel` becomes the parent of the pipeline's
    /// token, so cancelling it stops the pipeline.
    ///
    /// Must be called inside a tokio runtime.
    ///
    /// # Errors
    /// [`Error::InvalidPipeline`] if the pipeline, a branch or a parallel block is empty;
    /// errors sending `Start`.
    pub async fn start(self, links: Links, cancel: &CancellationToken) -> Result<RunningPipeline> {
        let name = self.name;
        let mut graph = Graph::default();
        let (first, last) = graph
            .build_pipeline(self)?
            .ok_or(Error::InvalidPipeline("pipeline has no processors"))?;
        let input = graph.sender(first, Direction::Downstream)?;
        let upstream_input = graph.sender(last, Direction::Upstream)?;
        graph.unit_mut(last)?.downstream = links.downstream;
        graph.unit_mut(first)?.upstream = links.upstream;

        let token = cancel.child_token();
        let mut tasks = JoinSet::new();
        let mut names = HashMap::with_capacity(graph.units.len());
        let mut stages = Vec::with_capacity(graph.units.len());
        for unit in graph.units {
            let stale_dropped = Arc::new(AtomicU64::new(0));
            stages.push(StageProbe {
                name: unit.name,
                inbox: unit.inbox.probe(),
                stale_dropped: Arc::clone(&stale_dropped),
            });
            let ctx = ProcessorContext::new(unit.downstream, unit.upstream)
                .with_stale_counter(stale_dropped);
            let task: TaskFuture = match unit.kind {
                UnitKind::Processor(spawn) => spawn(unit.inbox, ctx, token.clone()),
                UnitKind::Fanout { branches, ledger } => {
                    Box::pin(run_fanout(unit.inbox, ctx, branches, ledger, token.clone()))
                }
                UnitKind::Merge { branches, ledger } => {
                    Box::pin(run_merge(unit.inbox, ctx, branches, ledger, token.clone()))
                }
            };
            let handle = tasks.spawn(task);
            names.insert(handle.id(), unit.name);
        }

        let running = RunningPipeline {
            name,
            input,
            upstream_input,
            tasks,
            names,
            stages,
            cancel: token,
            failure: None,
        };
        running.input.send(SystemFrame::Start.into()).await?;
        Ok(running)
    }
}

enum UnitKind {
    Processor(SpawnFn),
    Fanout {
        branches: Vec<FrameSender>,
        ledger: Arc<Ledger>,
    },
    Merge {
        branches: Vec<FrameSender>,
        ledger: Arc<Ledger>,
    },
}

/// One task-to-be: its inbox, what runs it, and its neighbours.
struct Unit {
    name: &'static str,
    inbox: Inbox,
    kind: UnitKind,
    downstream: Option<FrameSender>,
    upstream: Option<FrameSender>,
}

/// Wiring under construction. Units are created first (so their inboxes exist), then linked.
#[derive(Default)]
struct Graph {
    units: Vec<Unit>,
}

const INTERNAL: Error = Error::InvalidPipeline("internal wiring error");

impl Graph {
    fn add(&mut self, name: &'static str, queue: QueueConfig, kind: UnitKind) -> usize {
        self.units.push(Unit {
            name,
            inbox: Inbox::new(queue),
            kind,
            downstream: None,
            upstream: None,
        });
        self.units.len() - 1
    }

    fn sender(&self, unit: usize, direction: Direction) -> Result<FrameSender> {
        self.sender_from(unit, direction, 0)
    }

    fn sender_from(&self, unit: usize, direction: Direction, source: u16) -> Result<FrameSender> {
        self.units
            .get(unit)
            .map(|u| u.inbox.sender_from(direction, source))
            .ok_or(INTERNAL)
    }

    fn unit_mut(&mut self, unit: usize) -> Result<&mut Unit> {
        self.units.get_mut(unit).ok_or(INTERNAL)
    }

    /// Links `a → b` (a is upstream of b) in both directions.
    fn link(&mut self, a: usize, b: usize) -> Result<()> {
        let to_b = self.sender(b, Direction::Downstream)?;
        let to_a = self.sender(a, Direction::Upstream)?;
        self.unit_mut(a)?.downstream = Some(to_b);
        self.unit_mut(b)?.upstream = Some(to_a);
        Ok(())
    }

    /// Builds a pipeline's stages in sequence; returns its (first, last) units, or `None` for a
    /// pipeline without processors (skipped when nested; an error at the top or as a branch).
    fn build_pipeline(&mut self, pipeline: Pipeline) -> Result<Option<(usize, usize)>> {
        let queue = pipeline.queue;
        let mut ends: Option<(usize, usize)> = None;
        for stage in pipeline.stages {
            let (first, last) = match stage {
                Stage::Processor(p) => {
                    let unit = self.add(p.name, p.queue, UnitKind::Processor(p.spawn));
                    (unit, unit)
                }
                Stage::Pipeline(p) => match self.build_pipeline(p)? {
                    Some(ends) => ends,
                    None => continue,
                },
                Stage::Parallel(branches) => self.build_parallel(branches, queue)?,
            };
            ends = Some(match ends {
                None => (first, last),
                Some((head, previous)) => {
                    self.link(previous, first)?;
                    (head, last)
                }
            });
        }
        Ok(ends)
    }

    /// Builds fan-out → branches → merge; returns (fan-out, merge).
    fn build_parallel(
        &mut self,
        branches: Vec<Pipeline>,
        queue: QueueConfig,
    ) -> Result<(usize, usize)> {
        if branches.is_empty() {
            return Err(Error::InvalidPipeline("parallel block has no branches"));
        }
        if branches.len() > MAX_BRANCHES {
            return Err(Error::InvalidPipeline(
                "parallel block has more than 64 branches",
            ));
        }
        let ledger = Arc::new(Ledger::new(branches.len()));
        let fanout = self.add(
            "fanout",
            queue,
            UnitKind::Fanout {
                branches: Vec::new(),
                ledger: Arc::clone(&ledger),
            },
        );
        let mut ends = Vec::with_capacity(branches.len());
        for branch in branches {
            ends.push(
                self.build_pipeline(branch)?
                    .ok_or(Error::InvalidPipeline("parallel branch has no processors"))?,
            );
        }
        let merge = self.add(
            "merge",
            queue,
            UnitKind::Merge {
                branches: Vec::new(),
                ledger,
            },
        );
        for (index, (head, tail)) in ends.into_iter().enumerate() {
            // Tag each branch's way into the fan-out and merge with its index (< 64, checked).
            let branch = u16::try_from(index).map_err(|_| INTERNAL)?;
            let to_head = self.sender(head, Direction::Downstream)?;
            let to_fanout = self.sender_from(fanout, Direction::Upstream, branch)?;
            let to_tail = self.sender(tail, Direction::Upstream)?;
            let to_merge = self.sender_from(merge, Direction::Downstream, branch)?;
            self.unit_mut(head)?.upstream = Some(to_fanout);
            self.unit_mut(tail)?.downstream = Some(to_merge);
            match &mut self.unit_mut(fanout)?.kind {
                UnitKind::Fanout { branches, .. } => branches.push(to_head),
                _ => return Err(INTERNAL),
            }
            match &mut self.unit_mut(merge)?.kind {
                UnitKind::Merge { branches, .. } => branches.push(to_tail),
                _ => return Err(INTERNAL),
            }
        }
        Ok((fanout, merge))
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("name", &self.name)
            .field("processors", &self.processor_count())
            .finish_non_exhaustive()
    }
}

/// Where a pipeline's edge frames go.
#[derive(Debug, Default, Clone)]
pub struct Links {
    /// Receives frames leaving the last processor downstream.
    pub downstream: Option<FrameSender>,
    /// Receives frames leaving the first processor upstream.
    pub upstream: Option<FrameSender>,
}

/// What `RunningPipeline::stats` reads for one stage.
struct StageProbe {
    name: &'static str,
    inbox: InboxProbe,
    stale_dropped: Arc<AtomicU64>,
}

/// Counters of one stage (processor, fan-out or merge unit), see [`RunningPipeline::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageStats {
    /// The processor's [`name`](crate::processor::Processor::name) (`"fanout"` / `"merge"` for
    /// the units of a parallel block).
    pub name: &'static str,
    /// Frames lost to the inbox's overflow policies (see [`QueueStats`]).
    pub queue: QueueStats,
    /// Frames queued in the inbox right now, all lanes.
    pub queued: usize,
    /// Outgoing frames the stage dropped because they were (or became) stale
    /// ([`ProcessorContext::stale_dropped`]).
    pub stale_dropped: u64,
}

/// Snapshot of every stage of a [`RunningPipeline`], in build order (the order of
/// [`Pipeline::then`] calls; parallel blocks add fan-out, branches, merge).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PipelineStats {
    /// One entry per task.
    pub stages: Vec<StageStats>,
}

impl PipelineStats {
    /// The first stage called `name`.
    pub fn stage(&self, name: &str) -> Option<&StageStats> {
        self.stages.iter().find(|s| s.name == name)
    }

    /// Frames evicted by overflow policies, over all stages.
    pub fn total_dropped(&self) -> u64 {
        self.stages
            .iter()
            .map(|s| {
                s.queue
                    .dropped_oldest
                    .saturating_add(s.queue.dropped_newest)
            })
            .fold(0, u64::saturating_add)
    }

    /// Stale-epoch drops over all stages.
    pub fn total_stale_dropped(&self) -> u64 {
        self.stages
            .iter()
            .map(|s| s.stale_dropped)
            .fold(0, u64::saturating_add)
    }
}

/// A started pipeline. Dropping it cancels and aborts every task.
pub struct RunningPipeline {
    name: &'static str,
    input: FrameSender,
    upstream_input: FrameSender,
    tasks: JoinSet<Result<()>>,
    names: HashMap<Id, &'static str>,
    stages: Vec<StageProbe>,
    cancel: CancellationToken,
    /// Root-cause failure so far. Kept on `self` so `wait` stays cancel-safe.
    failure: Option<Error>,
}

impl RunningPipeline {
    /// Sends frames into the first processor, travelling downstream.
    pub const fn input(&self) -> &FrameSender {
        &self.input
    }

    /// Sends frames into the last processor, travelling upstream.
    pub const fn upstream_input(&self) -> &FrameSender {
        &self.upstream_input
    }

    /// The pipeline's cancellation token (a child of the token given to `start`).
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Queue depth, overflow drops and stale drops of every stage (the tasks are not
    /// interrupted). Safe to poll while running and after the pipeline stopped (the last values
    /// stay).
    ///
    /// Not lock-free: the stale-drop counter is an atomic, but the queue depth and the overflow
    /// counters are read under each stage's inbox mutex, the same mutex the stage's send and
    /// receive path takes, twice per stage per call (a few hundred nanoseconds each). Poll at
    /// 1 Hz or slower from a metrics task; never per frame.
    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            stages: self
                .stages
                .iter()
                .map(|stage| StageStats {
                    name: stage.name,
                    queue: stage.inbox.stats(),
                    queued: stage.inbox.len(),
                    stale_dropped: stage.stale_dropped.load(Ordering::Relaxed),
                })
                .collect(),
        }
    }

    /// Stops every task immediately; queued frames are dropped.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Graceful stop: sends `SystemFrame::Stop`, which every processor forwards after
    /// processing what it already has, then waits for all tasks. The whole operation (including
    /// sending `Stop` into a full system lane) is bounded by `timeout`; on expiry the pipeline
    /// is cancelled and its tasks aborted.
    ///
    /// # Errors
    /// A processor failure (as [`Self::wait`]) if one happened; otherwise [`Error::Timeout`] if
    /// the drain did not finish in time. Either way every task is gone when this returns.
    pub async fn stop(&mut self, timeout: Duration) -> Result<()> {
        let input = self.input.clone();
        let graceful = async {
            // If the first processor is already gone there is nothing to drain: just wait.
            let _ = input.send(SystemFrame::Stop.into()).await;
            self.wait().await
        };
        match tokio::time::timeout(timeout, graceful).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    pipeline = self.name,
                    ?timeout,
                    "graceful stop timed out; cancelling"
                );
                self.cancel();
                // Backstop: aborted tasks end at their next await even if they ignore the token.
                self.tasks.abort_all();
                match self.wait().await {
                    Err(failure) => Err(failure),
                    Ok(()) => Err(Error::Timeout {
                        what: "pipeline stop",
                        after: timeout,
                    }),
                }
            }
        }
    }

    /// Waits for every task. The first failing processor cancels the rest; its error (or
    /// [`Error::ProcessorPanicked`]) is returned once all tasks have finished.
    ///
    /// `ChannelClosed` errors are treated as secondary: when a processor dies, its neighbours
    /// may fail pushing into its dropped inbox before the supervisor sees the original failure,
    /// so a later non-`ChannelClosed` error replaces them. Cancel-safe.
    ///
    /// # Errors
    /// The root-cause processor error, or [`Error::ProcessorPanicked`].
    pub async fn wait(&mut self) -> Result<()> {
        while let Some(joined) = self.tasks.join_next_with_id().await {
            let failure = match joined {
                Ok((_, Ok(()))) => None,
                Ok((id, Err(error))) => {
                    let stage = self.names.get(&id).copied().unwrap_or("?");
                    tracing::error!(pipeline = self.name, stage, %error, "processor failed");
                    Some(error)
                }
                Err(join_error) => {
                    let stage = self.names.get(&join_error.id()).copied().unwrap_or("?");
                    if join_error.is_panic() {
                        tracing::error!(pipeline = self.name, stage, "processor panicked");
                        Some(Error::ProcessorPanicked(stage))
                    } else {
                        // Aborted: only happens if we aborted it; not a failure.
                        None
                    }
                }
            };
            if let Some(error) = failure {
                self.record_failure(error);
            }
        }
        self.failure.clone().map_or(Ok(()), Err)
    }

    fn record_failure(&mut self, error: Error) {
        let replace = match &self.failure {
            None => true,
            Some(Error::ChannelClosed(_)) => !matches!(error, Error::ChannelClosed(_)),
            Some(_) => false,
        };
        if replace {
            self.failure = Some(error);
        }
        self.cancel();
    }
}

impl Drop for RunningPipeline {
    fn drop(&mut self) {
        // Cancel the parent-linked token too, so anything else holding it observes the end;
        // dropping the JoinSet then aborts the tasks.
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for RunningPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningPipeline")
            .field("name", &self.name)
            .field("tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wake_tests;
