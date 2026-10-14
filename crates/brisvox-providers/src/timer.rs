//! A reusable deadline timer for the session loops of the self-hosted speech adapters.
//!
//! A loop that rebuilds `clock.sleep_until(..)` for every `select!` pass registers (and drops) a
//! timer entry per audio frame, several times over. [`Timer`] keeps one pinned
//! [`tokio::time::Sleep`] and resets it only when its deadline actually changes, the pattern
//! `brisvox_core::clock` recommends for periodic loops (perf review SHOULD-FIX 2).

use std::pin::Pin;

use brisvox_core::Clock;
use tokio::time::{Instant, Sleep};

/// One deadline, armed with [`set`](Timer::set) and awaited with [`wait`](Timer::wait).
pub(crate) struct Timer {
    sleep: Pin<Box<Sleep>>,
    /// The armed deadline; `None` when nothing is due.
    at: Option<Instant>,
}

impl Timer {
    /// An unarmed timer. Allocates once; call it inside a tokio runtime with a time driver.
    pub(crate) fn new(clock: &dyn Clock) -> Self {
        Self {
            sleep: Box::pin(clock.sleep_until(clock.now())),
            at: None,
        }
    }

    /// Arms the timer for `due` (`None` disarms it). The underlying sleep is touched only when
    /// the deadline differs from the armed one.
    pub(crate) fn set(&mut self, due: Option<Instant>) {
        if due == self.at {
            return;
        }
        if let Some(at) = due {
            self.sleep.as_mut().reset(at);
        }
        self.at = due;
    }

    /// The armed deadline.
    #[cfg(test)]
    pub(crate) fn armed(&self) -> Option<Instant> {
        self.at
    }

    /// Completes when the armed deadline comes; never for a disarmed timer. A deadline that is
    /// already due completes at once without touching the timer (tokio rounds a sleep up to its
    /// next tick, ~15.6 ms on Windows). Cancel-safe.
    pub(crate) async fn wait(&mut self, clock: &dyn Clock) {
        match self.at {
            None => std::future::pending().await,
            Some(at) if at <= clock.now() => {}
            Some(_) => self.sleep.as_mut().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use brisvox_core::SimClock;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_timer_fires_at_its_deadline_and_is_reusable() {
        let clock = SimClock::new();
        let mut timer = Timer::new(&clock);
        let start = clock.now();
        timer.set(Some(start + Duration::from_millis(50)));
        timer.wait(&clock).await;
        assert!(clock.now() >= start + Duration::from_millis(50));
        // Re-armed for later: waits again.
        timer.set(Some(start + Duration::from_millis(120)));
        timer.wait(&clock).await;
        assert!(clock.now() >= start + Duration::from_millis(120));
        // A due deadline returns at once.
        let before = clock.now();
        timer.wait(&clock).await;
        assert_eq!(clock.now(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disarmed_timer_never_fires_and_an_unchanged_deadline_is_not_reset() {
        let clock = SimClock::new();
        let mut timer = Timer::new(&clock);
        let waited = tokio::time::timeout(Duration::from_secs(5), timer.wait(&clock)).await;
        assert!(waited.is_err(), "a disarmed timer must not complete");
        let due = clock.now() + Duration::from_secs(1);
        timer.set(Some(due));
        timer.set(Some(due));
        assert_eq!(timer.armed(), Some(due));
        timer.set(None);
        assert_eq!(timer.armed(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn moving_the_deadline_later_while_waiting_is_honoured() {
        let clock = SimClock::new();
        let mut timer = Timer::new(&clock);
        let start = clock.now();
        timer.set(Some(start + Duration::from_millis(10)));
        // A cancelled wait (like a select! arm losing) leaves the timer usable.
        let _ = tokio::time::timeout(Duration::from_millis(5), timer.wait(&clock)).await;
        timer.set(Some(start + Duration::from_millis(80)));
        timer.wait(&clock).await;
        assert!(clock.now() >= start + Duration::from_millis(80));
    }
}
