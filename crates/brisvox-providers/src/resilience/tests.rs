//! Unit tests of the breaker, the budget and the attempt helpers (the wrappers are tested with
//! the simulator's mocks in `tests/resilience.rs`).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock};

use super::attempt::{LegId, order};
use super::*;
use crate::Error;

const SEC: Duration = Duration::from_secs(1);

fn breaker(clock: &SharedClock, threshold: u32) -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(
        "test",
        BreakerConfig {
            failure_threshold: threshold,
            open_for: 10 * SEC,
        },
        clock.clone(),
    ))
}

#[tokio::test(start_paused = true)]
async fn breaker_opens_after_consecutive_failures_and_a_success_resets_the_count() {
    let clock = SimClock::shared();
    let b = breaker(&clock, 3);
    b.try_acquire().unwrap().failure();
    b.try_acquire().unwrap().failure();
    b.try_acquire().unwrap().success();
    b.try_acquire().unwrap().failure();
    b.try_acquire().unwrap().failure();
    assert_eq!(
        b.state(),
        BreakerState::Closed,
        "the success reset the count"
    );
    b.try_acquire().unwrap().failure();
    assert_eq!(b.state(), BreakerState::Open);
    assert!(b.try_acquire().is_none());
    let stats = b.stats();
    assert_eq!((stats.opened, stats.failures, stats.refused), (1, 5, 1));
}

#[tokio::test(start_paused = true)]
async fn breaker_half_opens_with_one_probe_that_closes_or_reopens_it() {
    let clock = SimClock::shared();
    let b = breaker(&clock, 1);
    b.try_acquire().unwrap().failure();
    assert_eq!(b.state(), BreakerState::Open);
    clock.sleep(10 * SEC - Duration::from_millis(1)).await;
    assert!(b.try_acquire().is_none(), "still open 1 ms before the end");
    clock.sleep(Duration::from_millis(1)).await;
    assert_eq!(b.state(), BreakerState::HalfOpen);

    // One probe at a time.
    let probe = b.try_acquire().unwrap();
    assert!(probe.is_probe());
    assert!(b.try_acquire().is_none(), "the probe slot is taken");
    // A failed probe opens it again for a whole period.
    probe.failure();
    assert_eq!(b.state(), BreakerState::Open);
    assert_eq!(b.stats().opened, 2);
    clock.sleep(10 * SEC).await;

    // An abandoned probe frees the slot without a verdict.
    drop(b.try_acquire().unwrap());
    assert_eq!(b.state(), BreakerState::HalfOpen);
    let probe = b.try_acquire().unwrap();
    probe.success();
    assert_eq!(b.state(), BreakerState::Closed);
    assert!(!b.try_acquire().unwrap().is_probe());
    assert_eq!(b.stats().closed, 1);
}

#[tokio::test(start_paused = true)]
async fn budget_spends_refills_on_the_clock_and_takes_refunds() {
    let clock = SimClock::shared();
    let budget = RetryBudget::new(
        BudgetConfig {
            capacity: 2,
            refill_every: 5 * SEC,
        },
        clock.clone(),
    );
    assert!(budget.try_take());
    assert!(budget.try_take());
    assert!(!budget.try_take());
    assert_eq!(budget.denied(), 1);
    clock.sleep(4 * SEC).await;
    assert_eq!(budget.available(), 0);
    clock.sleep(SEC).await;
    assert_eq!(budget.available(), 1);
    clock.sleep(12 * SEC).await;
    assert_eq!(budget.available(), 2, "never above the capacity");
    assert!(budget.try_take());
    budget.refund();
    assert_eq!(budget.available(), 2);
    // A full bucket earns nothing while full: the next token is a whole period after a spend.
    clock.sleep(30 * SEC).await;
    assert!(budget.try_take());
    assert!(budget.try_take());
    clock.sleep(5 * SEC - Duration::from_millis(1)).await;
    assert_eq!(budget.available(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_zero_refill_period_is_a_bucket_that_is_always_full() {
    let clock = SimClock::shared();
    let budget = RetryBudget::new(
        BudgetConfig {
            capacity: 1,
            refill_every: Duration::ZERO,
        },
        clock,
    );
    for _ in 0..5 {
        assert!(budget.try_take());
    }
}

#[test]
fn request_errors_and_cancellations_are_not_provider_failures() {
    let rejected = |status| Error::Rejected {
        provider: "x",
        status,
    };
    for status in [400, 404, 413, 422] {
        assert!(!is_provider_failure(&rejected(status)), "{status}");
    }
    for status in [401, 403, 408, 429, 500, 503, 1011] {
        assert!(is_provider_failure(&rejected(status)), "{status}");
    }
    assert!(!is_provider_failure(&Error::Core(
        brisvox_core::Error::Cancelled
    )));
    assert!(is_provider_failure(&Error::Core(
        brisvox_core::Error::Timeout {
            what: "x",
            after: SEC,
        }
    )));
    assert!(is_provider_failure(&Error::Disconnected { provider: "x" }));
    assert!(!is_provider_failure(&Error::Unavailable {
        provider: "x",
        reason: "circuit open",
    }));
}

#[test]
fn a_recovery_tries_the_other_leg_first() {
    use LegId::{Primary, Secondary};
    assert_eq!(order(false, None), [Some(Primary), None]);
    assert_eq!(order(false, Some(Primary)), [Some(Primary), None]);
    assert_eq!(order(true, None), [Some(Primary), Some(Secondary)]);
    assert_eq!(order(true, Some(Primary)), [Some(Secondary), Some(Primary)]);
    assert_eq!(
        order(true, Some(Secondary)),
        [Some(Primary), Some(Secondary)]
    );
}
