//! Admission policy through the session manager: refusal reasons, their order and
//! counters, hysteresis on the injected clock, and provider permits given back on every end path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
// test helpers

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock, SystemClock};
use brisvox_server::{
    AdmissionError, AdmissionPolicy, AdmissionSettings, Error, LoadProbe, LoadSample,
    ManagerConfig, OverloadConfig, Readiness, ReadyStatus, SessionContext, SessionEnded,
    SessionHandle, SessionManager, ShutdownPolicy,
};
use tokio::sync::oneshot;

type Done = Result<(), String>;

/// A pool the test drives by hand.
#[derive(Default)]
struct Pool {
    depth: AtomicUsize,
    saturated: AtomicU64,
}

impl Pool {
    fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn depth(&self, n: usize) {
        self.depth.store(n, Ordering::SeqCst);
    }
}

impl LoadProbe for Pool {
    fn sample(&self) -> LoadSample {
        LoadSample {
            threads: 2,
            queue_depth: self.depth.load(Ordering::SeqCst),
            completed: 0,
            wait_total: Duration::ZERO,
            saturated: self.saturated.load(Ordering::SeqCst),
        }
    }
}

fn overload() -> OverloadConfig {
    OverloadConfig {
        shed_queue_per_thread: 4,
        resume_queue_per_thread: 1,
        depth_confirm: Duration::ZERO,
        wait_limit: Duration::ZERO,
        window: Duration::from_secs(2),
        learn_ceiling: false,
    }
}

fn manager_with(
    max: usize,
    clock: SharedClock,
    pool: Option<&Arc<Pool>>,
    limits: &[(&str, usize)],
) -> SessionManager {
    manager_with_overload(max, clock, pool, limits, overload())
}

fn manager_with_overload(
    max: usize,
    clock: SharedClock,
    pool: Option<&Arc<Pool>>,
    limits: &[(&str, usize)],
    overload: OverloadConfig,
) -> SessionManager {
    let settings = AdmissionSettings {
        overload: Some(overload),
        provider_limits: limits
            .iter()
            .map(|(name, limit)| ((*name).to_owned(), *limit))
            .collect::<BTreeMap<_, _>>(),
    };
    let probe = pool.cloned().map(|p| -> Arc<dyn LoadProbe> { p });
    SessionManager::with_admission(
        ManagerConfig::new(max).unwrap(),
        clock,
        AdmissionPolicy::new(&settings, probe),
    )
    .unwrap()
}

fn sim() -> SharedClock {
    Arc::new(SimClock::new())
}

fn in_use(mgr: &SessionManager, provider: &str) -> usize {
    mgr.admission_snapshot()
        .providers
        .iter()
        .find(|p| p.name == provider)
        .unwrap()
        .in_use
}

/// Holds the session until it is cancelled.
async fn hold(ctx: SessionContext) -> Done {
    ctx.cancel.cancelled().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn pool_pressure_refuses_with_a_typed_counted_error_and_hysteresis() {
    let clock = sim();
    let pool = Pool::shared();
    let mgr = manager_with(8, Arc::clone(&clock), Some(&pool), &[]);

    let first = mgr.start(hold).unwrap();
    // 8 queued on 2 threads: 4 per thread, the shed limit.
    pool.depth(8);
    let refused = mgr.try_admit().unwrap_err();
    assert_eq!(refused, AdmissionError::Overloaded);
    assert!(matches!(Error::from(refused), Error::Overloaded));
    let stats = mgr.stats();
    assert_eq!(stats.rejected_overloaded, 1);
    assert_eq!((stats.rejected_at_capacity, stats.reserved), (0, 0));
    assert!(mgr.admission_snapshot().overloaded());
    // The running call is not touched.
    assert_eq!(mgr.active(), 1);

    // The queue drains to the resume limit: still refused until a whole window has passed.
    pool.depth(2);
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    tokio::time::advance(Duration::from_millis(1_900)).await;
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    tokio::time::advance(Duration::from_millis(200)).await;
    let slot = mgr
        .try_admit()
        .expect("calm for a full window: admitted again");
    drop(slot);

    let pressure = mgr.admission_snapshot().pressure.unwrap();
    assert!(!pressure.overloaded);
    assert_eq!(pressure.entered, 1);
    assert_eq!(mgr.stats().rejected_overloaded, 3);
    mgr.cancel_all();
    first.ended().await;
}

#[tokio::test(start_paused = true)]
async fn the_session_limit_and_a_drain_are_reported_before_pressure() {
    let pool = Pool::shared();
    let mgr = manager_with(1, sim(), Some(&pool), &[]);
    let held = mgr.try_admit().unwrap();
    // Full and overloaded at once: the session limit is the reason given.
    pool.depth(100);
    assert_eq!(
        mgr.try_admit().unwrap_err(),
        AdmissionError::AtCapacity { max: 1 }
    );
    // Room again, but the pool is still hot (no time has passed): pressure is the reason.
    drop(held);
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    // Draining beats both.
    mgr.begin_drain();
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::ShuttingDown);
    let s = mgr.stats();
    assert_eq!(
        (
            s.rejected_overloaded,
            s.rejected_at_capacity,
            s.rejected_shutting_down
        ),
        (1, 1, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_budget_refuses_all_or_none_and_counts_per_provider() {
    let mgr = manager_with(8, sim(), None, &[("deepgram", 2), ("openai", 1)]);
    let a = mgr.start(hold).unwrap();
    assert_eq!((in_use(&mgr, "deepgram"), in_use(&mgr, "openai")), (1, 1));

    let refused = mgr.try_admit().unwrap_err();
    assert_eq!(
        refused,
        AdmissionError::ProviderBudget {
            provider: Arc::from("openai"),
            limit: 1
        }
    );
    assert!(refused.to_string().contains("openai"));
    assert!(matches!(
        Error::from(refused),
        Error::ProviderBudget { limit: 1, .. }
    ));
    // All or none: the refused attempt did not keep the deepgram permit it took first.
    assert_eq!((in_use(&mgr, "deepgram"), in_use(&mgr, "openai")), (1, 1));
    assert_eq!(mgr.stats().rejected_provider_budget, 1);
    assert_eq!(mgr.stats().reserved, 0);
    let snap = mgr.admission_snapshot();
    let rejected: Vec<_> = snap
        .providers
        .iter()
        .map(|p| (&*p.name, p.rejected))
        .collect();
    assert_eq!(rejected, [("deepgram", 0), ("openai", 1)]);
    assert_eq!(snap.exhausted_provider().unwrap().name, "openai");

    a.cancel();
    assert_eq!(a.ended().await, SessionEnded::Cancelled);
    assert_eq!((in_use(&mgr, "deepgram"), in_use(&mgr, "openai")), (0, 0));
    mgr.start(hold).expect("the budget is free again").cancel();
}

#[tokio::test(start_paused = true)]
async fn readiness_reports_pressure_and_an_exhausted_budget() {
    let pool = Pool::shared();
    let mgr = manager_with(8, sim(), Some(&pool), &[("openai", 1)]);
    let ready = Readiness::of(&mgr);
    assert_eq!(ready.status, ReadyStatus::Ready);
    let json = ready.to_json();
    assert!(
        json.contains(r#""pressure":{"overloaded":false,"#),
        "{json}"
    );
    assert!(
        json.contains(r#""providers":[{"name":"openai","limit":1,"in_use":0,"rejected":0}]"#),
        "{json}"
    );

    let call = mgr.start(hold).unwrap();
    let busy = Readiness::of(&mgr);
    assert_eq!(busy.status, ReadyStatus::ProviderBudget);
    assert_eq!(busy.status.http_status(), 503);
    assert!(
        busy.to_json()
            .starts_with(r#"{"status":"provider_budget","accepting":true"#)
    );

    call.cancel();
    call.ended().await;
    pool.depth(8);
    let hot = Readiness::of(&mgr);
    assert_eq!(hot.status, ReadyStatus::Overloaded);
    assert_eq!(hot.status.http_status(), 503);
    assert!(hot.to_json().contains(r#""overloaded":true"#));
    // A refusal shows in the counters of the same body.
    assert!(mgr.try_admit().is_err());
    assert!(
        Readiness::of(&mgr).to_json().contains(
            r#""rejected":{"full":0,"overloaded":1,"provider_budget":0,"shutting_down":0}"#
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permits_are_held_for_the_calls_life_and_given_back_on_every_end() {
    let mgr = manager_with(16, SystemClock::shared(), None, &[("deepgram", 16)]);

    // Completed.
    let (release, wait) = oneshot::channel::<()>();
    let h = mgr
        .start(move |_c| async move {
            let _ = wait.await;
            Done::Ok(())
        })
        .unwrap();
    assert_eq!(in_use(&mgr, "deepgram"), 1, "held while the call runs");
    release.send(()).unwrap();
    assert_eq!(h.ended().await, SessionEnded::Completed);
    assert_eq!(in_use(&mgr, "deepgram"), 0, "completed");

    // Failed.
    let h = mgr.start(|_c| async { Done::Err("nope".into()) }).unwrap();
    assert!(matches!(h.ended().await, SessionEnded::Failed(_)));
    assert_eq!(in_use(&mgr, "deepgram"), 0, "failed");

    // Panicked while running, and while building the future.
    let h = mgr
        .start(|_c| async {
            if true {
                panic!("boom");
            }
            Done::Ok(())
        })
        .unwrap();
    assert!(matches!(h.ended().await, SessionEnded::Panicked(_)));
    assert_eq!(in_use(&mgr, "deepgram"), 0, "panicked");
    let h = mgr
        .start(|_c| -> std::future::Ready<Done> { panic!("factory") })
        .unwrap();
    assert!(matches!(h.ended().await, SessionEnded::Panicked(_)));
    assert_eq!(in_use(&mgr, "deepgram"), 0, "panicked while building");

    // Cancelled.
    let h = mgr.start(hold).unwrap();
    assert_eq!(in_use(&mgr, "deepgram"), 1);
    h.cancel();
    assert_eq!(h.ended().await, SessionEnded::Cancelled);
    assert_eq!(in_use(&mgr, "deepgram"), 0, "cancelled");

    // A slot dropped without a session.
    let slot = mgr.try_admit().unwrap();
    assert_eq!(
        in_use(&mgr, "deepgram"),
        1,
        "a reservation holds the budget"
    );
    drop(slot);
    assert_eq!(in_use(&mgr, "deepgram"), 0, "unspent slot");

    // Aborted by the shutdown's last stage (a session that ignores its token).
    let _stuck = mgr
        .start(|_c| async {
            std::future::pending::<()>().await;
            Done::Ok(())
        })
        .unwrap();
    assert_eq!(in_use(&mgr, "deepgram"), 1);
    let report = mgr
        .shutdown(ShutdownPolicy {
            drain_timeout: Duration::ZERO,
            cancel_grace: Duration::from_millis(20),
            abort_wait: Duration::from_millis(500),
        })
        .await;
    assert_eq!(report.aborted, 1, "{report:?}");
    assert_eq!(in_use(&mgr, "deepgram"), 0, "aborted");
}

#[test]
fn dropping_the_runtime_gives_the_permits_back() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mgr = rt.block_on(async { manager_with(8, SystemClock::shared(), None, &[("openai", 8)]) });
    let handles = rt.block_on(async {
        // Polled once, then parked forever.
        let a = mgr
            .start(|_c| async {
                std::future::pending::<()>().await;
                Done::Ok(())
            })
            .unwrap();
        tokio::task::yield_now().await;
        // Spawned, but never polled before the runtime goes.
        let b = mgr
            .start(|_c| async {
                std::future::pending::<()>().await;
                Done::Ok(())
            })
            .unwrap();
        (a, b)
    });
    assert_eq!(in_use(&mgr, "openai"), 2);
    drop(rt);
    assert_eq!(
        in_use(&mgr, "openai"),
        0,
        "the drop guards released the permits"
    );
    drop(handles);
}

fn learning(pool: &Arc<Pool>, clock: SharedClock) -> SessionManager {
    manager_with_overload(
        1_000,
        clock,
        Some(pool),
        &[],
        OverloadConfig {
            learn_ceiling: true,
            ..overload()
        },
    )
}

/// Admits until `held` calls run, letting the cautious start double as often as it needs.
async fn ramp_to(mgr: &SessionManager, calls: &mut Vec<SessionHandle>, held: usize) {
    while calls.len() < held {
        match mgr.try_admit() {
            Ok(slot) => calls.push(slot.spawn(hold)),
            Err(_) => tokio::time::advance(Duration::from_millis(2_100)).await,
        }
    }
}

async fn end(calls: &mut Vec<SessionHandle>, n: usize) {
    for call in calls.drain(..n) {
        call.cancel();
        call.ended().await;
    }
}

/// The learned ceiling: the pool turns hot at 200 held (cap 90%: 180); calls ending during the
/// spell do not lower it; it recovers at the calls held then (140); it grows 10% per calm window
/// at the cap; running calls are never touched.
#[tokio::test(start_paused = true)]
async fn a_hot_pool_teaches_a_ceiling_that_holds_and_then_grows_by_calm_windows() {
    let clock = sim();
    let pool = Pool::shared();
    let mgr = learning(&pool, Arc::clone(&clock));
    let mut calls = Vec::new();
    ramp_to(&mgr, &mut calls, 200).await;
    assert_eq!(
        mgr.admission_ceiling(),
        Some(256),
        "cautious start: 16 doubled four times"
    );

    pool.depth(8);
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    assert_eq!(mgr.admission_ceiling(), Some(180));
    assert_eq!(mgr.active(), 200, "running calls are never touched");
    end(&mut calls, 60).await;
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    assert_eq!(
        mgr.admission_ceiling(),
        Some(180),
        "not lowered during the spell"
    );

    // Calm again with 140 held: that is the cap; then +10% per calm window at it.
    pool.depth(0);
    let _ = mgr.try_admit();
    tokio::time::advance(Duration::from_millis(2_100)).await;
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    assert_eq!(mgr.admission_ceiling(), Some(140));
    tokio::time::advance(Duration::from_millis(2_000)).await;
    let slot = mgr.try_admit().expect("the cap grew past the calls held");
    assert_eq!(mgr.admission_ceiling(), Some(154));
    drop(slot);
    mgr.cancel_all();
    for call in calls {
        call.ended().await;
    }
}

/// A hot moment with few calls held halves the cap at most (154 -> 77, not 9), also when nearly
/// every call ended by the recovery; and an idle spell is no recovery (no halving after it).
#[tokio::test(start_paused = true)]
async fn one_hot_spell_at_most_halves_the_cap_and_an_idle_spell_changes_nothing() {
    let clock = sim();
    let pool = Pool::shared();
    let mgr = learning(&pool, Arc::clone(&clock));
    let mut calls = Vec::new();
    ramp_to(&mgr, &mut calls, 200).await;
    pool.depth(8);
    let _ = mgr.try_admit();
    assert_eq!(mgr.admission_ceiling(), Some(180));
    pool.depth(0);
    end(&mut calls, 190).await;
    let _ = mgr.try_admit();
    tokio::time::advance(Duration::from_millis(2_100)).await;
    let _ = mgr.try_admit();
    assert_eq!(
        mgr.admission_ceiling(),
        Some(90),
        "recovered with 10 held: halved, not 10"
    );

    // A hot moment with 10 held: 90 -> 45 at most, not 9.
    pool.depth(8);
    let _ = mgr.try_admit();
    assert_eq!(mgr.admission_ceiling(), Some(45));
    // Hot at its last sample, then an hour without a call: the first dial afterwards finds a
    // calm pool and is no recovery step (no further halving).
    pool.depth(0);
    tokio::time::advance(Duration::from_secs(3_600)).await;
    let slot = mgr.try_admit().expect("calm after an idle hour");
    assert_eq!(mgr.admission_ceiling(), Some(45));
    drop(slot);
    mgr.cancel_all();
    for call in calls {
        call.ended().await;
    }
}

/// The cautious start: a fresh server admits 16 calls, then doubles its cap for every calm
/// window spent at it; the first hot sample ends it and the ceiling is learned from there.
#[tokio::test(start_paused = true)]
async fn a_fresh_server_starts_cautiously_and_doubles_per_calm_window() {
    let clock = sim();
    let pool = Pool::shared();
    let mgr = manager_with_overload(
        1_000,
        Arc::clone(&clock),
        Some(&pool),
        &[],
        OverloadConfig {
            learn_ceiling: true,
            ..overload()
        },
    );
    let mut calls: Vec<_> = Vec::new();
    let admit_all = |calls: &mut Vec<_>| {
        while let Ok(slot) = mgr.try_admit() {
            calls.push(slot.spawn(hold));
        }
    };
    // A rush of dials: 16 get in, the rest are refused as overloaded.
    admit_all(&mut calls);
    assert_eq!(calls.len(), 16);
    assert_eq!(mgr.admission_ceiling(), Some(16));
    assert!(
        mgr.admission_snapshot().overloaded(),
        "at the cap /readyz says overloaded, so a balancer sends calls elsewhere"
    );
    // Not doubled before a whole window has passed.
    tokio::time::advance(Duration::from_millis(1_900)).await;
    admit_all(&mut calls);
    assert_eq!(calls.len(), 16);
    // 32, then 64, a window apart.
    tokio::time::advance(Duration::from_millis(200)).await;
    admit_all(&mut calls);
    assert_eq!(calls.len(), 32);
    tokio::time::advance(Duration::from_millis(2_100)).await;
    admit_all(&mut calls);
    assert_eq!(calls.len(), 64);
    // The pool turns hot at 64: learned at 57 (90%), and it stays the learned rule from now on.
    pool.depth(8);
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::Overloaded);
    assert_eq!(mgr.admission_ceiling(), Some(57));
    assert_eq!(mgr.active(), 64, "running calls are never touched");
    mgr.cancel_all();
    for call in calls {
        call.ended().await;
    }
}
