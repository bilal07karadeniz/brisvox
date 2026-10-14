//! Graceful drain and health endpoints: a shutdown signal stops admissions at once,
//! `/readyz` turns 503 while `/healthz` stays 200, running sessions get the drain timeout to end
//! by themselves, the rest is cancelled (or aborted) and the exit status says which.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock, SimClock, SystemClock};
use brisvox_server::{
    AdmissionError, DrainOutcome, ManagerConfig, Server, ServerSettings, SessionContext,
    ShutdownPolicy, ShutdownSignal, ShutdownSignals,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

type Done = Result<(), String>;

const SECOND: Duration = Duration::from_secs(1);

fn settings(max: usize, health: Option<SocketAddr>, drain: Duration) -> ServerSettings {
    ServerSettings {
        manager: ManagerConfig::new(max).unwrap(),
        health_addr: health,
        shutdown: ShutdownPolicy {
            drain_timeout: drain,
            cancel_grace: Duration::from_secs(5),
            abort_wait: SECOND,
        },
        ..ServerSettings::default()
    }
}

/// Waits (yielding, in simulated time) until the manager stopped admitting.
async fn until_draining(server: &brisvox_server::SessionManager) {
    for _ in 0..1000 {
        if !server.is_accepting() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the drain never began");
}

#[tokio::test(start_paused = true)]
async fn drain_waits_for_active_sessions_then_finishes() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let start = clock.now();
    let server = Server::start(settings(8, None, 30 * SECOND), clock.clone())
        .await
        .unwrap();
    let mgr = server.manager().clone();
    for secs in [3, 5] {
        mgr.start(move |ctx: SessionContext| async move {
            ctx.clock.sleep(secs * SECOND).await;
            Done::Ok(())
        })
        .unwrap();
    }
    // A slot reserved before the signal is honoured and drained like any session.
    let slot = mgr.try_admit().unwrap();

    let (trigger, mut signals) = ShutdownSignals::manual();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(ShutdownSignal::Terminate));
    until_draining(&mgr).await;

    // New calls are refused with the reason transports turn into close 1013 / 503.
    assert_eq!(mgr.try_admit().unwrap_err(), AdmissionError::ShuttingDown);
    slot.spawn(|ctx: SessionContext| async move {
        ctx.clock.sleep(SECOND).await;
        Done::Ok(())
    });

    let report = run.await.unwrap();
    assert_eq!(report.initial, 3);
    assert_eq!(report.drained, 3);
    assert_eq!((report.cancelled, report.aborted, report.leaked), (0, 0, 0));
    assert!(!report.forced);
    assert_eq!(report.outcome(), DrainOutcome::Drained);
    assert_eq!(report.outcome().exit_code(), 0);
    // It waited for the longest session (5 s), not for the drain timeout (30 s).
    let took = clock.now() - start;
    assert!(took >= 5 * SECOND && took < 6 * SECOND, "took {took:?}");
    assert!(report.elapsed >= 5 * SECOND && report.elapsed < 6 * SECOND);
    assert_eq!(mgr.stats().ended.completed, 3);
    assert_eq!(mgr.stats().ended.cancelled, 0);
}

#[tokio::test(start_paused = true)]
async fn drain_timeout_cancels_the_remaining_sessions() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let start = clock.now();
    let server = Server::start(settings(8, None, 10 * SECOND), clock.clone())
        .await
        .unwrap();
    let mgr = server.manager().clone();
    // One call ends by itself during the drain, two only when cancelled.
    mgr.start(|ctx: SessionContext| async move {
        ctx.clock.sleep(2 * SECOND).await;
        Done::Ok(())
    })
    .unwrap();
    for _ in 0..2 {
        mgr.start(|ctx: SessionContext| async move {
            ctx.cancel.cancelled().await;
            Done::Ok(())
        })
        .unwrap();
    }

    let (trigger, mut signals) = ShutdownSignals::manual();
    assert!(trigger.trigger(ShutdownSignal::Interrupt));
    let report = server.run(&mut signals).await;

    assert_eq!(report.initial, 3);
    assert_eq!(report.drained, 1);
    assert_eq!(report.cancelled, 2);
    assert_eq!((report.aborted, report.leaked), (0, 0));
    assert_eq!(report.outcome(), DrainOutcome::Cancelled);
    assert_eq!(report.outcome().exit_code(), 3);
    let took = clock.now() - start;
    assert!(took >= 10 * SECOND && took < 11 * SECOND, "took {took:?}");
    assert_eq!(mgr.stats().ended.cancelled, 2);
    assert_eq!(mgr.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_second_signal_cuts_the_drain_short() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let start = clock.now();
    let server = Server::start(settings(8, None, 600 * SECOND), clock.clone())
        .await
        .unwrap();
    let mgr = server.manager().clone();
    mgr.start(|ctx: SessionContext| async move {
        ctx.cancel.cancelled().await;
        Done::Ok(())
    })
    .unwrap();

    let (trigger, mut signals) = ShutdownSignals::manual();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(ShutdownSignal::Terminate));
    until_draining(&mgr).await;
    clock.sleep(2 * SECOND).await;
    assert_eq!(mgr.active(), 1, "still draining before the second signal");
    assert!(trigger.trigger(ShutdownSignal::Terminate));

    let report = run.await.unwrap();
    assert!(report.forced);
    assert_eq!((report.drained, report.cancelled), (0, 1));
    assert_eq!(report.outcome().exit_code(), 3);
    let took = clock.now() - start;
    assert!(took >= 2 * SECOND && took < 3 * SECOND, "took {took:?}");
}

#[tokio::test(start_paused = true)]
async fn sessions_that_ignore_cancellation_are_aborted() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let server = Server::start(settings(8, None, SECOND), clock.clone())
        .await
        .unwrap();
    let mgr = server.manager().clone();
    mgr.start(|_ctx: SessionContext| async move {
        std::future::pending::<()>().await;
        Done::Ok(())
    })
    .unwrap();

    let (trigger, mut signals) = ShutdownSignals::manual();
    // A signal stream that ends (no trigger left) is a shutdown request too.
    drop(trigger);
    let report = server.run(&mut signals).await;
    assert_eq!((report.cancelled, report.aborted, report.leaked), (0, 1, 0));
    assert_eq!(report.outcome(), DrainOutcome::Aborted);
    assert_eq!(report.outcome().exit_code(), 4);
    assert_eq!(mgr.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_idle_server_drains_at_once() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let start = clock.now();
    let server = Server::start(settings(8, None, 30 * SECOND), clock.clone())
        .await
        .unwrap();
    let (trigger, mut signals) = ShutdownSignals::manual();
    trigger.trigger(ShutdownSignal::Requested);
    let report = server.run(&mut signals).await;
    assert_eq!(report.initial, 0);
    assert_eq!(report.outcome(), DrainOutcome::Drained);
    assert!(clock.now() - start < SECOND);
}

/// One HTTP/1.1 exchange; returns the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: probe\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split("\r\n\r\n").nth(1).unwrap().to_owned();
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_flips_to_503_at_drain_start_and_healthz_stays_200() {
    let any_port = "127.0.0.1:0".parse().unwrap();
    let server = Server::start(
        settings(4, Some(any_port), Duration::from_secs(20)),
        SystemClock::shared(),
    )
    .await
    .unwrap();
    let addr = server.health_addr().unwrap();
    let mgr = server.manager().clone();
    let (release, wait) = oneshot::channel::<()>();
    mgr.start(move |_ctx: SessionContext| async move {
        let _ = wait.await;
        Done::Ok(())
    })
    .unwrap();

    assert_eq!(
        get(addr, "/healthz").await,
        (200, r#"{"status":"ok"}"#.into())
    );
    assert_eq!(
        get(addr, "/readyz").await,
        (
            200,
            r#"{"status":"ready","accepting":true,"active":1,"reserved":0,"max":4,"rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},"providers":[]}"#.into()
        )
    );

    let (trigger, mut signals) = ShutdownSignals::manual();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(ShutdownSignal::Terminate));
    let mut flipped = false;
    for _ in 0..500 {
        if !mgr.is_accepting() {
            flipped = true;
            break;
        }
        SystemClock.sleep(Duration::from_millis(2)).await;
    }
    assert!(flipped, "drain did not begin");

    // Draining: not ready, still alive, the session still counted.
    assert_eq!(
        get(addr, "/readyz").await,
        (
            503,
            r#"{"status":"draining","accepting":false,"active":1,"reserved":0,"max":4,"rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},"providers":[]}"#.into()
        )
    );
    assert_eq!(get(addr, "/healthz").await.0, 200);
    assert!(!run.is_finished(), "the drain waits for the active call");

    release.send(()).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome(), DrainOutcome::Drained);
    assert_eq!(report.drained, 1);

    // The health listener is closed once the drain is over.
    let connect = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await;
    assert!(connect.is_err() || connect.unwrap().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_handlers_install_inside_a_runtime() {
    let signals = ShutdownSignals::install().unwrap();
    drop(signals);
}

#[test]
fn signal_handlers_need_a_runtime() {
    assert!(ShutdownSignals::install().is_err());
}

#[test]
fn exit_codes_are_distinct() {
    let codes = [
        brisvox_server::EXIT_STARTUP_FAILURE,
        DrainOutcome::Drained.exit_code(),
        DrainOutcome::Cancelled.exit_code(),
        DrainOutcome::Aborted.exit_code(),
        DrainOutcome::Leaked.exit_code(),
    ];
    assert_eq!(codes, [1, 0, 3, 4, 5]);
}

/// Regression: the binary must not wait for a `spawn_blocking` closure an
/// aborted session left behind, or the process never reports its exit status. Dropping the
/// runtime (the old `main`) blocks until the closure returns; `shutdown_runtime` abandons it.
#[test]
fn shutdown_runtime_does_not_wait_for_a_stuck_blocking_task() {
    use std::sync::mpsc;
    use std::time::Duration;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    runtime.spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            // Stands in for a stuck inference call; bounded so the test cannot hang forever.
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
            let _ = done_tx.send(());
        })
        .await;
    });
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("blocking task started");

    brisvox_server::shutdown_runtime(runtime);

    assert_eq!(
        done_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "shutdown_runtime waited for the stuck blocking task"
    );
    drop(release_tx);
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("blocking task ends once released");
}
