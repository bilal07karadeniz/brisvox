//! Session manager behaviour: panic isolation, admission, classification, runner
//! adapter, staged shutdown (drain, cancel, abort) and cleanup on every exit path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
// test helpers

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock, SystemClock};
use brisvox_server::{
    AdmissionError, Error, ManagerConfig, SessionContext, SessionEnded, SessionManager,
    SessionRunner, ShutdownPolicy,
};
use brisvox_sim::leak::TaskCounter;
use tokio::sync::oneshot;

const MS: Duration = Duration::from_millis(1);

fn manager(max: usize, clock: SharedClock) -> SessionManager {
    SessionManager::new(ManagerConfig::new(max).unwrap(), clock).unwrap()
}

fn sim() -> SharedClock {
    Arc::new(SimClock::new())
}

type Done = Result<(), String>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panic_ends_only_that_session() {
    let mgr = manager(16, SystemClock::shared());

    // B is a healthy session that stays alive across A's panic until released.
    let (release_b, wait_b) = oneshot::channel::<()>();
    let b = mgr
        .start(move |_ctx| async move {
            let _ = wait_b.await;
            Done::Ok(())
        })
        .unwrap();
    let a = mgr
        .start(|_ctx| async move {
            tokio::task::yield_now().await;
            if true {
                panic!("boom in session a");
            }
            Done::Ok(())
        })
        .unwrap();
    let a_id = a.id();
    assert_eq!(
        a.ended().await,
        SessionEnded::Panicked("boom in session a".into())
    );

    // Only A is gone; B is untouched and the manager still admits and runs new sessions.
    assert!(!mgr.contains(a_id));
    assert_eq!(mgr.active(), 1);
    assert!(mgr.contains(b.id()));
    let c = mgr
        .start(|_ctx| async { Done::Ok(()) })
        .expect("admits after a panic");
    assert_eq!(c.ended().await, SessionEnded::Completed);

    release_b.send(()).unwrap();
    assert_eq!(b.ended().await, SessionEnded::Completed);
    let stats = mgr.stats();
    assert_eq!(stats.active, 0);
    assert_eq!(stats.ended.panicked, 1);
    assert_eq!(stats.ended.completed, 2);
    assert_eq!(stats.started, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_while_building_the_future_is_contained() {
    let mgr = manager(4, SystemClock::shared());
    let h = mgr
        .start(|_ctx| -> std::future::Ready<Done> { panic!("factory exploded") })
        .unwrap();
    assert_eq!(
        h.ended().await,
        SessionEnded::Panicked("factory exploded".into())
    );
    assert!(mgr.wait_idle(Duration::from_secs(5)).await);
    assert_eq!(mgr.stats().ended.panicked, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_payload_with_a_panicking_drop_is_contained() {
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                panic!("payload drop");
            }
        }
    }
    let mgr = manager(4, SystemClock::shared());
    let h = mgr
        .start(|_ctx| async move {
            if true {
                std::panic::panic_any(Bomb);
            }
            Done::Ok(())
        })
        .unwrap();
    let ended = h.ended().await;
    assert!(matches!(ended, SessionEnded::Panicked(_)), "{ended:?}");
    // The worker survived: a new session still runs.
    let ok = mgr.start(|_c| async { Done::Ok(()) }).unwrap();
    assert_eq!(ok.ended().await, SessionEnded::Completed);
}

#[tokio::test(start_paused = true)]
async fn ends_are_classified_and_counted() {
    let mgr = manager(16, sim());

    let ok = mgr.start(|_c| async { Done::Ok(()) }).unwrap();
    let failed = mgr
        .start(|_c| async { Err::<(), _>("provider said no") })
        .unwrap();
    let cancelled = mgr
        .start(|ctx: SessionContext| async move {
            ctx.cancel.cancelled().await;
            // A teardown error after a cancel is still a cancel, not a failure.
            Err::<(), _>("closing: peer gone")
        })
        .unwrap();
    cancelled.cancel();
    cancelled.cancel(); // idempotent

    assert_eq!(ok.ended().await, SessionEnded::Completed);
    assert_eq!(
        failed.ended().await,
        SessionEnded::Failed("provider said no".into())
    );
    assert_eq!(cancelled.ended().await, SessionEnded::Cancelled);

    let s = mgr.stats();
    assert_eq!(
        (s.ended.completed, s.ended.failed, s.ended.cancelled),
        (1, 1, 1)
    );
    assert_eq!(s.started, s.ended.total() + s.active as u64);
}

#[tokio::test(start_paused = true)]
async fn cancel_by_id_reaches_only_that_session() {
    let mgr = manager(8, sim());
    let wait = |ctx: SessionContext| async move {
        ctx.cancel.cancelled().await;
        Done::Ok(())
    };
    let a = mgr.start(wait).unwrap();
    let b = mgr.start(wait).unwrap();
    let a_id = a.id();
    assert!(mgr.cancel(a_id));
    assert_eq!(a.ended().await, SessionEnded::Cancelled);
    assert!(
        !mgr.cancel(a_id),
        "an ended session is no longer registered"
    );
    assert!(mgr.contains(b.id()));
    mgr.cancel_all();
    assert_eq!(b.ended().await, SessionEnded::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn admission_is_bounded_and_typed() {
    let mgr = manager(2, sim());
    let hold = |ctx: SessionContext| async move {
        ctx.cancel.cancelled().await;
        Done::Ok(())
    };
    let a = mgr.start(hold).unwrap();
    let b = mgr.start(hold).unwrap();
    assert_ne!(a.id(), b.id());
    let full = mgr.start(hold).unwrap_err();
    assert_eq!(full, AdmissionError::AtCapacity { max: 2 });
    assert!(matches!(Error::from(full), Error::AtCapacity));
    assert_eq!(mgr.stats().rejected_at_capacity, 1);

    // A freed place is reusable at once.
    a.cancel();
    assert_eq!(a.ended().await, SessionEnded::Cancelled);
    let c = mgr.start(hold).unwrap();
    assert_eq!(mgr.stats().active, 2);
    mgr.cancel_all();
    b.ended().await;
    c.ended().await;
}

#[tokio::test(start_paused = true)]
async fn a_reserved_slot_counts_until_spawned_or_dropped() {
    let mgr = manager(1, sim());
    let slot = mgr.try_admit().unwrap();
    assert_eq!(mgr.stats().reserved, 1);
    assert_eq!(
        mgr.try_admit().unwrap_err(),
        AdmissionError::AtCapacity { max: 1 }
    );
    drop(slot);
    assert_eq!(mgr.stats().reserved, 0);

    let slot = mgr.try_admit().unwrap();
    let id = slot.id();
    let h = slot.spawn(|_c| async { Done::Ok(()) });
    assert_eq!(h.id(), id);
    assert_eq!(h.ended().await, SessionEnded::Completed);
    let s = mgr.stats();
    assert_eq!((s.reserved, s.active, s.started), (0, 0, 1));
}

#[tokio::test(start_paused = true)]
async fn a_slot_reserved_before_drain_is_honoured() {
    let mgr = manager(2, sim());
    let slot = mgr.try_admit().unwrap();
    mgr.begin_drain();
    assert!(!mgr.is_accepting());
    // The transport already accepted this call: the session runs instead of being dropped
    // without an answer on its link.
    let h = slot.spawn(|_c| async { Done::Ok(()) });
    assert_eq!(h.ended().await, SessionEnded::Completed);
    let s = mgr.stats();
    assert_eq!((s.reserved, s.active, s.started), (0, 0, 1));
    // New admissions are refused.
    let err = mgr.try_admit().unwrap_err();
    assert_eq!(err, AdmissionError::ShuttingDown);
    assert!(matches!(Error::from(err), Error::ShuttingDown));
    assert_eq!(mgr.stats().rejected_shutting_down, 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_a_reserved_slot_and_drains_its_session() {
    let clock = sim();
    let mgr = manager(2, Arc::clone(&clock));
    let slot = mgr.try_admit().unwrap();
    let shutdown = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.shutdown(ShutdownPolicy::default()).await }
    });
    // The upgrade completes while the shutdown is already draining.
    clock.sleep(Duration::from_secs(1)).await;
    assert!(!mgr.is_accepting());
    assert!(
        !shutdown.is_finished(),
        "an outstanding reservation is waited for"
    );
    let h = slot.spawn(|ctx: SessionContext| async move {
        ctx.clock.sleep(Duration::from_secs(2)).await;
        Done::Ok(())
    });
    let report = shutdown.await.unwrap();
    assert_eq!(h.ended().await, SessionEnded::Completed);
    assert_eq!(
        (
            report.initial,
            report.drained,
            report.cancelled,
            report.aborted,
            report.leaked
        ),
        (1, 1, 0, 0, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn a_slot_spawned_after_the_abort_stage_is_aborted_at_once() {
    let clock = sim();
    let mgr = manager(2, Arc::clone(&clock));
    let slot = mgr.try_admit().unwrap();
    let report = mgr
        .shutdown(ShutdownPolicy {
            drain_timeout: Duration::from_secs(1),
            cancel_grace: Duration::from_secs(1),
            abort_wait: Duration::from_secs(1),
        })
        .await;
    assert_eq!(report.leaked, 1, "a reservation still held is reported");
    // Spawned after the shutdown returned: it must not run on unnoticed.
    let h = slot.spawn(|_ctx: SessionContext| async move {
        std::future::pending::<()>().await;
        Done::Ok(())
    });
    assert_eq!(h.ended().await, SessionEnded::Aborted);
    let s = mgr.stats();
    assert_eq!((s.active, s.reserved, s.ended.aborted), (0, 0, 1));
}

#[tokio::test(start_paused = true)]
async fn cancel_all_does_not_poison_later_sessions() {
    let mgr = manager(4, sim());
    let old = mgr
        .start(|ctx: SessionContext| async move {
            ctx.cancel.cancelled().await;
            Done::Ok(())
        })
        .unwrap();
    mgr.cancel_all();
    assert_eq!(old.ended().await, SessionEnded::Cancelled);

    // The manager still says it is ready, so a session it admits must not be born cancelled.
    assert!(mgr.is_accepting());
    let (release, wait) = oneshot::channel::<()>();
    let new = mgr
        .start(move |ctx: SessionContext| async move {
            assert!(!ctx.cancel.is_cancelled(), "admitted already cancelled");
            let _ = wait.await;
            Done::Ok(())
        })
        .unwrap();
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    assert_eq!(new.ended().await, SessionEnded::Completed);
    assert_eq!(mgr.stats().ended.cancelled, 1);
}

#[tokio::test(start_paused = true)]
async fn panics_while_classifying_the_result_count_as_panicked() {
    /// An error whose `Display` panics (a provider body sliced at a bad byte index, say).
    struct BadDisplay;

    impl std::fmt::Display for BadDisplay {
        fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("display exploded")
        }
    }

    /// An output whose destructor panics.
    struct BadDrop;

    impl Drop for BadDrop {
        fn drop(&mut self) {
            panic!("drop exploded");
        }
    }

    let mgr = manager(4, sim());
    let display = mgr
        .start(|_c| async { Result::<(), BadDisplay>::Err(BadDisplay) })
        .unwrap();
    assert_eq!(
        display.ended().await,
        SessionEnded::Panicked("display exploded".into())
    );
    let output = mgr
        .start(|_c| async { Result::<BadDrop, String>::Ok(BadDrop) })
        .unwrap();
    assert_eq!(
        output.ended().await,
        SessionEnded::Panicked("drop exploded".into())
    );
    let s = mgr.stats();
    assert_eq!((s.ended.panicked, s.ended.aborted, s.active), (2, 0, 0));
}

struct EchoRunner {
    started: AtomicUsize,
}

impl SessionRunner for EchoRunner {
    type Request = u32;
    type Output = u32;
    type Error = String;

    async fn run(self: Arc<Self>, ctx: SessionContext, request: u32) -> Result<u32, String> {
        self.started.fetch_add(1, Ordering::SeqCst);
        ctx.clock.sleep(10 * MS).await;
        if request == 13 {
            return Err(format!("unlucky {request}"));
        }
        Ok(request)
    }
}

#[tokio::test(start_paused = true)]
async fn start_with_drives_a_runner() {
    let mgr = manager(4, sim());
    let runner = Arc::new(EchoRunner {
        started: AtomicUsize::new(0),
    });
    let a = mgr.start_with(&runner, 1).unwrap();
    let b = mgr.start_with(&runner, 13).unwrap();
    assert_eq!(a.ended().await, SessionEnded::Completed);
    assert_eq!(b.ended().await, SessionEnded::Failed("unlucky 13".into()));
    assert_eq!(runner.started.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn reserve_then_spawn_with_runs_the_request() {
    let mgr = manager(1, sim());
    let runner = Arc::new(EchoRunner {
        started: AtomicUsize::new(0),
    });
    let slot = mgr.try_admit().unwrap();
    // Full: the transport still owns its request and can answer the refusal on it.
    let request = 7_u32;
    assert_eq!(
        mgr.try_admit().unwrap_err(),
        AdmissionError::AtCapacity { max: 1 }
    );
    let h = slot.spawn_with(&runner, request);
    assert_eq!(h.ended().await, SessionEnded::Completed);
    assert_eq!(mgr.stats().ended.completed, 1);
    assert_eq!(runner.started.load(Ordering::SeqCst), 1);

    // Drain begins between the reservation and the spawn (SIGTERM during the upgrade): the
    // request is not dropped unanswered, it reaches the runner.
    let slot = mgr.try_admit().unwrap();
    mgr.begin_drain();
    let h = slot.spawn_with(&runner, 13);
    assert_eq!(h.ended().await, SessionEnded::Failed("unlucky 13".into()));
    assert_eq!(runner.started.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn shutdown_drains_then_cancels_then_aborts() {
    let clock = sim();
    let mgr = manager(16, Arc::clone(&clock));
    let counter = TaskCounter::start_settled(&*clock, 20 * MS).await.unwrap();

    // Two sessions that finish by themselves inside the drain window.
    let natural: Vec<_> = (0..2)
        .map(|_| {
            mgr.start(|ctx: SessionContext| async move {
                ctx.clock.sleep(Duration::from_secs(3)).await;
                Done::Ok(())
            })
            .unwrap()
        })
        .collect();
    // Three that only stop when cancelled.
    let obedient: Vec<_> = (0..3)
        .map(|_| {
            mgr.start(|ctx: SessionContext| async move {
                ctx.cancel.cancelled().await;
                ctx.clock.sleep(100 * MS).await; // tidy-up work after the cancel
                Done::Ok(())
            })
            .unwrap()
        })
        .collect();
    // One that ignores its token (stuck on something that never completes).
    let stuck = mgr
        .start(|_ctx: SessionContext| async move {
            std::future::pending::<()>().await;
            Done::Ok(())
        })
        .unwrap();
    // A panicking one is gone before the shutdown starts and must not confuse the report.
    let boom = mgr
        .start(|_c| async {
            if true {
                panic!("early");
            }
            Done::Ok(())
        })
        .unwrap();
    assert!(matches!(boom.ended().await, SessionEnded::Panicked(_)));

    let policy = ShutdownPolicy {
        drain_timeout: Duration::from_secs(10),
        cancel_grace: Duration::from_secs(2),
        abort_wait: Duration::from_secs(1),
    };
    let t0 = clock.now();
    let report = mgr.shutdown(policy).await;
    let took = clock.now() - t0;

    assert_eq!(report.initial, 6);
    assert_eq!(report.drained, 2);
    assert_eq!(report.cancelled, 3);
    assert_eq!(report.aborted, 1);
    assert_eq!(report.leaked, 0);
    // Drain stage: the stuck one keeps the registry non-empty, so it burns its full 10 s. Cancel
    // stage: the same, 2 s. The abort stage completes as soon as the guard has cleaned up.
    assert!(
        took >= Duration::from_secs(12) && took < Duration::from_secs(13),
        "{took:?}"
    );

    for h in natural {
        assert_eq!(h.ended().await, SessionEnded::Completed);
    }
    for h in obedient {
        assert_eq!(h.ended().await, SessionEnded::Cancelled);
    }
    assert_eq!(stuck.ended().await, SessionEnded::Aborted);

    let s = mgr.stats();
    assert_eq!(s.active, 0);
    assert_eq!(s.ended.aborted, 1);
    assert_eq!(s.started, s.ended.total());
    assert_eq!(
        mgr.start(|_c| async { Done::Ok(()) }).unwrap_err(),
        AdmissionError::ShuttingDown
    );
    counter
        .wait_for_baseline(&*clock, Duration::from_secs(1))
        .await
        .expect("no session task is left");
}

#[tokio::test(start_paused = true)]
async fn shutdown_ends_early_when_every_session_finishes() {
    let clock = sim();
    let mgr = manager(4, Arc::clone(&clock));
    let h = mgr
        .start(|ctx: SessionContext| async move {
            ctx.clock.sleep(Duration::from_secs(2)).await;
            Done::Ok(())
        })
        .unwrap();
    let t0 = clock.now();
    let r = mgr.shutdown(ShutdownPolicy::default()).await;
    let took = clock.now() - t0;
    assert_eq!((r.initial, r.drained, r.cancelled, r.aborted), (1, 1, 0, 0));
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert_eq!(h.ended().await, SessionEnded::Completed);
}

#[tokio::test(start_paused = true)]
async fn shutdown_of_an_idle_manager_returns_at_once_and_twice_is_fine() {
    let clock = sim();
    let mgr = manager(4, Arc::clone(&clock));
    let t0 = clock.now();
    let r = mgr.shutdown(ShutdownPolicy::default()).await;
    assert_eq!((r.initial, r.leaked), (0, 0));
    let r2 = mgr.shutdown(ShutdownPolicy::default()).await;
    assert_eq!(r2.initial, 0);
    assert!(clock.now() - t0 < Duration::from_millis(50));
}

#[tokio::test(start_paused = true)]
async fn zero_drain_timeout_cancels_immediately() {
    let clock = sim();
    let mgr = manager(4, Arc::clone(&clock));
    let h = mgr
        .start(|ctx: SessionContext| async move {
            ctx.cancel.cancelled().await;
            Done::Ok(())
        })
        .unwrap();
    let r = mgr
        .shutdown(ShutdownPolicy {
            drain_timeout: Duration::ZERO,
            ..ShutdownPolicy::default()
        })
        .await;
    assert_eq!((r.drained, r.cancelled, r.aborted), (0, 1, 0));
    assert_eq!(h.ended().await, SessionEnded::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn a_detached_handle_does_not_stop_the_session() {
    let mgr = manager(4, sim());
    drop(
        mgr.start(|ctx: SessionContext| async move {
            ctx.clock.sleep(50 * MS).await;
            Done::Ok(())
        })
        .unwrap(),
    );
    assert_eq!(mgr.active(), 1);
    assert!(mgr.wait_idle(Duration::from_secs(1)).await);
    assert_eq!(mgr.stats().ended.completed, 1);
}

#[tokio::test(start_paused = true)]
async fn wait_idle_times_out_on_the_injected_clock() {
    let clock = sim();
    let mgr = manager(4, Arc::clone(&clock));
    let h = mgr
        .start(|ctx: SessionContext| async move {
            ctx.cancel.cancelled().await;
            Done::Ok(())
        })
        .unwrap();
    let t0 = clock.now();
    assert!(!mgr.wait_idle(Duration::from_secs(7)).await);
    assert!(clock.now() - t0 >= Duration::from_secs(7));
    h.cancel();
    assert!(mgr.wait_idle(Duration::from_secs(1)).await);
}

#[test]
fn constructing_outside_a_runtime_is_an_error_not_a_panic() {
    let err = SessionManager::new(ManagerConfig::default(), SystemClock::shared()).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

/// The last exit path: the runtime itself goes away and drops the task, polled or not.
#[test]
fn dropping_the_runtime_cleans_the_registry_as_aborted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mgr = rt.block_on(async { manager(8, SystemClock::shared()) });
    let handles = rt.block_on(async {
        // Polled once, then parked forever.
        let a = mgr
            .start(|_c| async {
                std::future::pending::<()>().await;
                Done::Ok(())
            })
            .unwrap();
        tokio::task::yield_now().await;
        // Spawned, but the runtime never gets to poll it before it is dropped.
        let b = mgr
            .start(|_c| async {
                std::future::pending::<()>().await;
                Done::Ok(())
            })
            .unwrap();
        (a, b)
    });
    assert_eq!(mgr.active(), 2);
    drop(rt);
    assert_eq!(mgr.active(), 0, "the drop guards ran");
    let s = mgr.stats();
    assert_eq!((s.ended.aborted, s.started), (2, 2));
    drop(handles);
}
