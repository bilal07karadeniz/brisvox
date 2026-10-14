//! Per-processor inbox with priority lanes.
//!
//! Every processor owns one [`Inbox`]. It has two lanes per [`Direction`]:
//! - **system lane**: [`SystemFrame`]s. Always drained first, so an
//!   interrupt overtakes any amount of queued data. Bounded; senders wait when it is full
//!   (system frames are never dropped).
//! - **data lane**: Control and Data frames, in order. Bounded; what happens when it is full is
//!   decided per frame kind by [`OverflowPolicies`].
//!
//! Neighbours push through a [`FrameSender`] bound to one direction. Buffers are allocated once
//! at construction; sending and receiving never allocate. The lock is a `std::sync::Mutex` held
//! only for queue operations (never across an `.await`); waiting uses `tokio::sync::Notify`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

use crate::epoch::TurnEpoch;
use crate::error::{Error, Result};
use crate::frame::{DataFrame, Frame, FrameLevel, SystemFrame};
use crate::processor::Direction;

/// What a full data lane does with a new frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverflowPolicy {
    /// The sender waits for space. Nothing is lost.
    Backpressure,
    /// Drop the oldest queued frame of the same kind, then enqueue the new one. If none of that
    /// kind is queued, the new frame is dropped. For real-time input where stale data is useless.
    DropOldest,
    /// Drop the new frame. For observability data that must never stall the audio path.
    DropNewest,
}

/// Overflow policy per frame kind for one inbox.
///
/// Only audio and metrics are configurable: system frames, control frames, transcripts, LLM
/// text, tool frames, word timestamps and DTMF always use [`OverflowPolicy::Backpressure`], because
/// losing them would corrupt the conversation (or the played-text truncation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowPolicies {
    /// Policy for [`DataFrame::Audio`].
    pub audio: OverflowPolicy,
    /// Policy for [`DataFrame::Metric`].
    pub metric: OverflowPolicy,
}

impl OverflowPolicies {
    /// Lossless audio (TTS output is paced downstream); metrics dropped when full.
    pub const DEFAULT: Self = Self {
        audio: OverflowPolicy::Backpressure,
        metric: OverflowPolicy::DropNewest,
    };

    /// For the stage right after `transport.in`: a slow consumer drops the oldest caller audio
    /// instead of building latency.
    pub const REALTIME_INPUT: Self = Self {
        audio: OverflowPolicy::DropOldest,
        metric: OverflowPolicy::DropNewest,
    };

    /// The policy applied to `frame` when its lane is full.
    pub fn policy_for(&self, frame: &Frame) -> OverflowPolicy {
        match frame {
            Frame::Data(DataFrame::Audio(_)) => self.audio,
            Frame::Data(DataFrame::Metric(_)) => self.metric,
            _ => OverflowPolicy::Backpressure,
        }
    }
}

impl Default for OverflowPolicies {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Capacities and policies of one [`Inbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    /// Frames per system lane (per direction). Values below 1 are treated as 1.
    pub system_capacity: usize,
    /// Frames per data lane (per direction). Values below 1 are treated as 1. Keep it small:
    /// every queued audio frame is added latency.
    pub data_capacity: usize,
    /// Overflow behaviour of the data lanes.
    pub policies: OverflowPolicies,
}

impl QueueConfig {
    /// 32 system frames and 32 data frames per direction, [`OverflowPolicies::DEFAULT`].
    pub const DEFAULT: Self = Self {
        system_capacity: 32,
        data_capacity: 32,
        policies: OverflowPolicies::DEFAULT,
    };
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Counters of frames lost to overflow policies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    /// Queued frames evicted by [`OverflowPolicy::DropOldest`].
    pub dropped_oldest: u64,
    /// New frames discarded ([`OverflowPolicy::DropNewest`], or `DropOldest` with nothing of
    /// that kind queued).
    pub dropped_newest: u64,
}

/// Why [`FrameSender::try_send`] did not enqueue a frame. The frame is handed back.
#[derive(Debug, thiserror::Error)]
pub enum TrySendError {
    /// The lane is full and the frame's policy is backpressure.
    #[error("inbox lane full")]
    Full(Frame),
    /// The inbox was dropped.
    #[error("inbox closed")]
    Closed(Frame),
}

/// Why [`Inbox::try_recv`] returned no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TryRecvError {
    /// Nothing queued right now.
    #[error("inbox empty")]
    Empty,
    /// Nothing queued and every sender is gone.
    #[error("inbox disconnected")]
    Disconnected,
}

/// A queued frame and the source tag of the sender that queued it.
struct Slot {
    frame: Frame,
    source: u16,
}

struct Lanes {
    system: VecDeque<Slot>,
    data: VecDeque<Slot>,
}

impl Lanes {
    fn with_capacity(system: usize, data: usize) -> Self {
        Self {
            system: VecDeque::with_capacity(system),
            data: VecDeque::with_capacity(data),
        }
    }
}

struct State {
    downstream: Lanes,
    upstream: Lanes,
    senders: usize,
    receiver_alive: bool,
    /// Direction whose data lane is tried first on the next receive (fairness).
    next_data: Direction,
    stats: QueueStats,
}

impl State {
    fn lanes(&mut self, direction: Direction) -> &mut Lanes {
        match direction {
            Direction::Downstream => &mut self.downstream,
            Direction::Upstream => &mut self.upstream,
        }
    }
}

struct Shared {
    state: Mutex<State>,
    system_capacity: usize,
    data_capacity: usize,
    policies: OverflowPolicies,
    /// Wakes the receiver when a frame arrives or the last sender leaves.
    readable: Notify,
    /// Wakes senders waiting for system-lane space.
    system_space: Notify,
    /// Wakes senders waiting for data-lane space.
    data_space: Notify,
    /// Highest `Interrupt` epoch ever enqueued here (see [`EpochWatch`]).
    epoch_hint: AtomicU32,
    /// Wakes [`EpochWatch::wait_beyond`] when `epoch_hint` rises.
    epoch_advanced: Notify,
}

impl Shared {
    /// Critical sections never panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Frames queued across all lanes of both directions (one brief lock).
    fn queued(&self) -> usize {
        let mut state = self.lock();
        let mut total = 0;
        for direction in [Direction::Downstream, Direction::Upstream] {
            let lanes = state.lanes(direction);
            total += lanes.system.len() + lanes.data.len();
        }
        total
    }
}

/// The receiving side of a processor's queues. Not cloneable: one consumer per inbox.
pub struct Inbox {
    shared: Arc<Shared>,
}

impl Inbox {
    /// Creates an inbox. Buffers for all four lanes are allocated here, once.
    ///
    /// Closing by dropping senders only works for one-way wiring: in a bidirectional chain each
    /// neighbour holds a sender into the other's inbox, so neither ever sees zero senders.
    /// Pipelines stop with a `Stop`/`Cancel` system frame or the `CancellationToken`.
    pub fn new(config: QueueConfig) -> Self {
        let system_capacity = config.system_capacity.max(1);
        let data_capacity = config.data_capacity.max(1);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    downstream: Lanes::with_capacity(system_capacity, data_capacity),
                    upstream: Lanes::with_capacity(system_capacity, data_capacity),
                    senders: 0,
                    receiver_alive: true,
                    next_data: Direction::Downstream,
                    stats: QueueStats::default(),
                }),
                system_capacity,
                data_capacity,
                policies: config.policies,
                readable: Notify::new(),
                system_space: Notify::new(),
                data_space: Notify::new(),
                epoch_hint: AtomicU32::new(0),
                epoch_advanced: Notify::new(),
            }),
        }
    }

    /// A sender whose frames arrive travelling in `direction`.
    ///
    /// Create every sender before the first `recv`: with no live sender and nothing queued,
    /// `recv` reports disconnection.
    pub fn sender(&self, direction: Direction) -> FrameSender {
        self.sender_from(direction, 0)
    }

    /// Like [`Self::sender`], but every frame it queues is tagged with `source`, reported by
    /// [`Self::recv_from`]. Used where one inbox must tell several senders apart (the merge of
    /// parallel branches).
    pub fn sender_from(&self, direction: Direction, source: u16) -> FrameSender {
        self.shared.lock().senders += 1;
        FrameSender {
            shared: Arc::clone(&self.shared),
            direction,
            source,
        }
    }

    /// The next frame: any queued system frame first (downstream, then upstream), otherwise the
    /// next data-lane frame, alternating directions when both have data.
    ///
    /// Returns `None` once every sender is dropped and the inbox is empty.
    pub async fn recv(&mut self) -> Option<(Frame, Direction)> {
        self.recv_from()
            .await
            .map(|(frame, direction, _)| (frame, direction))
    }

    /// [`Self::recv`], also returning the source tag of the sender (see [`Self::sender_from`]).
    pub async fn recv_from(&mut self) -> Option<(Frame, Direction, u16)> {
        // Fast path: no waiter registration when a frame is ready.
        match pop(&self.shared) {
            Ok(item) => return Some(item),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        loop {
            // Register before re-checking so a frame arriving in between cannot be missed.
            let notified = self.shared.readable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match pop(&self.shared) {
                Ok(item) => return Some(item),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    /// Non-blocking [`Inbox::recv`].
    ///
    /// # Errors
    /// [`TryRecvError::Empty`] if nothing is queued, [`TryRecvError::Disconnected`] if
    /// additionally no sender is left.
    pub fn try_recv(&mut self) -> std::result::Result<(Frame, Direction), TryRecvError> {
        pop(&self.shared).map(|(frame, direction, _)| (frame, direction))
    }

    /// Non-blocking [`Inbox::recv_from`].
    ///
    /// # Errors
    /// As [`Self::try_recv`].
    pub fn try_recv_from(&mut self) -> std::result::Result<(Frame, Direction, u16), TryRecvError> {
        pop(&self.shared)
    }

    /// Frames currently queued across all lanes.
    pub fn len(&self) -> usize {
        self.shared.queued()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Frames lost to overflow policies so far.
    pub fn stats(&self) -> QueueStats {
        self.shared.lock().stats
    }

    /// Drops every queued data-lane frame that is stale at `current` ([`Frame::is_stale`]:
    /// tool results are kept) and returns how many were dropped. System frames are never purged.
    pub fn purge_stale(&self, current: TurnEpoch) -> usize {
        let mut state = self.shared.lock();
        let mut removed = 0;
        for direction in [Direction::Downstream, Direction::Upstream] {
            let lane = &mut state.lanes(direction).data;
            let before = lane.len();
            lane.retain(|slot| !slot.frame.is_stale(current));
            removed += before - lane.len();
        }
        drop(state);
        if removed > 0 {
            self.shared.data_space.notify_waiters();
        }
        removed
    }

    /// A handle that observes interrupts arriving in this inbox while its owner is busy (e.g.
    /// blocked pushing into a full neighbour).
    pub fn epoch_watch(&self) -> EpochWatch {
        EpochWatch {
            shared: Arc::clone(&self.shared),
        }
    }

    /// A read-only handle on this inbox's depth and drop counters that stays valid after the
    /// inbox moved into its task (see [`RunningPipeline::stats`](crate::pipeline::RunningPipeline::stats)).
    /// It is not a sender and does not keep the inbox connected.
    pub fn probe(&self) -> InboxProbe {
        InboxProbe {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Read-only view of one [`Inbox`]: queue depth and overflow counters. Cheap to clone; never
/// counts as a sender. Each read takes the inbox's mutex briefly (the one the send and receive
/// path uses), so poll it at a low rate, not per frame.
#[derive(Clone)]
pub struct InboxProbe {
    shared: Arc<Shared>,
}

impl InboxProbe {
    /// Frames lost to overflow policies so far (see [`Inbox::stats`]).
    pub fn stats(&self) -> QueueStats {
        self.shared.lock().stats
    }

    /// Frames queued right now across all lanes (see [`Inbox::len`]).
    pub fn len(&self) -> usize {
        self.shared.queued()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for InboxProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxProbe")
            .field("queued", &self.len())
            .field("stats", &self.stats())
            .finish()
    }
}

/// Observes the newest `Interrupt` epoch enqueued into one inbox, before its owner dequeues it.
///
/// Lets a processor abandon a push of a frame that became stale while it was blocked. Does not
/// count as a sender.
#[derive(Clone)]
pub struct EpochWatch {
    shared: Arc<Shared>,
}

impl EpochWatch {
    /// A sender into the watched inbox (frames arrive travelling downstream). Used to give a
    /// processor its self-wake handle; it counts as a sender, so the inbox stays connected for
    /// as long as the handle lives.
    pub(crate) fn sender(&self) -> FrameSender {
        self.shared.lock().senders += 1;
        FrameSender {
            shared: Arc::clone(&self.shared),
            direction: Direction::Downstream,
            source: 0,
        }
    }

    /// The newest interrupt epoch enqueued so far (`TurnEpoch::ZERO` if none).
    pub fn latest(&self) -> TurnEpoch {
        TurnEpoch::new(self.shared.epoch_hint.load(Ordering::Acquire))
    }

    /// Completes once an interrupt to an epoch newer than `epoch` has been enqueued.
    pub async fn wait_beyond(&self, epoch: TurnEpoch) {
        loop {
            let notified = self.shared.epoch_advanced.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.latest() > epoch {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for EpochWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochWatch")
            .field("latest", &self.latest())
            .finish()
    }
}

/// Takes the next frame by priority (see [`Inbox::recv`]) and wakes senders waiting for space.
fn pop(shared: &Shared) -> std::result::Result<(Frame, Direction, u16), TryRecvError> {
    let mut state = shared.lock();
    for direction in [Direction::Downstream, Direction::Upstream] {
        if let Some(slot) = state.lanes(direction).system.pop_front() {
            drop(state);
            shared.system_space.notify_waiters();
            return Ok((slot.frame, direction, slot.source));
        }
    }
    let first = state.next_data;
    for direction in [first, first.reverse()] {
        if let Some(slot) = state.lanes(direction).data.pop_front() {
            state.next_data = direction.reverse();
            drop(state);
            shared.data_space.notify_waiters();
            return Ok((slot.frame, direction, slot.source));
        }
    }
    if state.senders == 0 {
        Err(TryRecvError::Disconnected)
    } else {
        Err(TryRecvError::Empty)
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        let (downstream, upstream) = {
            let mut state = self.shared.lock();
            state.receiver_alive = false;
            // Release queued buffers now rather than when the last sender goes away.
            let empty = || Lanes::with_capacity(0, 0);
            (
                std::mem::replace(&mut state.downstream, empty()),
                std::mem::replace(&mut state.upstream, empty()),
            )
        };
        drop((downstream, upstream));
        self.shared.system_space.notify_waiters();
        self.shared.data_space.notify_waiters();
    }
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox")
            .field("queued", &self.len())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Pushes frames travelling in one direction into an [`Inbox`]. Cheap to clone.
pub struct FrameSender {
    shared: Arc<Shared>,
    direction: Direction,
    source: u16,
}

impl FrameSender {
    /// Direction of the frames this sender delivers.
    pub const fn direction(&self) -> Direction {
        self.direction
    }

    /// Source tag attached to every frame this sender queues (0 unless created with
    /// [`Inbox::sender_from`]).
    pub const fn source(&self) -> u16 {
        self.source
    }

    /// Enqueues `frame`, waiting for space if its lane is full and its policy is
    /// backpressure. Cancel-safe: dropping the future before completion drops the frame and
    /// leaves the inbox unchanged.
    ///
    /// # Errors
    /// [`Error::ChannelClosed`] if the inbox was dropped.
    pub async fn send(&self, frame: Frame) -> Result<()> {
        let space = match frame.level() {
            FrameLevel::System => &self.shared.system_space,
            FrameLevel::Control | FrameLevel::Data => &self.shared.data_space,
        };
        // Fast path: no waiter registration when there is space.
        let mut frame = match self.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Closed(_)) => return Err(Error::ChannelClosed("inbox")),
            Err(TrySendError::Full(back)) => back,
        };
        loop {
            // Register before re-checking so space freed in between cannot be missed.
            let notified = space.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_send(frame) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(_)) => return Err(Error::ChannelClosed("inbox")),
                Err(TrySendError::Full(back)) => {
                    frame = back;
                    notified.await;
                }
            }
        }
    }

    /// Enqueues `frame` without waiting. Drop policies are applied here; only
    /// backpressure-policy frames can come back as [`TrySendError::Full`].
    ///
    /// # Errors
    /// [`TrySendError::Full`] or [`TrySendError::Closed`], returning the frame.
    pub fn try_send(&self, frame: Frame) -> std::result::Result<(), TrySendError> {
        let is_system = frame.level() == FrameLevel::System;
        let interrupt_epoch = match &frame {
            Frame::System(SystemFrame::Interrupt { new_epoch }) => Some(*new_epoch),
            _ => None,
        };
        let policy = if is_system {
            OverflowPolicy::Backpressure
        } else {
            self.shared.policies.policy_for(&frame)
        };
        let capacity = if is_system {
            self.shared.system_capacity
        } else {
            self.shared.data_capacity
        };

        let mut state = self.shared.lock();
        if !state.receiver_alive {
            return Err(TrySendError::Closed(frame));
        }
        let State {
            downstream,
            upstream,
            stats,
            ..
        } = &mut *state;
        let lanes = match self.direction {
            Direction::Downstream => downstream,
            Direction::Upstream => upstream,
        };
        let lane = if is_system {
            &mut lanes.system
        } else {
            &mut lanes.data
        };

        // (frame to free, whether the new frame was enqueued)
        let source = self.source;
        let (discarded, enqueued) = if lane.len() < capacity {
            lane.push_back(Slot { frame, source });
            (None, true)
        } else {
            match policy {
                OverflowPolicy::Backpressure => return Err(TrySendError::Full(frame)),
                OverflowPolicy::DropNewest => {
                    stats.dropped_newest += 1;
                    (Some(frame), false)
                }
                OverflowPolicy::DropOldest => {
                    let kind = frame.name();
                    match lane.iter().position(|queued| queued.frame.name() == kind) {
                        Some(index) => {
                            let oldest = lane.remove(index).map(|slot| slot.frame);
                            lane.push_back(Slot { frame, source });
                            stats.dropped_oldest += 1;
                            (oldest, true)
                        }
                        None => {
                            stats.dropped_newest += 1;
                            (Some(frame), false)
                        }
                    }
                }
            }
        };
        drop(state);
        // Free evicted/discarded buffers outside the lock.
        drop(discarded);
        if enqueued {
            if let Some(epoch) = interrupt_epoch {
                self.shared
                    .epoch_hint
                    .fetch_max(epoch.get(), Ordering::AcqRel);
                self.shared.epoch_advanced.notify_waiters();
            }
            self.shared.readable.notify_one();
        }
        Ok(())
    }
}

impl Clone for FrameSender {
    fn clone(&self) -> Self {
        self.shared.lock().senders += 1;
        Self {
            shared: Arc::clone(&self.shared),
            direction: self.direction,
            source: self.source,
        }
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        let last = {
            let mut state = self.shared.lock();
            state.senders = state.senders.saturating_sub(1);
            state.senders == 0
        };
        if last {
            self.shared.readable.notify_one();
        }
    }
}

impl std::fmt::Debug for FrameSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSender")
            .field("direction", &self.direction)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
