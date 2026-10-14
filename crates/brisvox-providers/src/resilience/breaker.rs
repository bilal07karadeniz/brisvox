//! A per-provider circuit breaker on the injected clock (closed, open, half-open).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_core::clock::deadline_after;
use tokio::time::Instant;

/// Settings of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that open the breaker. Default 5. Zero behaves as one.
    pub failure_threshold: u32,
    /// How long an open breaker refuses attempts before it lets one probe through. Default 30 s.
    pub open_for: Duration,
}

impl BreakerConfig {
    /// Five consecutive failures open the breaker for 30 s.
    pub const DEFAULT: Self = Self {
        failure_threshold: 5,
        open_for: Duration::from_secs(30),
    };
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where a [`CircuitBreaker`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Attempts go through; consecutive failures are counted.
    Closed,
    /// Attempts are refused until the open period ends.
    Open,
    /// The open period ended: one probe attempt goes through, the rest are refused.
    HalfOpen,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen { probing: bool },
}

/// Counters of a [`CircuitBreaker`] (monotonic since it was built).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BreakerStats {
    /// Attempts let through (probes included).
    pub admitted: u64,
    /// Attempts refused (open, or half-open with the probe taken).
    pub refused: u64,
    /// Attempts that failed.
    pub failures: u64,
    /// Times the breaker opened (from closed or after a failed probe).
    pub opened: u64,
    /// Probes let through while half-open.
    pub probes: u64,
    /// Times a probe (or another success while half-open) closed the breaker again (on
    /// probation included, see [`Permit::probation`]).
    pub closed: u64,
}

/// A circuit breaker for one provider instance, shared by every call of the process (put it in
/// an `Arc`). All transitions happen under a short mutex that is never held across an await.
#[derive(Debug)]
pub struct CircuitBreaker {
    name: &'static str,
    config: BreakerConfig,
    clock: SharedClock,
    state: Mutex<State>,
    admitted: AtomicU64,
    refused: AtomicU64,
    failures: AtomicU64,
    opened: AtomicU64,
    probes: AtomicU64,
    closed: AtomicU64,
}

impl CircuitBreaker {
    /// A closed breaker for the provider `name`.
    pub fn new(name: &'static str, config: BreakerConfig, clock: SharedClock) -> Self {
        Self {
            name,
            config,
            clock,
            state: Mutex::new(State::Closed { failures: 0 }),
            admitted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            opened: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        }
    }

    /// The provider name the breaker was built for.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current state (an open breaker whose period ended reads as half-open).
    pub fn state(&self) -> BreakerState {
        match *self.lock() {
            State::Closed { .. } => BreakerState::Closed,
            State::Open { until } if self.clock.now() < until => BreakerState::Open,
            State::Open { .. } | State::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> BreakerStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        BreakerStats {
            admitted: get(&self.admitted),
            refused: get(&self.refused),
            failures: get(&self.failures),
            opened: get(&self.opened),
            probes: get(&self.probes),
            closed: get(&self.closed),
        }
    }

    /// Asks to make an attempt: `None` when the breaker refuses it. The returned permit must
    /// report the outcome ([`Permit::success`] / [`Permit::failure`]); dropping it reports
    /// nothing (an abandoned attempt) and frees the probe slot.
    pub fn try_acquire(self: &Arc<Self>) -> Option<Permit> {
        let probe = {
            let mut state = self.lock();
            match *state {
                State::Closed { .. } => false,
                State::Open { until } if self.clock.now() < until => {
                    drop(state);
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                State::Open { .. } | State::HalfOpen { probing: false } => {
                    *state = State::HalfOpen { probing: true };
                    true
                }
                State::HalfOpen { probing: true } => {
                    drop(state);
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        };
        self.admitted.fetch_add(1, Ordering::Relaxed);
        if probe {
            self.probes.fetch_add(1, Ordering::Relaxed);
        }
        Some(Permit {
            breaker: Arc::clone(self),
            probe,
            settled: false,
        })
    }

    fn on_success(&self) {
        let mut state = self.lock();
        match *state {
            State::Closed { .. } => *state = State::Closed { failures: 0 },
            State::HalfOpen { .. } => {
                *state = State::Closed { failures: 0 };
                drop(state);
                self.closed.fetch_add(1, Ordering::Relaxed);
            }
            // A late success of an attempt admitted before the breaker opened: the open period
            // stands (a provider that fails most attempts must not flap).
            State::Open { .. } => {}
        }
    }

    fn on_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let now = self.clock.now();
        let open = State::Open {
            until: deadline_after(now, self.config.open_for),
        };
        let mut state = self.lock();
        let opened = match *state {
            State::Closed { failures } => {
                let failures = failures.saturating_add(1);
                if failures >= self.config.failure_threshold.max(1) {
                    *state = open;
                    true
                } else {
                    *state = State::Closed { failures };
                    false
                }
            }
            State::HalfOpen { .. } => {
                *state = open;
                true
            }
            State::Open { .. } => false,
        };
        drop(state);
        if opened {
            self.opened.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_probation(&self) {
        let mut state = self.lock();
        if let State::HalfOpen { probing: true } = *state {
            // One failure short of the threshold: the next failure opens it again at once.
            *state = State::Closed {
                failures: self.config.failure_threshold.max(1).saturating_sub(1),
            };
            drop(state);
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_abandon(&self, probe: bool) {
        if !probe {
            return;
        }
        let mut state = self.lock();
        if let State::HalfOpen { probing: true } = *state {
            *state = State::HalfOpen { probing: false };
        }
    }
}

/// Leave to make one attempt through a [`CircuitBreaker`]; reports its outcome once.
#[derive(Debug)]
pub struct Permit {
    breaker: Arc<CircuitBreaker>,
    probe: bool,
    settled: bool,
}

impl Permit {
    /// The attempt succeeded (a closed breaker forgets earlier failures; a half-open one closes).
    pub fn success(mut self) {
        self.settled = true;
        self.breaker.on_success();
    }

    /// The attempt failed because of the provider.
    pub fn failure(mut self) {
        self.settled = true;
        self.breaker.on_failure();
    }

    /// The attempt is under way and so far healthy (its connect succeeded, its response
    /// started), but its outcome is not known yet. A half-open probe then closes the breaker
    /// *on probation*: other attempts are admitted again at once (a probe session that has to
    /// prove itself for seconds must not refuse every other call meanwhile), and the next
    /// failure (this attempt's included) opens it again at once. The outcome is still to be
    /// reported. No effect on a permit that is not the probe.
    pub fn probation(&mut self) {
        if self.probe {
            self.probe = false;
            self.breaker.on_probation();
        }
    }

    /// Whether this is the half-open probe (`false` again after [`Self::probation`]).
    pub const fn is_probe(&self) -> bool {
        self.probe
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.settled {
            self.breaker.on_abandon(self.probe);
        }
    }
}
