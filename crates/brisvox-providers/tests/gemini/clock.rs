//! A clock that runs ahead of real time, to prove that deadlines come from the injected clock.

use std::time::{Duration, SystemTime};

use brisvox_core::Clock;
use tokio::time::{Instant, Sleep};

/// `now()` is real time plus `skew`; timers are tokio's, so every deadline the adapter computes
/// from this clock lies `skew` later in real time than the same deadline on a `SystemClock`.
/// An adapter that read `Instant::now()` or slept with `tokio::time` directly would time out
/// `skew` earlier.
#[derive(Debug)]
pub struct SkewedClock {
    skew: Duration,
}

impl SkewedClock {
    pub fn new(skew: Duration) -> Self {
        Self { skew }
    }
}

impl Clock for SkewedClock {
    fn now(&self) -> Instant {
        Instant::now() + self.skew
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        tokio::time::sleep_until(deadline)
    }
}
