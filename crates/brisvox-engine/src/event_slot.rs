//! [`EventSlot`]: the typed mailbox behind `SystemFrame::Wake`.
//!
//! A stage that needs a helper task to hand it something (a timer tick, a Smart Turn verdict, a
//! finals-gate deadline) owns an `EventSlot<T>` and gives clones to its helpers. A helper pushes
//! the typed event into the slot and then sends `SystemFrame::Wake{token}` through the stage's
//! `ProcessorContext::self_sender`. The stage's `process` sees the `Wake` in the same single
//! task and order as every other frame and pops the events. Wake frames carry no payload, so
//! `Frame` keeps its derived `PartialEq`/`Eq` and its 64-byte size, and the stage stays
//! single-threaded and deterministic (the fallback design was `Custom(Arc<dyn Any>)` frames).
//!
//! The slot is a bounded FIFO behind a `std::sync::Mutex` that is never held across an await.
//! Its overflow policy is chosen at construction ([`SlotOverflow`]) and every loss is counted.
//! A wake without a matching event is harmless (the stage pops nothing).
//!
//! # Wake coalescing
//! The slot tracks its wake in three states: idle, *sending* (a helper is awaiting space in the
//! system lane) and *queued* (a `Wake` sits in the lane). [`EventSlot::push_and_wake`] skips its
//! send only in the *queued* state, and the state returns to idle when [`EventSlot::drain`] or
//! [`EventSlot::pop`] leaves the slot empty. A helper that ticks every few milliseconds while
//! its stage is blocked (an awaited downstream push under backpressure) therefore cannot fill
//! the stage's system lane, which `UserStartedSpeaking`, `Interrupt` and `Error` share: after
//! its first wake is queued, later pushes coalesce into it. Several events thus share one wake,
//! so a stage must empty the slot on every `Wake` (`drain`, or `pop` until `None`); popping a
//! single event and leaving the rest strands them until the next push after an empty slot.
//!
//! A helper that finds another helper still *sending* does not trust that send (it may be
//! cancelled, e.g. by a `TaskGuard` drop or a timeout) and sends its own wake. So the number of
//! wakes in flight is bounded by the number of helpers concurrently awaiting the lane, never
//! by the number of pushes, and a queued event is never left without a wake behind it. This
//! holds for a rejected push too: a rejected event proves the slot is non-empty, so it still
//! wakes the stage unless a wake is already queued.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use brisvox_core::FrameSender;
use brisvox_core::frame::{Frame, SystemFrame};

use crate::error::Result;

/// What a full [`EventSlot`] does with a new event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotOverflow {
    /// Evict the oldest queued event and keep the new one. For state-like events where the
    /// newest value wins (a verdict, a deadline).
    DropOldest,
    /// Keep what is queued and reject the new event. For events that must not be reordered or
    /// replaced.
    RejectNewest,
}

/// What [`EventSlot::push`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome<T> {
    /// Queued; nothing was lost.
    Queued,
    /// Queued after evicting the returned oldest event ([`SlotOverflow::DropOldest`]).
    Evicted(T),
    /// The slot was full and the new event is handed back ([`SlotOverflow::RejectNewest`]).
    Rejected(T),
}

struct Inner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    overflow: SlotOverflow,
    lost: AtomicU64,
    /// One of [`WAKE_IDLE`], [`WAKE_SENDING`], [`WAKE_QUEUED`].
    wake_state: AtomicU8,
}

/// No wake in flight for the current contents (the slot was emptied since the last one).
const WAKE_IDLE: u8 = 0;
/// A helper is awaiting space in the stage's system lane to queue a `Wake`.
const WAKE_SENDING: u8 = 1;
/// A `Wake` is queued in the stage's system lane and the slot was not emptied since.
const WAKE_QUEUED: u8 = 2;

/// Returns the state from *sending* to *idle* if `push_and_wake` is cancelled or fails before
/// its wake was queued, so a later push wakes the stage.
struct SendingGuard<'a> {
    state: &'a AtomicU8,
    armed: bool,
}

impl Drop for SendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.state.compare_exchange(
                WAKE_SENDING,
                WAKE_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// A bounded, typed FIFO shared between a stage and its helper tasks. Cheap to clone.
pub struct EventSlot<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for EventSlot<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> std::fmt::Debug for EventSlot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSlot")
            .field("len", &self.len())
            .field("capacity", &self.inner.capacity)
            .field("overflow", &self.inner.overflow)
            .field("lost", &self.lost())
            .finish()
    }
}

/// The frame a helper sends through `ProcessorContext::self_sender` after pushing an event.
pub fn wake_frame(token: u32) -> Frame {
    SystemFrame::Wake { token }.into()
}

impl<T> EventSlot<T> {
    /// A slot holding at most `capacity` events (at least 1 is used), allocated up front so a
    /// push never allocates.
    pub fn new(capacity: usize, overflow: SlotOverflow) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(VecDeque::with_capacity(capacity)),
                capacity,
                overflow,
                lost: AtomicU64::new(0),
                wake_state: AtomicU8::new(WAKE_IDLE),
            }),
        }
    }

    /// Critical sections never panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, VecDeque<T>> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `event` according to the overflow policy. The returned event (if any) is dropped
    /// by the caller outside the lock.
    pub fn push(&self, event: T) -> PushOutcome<T> {
        let mut queue = self.lock();
        if queue.len() < self.inner.capacity {
            queue.push_back(event);
            return PushOutcome::Queued;
        }
        self.inner.lost.fetch_add(1, Ordering::Relaxed);
        match self.inner.overflow {
            SlotOverflow::RejectNewest => PushOutcome::Rejected(event),
            SlotOverflow::DropOldest => match queue.pop_front() {
                Some(oldest) => {
                    queue.push_back(event);
                    PushOutcome::Evicted(oldest)
                }
                // Unreachable for capacity >= 1; reject rather than lose the event silently.
                None => PushOutcome::Rejected(event),
            },
        }
    }

    /// Queues `event` and wakes the owning stage through `me` (its `self_sender`) with `token`.
    /// Awaits space in the stage's system lane, so call it from a helper task, never from the
    /// stage's own `process`.
    ///
    /// The wake is sent whenever the stage may not know about the queued events: also when an
    /// older event was evicted (the stage still has something to pop) and also when the event
    /// was rejected (the slot is full, so there is something to pop and an earlier helper's
    /// wake may have been cancelled). It is skipped only while a wake of this slot is already
    /// queued in the lane (see the module docs): that wake will find this event too.
    /// Cancel-safe: a cancelled or failed send returns the slot to idle, and a helper that
    /// meets another helper's unfinished send sends its own wake, so no queued event is left
    /// without a wake behind it.
    ///
    /// # Errors
    /// [`Error::Core`](crate::Error::Core) with `ChannelClosed` when the stage is gone.
    pub async fn push_and_wake(
        &self,
        event: T,
        me: &FrameSender,
        token: u32,
    ) -> Result<PushOutcome<T>> {
        let outcome = self.push(event);
        let state = &self.inner.wake_state;
        // Claim the sending state (also when another helper holds it: its send may be
        // cancelled) unless a wake is already queued.
        let mut seen = state.load(Ordering::Acquire);
        loop {
            if seen == WAKE_QUEUED {
                return Ok(outcome);
            }
            match state.compare_exchange(seen, WAKE_SENDING, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => seen = now,
            }
        }
        let mut guard = SendingGuard { state, armed: true };
        me.send(wake_frame(token)).await?;
        guard.armed = false;
        // A drain that ran meanwhile already reset the state to idle; keep that.
        let _ = state.compare_exchange(
            WAKE_SENDING,
            WAKE_QUEUED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(outcome)
    }

    /// The oldest queued event. Emptying the slot (including a `pop` on an empty slot) lets the
    /// next `push_and_wake` send a wake again.
    pub fn pop(&self) -> Option<T> {
        let mut queue = self.lock();
        let event = queue.pop_front();
        if queue.is_empty() {
            self.inner.wake_state.store(WAKE_IDLE, Ordering::Release);
        }
        event
    }

    /// Removes and returns every queued event, oldest first, and lets the next `push_and_wake`
    /// send a wake again.
    pub fn drain(&self) -> Vec<T> {
        let mut queue = self.lock();
        let events = queue.drain(..).collect();
        self.inner.wake_state.store(WAKE_IDLE, Ordering::Release);
        events
    }

    /// Events queued now.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of queued events.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Events lost to the overflow policy so far (evicted or rejected).
    pub fn lost(&self) -> u64 {
        self.inner.lost.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use brisvox_core::{Clock, Links};
    use brisvox_core::{
        Direction, Pipeline, Processor, ProcessorContext, QueueConfig, SimClock,
        frame::SystemFrame as Sys,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::guard::TaskGuard;

    #[test]
    fn fifo_with_drop_oldest() {
        let slot = EventSlot::new(2, SlotOverflow::DropOldest);
        assert_eq!(slot.capacity(), 2);
        assert_eq!(slot.push(1), PushOutcome::Queued);
        assert_eq!(slot.push(2), PushOutcome::Queued);
        assert_eq!(slot.push(3), PushOutcome::Evicted(1));
        assert_eq!(slot.lost(), 1);
        assert_eq!(slot.len(), 2);
        assert_eq!(slot.pop(), Some(2));
        assert_eq!(slot.drain(), vec![3]);
        assert!(slot.is_empty());
        assert_eq!(slot.pop(), None);
    }

    #[test]
    fn reject_newest_keeps_what_is_queued() {
        let slot = EventSlot::new(1, SlotOverflow::RejectNewest);
        assert_eq!(slot.push("a"), PushOutcome::Queued);
        assert_eq!(slot.push("b"), PushOutcome::Rejected("b"));
        assert_eq!(slot.lost(), 1);
        assert_eq!(slot.drain(), vec!["a"]);
    }

    #[test]
    fn capacity_is_at_least_one_and_clones_share_the_queue() {
        let slot = EventSlot::new(0, SlotOverflow::RejectNewest);
        assert_eq!(slot.capacity(), 1);
        let clone = slot.clone();
        clone.push(7);
        assert_eq!(slot.pop(), Some(7));
        assert!(format!("{slot:?}").contains("EventSlot"));
    }

    #[test]
    fn a_poisoned_lock_does_not_break_the_slot() {
        let slot = EventSlot::new(2, SlotOverflow::RejectNewest);
        let clone = slot.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.inner.queue.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(slot.push(1), PushOutcome::Queued);
        assert_eq!(slot.pop(), Some(1));
    }

    /// A stage that is woken by a helper with a typed event, and records what it popped and when.
    struct Receiver {
        slot: EventSlot<&'static str>,
        clock: SimClock,
        seen: Arc<Mutex<Vec<String>>>,
        helper: Option<TaskGuard>,
    }

    impl Processor for Receiver {
        fn name(&self) -> &'static str {
            "receiver"
        }

        async fn process(
            &mut self,
            frame: Frame,
            dir: Direction,
            ctx: &mut ProcessorContext,
        ) -> brisvox_core::Result<()> {
            match &frame {
                Frame::System(Sys::Start) => {
                    let me = ctx.self_sender().expect("attached");
                    let (slot, clock) = (self.slot.clone(), self.clock);
                    self.helper = Some(TaskGuard::spawn("verdicts", async move {
                        clock.sleep(Duration::from_millis(30)).await;
                        slot.push_and_wake("first", &me, 4).await.unwrap();
                        clock.sleep(Duration::from_millis(70)).await;
                        slot.push_and_wake("second", &me, 4).await.unwrap();
                    }));
                }
                Frame::System(Sys::Wake { token }) => {
                    for event in self.slot.drain() {
                        self.seen.lock().unwrap().push(format!(
                            "{event}/{token}@{}",
                            self.clock.elapsed().as_millis()
                        ));
                    }
                    return Ok(());
                }
                _ => {}
            }
            ctx.forward(frame, dir).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn push_and_wake_hands_a_typed_event_to_a_single_threaded_stage() {
        let clock = SimClock::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut running = Pipeline::new("p")
            .with_queue_config(QueueConfig::DEFAULT)
            .then(Receiver {
                slot: EventSlot::new(4, SlotOverflow::RejectNewest),
                clock,
                seen: Arc::clone(&seen),
                helper: None,
            })
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        clock.sleep(Duration::from_millis(200)).await;
        assert_eq!(*seen.lock().unwrap(), ["first/4@30", "second/4@100"]);
        running.stop(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_push_on_a_full_unwoken_slot_still_wakes_the_stage() {
        let mut inbox = brisvox_core::Inbox::new(QueueConfig::DEFAULT);
        let me = inbox.sender(Direction::Downstream);

        // Full slot, no wake pending (the first event was queued without a wake, as after a
        // cancelled helper): the rejected push must still wake the stage exactly once.
        let reject = EventSlot::new(1, SlotOverflow::RejectNewest);
        assert_eq!(reject.push(1), PushOutcome::Queued);
        let outcome = reject.push_and_wake(2, &me, 2).await.unwrap();
        assert_eq!(outcome, PushOutcome::Rejected(2));
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(2));
        // A further rejected push coalesces into the queued wake.
        reject.push_and_wake(3, &me, 2).await.unwrap();
        assert!(inbox.try_recv().is_err(), "one wake is enough");
        assert_eq!(reject.drain(), vec![1]);

        // DropOldest: an evicting push on a full, unwoken slot wakes the stage.
        let evict = EventSlot::new(1, SlotOverflow::DropOldest);
        assert_eq!(evict.push(1), PushOutcome::Queued);
        let outcome = evict.push_and_wake(2, &me, 6).await.unwrap();
        assert_eq!(outcome, PushOutcome::Evicted(1));
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(6));
        assert!(inbox.try_recv().is_err(), "exactly one wake");
        assert_eq!(evict.drain(), vec![2]);

        // The drain re-arms the slot; a closed stage is an error.
        drop(inbox);
        assert!(evict.push_and_wake(3, &me, 7).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_push_after_a_cancelled_wake_does_not_strand_the_event() {
        let config = QueueConfig {
            system_capacity: 1,
            ..QueueConfig::DEFAULT
        };
        let mut inbox = brisvox_core::Inbox::new(config);
        let me = inbox.sender(Direction::Downstream);
        me.send(Sys::Start.into()).await.unwrap();

        // H1 queues E1 and is cancelled while waiting for lane space.
        let slot = EventSlot::new(1, SlotOverflow::RejectNewest);
        let cancelled =
            tokio::time::timeout(Duration::from_millis(5), slot.push_and_wake(1, &me, 9)).await;
        assert!(cancelled.is_err());
        assert_eq!(slot.len(), 1);

        // H2's push is rejected (slot full), but it must still wake the stage.
        assert_eq!(inbox.try_recv().unwrap().0, Sys::Start.into());
        let outcome = slot.push_and_wake(2, &me, 9).await.unwrap();
        assert_eq!(outcome, PushOutcome::Rejected(2));
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(9));
        assert_eq!(slot.drain(), vec![1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_stage_gets_one_wake_not_a_flood() {
        let mut inbox = brisvox_core::Inbox::new(QueueConfig::DEFAULT);
        let me = inbox.sender(Direction::Downstream);
        let slot = EventSlot::new(8, SlotOverflow::DropOldest);

        // The stage does not run; a fast helper pushes far more events than the system lane
        // (32) holds. None of these awaits may block.
        for i in 0..200u32 {
            tokio::time::timeout(Duration::from_millis(1), slot.push_and_wake(i, &me, 3))
                .await
                .expect("never blocks on a full system lane")
                .unwrap();
        }
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(3));
        assert!(inbox.try_recv().is_err(), "exactly one wake queued");
        assert_eq!(slot.drain(), (192..200).collect::<Vec<_>>());

        // After the stage emptied the slot the next push wakes it again.
        slot.push_and_wake(1000, &me, 3).await.unwrap();
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(3));
        // pop until None also re-arms.
        slot.push_and_wake(1001, &me, 3).await.unwrap();
        assert!(inbox.try_recv().is_err());
        assert_eq!(slot.pop(), Some(1000));
        assert_eq!(slot.pop(), Some(1001));
        assert_eq!(slot.pop(), None);
        slot.push_and_wake(1002, &me, 3).await.unwrap();
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(3));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wake_send_re_arms_the_slot() {
        let config = QueueConfig {
            system_capacity: 1,
            ..QueueConfig::DEFAULT
        };
        let mut inbox = brisvox_core::Inbox::new(config);
        let me = inbox.sender(Direction::Downstream);
        // Fill the one system slot with something else so the wake send has to wait.
        me.send(Sys::Start.into()).await.unwrap();

        let slot = EventSlot::new(4, SlotOverflow::RejectNewest);
        let cancelled =
            tokio::time::timeout(Duration::from_millis(5), slot.push_and_wake(1, &me, 9)).await;
        assert!(cancelled.is_err(), "the send was waiting for space");

        // Space again: the next push must send a wake (the cancelled one left no stale flag).
        assert_eq!(inbox.try_recv().unwrap().0, Sys::Start.into());
        slot.push_and_wake(2, &me, 9).await.unwrap();
        assert_eq!(inbox.try_recv().unwrap().0, wake_frame(9));
        assert_eq!(slot.drain(), vec![1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_aborted_helper_does_not_strand_events_of_a_coalesced_helper() {
        let clock = SimClock::new();
        let config = QueueConfig {
            system_capacity: 1,
            ..QueueConfig::DEFAULT
        };
        let mut inbox = brisvox_core::Inbox::new(config);
        let me = inbox.sender(Direction::Downstream);
        me.send(Sys::Start.into()).await.unwrap();
        let slot = EventSlot::new(4, SlotOverflow::RejectNewest);

        // Helper A claims the wake and blocks on the full lane; helper B pushes while A is
        // still sending, so it must not rely on A's wake.
        let a = {
            let (slot, me) = (slot.clone(), me.clone());
            TaskGuard::spawn("helper-a", async move {
                let _ = slot.push_and_wake(1, &me, 9).await;
            })
        };
        clock.sleep(Duration::from_millis(5)).await;
        let b = {
            let (slot, me) = (slot.clone(), me.clone());
            TaskGuard::spawn("helper-b", async move {
                let _ = slot.push_and_wake(2, &me, 9).await;
            })
        };
        clock.sleep(Duration::from_millis(5)).await;
        assert_eq!(slot.len(), 2);

        // A is aborted (its TaskGuard drops); space frees up afterwards.
        drop(a);
        clock.sleep(Duration::from_millis(5)).await;
        assert_eq!(inbox.try_recv().unwrap().0, Sys::Start.into());
        clock.sleep(Duration::from_millis(5)).await;
        assert_eq!(
            inbox.try_recv().unwrap().0,
            wake_frame(9),
            "B's own wake reaches the stage"
        );
        assert_eq!(slot.drain(), vec![1, 2]);
        drop(b);
    }
}
