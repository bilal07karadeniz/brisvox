//! [`DeadlineTimer`]: the one timer task of a stage.
//!
//! A stage that has deadlines (the turn machine's pause deadline, the finals gate, the response
//! backstop) keeps ONE helper task for all of them instead of one task per deadline. The stage
//! tells the timer the earliest deadline with [`DeadlineTimer::set`] (`None`: nothing to wait
//! for); when the clock reaches it, the helper pushes an event into the stage's
//! [`EventSlot`] and wakes the stage through its self-sender (`SystemFrame::Wake`). The stage
//! handles whatever is due, computes the next earliest deadline and calls `set` again.
//!
//! The protocol is level-triggered: the stage decides what is due by comparing its deadlines with
//! `clock.now()` whenever it runs, not by the event it received, so a lost, duplicated or
//! coalesced event cannot make a deadline be missed (the helper fires again after every `set` that
//! follows a fire, even when the value is unchanged).
//!
//! All time comes from the injected clock ([`brisvox_core::clock::wait_until`]), so a paused-time
//! simulation fires the timer deterministically. The helper is owned by the timer (a
//! [`TaskGuard`]) and is aborted when the stage drops it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use brisvox_core::{FrameSender, SharedClock};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::event_slot::EventSlot;
use crate::guard::TaskGuard;

/// A single re-armable deadline with its helper task. See the module docs.
#[derive(Debug)]
pub struct DeadlineTimer {
    tx: watch::Sender<Option<Instant>>,
    last: Option<Instant>,
    fired: Arc<AtomicBool>,
    _task: TaskGuard,
}

impl DeadlineTimer {
    /// Starts the helper. When a deadline passes it pushes `event()` into `slot` and sends a
    /// `Wake{token}` through `me` (the stage's own sender, `ProcessorContext::self_sender`).
    ///
    /// Must run inside a tokio runtime. The helper ends when the timer is dropped or the stage's
    /// inbox is gone.
    pub fn spawn<E>(
        clock: SharedClock,
        slot: EventSlot<E>,
        event: fn() -> E,
        me: FrameSender,
        token: u32,
    ) -> Self
    where
        E: Send + 'static,
    {
        let (tx, rx) = watch::channel(None);
        let fired = Arc::new(AtomicBool::new(false));
        let task = TaskGuard::spawn(
            "deadline_timer",
            run(clock, rx, slot, event, me, token, Arc::clone(&fired)),
        );
        Self {
            tx,
            last: None,
            fired,
            _task: task,
        }
    }

    /// Makes `deadline` the one pending deadline (`None`: cancel). Call it after every pass that
    /// may have changed the stage's deadlines; it does nothing when the deadline is the same as
    /// before, unless the timer fired since the last call (the helper then waits for this call).
    pub fn set(&mut self, deadline: Option<Instant>) {
        let fired = self.fired.swap(false, Ordering::AcqRel);
        if !fired && deadline == self.last {
            return;
        }
        self.last = deadline;
        self.tx.send_replace(deadline);
    }

    /// The deadline last passed to [`Self::set`].
    pub const fn deadline(&self) -> Option<Instant> {
        self.last
    }
}

async fn run<E>(
    clock: SharedClock,
    mut rx: watch::Receiver<Option<Instant>>,
    slot: EventSlot<E>,
    event: fn() -> E,
    me: FrameSender,
    token: u32,
    fired: Arc<AtomicBool>,
) where
    E: Send + 'static,
{
    loop {
        let deadline = *rx.borrow_and_update();
        let Some(deadline) = deadline else {
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        };
        tokio::select! {
            biased;
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = brisvox_core::clock::wait_until(&*clock, deadline) => {
                fired.store(true, Ordering::Release);
                if slot.push_and_wake(event(), &me, token).await.is_err() {
                    return;
                }
                // Wait for the stage to handle the event and set the next deadline.
                if rx.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use brisvox_core::frame::{Frame, SystemFrame};
    use brisvox_core::{Clock, Direction, Inbox, QueueConfig, SimClock};

    use super::*;
    use crate::event_slot::SlotOverflow;

    const MS: Duration = Duration::from_millis(1);

    struct Rig {
        clock: SimClock,
        inbox: Inbox,
        slot: EventSlot<u8>,
        timer: DeadlineTimer,
        origin: Instant,
    }

    fn rig() -> Rig {
        let clock = SimClock::new();
        let inbox = Inbox::new(QueueConfig::DEFAULT);
        let slot = EventSlot::new(4, SlotOverflow::DropOldest);
        let timer = DeadlineTimer::spawn(
            Arc::new(clock),
            slot.clone(),
            || 7,
            inbox.sender(Direction::Downstream),
            5,
        );
        Rig {
            clock,
            inbox,
            slot,
            timer,
            origin: clock.now(),
        }
    }

    fn wakes(inbox: &mut Inbox) -> usize {
        let mut n = 0;
        while let Ok((frame, _)) = inbox.try_recv() {
            assert_eq!(frame, Frame::System(SystemFrame::Wake { token: 5 }));
            n += 1;
        }
        n
    }

    #[tokio::test(start_paused = true)]
    async fn fires_once_at_the_deadline() {
        let mut r = rig();
        r.timer.set(Some(r.origin + 100 * MS));
        assert_eq!(r.timer.deadline(), Some(r.origin + 100 * MS));
        r.clock.sleep(99 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 0, "not yet");
        r.clock.sleep(2 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 1);
        assert_eq!(r.slot.drain(), vec![7]);
        // Nothing fires again until the stage sets a deadline.
        r.clock.sleep(Duration::from_secs(5)).await;
        assert_eq!(wakes(&mut r.inbox), 0);
        assert!(r.slot.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_deadline_replaces_the_old_and_none_cancels() {
        let mut r = rig();
        r.timer.set(Some(r.origin + 100 * MS));
        r.clock.sleep(50 * MS).await;
        r.timer.set(Some(r.origin + 300 * MS));
        r.clock.sleep(100 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 0, "the first deadline was replaced");
        r.timer.set(None);
        r.clock.sleep(Duration::from_secs(1)).await;
        assert_eq!(wakes(&mut r.inbox), 0, "cancelled");
        assert!(r.slot.is_empty());
        // Re-arm in the past: fires at once.
        r.timer.set(Some(r.origin));
        r.clock.sleep(2 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn setting_the_same_deadline_after_a_fire_fires_again() {
        let mut r = rig();
        let deadline = r.origin + 10 * MS;
        r.timer.set(Some(deadline));
        r.clock.sleep(20 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 1);
        let _ = r.slot.drain();
        // The stage handled the event but the deadline (already past) is unchanged: the timer
        // still fires again, because it waits for a `set` after every fire.
        r.timer.set(Some(deadline));
        r.clock.sleep(2 * MS).await;
        assert_eq!(wakes(&mut r.inbox), 1);
        // Repeated identical sets without a fire in between do nothing more.
        let _ = r.slot.drain();
        r.timer.set(Some(deadline));
        r.timer.set(Some(deadline));
        r.clock.sleep(5 * MS).await;
        assert_eq!(
            wakes(&mut r.inbox),
            1,
            "one fire for the first set after the fire"
        );
    }
}
