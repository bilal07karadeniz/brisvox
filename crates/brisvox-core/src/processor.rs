//! The `Processor` trait and its task loop.
//!
//! Each processor runs as its own task: [`run_processor`] takes frames from the processor's
//! [`Inbox`] (system frames first) and hands them to [`Processor::process`], which pushes
//! results to its neighbours through a [`ProcessorContext`].
//!
//! ```text
//!  upstream neighbour            this processor             downstream neighbour
//!  ┌─────────┐  Downstream  ┌──────────────────────┐  Downstream  ┌─────────┐
//!  │         │ ───────────► │ inbox → process(ctx) │ ───────────► │         │
//!  │         │ ◄─────────── │                      │ ◄─────────── │         │
//!  └─────────┘   Upstream   └──────────────────────┘   Upstream   └─────────┘
//! ```
//!
//! # Pushing upstream without deadlock (rule)
//! A pipeline is a bidirectional chain: every neighbour pair has a data lane in each direction,
//! and each lane is bounded. An *awaited* push from inside [`Processor::process`] blocks that
//! processor's task until the neighbour has room. If neighbour A awaits a push into B's full
//! downstream lane while B, inside `process`, awaits a push into A's full upstream lane, neither
//! task can ever drain its inbox again: deadlock. The rule that rules the cycle out:
//!
//! - **Downstream** pushes from `process` may await (backpressure towards the source is wanted).
//! - **Upstream** traffic is rare control information (`TtsStarted`, `TtsEnded`, acknowledgements).
//!   It is sent only (a) from a helper task the processor owns, which may await because no
//!   processor loop depends on it, (b) from the pump/transport task, or (c) from inside `process`
//!   with a non-blocking `try_send` on [`ProcessorContext::sender`]`(Direction::Upstream)`,
//!   where a full lane means the frame is counted and dropped (or retried by a helper task) but
//!   never waited for. A processor forwarding an upstream frame it received uses (c) as well.
//!
//! The stress test `bidirectional_chain_with_slow_consumer_never_deadlocks` in
//! `pipeline/wake_tests.rs` runs this pattern with one-slot lanes, and a control test shows that the
//! awaited variant does deadlock.
//!
//! # Self-wake
//! A processor that needs to run again at a time of its own choosing (a timer, a model verdict
//! computed off-task) spawns a helper task that sends [`SystemFrame::Wake`] through
//! [`ProcessorContext::self_sender`]. `Wake` is a system frame: it overtakes queued data, is
//! never purged, and is handled by the same `process` call sequence as every other frame, so
//! the processor stays single-threaded and its ordering deterministic. It arrives with
//! [`Direction::Downstream`] and is never forwarded. The helper task must be owned by the
//! processor (aborted when it drops), because the self-sender keeps the processor's own inbox
//! connected.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio_util::sync::CancellationToken;

use crate::epoch::{Admit, EpochGate, TurnEpoch};
use crate::error::{Error, Result};
use crate::frame::{Frame, SystemFrame};
use crate::queue::{EpochWatch, FrameSender, Inbox, TrySendError};

/// Which way a frame travels through the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the input transport towards the output transport.
    Downstream,
    /// Back towards the input transport (errors, metrics, acknowledgements).
    Upstream,
}

impl Direction {
    /// The opposite direction.
    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Self::Downstream => Self::Upstream,
            Self::Upstream => Self::Downstream,
        }
    }
}

/// How a processor's task treats turn epochs (see [`EpochGate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EpochPolicy {
    /// The default: on a newer `Interrupt`, queued stale frames are purged; stale frames are
    /// never handed to `process`; stale outgoing pushes are dropped, and a push blocked on a full
    /// neighbour is abandoned once its frame becomes stale.
    #[default]
    DropStale,
    /// Everything is processed and pushed (recorders, debugging taps).
    KeepAll,
}

/// A pipeline stage.
///
/// `process` is called with one frame at a time, in the order [`Inbox::recv`] yields them. It
/// must handle every frame it receives: forward it ([`ProcessorContext::forward`]), transform it,
/// or consume it deliberately. Heavy CPU work does not belong here (see the inference pool).
///
/// Interruptions are handled by the task loop, not by each processor: with the default
/// [`EpochPolicy::DropStale`], `process` only ever sees current-epoch frames and receives each
/// newer `Interrupt` once (it must forward it, and may reset per-turn state); a `KeepAll`
/// processor sees everything, duplicates included. A processor that *originates* an interrupt
/// just pushes it: the push moves its own epoch forward and the task loop then purges its inbox.
pub trait Processor: Send + 'static {
    /// Stable stage name, for spans and metrics.
    fn name(&self) -> &'static str;

    /// How the task loop applies turn epochs to this processor. Default: drop stale frames.
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::DropStale
    }

    /// Handles one frame that arrived travelling in `direction`.
    ///
    /// Must be cancel-safe: on cancellation the future is dropped at whatever `.await` it is
    /// parked on (typically a `push` into a full neighbour), so state must stay consistent at
    /// every await point.
    ///
    /// # Errors
    /// An error stops this processor's task ([`run_processor`] returns it).
    fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// A processor's connections to its neighbours, plus the epoch its task has reached.
#[derive(Debug, Default)]
pub struct ProcessorContext {
    downstream: Option<FrameSender>,
    upstream: Option<FrameSender>,
    epoch: TurnEpoch,
    policy: EpochPolicy,
    /// Interrupts arriving in this processor's own inbox (set by the task loop).
    watch: Option<EpochWatch>,
    /// Shared with the pipeline supervisor, which reads it for `RunningPipeline::stats`.
    stale_dropped: Arc<AtomicU64>,
}

impl ProcessorContext {
    /// Connects to the downstream neighbour's inbox (a sender for [`Direction::Downstream`])
    /// and the upstream neighbour's inbox (a sender for [`Direction::Upstream`]). `None` marks
    /// the pipeline's edge.
    pub fn new(downstream: Option<FrameSender>, upstream: Option<FrameSender>) -> Self {
        Self {
            downstream,
            upstream,
            ..Self::default()
        }
    }

    /// The turn epoch this processor's task is in.
    pub const fn epoch(&self) -> TurnEpoch {
        self.epoch
    }

    /// Outgoing frames dropped because they were (or became) stale.
    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped.load(Ordering::Relaxed)
    }

    /// Counts stale drops in `counter`, a counter the caller also holds (the pipeline reads it
    /// for its statistics).
    pub(crate) fn with_stale_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.stale_dropped = counter;
        self
    }

    fn note_stale(&self) {
        self.stale_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Wires the context to its task: the processor's epoch policy and a watch on its own inbox.
    pub(crate) fn attach(&mut self, policy: EpochPolicy, watch: EpochWatch) {
        self.policy = policy;
        self.watch = Some(watch);
    }

    pub(crate) fn set_epoch(&mut self, epoch: TurnEpoch) {
        self.epoch = epoch;
    }

    /// Sends `frame` to the neighbour in `direction`, waiting if its lane is full and the
    /// frame's policy is backpressure. At the pipeline's edge (no neighbour) the frame is
    /// dropped. An upstream neighbour that has already stopped (graceful stop finishes
    /// upstream-first) also drops the frame silently.
    ///
    /// # Errors
    /// [`Error::ChannelClosed`] if the downstream neighbour is gone.
    ///
    /// With [`EpochPolicy::DropStale`], a frame older than [`Self::epoch`] is dropped instead of
    /// sent, and a push waiting for space is abandoned (the frame dropped) as soon as an
    /// interrupt that makes the frame stale arrives in this processor's own inbox.
    pub async fn push(&mut self, frame: Frame, direction: Direction) -> Result<()> {
        // `Wake` is addressed to its creator only: a forwarded one is discarded here.
        if matches!(&frame, Frame::System(SystemFrame::Wake { .. })) {
            tracing::debug!("wake frame pushed to a neighbour; discarded");
            return Ok(());
        }
        // An interrupt this processor originates moves its own epoch too.
        if let Frame::System(SystemFrame::Interrupt { new_epoch }) = &frame
            && *new_epoch > self.epoch
        {
            self.epoch = *new_epoch;
        }
        let drop_stale = self.policy == EpochPolicy::DropStale;
        if drop_stale && frame.is_stale(self.epoch) {
            self.note_stale();
            return Ok(());
        }
        // An interrupt already waiting in this processor's own inbox dooms older frames too
        // (one atomic load; no waiter registration).
        if drop_stale
            && let Some(watch) = &self.watch
            && frame.is_stale(watch.latest())
        {
            self.note_stale();
            return Ok(());
        }
        let neighbour = match direction {
            Direction::Downstream => &self.downstream,
            Direction::Upstream => &self.upstream,
        };
        let Some(sender) = neighbour else {
            return Ok(());
        };
        // Fast path: no waiting, no watch registration.
        let frame = match sender.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Closed(_)) => return closed(direction),
            Err(TrySendError::Full(frame)) => frame,
        };
        // Slow path: wait for space, unless the frame becomes stale first.
        let abandon_after = match (&self.watch, frame.epoch()) {
            (Some(watch), Some(epoch)) if drop_stale && frame.is_stale(epoch.next()) => {
                Some((watch.clone(), epoch))
            }
            _ => None,
        };
        let sent = match abandon_after {
            None => sender.send(frame).await,
            Some((watch, epoch)) => {
                tokio::select! {
                    biased;
                    () = watch.wait_beyond(epoch) => {
                        self.note_stale();
                        return Ok(());
                    }
                    sent = sender.send(frame) => sent,
                }
            }
        };
        match sent {
            Err(Error::ChannelClosed(_)) => closed(direction),
            other => other,
        }
    }

    /// Passes `frame` on in the direction it was travelling. Same as [`Self::push`].
    ///
    /// # Errors
    /// See [`Self::push`].
    pub async fn forward(&mut self, frame: Frame, direction: Direction) -> Result<()> {
        self.push(frame, direction).await
    }

    /// Whether there is a neighbour in `direction`.
    pub fn has_neighbour(&self, direction: Direction) -> bool {
        match direction {
            Direction::Downstream => self.downstream.is_some(),
            Direction::Upstream => self.upstream.is_some(),
        }
    }

    /// Watch on interrupts arriving in this processor's own inbox (set by the task loop), so a
    /// processor waiting inside `process` (e.g. pacing output) can stop as soon as an interrupt
    /// is queued for it.
    pub fn epoch_watch(&self) -> Option<&EpochWatch> {
        self.watch.as_ref()
    }

    /// A new sender into this processor's **own** inbox, for helper tasks that must wake it
    /// (see "Self-wake" in the module docs): send [`SystemFrame::Wake`] through it. `None`
    /// before the task loop attached the context (never inside `process`).
    ///
    /// The returned sender keeps the inbox connected while it lives, so a helper holding it must
    /// be aborted with the processor. The system lane is bounded: awaiting `send` in a helper is
    /// fine, calling `try_send` from `process` for its own wake is also fine.
    pub fn self_sender(&self) -> Option<FrameSender> {
        self.watch.as_ref().map(EpochWatch::sender)
    }

    /// A clone of the sender towards the neighbour in `direction`, for helper tasks a processor
    /// spawns (e.g. a provider stream reader). Frames sent through it bypass this context's
    /// epoch filtering: tag them with the right epoch (downstream gates drop stale ones), and
    /// end the task with the processor (abort it on drop).
    ///
    /// For [`Direction::Upstream`] see "Pushing upstream without deadlock" in the module docs.
    pub fn sender(&self, direction: Direction) -> Option<FrameSender> {
        match direction {
            Direction::Downstream => self.downstream.clone(),
            Direction::Upstream => self.upstream.clone(),
        }
    }
}

/// The result of pushing into a neighbour that is gone: an upstream neighbour that has already
/// stopped (graceful stop ends upstream-first) silently drops the frame; a missing downstream
/// neighbour is an error.
fn closed(direction: Direction) -> Result<()> {
    match direction {
        Direction::Upstream => {
            tracing::debug!("upstream neighbour has stopped; frame discarded");
            Ok(())
        }
        Direction::Downstream => Err(Error::ChannelClosed("inbox")),
    }
}

/// Runs `processor` until it handles a `Stop`/`Cancel` frame, its inbox is closed and drained,
/// `cancel` fires, or `process` fails.
///
/// Lifecycle frames (both are passed to `process` like any frame, which forwards them):
/// - `SystemFrame::Stop` travelling downstream: every frame already queued is processed first,
///   in receive order, then `Stop`, then the task ends. Because each processor forwards `Stop`
///   only after its own drain, a graceful stop flushes the whole pipeline in order.
/// - `SystemFrame::Cancel`: processed immediately (queued frames are dropped), then the task ends.
///
/// Cancellation through `cancel` interrupts the loop wherever it waits, including a `process`
/// call parked on a full neighbour, so a cancelled pipeline never hangs on backpressure.
///
/// # Errors
/// The first error returned by [`Processor::process`]. Stopping, closing and cancellation
/// return `Ok`.
pub async fn run_processor<P: Processor>(
    mut processor: P,
    mut inbox: Inbox,
    mut ctx: ProcessorContext,
    cancel: CancellationToken,
) -> Result<()> {
    let policy = processor.epoch_policy();
    ctx.attach(policy, inbox.epoch_watch());
    let mut gate = EpochGate::default();
    // One `cancelled()` waiter for the task's whole life: registering a waiter per frame takes
    // the token's lock, which all processors of a session share (measured: 4x slower chains on
    // multi-thread runtimes). The atomic check covers loops that never return `Pending`.
    let work = async {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some((frame, direction)) = inbox.recv().await else {
                return Ok(());
            };
            if !admit(policy, &mut gate, &inbox, &mut ctx, &frame) {
                continue;
            }
            match (&frame, direction) {
                (Frame::System(SystemFrame::Stop), Direction::Downstream) => {
                    while !cancel.is_cancelled() {
                        let Ok((queued, queued_direction)) = inbox.try_recv() else {
                            break;
                        };
                        if !admit(policy, &mut gate, &inbox, &mut ctx, &queued) {
                            continue;
                        }
                        let is_cancel = matches!(queued, Frame::System(SystemFrame::Cancel));
                        processor
                            .process(queued, queued_direction, &mut ctx)
                            .await?;
                        sync_after_process(policy, &mut gate, &inbox, &ctx);
                        if is_cancel {
                            return Ok(());
                        }
                    }
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    processor.process(frame, direction, &mut ctx).await?;
                    return Ok(());
                }
                (Frame::System(SystemFrame::Cancel), _) => {
                    processor.process(frame, direction, &mut ctx).await?;
                    return Ok(());
                }
                _ => {
                    processor.process(frame, direction, &mut ctx).await?;
                    sync_after_process(policy, &mut gate, &inbox, &ctx);
                }
            }
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(()),
        result = work => result,
    }
}

/// After `process`: if the processor originated a newer interrupt (its context epoch moved
/// ahead of the gate), adopt that epoch and purge stale queued frames.
fn sync_after_process(
    policy: EpochPolicy,
    gate: &mut EpochGate,
    inbox: &Inbox,
    ctx: &ProcessorContext,
) {
    if gate.advance_to(ctx.epoch()) && policy == EpochPolicy::DropStale {
        inbox.purge_stale(ctx.epoch());
    }
}

/// Applies the epoch gate to `frame`; returns whether to hand it to `process`. On a newer
/// interrupt it also updates the context's epoch and purges stale frames from the inbox.
fn admit(
    policy: EpochPolicy,
    gate: &mut EpochGate,
    inbox: &Inbox,
    ctx: &mut ProcessorContext,
    frame: &Frame,
) -> bool {
    let admitted = gate.admit(frame);
    if let Admit::Advance(epoch) = admitted {
        ctx.set_epoch(epoch);
    }
    match (policy, admitted) {
        (EpochPolicy::KeepAll, _) | (EpochPolicy::DropStale, Admit::Process) => true,
        (EpochPolicy::DropStale, Admit::Drop) => false,
        (EpochPolicy::DropStale, Admit::Advance(epoch)) => {
            let purged = inbox.purge_stale(epoch);
            if purged > 0 {
                tracing::debug!(purged, epoch = epoch.get(), "purged stale frames");
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::Instant;

    use super::*;
    use crate::frame::{AudioFrame, DataFrame, SampleRate, ToolResult};
    use crate::queue::{OverflowPolicies, QueueConfig};

    #[allow(clippy::disallowed_methods)] // test timestamp
    fn audio(seq: u64, epoch: u32) -> Frame {
        AudioFrame::new(
            Bytes::new(),
            SampleRate::Hz16000,
            Instant::now(),
            seq,
            TurnEpoch::new(epoch),
        )
        .unwrap()
        .into()
    }

    fn interrupt(epoch: u32) -> Frame {
        SystemFrame::Interrupt {
            new_epoch: TurnEpoch::new(epoch),
        }
        .into()
    }

    fn tool_result(epoch: u32) -> Frame {
        DataFrame::ToolResult(Box::new(ToolResult {
            epoch: TurnEpoch::new(epoch),
            call_id: Arc::from("c"),
            content: Arc::from("{}"),
            is_error: false,
        }))
        .into()
    }

    fn one_slot() -> QueueConfig {
        QueueConfig {
            system_capacity: 4,
            data_capacity: 1,
            policies: OverflowPolicies::DEFAULT,
        }
    }

    #[test]
    fn purge_stale_keeps_system_frames_tool_results_and_current_epoch() {
        let mut inbox = Inbox::new(QueueConfig::DEFAULT);
        let tx = inbox.sender(Direction::Downstream);
        let up = inbox.sender(Direction::Upstream);
        tx.try_send(audio(1, 0)).unwrap();
        tx.try_send(tool_result(0)).unwrap();
        tx.try_send(audio(2, 1)).unwrap();
        up.try_send(audio(3, 0)).unwrap();
        tx.try_send(interrupt(1)).unwrap();
        assert_eq!(inbox.purge_stale(TurnEpoch::new(1)), 2);
        let names: Vec<&str> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|(f, _)| f.name())
            .collect();
        assert_eq!(names, ["interrupt", "tool_result", "audio"]);
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_watch_sees_enqueued_interrupts() {
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let watch = inbox.epoch_watch();
        let tx = inbox.sender(Direction::Downstream);
        assert_eq!(watch.latest(), TurnEpoch::ZERO);
        let waiter = tokio::spawn({
            let watch = watch.clone();
            async move { watch.wait_beyond(TurnEpoch::new(1)).await }
        });
        tx.send(interrupt(1)).await.unwrap();
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "epoch 1 is not beyond 1");
        tx.send(interrupt(3)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("watch missed the interrupt")
            .unwrap();
        assert_eq!(watch.latest(), TurnEpoch::new(3));
        // An older interrupt never lowers the hint.
        tx.send(interrupt(2)).await.unwrap();
        assert_eq!(watch.latest(), TurnEpoch::new(3));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_push_is_abandoned_when_its_frame_becomes_stale() {
        // `own` is the pushing processor's inbox; `neighbour` is full and never drained.
        let own = Inbox::new(QueueConfig::DEFAULT);
        let own_tx = own.sender(Direction::Downstream);
        let neighbour = Inbox::new(one_slot());
        let mut ctx = ProcessorContext::new(Some(neighbour.sender(Direction::Downstream)), None);
        ctx.attach(EpochPolicy::DropStale, own.epoch_watch());

        ctx.push(audio(0, 0), Direction::Downstream).await.unwrap();
        let pushing = tokio::spawn(async move {
            let result = ctx.push(audio(1, 0), Direction::Downstream).await;
            (result, ctx.stale_dropped())
        });
        tokio::task::yield_now().await;
        assert!(
            !pushing.is_finished(),
            "push should block on the full neighbour"
        );

        own_tx.send(interrupt(1)).await.unwrap();
        let (result, dropped) = tokio::time::timeout(Duration::from_secs(1), pushing)
            .await
            .expect("stale push was not abandoned")
            .unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(dropped, 1);
        assert_eq!(
            neighbour.len(),
            1,
            "the abandoned frame must not be delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tool_results_are_never_abandoned() {
        let own = Inbox::new(QueueConfig::DEFAULT);
        let own_tx = own.sender(Direction::Downstream);
        let mut neighbour = Inbox::new(one_slot());
        let mut ctx = ProcessorContext::new(Some(neighbour.sender(Direction::Downstream)), None);
        ctx.attach(EpochPolicy::DropStale, own.epoch_watch());
        ctx.push(audio(0, 0), Direction::Downstream).await.unwrap();
        let pushing =
            tokio::spawn(async move { ctx.push(tool_result(0), Direction::Downstream).await });
        tokio::task::yield_now().await;
        own_tx.send(interrupt(1)).await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            !pushing.is_finished(),
            "a tool result must wait, not be dropped"
        );
        let _ = neighbour.recv().await;
        tokio::time::timeout(Duration::from_secs(1), pushing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(neighbour.try_recv().unwrap().0.name(), "tool_result");
    }

    #[tokio::test]
    async fn stale_outgoing_frames_are_dropped_unless_keep_all() {
        let mut out = Inbox::new(QueueConfig::DEFAULT);
        let own = Inbox::new(QueueConfig::DEFAULT);
        let mut ctx = ProcessorContext::new(Some(out.sender(Direction::Downstream)), None);
        ctx.attach(EpochPolicy::DropStale, own.epoch_watch());
        ctx.set_epoch(TurnEpoch::new(2));
        ctx.push(audio(0, 1), Direction::Downstream).await.unwrap();
        ctx.push(tool_result(1), Direction::Downstream)
            .await
            .unwrap();
        ctx.push(audio(1, 2), Direction::Downstream).await.unwrap();
        assert_eq!(ctx.stale_dropped(), 1);
        assert_eq!(out.len(), 2);

        ctx.attach(EpochPolicy::KeepAll, own.epoch_watch());
        ctx.push(audio(2, 1), Direction::Downstream).await.unwrap();
        assert_eq!(out.len(), 3);
        while out.try_recv().is_ok() {}
    }
}
