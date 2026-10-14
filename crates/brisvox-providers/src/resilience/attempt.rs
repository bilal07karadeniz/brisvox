//! What the three wrappers share: the call's budgets and counters, and the attempt loop over the
//! legs of an operation.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::SharedClock;

use super::breaker::{CircuitBreaker, Permit};
use super::budget::RetryBudget;
use super::{Resilience, ResilienceConfig, is_provider_failure};
use crate::error::{Error, Result};

/// Which provider of a stage served (or is to serve) an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegId {
    /// The stage's primary provider.
    Primary,
    /// The stage's optional secondary provider.
    Secondary,
}

/// Counters of one wrapper instance (one call), monotonic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResilienceStats {
    /// Attempts made (connects, requests, recoveries; first attempts and retries).
    pub attempts: u64,
    /// Attempts that were retries (each paid one token of each budget).
    pub retries: u64,
    /// Attempts served by the primary.
    pub served_primary: u64,
    /// Attempts served by the secondary.
    pub served_secondary: u64,
    /// Legs skipped because their breaker refused.
    pub breaker_skips: u64,
    /// Retries refused because a budget was empty.
    pub budget_denied: u64,
    /// Operations that failed with every leg refused (nothing was attempted).
    pub unavailable: u64,
    /// Sessions recovered mid-call (STT, TTS) or responses re-issued (LLM).
    pub recoveries: u64,
    /// Mid-call failures that could not be recovered (the error reached the engine).
    pub recovery_failures: u64,
    /// Inputs replayed into a recovered session (STT audio chunks and finalizes, TTS texts and
    /// flushes).
    pub replayed: u64,
    /// Idle TTS sessions replaced by a fresh one.
    pub refreshes: u64,
    /// Idle refreshes whose connect failed.
    pub refresh_failures: u64,
}

/// A read-only handle on one wrapper's counters and call budget (see `probe()` on the
/// wrappers). Cheap to clone.
#[derive(Debug, Clone)]
pub struct ResilienceProbe {
    common: Arc<Common>,
}

impl ResilienceProbe {
    pub(crate) const fn new(common: Arc<Common>) -> Self {
        Self { common }
    }

    /// The wrapper's counters now.
    pub fn stats(&self) -> ResilienceStats {
        self.common.counters.snapshot()
    }

    /// Tokens left in the call's retry budget.
    pub fn budget_available(&self) -> u32 {
        self.common.session.available()
    }
}

macro_rules! counters {
    ($($field:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub(crate) struct Counters {
            $(pub(crate) $field: AtomicU64,)*
        }

        impl Counters {
            pub(crate) fn snapshot(&self) -> ResilienceStats {
                ResilienceStats {
                    $($field: self.$field.load(Ordering::Relaxed),)*
                }
            }
        }
    };
}

counters!(
    attempts,
    retries,
    served_primary,
    served_secondary,
    breaker_skips,
    budget_denied,
    unavailable,
    recoveries,
    recovery_failures,
    replayed,
    refreshes,
    refresh_failures,
);

/// Adds one to `counter`.
pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Legs to try, in order (`None`: no leg in that slot).
pub(crate) type Order = [Option<LegId>; 2];

/// The leg order of an operation: primary first, the secondary (when there is one) next; a
/// recovery after `failed` tries the other leg first and `failed` last.
pub(crate) const fn order(has_secondary: bool, failed: Option<LegId>) -> Order {
    match (has_secondary, failed) {
        (false, _) => [Some(LegId::Primary), None],
        (true, Some(LegId::Primary)) => [Some(LegId::Secondary), Some(LegId::Primary)],
        (true, _) => [Some(LegId::Primary), Some(LegId::Secondary)],
    }
}

/// The value of a successful attempt, the leg that served it and the breaker permit whose
/// outcome is still to be reported.
#[derive(Debug)]
pub(crate) struct Attempted<T> {
    pub(crate) value: T,
    pub(crate) leg: LegId,
    pub(crate) permit: Permit,
}

/// The call's half of the layer: settings, clock, the call's budget, the process budget, the
/// legs' breakers and the counters.
#[derive(Debug)]
pub(crate) struct Common {
    pub(crate) config: ResilienceConfig,
    pub(crate) clock: SharedClock,
    pub(crate) name: &'static str,
    session: RetryBudget,
    process: Arc<RetryBudget>,
    primary: Arc<CircuitBreaker>,
    secondary: Option<Arc<CircuitBreaker>>,
    pub(crate) counters: Counters,
}

impl Common {
    pub(crate) fn new(
        resilience: &Resilience,
        name: &'static str,
        primary: Arc<CircuitBreaker>,
        secondary: Option<Arc<CircuitBreaker>>,
    ) -> Self {
        Self {
            config: resilience.config,
            clock: resilience.clock.clone(),
            name,
            session: RetryBudget::new(resilience.config.session_budget, resilience.clock.clone()),
            process: Arc::clone(&resilience.process),
            primary,
            secondary,
            counters: Counters::default(),
        }
    }

    /// A fresh call state (new session budget and counters) with `secondary` as the second leg.
    pub(crate) fn with_secondary(&self, secondary: Arc<CircuitBreaker>) -> Self {
        Self {
            config: self.config,
            clock: self.clock.clone(),
            name: self.name,
            session: RetryBudget::new(self.config.session_budget, self.clock.clone()),
            process: Arc::clone(&self.process),
            primary: Arc::clone(&self.primary),
            secondary: Some(secondary),
            counters: Counters::default(),
        }
    }

    pub(crate) const fn has_secondary(&self) -> bool {
        self.secondary.is_some()
    }

    fn breaker(&self, leg: LegId) -> Option<&Arc<CircuitBreaker>> {
        match leg {
            LegId::Primary => Some(&self.primary),
            LegId::Secondary => self.secondary.as_ref(),
        }
    }

    /// One token from the call's budget and one from the process budget, or neither.
    fn try_retry(&self) -> bool {
        if !self.session.try_take() {
            return false;
        }
        if self.process.try_take() {
            return true;
        }
        self.session.refund();
        false
    }

    /// The call's retry budget.
    pub(crate) const fn session_budget(&self) -> &RetryBudget {
        &self.session
    }

    fn unavailable(&self, reason: &'static str) -> Error {
        Error::Unavailable {
            provider: self.name,
            reason,
        }
    }

    /// Runs one operation: tries the legs of `order` in turn (see the module docs of
    /// [`super`]). `retrying`: the first attempt is already a retry (a recovery). `timeout_last`:
    /// the last attempt runs under the attempt timeout as well.
    ///
    /// # Errors
    /// The last provider error; a request error at once (it is not the provider's health);
    /// [`Error::Unavailable`] when no leg was allowed or the budget is spent before any attempt.
    pub(crate) async fn attempt<T, F, Fut>(
        &self,
        order: Order,
        mut retrying: bool,
        timeout_last: bool,
        mut run: F,
    ) -> Result<Attempted<T>>
    where
        F: FnMut(LegId) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut last_error = None;
        for (index, slot) in order.iter().enumerate() {
            let Some(leg) = *slot else { continue };
            let Some(breaker) = self.breaker(leg) else {
                continue;
            };
            let Some(permit) = breaker.try_acquire() else {
                bump(&self.counters.breaker_skips);
                continue;
            };
            if retrying {
                if !self.try_retry() {
                    bump(&self.counters.budget_denied);
                    drop(permit);
                    return Err(
                        last_error.unwrap_or_else(|| self.unavailable("retry budget spent"))
                    );
                }
                bump(&self.counters.retries);
            }
            bump(&self.counters.attempts);
            let more = order
                .get(index + 1..)
                .is_some_and(|rest| rest.iter().any(Option::is_some));
            let result = if more || timeout_last {
                with_timeout(
                    &self.clock,
                    self.config.attempt_timeout,
                    "provider attempt",
                    run(leg),
                )
                .await
            } else {
                run(leg).await
            };
            match result {
                Ok(value) => {
                    bump(match leg {
                        LegId::Primary => &self.counters.served_primary,
                        LegId::Secondary => &self.counters.served_secondary,
                    });
                    return Ok(Attempted { value, leg, permit });
                }
                Err(error) if is_provider_failure(&error) => {
                    permit.failure();
                    last_error = Some(error);
                    retrying = true;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            bump(&self.counters.unavailable);
            self.unavailable("circuit open")
        }))
    }
}

/// The longest the secondary's warm-up may take inside a wrapper's `warm()` (the engine gives a
/// warm-up at most 400 ms; the primary's result must not wait for the secondary's).
pub(crate) const SECONDARY_WARM_BUDGET: Duration = Duration::from_millis(300);

/// `future` under a deadline of `after` on `clock`.
///
/// # Errors
/// The future's error, or `Core(Timeout)` named `what`.
pub(crate) async fn with_timeout<T>(
    clock: &SharedClock,
    after: Duration,
    what: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        result = future => result,
        () = clock.sleep(after) => Err(Error::Core(brisvox_core::Error::Timeout { what, after })),
    }
}
