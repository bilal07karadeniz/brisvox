//! A retry budget: a token bucket on the injected clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use tokio::time::Instant;

/// Settings of a [`RetryBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetConfig {
    /// Tokens when full (and at the start).
    pub capacity: u32,
    /// One token comes back per this much time (zero: the bucket is always full).
    pub refill_every: Duration,
}

impl BudgetConfig {
    /// One call: 3 retries, one more every 20 s.
    pub const SESSION: Self = Self {
        capacity: 3,
        refill_every: Duration::from_secs(20),
    };

    /// The process: 50 retries, 10 more per second (an outage costs every live call at most
    /// one recovery before the bucket, not the providers, says no).
    pub const PROCESS: Self = Self {
        capacity: 50,
        refill_every: Duration::from_millis(100),
    };
}

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    /// When the last whole token was credited.
    since: Instant,
}

/// A token bucket of retries. One per call (inside each wrapper) and one per process (shared).
#[derive(Debug)]
pub struct RetryBudget {
    config: BudgetConfig,
    clock: SharedClock,
    bucket: Mutex<Bucket>,
    granted: AtomicU64,
    denied: AtomicU64,
}

impl RetryBudget {
    /// A full bucket.
    pub fn new(config: BudgetConfig, clock: SharedClock) -> Self {
        let since = clock.now();
        Self {
            config,
            clock,
            bucket: Mutex::new(Bucket {
                tokens: config.capacity,
                since,
            }),
            granted: AtomicU64::new(0),
            denied: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Bucket> {
        self.bucket.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Credits the tokens earned since the last credit.
    fn refill(&self, bucket: &mut Bucket) {
        let now = self.clock.now();
        if self.config.refill_every.is_zero() {
            bucket.tokens = self.config.capacity;
            bucket.since = now;
            return;
        }
        if bucket.tokens >= self.config.capacity {
            // A full bucket earns nothing: the next token is a whole period after it is spent.
            bucket.since = now;
            return;
        }
        let elapsed = now.saturating_duration_since(bucket.since);
        let earned = elapsed.as_nanos() / self.config.refill_every.as_nanos().max(1);
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        bucket.tokens = bucket
            .tokens
            .saturating_add(earned)
            .min(self.config.capacity);
        bucket.since = if bucket.tokens >= self.config.capacity {
            now
        } else {
            bucket
                .since
                .checked_add(self.config.refill_every.saturating_mul(earned))
                .unwrap_or(now)
        };
    }

    /// Takes one token: `false` (counted) when the bucket is empty.
    pub fn try_take(&self) -> bool {
        let taken = {
            let mut bucket = self.lock();
            self.refill(&mut bucket);
            if bucket.tokens > 0 {
                bucket.tokens -= 1;
                true
            } else {
                false
            }
        };
        let counter = if taken { &self.granted } else { &self.denied };
        counter.fetch_add(1, Ordering::Relaxed);
        taken
    }

    /// Gives back a token taken by [`Self::try_take`] that was not used.
    pub fn refund(&self) {
        let mut bucket = self.lock();
        bucket.tokens = bucket.tokens.saturating_add(1).min(self.config.capacity);
    }

    /// Tokens available now.
    pub fn available(&self) -> u32 {
        let mut bucket = self.lock();
        self.refill(&mut bucket);
        bucket.tokens
    }

    /// Tokens handed out so far (a refunded token stays counted).
    pub fn granted(&self) -> u64 {
        self.granted.load(Ordering::Relaxed)
    }

    /// Requests refused so far for lack of tokens.
    pub fn denied(&self) -> u64 {
        self.denied.load(Ordering::Relaxed)
    }
}
