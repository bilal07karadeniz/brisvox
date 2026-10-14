//! Parallel branches: the fan-out and merge units around a set of branch pipelines.
//!
//! ```text
//!            ┌─► branch 0 ─┐
//!  prev ─► fanout          merge ─► next        (and the mirror image upstream)
//!            └─► branch 1 ─┘
//! ```
//!
//! Every frame entering the block is broadcast to every branch. Each branch reaches the merge
//! (downstream) and the fan-out (upstream) through a sender tagged with its index, so copies are
//! matched per branch: a branch's k-th copy of a frame always counts toward the k-th broadcast of
//! that frame it has not passed yet, even if another branch swallowed some.
//! - `Stop`: forwarded once every branch has passed it on (graceful stop waits for the slowest
//!   branch); the merge then drains and ends.
//! - `Cancel`: forwarded on the first copy; after that the merge forwards nothing more (the next
//!   stage is already shutting down) and ends once every branch has passed `Cancel`.
//! - any other broadcast System/Control frame: forwarded on the first copy (an interrupt is not
//!   held back by a slow branch); later copies are dropped.
//! - frames that originate inside a branch, and all data frames, pass through. Data broadcast to
//!   several pass-through branches therefore arrives once per branch: branches that should not
//!   emit data must consume it.
//!
//! Upstream is symmetric: the merge broadcasts upstream frames to every branch and the fan-out
//! collapses the copies.
//!
//! Broadcasts are sent to branches one after another, so a branch whose inbox is full stalls the
//! fan-out for all branches (head-of-line blocking) until it makes room. Unlike
//! `ProcessorContext::push`, a blocked broadcast is not abandoned when an interrupt arrives in the
//! fan-out's inbox: the interrupt waits until the slowest branch head makes room (branch heads
//! are expected to keep up; revisit if a branch can stall for long).
//!
//! Both units apply the epoch gate like every processor: after a newer `Interrupt`, stale frames
//! are dropped when dequeued (copies are first matched against the ledger so its entries still
//! complete).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio_util::sync::CancellationToken;

use crate::epoch::{Admit, EpochGate};
use crate::error::{Error, Result};
use crate::frame::{Frame, FrameLevel, SystemFrame};
use crate::processor::{Direction, EpochPolicy, ProcessorContext};
use crate::queue::{FrameSender, Inbox};

/// Most branches one parallel block supports (branch sets are tracked as `u64` bit masks).
pub(super) const MAX_BRANCHES: usize = 64;

/// Broadcast frames remembered per direction. An entry completes when every branch has passed
/// the frame on; a branch that swallows broadcast frames would otherwise grow the ledger, so it
/// is bounded (oldest entries evicted, warned about once).
const LEDGER_CAPACITY: usize = 256;

/// How a frame arriving where branches meet relates to the broadcasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    /// First copy of a broadcast frame: forward it.
    First,
    /// A later copy: drop it.
    Repeat,
    /// Not a broadcast frame this branch still owes (originated inside it): forward it.
    Original,
}

struct Entry {
    frame: Frame,
    /// Branches that have passed this broadcast on.
    passed: u64,
}

/// Shared record of System/Control frames broadcast into the branches, one list per direction.
pub(super) struct Ledger {
    all: u64,
    down: Mutex<VecDeque<Entry>>,
    up: Mutex<VecDeque<Entry>>,
    warned: AtomicBool,
}

/// The bit of `branch` in a branch set (0 for an out-of-range branch, which never matches).
pub(super) fn bit(branch: u16) -> u64 {
    1u64.checked_shl(u32::from(branch)).unwrap_or(0)
}

impl Ledger {
    /// A ledger for `branches` branches (at most [`MAX_BRANCHES`]).
    pub(super) fn new(branches: usize) -> Self {
        let all = match branches {
            0 => 0,
            n if n >= MAX_BRANCHES => u64::MAX,
            n => 1u64
                .checked_shl(u32::try_from(n).unwrap_or(0))
                .map_or(u64::MAX, |b| b - 1),
        };
        Self {
            all,
            down: Mutex::new(VecDeque::with_capacity(LEDGER_CAPACITY)),
            up: Mutex::new(VecDeque::with_capacity(LEDGER_CAPACITY)),
            warned: AtomicBool::new(false),
        }
    }

    /// The set of all branches.
    pub(super) const fn all(&self) -> u64 {
        self.all
    }

    fn list(&self, direction: Direction) -> &Mutex<VecDeque<Entry>> {
        match direction {
            Direction::Downstream => &self.down,
            Direction::Upstream => &self.up,
        }
    }

    /// Records `frame` as broadcast in `direction`. Call before sending it to the branches.
    pub(super) fn register(&self, direction: Direction, frame: &Frame) {
        let mut list = self
            .list(direction)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if list.len() >= LEDGER_CAPACITY {
            list.pop_front();
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!("parallel ledger full: a branch is swallowing broadcast frames");
            }
        }
        list.push_back(Entry {
            frame: frame.clone(),
            passed: 0,
        });
    }

    /// Classifies `frame` arriving from `branch`, travelling in `direction`: it is matched to
    /// the oldest equal broadcast this branch has not passed yet.
    pub(super) fn observe(&self, direction: Direction, branch: u16, frame: &Frame) -> Seen {
        let bit = bit(branch);
        let mut list = self
            .list(direction)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(index) = list
            .iter()
            .position(|e| e.passed & bit == 0 && e.frame == *frame)
        else {
            return Seen::Original;
        };
        let Some(entry) = list.get_mut(index) else {
            return Seen::Original;
        };
        let first = entry.passed == 0;
        entry.passed |= bit;
        if entry.passed & self.all == self.all {
            list.remove(index);
        }
        if first { Seen::First } else { Seen::Repeat }
    }
}

/// Whether a broadcast frame is collapsed through the ledger (Stop and Cancel are counted by the
/// units themselves; data is never collapsed).
pub(super) fn is_tracked(frame: &Frame) -> bool {
    frame.level() != FrameLevel::Data
        && !matches!(
            frame,
            Frame::System(SystemFrame::Stop | SystemFrame::Cancel)
        )
}

/// Sends a copy of `frame` to every target. A target travelling upstream that has already
/// stopped is skipped, as with [`ProcessorContext::push`].
async fn broadcast(targets: &[FrameSender], frame: Frame) -> Result<()> {
    let Some((last, rest)) = targets.split_last() else {
        return Ok(());
    };
    for target in rest {
        send(target, frame.clone()).await?;
    }
    send(last, frame).await
}

async fn send(target: &FrameSender, frame: Frame) -> Result<()> {
    match target.send(frame).await {
        Err(Error::ChannelClosed(_)) if target.direction() == Direction::Upstream => Ok(()),
        other => other,
    }
}

/// Runs `work` until it finishes or `cancel` fires (one waiter for the task's whole life).
async fn until_cancelled(
    cancel: &CancellationToken,
    work: impl Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(()),
        result = work => result,
    }
}

// ---------------------------------------------------------------------------------------------
// Fan-out

struct Fanout {
    ctx: ProcessorContext,
    branches: Vec<FrameSender>,
    ledger: std::sync::Arc<Ledger>,
    gate: EpochGate,
}

/// Applies the epoch gate to a frame that is to be handled; returns whether to keep it.
fn pass_gate(gate: &mut EpochGate, ctx: &mut ProcessorContext, frame: &Frame) -> bool {
    match gate.admit(frame) {
        Admit::Process => true,
        Admit::Drop => false,
        Admit::Advance(epoch) => {
            ctx.set_epoch(epoch);
            true
        }
    }
}

impl Fanout {
    /// Handles one frame; returns `true` when the unit must end.
    async fn handle(&mut self, frame: Frame, direction: Direction, source: u16) -> Result<bool> {
        match direction {
            Direction::Downstream => {
                if !pass_gate(&mut self.gate, &mut self.ctx, &frame) {
                    return Ok(false);
                }
                let is_cancel = matches!(frame, Frame::System(SystemFrame::Cancel));
                if is_tracked(&frame) {
                    self.ledger.register(Direction::Downstream, &frame);
                }
                broadcast(&self.branches, frame).await?;
                Ok(is_cancel)
            }
            Direction::Upstream => {
                if is_tracked(&frame)
                    && self.ledger.observe(Direction::Upstream, source, &frame) == Seen::Repeat
                {
                    return Ok(false);
                }
                if pass_gate(&mut self.gate, &mut self.ctx, &frame) {
                    self.ctx.push(frame, Direction::Upstream).await?;
                }
                Ok(false)
            }
        }
    }
}

/// Runs the entry unit of a parallel block. A downstream `Stop` is broadcast after the unit's
/// own queue is drained (as in `run_processor`); `Cancel` is broadcast and ends the unit.
pub(super) async fn run_fanout(
    mut inbox: Inbox,
    mut ctx: ProcessorContext,
    branches: Vec<FrameSender>,
    ledger: std::sync::Arc<Ledger>,
    cancel: CancellationToken,
) -> Result<()> {
    ctx.attach(EpochPolicy::DropStale, inbox.epoch_watch());
    let mut fanout = Fanout {
        ctx,
        branches,
        ledger,
        gate: EpochGate::default(),
    };
    let work = async {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some((frame, direction, source)) = inbox.recv_from().await else {
                return Ok(());
            };
            if let (Frame::System(SystemFrame::Stop), Direction::Downstream) = (&frame, direction) {
                while !cancel.is_cancelled() {
                    let Ok((queued, queued_direction, queued_source)) = inbox.try_recv_from()
                    else {
                        break;
                    };
                    if fanout
                        .handle(queued, queued_direction, queued_source)
                        .await?
                    {
                        return Ok(());
                    }
                }
                if !cancel.is_cancelled() {
                    fanout.handle(frame, direction, source).await?;
                }
                return Ok(());
            }
            if fanout.handle(frame, direction, source).await? {
                return Ok(());
            }
        }
    };
    until_cancelled(&cancel, work).await
}

// ---------------------------------------------------------------------------------------------
// Merge

/// What the merge loop does after handling one frame.
enum Flow {
    Continue,
    /// Every branch has passed `Stop` on: drain, forward it, end.
    StopReady(Frame),
    /// Every branch has passed `Cancel` on: end.
    Exit,
}

struct Merge {
    ctx: ProcessorContext,
    branches: Vec<FrameSender>,
    ledger: std::sync::Arc<Ledger>,
    gate: EpochGate,
    stopped: u64,
    cancelled: u64,
}

impl Merge {
    async fn handle(&mut self, frame: Frame, direction: Direction, source: u16) -> Result<Flow> {
        let all = self.ledger.all();
        if direction == Direction::Upstream {
            if self.cancelled == 0 && pass_gate(&mut self.gate, &mut self.ctx, &frame) {
                if is_tracked(&frame) {
                    self.ledger.register(Direction::Upstream, &frame);
                }
                broadcast(&self.branches, frame).await?;
            }
            return Ok(Flow::Continue);
        }
        match &frame {
            Frame::System(SystemFrame::Stop) => {
                self.stopped |= bit(source);
                if self.stopped & all == all {
                    return Ok(Flow::StopReady(frame));
                }
            }
            Frame::System(SystemFrame::Cancel) => {
                let first = self.cancelled == 0;
                self.cancelled |= bit(source);
                if first {
                    self.ctx.push(frame, Direction::Downstream).await?;
                }
                if self.cancelled & all == all {
                    return Ok(Flow::Exit);
                }
            }
            // After a Cancel the next stage is shutting down: forward nothing more.
            _ if self.cancelled != 0 => {}
            _ => {
                let first_copy = !is_tracked(&frame)
                    || self.ledger.observe(Direction::Downstream, source, &frame) != Seen::Repeat;
                if first_copy && pass_gate(&mut self.gate, &mut self.ctx, &frame) {
                    self.ctx.push(frame, Direction::Downstream).await?;
                }
            }
        }
        Ok(Flow::Continue)
    }
}

/// Runs the exit unit of a parallel block (see the module docs for its semantics).
pub(super) async fn run_merge(
    mut inbox: Inbox,
    mut ctx: ProcessorContext,
    branches: Vec<FrameSender>,
    ledger: std::sync::Arc<Ledger>,
    cancel: CancellationToken,
) -> Result<()> {
    ctx.attach(EpochPolicy::DropStale, inbox.epoch_watch());
    let mut merge = Merge {
        ctx,
        branches,
        ledger,
        gate: EpochGate::default(),
        stopped: 0,
        cancelled: 0,
    };
    let work = async {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some((frame, direction, source)) = inbox.recv_from().await else {
                return Ok(());
            };
            match merge.handle(frame, direction, source).await? {
                Flow::Continue => {}
                Flow::Exit => return Ok(()),
                Flow::StopReady(stop) => {
                    while !cancel.is_cancelled() {
                        let Ok((queued, queued_direction, queued_source)) = inbox.try_recv_from()
                        else {
                            break;
                        };
                        if let Flow::Exit = merge
                            .handle(queued, queued_direction, queued_source)
                            .await?
                        {
                            return Ok(());
                        }
                    }
                    if !cancel.is_cancelled() && merge.cancelled == 0 {
                        merge.ctx.push(stop, Direction::Downstream).await?;
                    }
                    return Ok(());
                }
            }
        }
    };
    until_cancelled(&cancel, work).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::epoch::TurnEpoch;
    use crate::frame::{AudioFrame, SampleRate};
    use crate::queue::QueueConfig;

    #[allow(clippy::disallowed_methods)] // test timestamp
    fn audio(seq: u64) -> Frame {
        AudioFrame::new(
            Bytes::new(),
            SampleRate::Hz16000,
            Instant::now(),
            seq,
            TurnEpoch::ZERO,
        )
        .unwrap()
        .into()
    }

    /// Deterministic version of the race an earlier review reproduced: after the merge has
    /// forwarded the first Cancel, the next stage exits; a slower branch that still pushes data
    /// must not make the merge fail with `ChannelClosed`.
    #[tokio::test(start_paused = true)]
    async fn merge_drops_everything_after_the_first_cancel() {
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let branch0 = inbox.sender_from(Direction::Downstream, 0);
        let branch1 = inbox.sender_from(Direction::Downstream, 1);
        let mut next = Inbox::new(QueueConfig::DEFAULT);
        let ctx = ProcessorContext::new(Some(next.sender(Direction::Downstream)), None);
        let merge = tokio::spawn(run_merge(
            inbox,
            ctx,
            Vec::new(),
            Arc::new(Ledger::new(2)),
            CancellationToken::new(),
        ));

        branch0.send(SystemFrame::Cancel.into()).await.unwrap();
        let (frame, _) = next.recv().await.unwrap();
        assert_eq!(frame.name(), "cancel");
        drop(next); // the next stage has processed Cancel and exited

        branch1.send(audio(1)).await.unwrap();
        // Let the merge handle the late data before branch 1's Cancel (which would overtake it).
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(!merge.is_finished(), "late data must not fail the merge");
        branch1.send(SystemFrame::Cancel.into()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), merge)
            .await
            .expect("merge did not end after every branch cancelled")
            .unwrap();
        assert_eq!(result, Ok(()));
    }

    /// An interrupt originating in one branch: stale frames that other branches emit afterwards
    /// must not leave the merge (merge gate, backed by the push-side epoch watch).
    #[tokio::test(start_paused = true)]
    async fn merge_gates_stale_frames_after_a_branch_interrupt() {
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let branch0 = inbox.sender_from(Direction::Downstream, 0);
        let branch1 = inbox.sender_from(Direction::Downstream, 1);
        let mut next = Inbox::new(QueueConfig::DEFAULT);
        let ctx = ProcessorContext::new(Some(next.sender(Direction::Downstream)), None);
        let merge = tokio::spawn(run_merge(
            inbox,
            ctx,
            Vec::new(),
            Arc::new(Ledger::new(2)),
            CancellationToken::new(),
        ));
        let interrupt: Frame = SystemFrame::Interrupt {
            new_epoch: TurnEpoch::new(1),
        }
        .into();
        branch0.send(interrupt).await.unwrap();
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        branch1.send(audio(7)).await.unwrap(); // epoch 0: stale now
        // Let the merge handle it before the Cancels (which would overtake it).
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        branch0.send(SystemFrame::Cancel.into()).await.unwrap();
        branch1.send(SystemFrame::Cancel.into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), merge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let names: Vec<&str> = std::iter::from_fn(|| next.try_recv().ok())
            .map(|(f, _)| f.name())
            .collect();
        assert_eq!(names, ["interrupt", "cancel"]);
    }

    #[test]
    fn ledger_matches_copies_per_branch() {
        let ledger = Ledger::new(2);
        let f = Frame::from(SystemFrame::Start);
        ledger.register(Direction::Downstream, &f);
        ledger.register(Direction::Downstream, &f);
        // Branch 0 passes both broadcasts, branch 1 swallowed the first.
        assert_eq!(ledger.observe(Direction::Downstream, 0, &f), Seen::First);
        assert_eq!(ledger.observe(Direction::Downstream, 0, &f), Seen::First);
        assert_eq!(ledger.observe(Direction::Downstream, 1, &f), Seen::Repeat);
        // A frame no branch owes is an original.
        assert_eq!(
            ledger.observe(Direction::Downstream, 0, &audio(1)),
            Seen::Original
        );
        assert_eq!(bit(64), 0);
        assert_eq!(Ledger::new(64).all(), u64::MAX);
        assert_eq!(Ledger::new(3).all(), 0b111);
    }
}
