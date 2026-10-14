//! Inference pool behaviour on real threads (real time, short waits; every blocking wait in a
//! job has its own timeout so a broken pool fails instead of hanging).
#![allow(clippy::unwrap_used, clippy::panic)] // test helpers outside #[test] fns

use std::sync::mpsc;
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock};
use brisvox_inference::{Done, Error, InferencePool, PoolConfig, PoolHandle, Priority};

const MS: Duration = Duration::from_millis(1);
const LONG: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn a_warm_claim_released_with_a_cooldown_is_refused_until_it_ends() {
    // A warm-up that failed on a busy pool gives its claim back only after a
    // cool-down, so the calls that start meanwhile do not each queue another set of warm jobs.
    let clock = brisvox_core::SimClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 4,
            name: "warm-claims".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let handle = pool.handle();
    assert!(handle.claim_warm("m"));
    assert!(!handle.claim_warm("m"), "claimed");
    assert_eq!(
        handle.warm_cooling_until("m"),
        None,
        "claimed, not cooling down"
    );
    handle.release_warm_after("m", Duration::from_secs(10));
    assert_eq!(
        handle.warm_cooling_until("m"),
        Some(clock.now() + Duration::from_secs(10))
    );
    assert!(!handle.claim_warm("m"), "cooling down");
    clock.sleep(Duration::from_secs(9)).await;
    assert!(!handle.claim_warm("m"), "still cooling down");
    clock.sleep(Duration::from_secs(1)).await;
    assert!(handle.claim_warm("m"), "the cool-down ended");
    assert!(!handle.claim_warm("m"), "and the new claim holds");
    // A plain release gives it back at once; other keys are independent.
    handle.release_warm("m");
    assert!(handle.claim_warm("m"));
    assert!(handle.claim_warm("other"));
    // An unrepresentable cool-down never ends (and does not panic).
    handle.release_warm_after("other", Duration::MAX);
    assert!(!handle.claim_warm("other"));
    assert_eq!(handle.queue_depth(), 0);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

fn pool(threads: usize, queue_capacity: usize) -> (InferencePool, PoolHandle, SharedClock) {
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads,
            queue_capacity,
            name: "test-infer".into(),
        },
        clock.clone(),
    )
    .unwrap();
    let handle = pool.handle();
    (pool, handle, clock)
}

/// A job that reports it started, then blocks until released (or 5 s pass).
fn blocker(
    handle: &PoolHandle,
) -> (
    mpsc::Receiver<()>,
    mpsc::SyncSender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (started_tx, started) = mpsc::sync_channel::<()>(1);
    let (release, gate) = mpsc::sync_channel::<()>(1);
    let handle = handle.clone();
    let task = tokio::spawn(async move {
        handle
            .run(LONG, move || {
                started_tx.send(()).unwrap();
                let _ = gate.recv_timeout(LONG);
            })
            .await
            .unwrap();
    });
    (started, release, task)
}

/// Waits (on the clock, bounded by 5 s of clock time) until `condition` holds.
async fn eventually(clock: &SharedClock, condition: impl Fn() -> bool) {
    let deadline = clock.now() + LONG;
    while clock.now() < deadline {
        if condition() {
            return;
        }
        clock.sleep(MS).await;
    }
    panic!("condition not reached within {LONG:?}");
}

/// Blocks the current (test) thread until the job started; fine on a multi-thread runtime.
fn wait_started(started: &mpsc::Receiver<()>) {
    started.recv_timeout(LONG).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_run_on_pool_threads_not_tokio_workers() {
    let (pool, handle, _clock) = pool(2, 8);
    let done = handle
        .run(LONG, || std::thread::current().name().map(str::to_owned))
        .await
        .unwrap();
    let name = done.value.unwrap();
    assert!(name.starts_with("test-infer-"), "{name}");
    let m = pool.metrics();
    assert_eq!((m.submitted, m.completed, m.queue_depth), (1, 1, 0));
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_queue_rejects_instead_of_waiting() {
    let (pool, handle, clock) = pool(1, 1);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    // The only thread is busy: the next job waits in the queue (capacity 1) ...
    let queued = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.run(LONG, || 7).await })
    };
    eventually(&clock, || pool.metrics().queue_depth == 1).await;
    // ... and one more is rejected at once.
    let rejected = handle.run(LONG, || 8).await;
    assert!(
        matches!(rejected, Err(Error::PoolSaturated { capacity: 1 })),
        "{rejected:?}"
    );
    release.send(()).unwrap();
    first.await.unwrap();
    assert_eq!(queued.await.unwrap().unwrap().value, 7);
    let m = pool.metrics();
    assert_eq!((m.submitted, m.rejected, m.completed), (2, 1, 2));
    assert_eq!(m.max_queue_depth, 1);
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_job_that_never_started_is_skipped() {
    let (pool, handle, _clock) = pool(1, 4);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let (ran_tx, ran) = mpsc::sync_channel::<()>(1);
    let late = handle
        .run(50 * MS, move || {
            ran_tx.send(()).unwrap();
        })
        .await;
    assert!(
        matches!(
            late,
            Err(Error::Core(brisvox_core::Error::Timeout {
                what: "inference job",
                ..
            }))
        ),
        "{late:?}"
    );
    release.send(()).unwrap();
    first.await.unwrap();
    // The worker reaches the abandoned job after the blocker and skips it.
    assert_eq!(handle.run(LONG, || 1).await.unwrap().value, 1);
    assert!(ran.try_recv().is_err(), "abandoned job ran");
    let m = pool.metrics();
    assert_eq!((m.timed_out, m.skipped, m.completed), (1, 1, 2));
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_job_fails_only_its_own_request() {
    let (pool, handle, _clock) = pool(1, 4);
    let panicked = handle
        .run(LONG, || -> u32 { panic!("model blew up") })
        .await;
    assert!(matches!(panicked, Err(Error::JobPanicked)), "{panicked:?}");
    // Same single worker thread, still alive.
    assert_eq!(handle.run(LONG, || 42).await.unwrap().value, 42);
    let m = pool.metrics();
    assert_eq!((m.panicked, m.completed), (1, 1));
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_and_run_times_are_measured() {
    let (pool, handle, clock) = pool(1, 4);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let queued = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .run(LONG, || {
                    // ~20 ms of "work" without sleep(): a timed wait that never fires early.
                    let (_keep, never) = mpsc::sync_channel::<()>(1);
                    let _ = never.recv_timeout(20 * MS);
                })
                .await
        })
    };
    // Once the job is queued, hold the blocker ~30 ms so it waits at least that long.
    eventually(&clock, || pool.metrics().queue_depth == 1).await;
    let (_keep, never) = mpsc::sync_channel::<()>(1);
    let _ = never.recv_timeout(30 * MS);
    release.send(()).unwrap();
    first.await.unwrap();
    let timing = queued.await.unwrap().unwrap().timing;
    assert!(timing.wait >= 25 * MS, "{timing:?}");
    assert!(timing.run >= 20 * MS, "{timing:?}");
    let m = pool.metrics();
    assert!(
        m.wait_max >= timing.wait && m.run_max >= timing.run,
        "{m:?}"
    );
    assert!(m.wait_total >= timing.wait && m.run_total >= timing.run);
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handles_fail_cleanly_after_shutdown() {
    let (pool, handle, _clock) = pool(2, 4);
    assert_eq!(handle.run(LONG, || 1).await.unwrap().value, 1);
    pool.shutdown();
    let after = handle.run(LONG, || 2).await;
    assert!(
        matches!(
            after,
            Err(Error::Core(brisvox_core::Error::ChannelClosed(_)))
        ),
        "{after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_request_skips_its_queued_job() {
    let (pool, handle, clock) = pool(1, 4);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let (ran_tx, ran) = mpsc::sync_channel::<()>(1);
    let cancelled = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let _ = handle
                .run(LONG, move || {
                    ran_tx.send(()).unwrap();
                })
                .await;
        })
    };
    eventually(&clock, || pool.metrics().queue_depth == 1).await;
    // Barge-in: the session drops its request.
    cancelled.abort();
    let _ = cancelled.await;
    release.send(()).unwrap();
    first.await.unwrap();
    assert_eq!(handle.run(LONG, || 3).await.unwrap().value, 3);
    assert!(ran.try_recv().is_err(), "cancelled job ran");
    let m = pool.metrics();
    assert_eq!((m.skipped, m.timed_out, m.completed), (1, 0, 2));
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_after_the_job_started_lets_it_finish() {
    let (pool, handle, clock) = pool(1, 4);
    // A job that runs longer than its own timeout: it starts at once (idle pool), so the timeout
    // cannot skip it.
    let slow = handle
        .run(20 * MS, || {
            let (_keep, never) = mpsc::sync_channel::<()>(1);
            let _ = never.recv_timeout(100 * MS);
            5
        })
        .await;
    assert!(matches!(slow, Err(Error::Core(_))), "{slow:?}");
    // The job still runs to completion in the background.
    eventually(&clock, || pool.metrics().completed == 1).await;
    let m = pool.metrics();
    assert_eq!((m.timed_out, m.skipped, m.completed), (1, 0, 1));
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_queued_jobs() {
    let (pool, handle, clock) = pool(1, 8);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let queued: Vec<_> = (0..3)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.run(LONG, move || i).await })
        })
        .collect();
    eventually(&clock, || pool.metrics().queue_depth == 3).await;
    release.send(()).unwrap();
    // shutdown blocks until the queue is drained: keep it off the async workers.
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
    first.await.unwrap();
    for (i, task) in queued.into_iter().enumerate() {
        assert_eq!(task.await.unwrap().unwrap().value, i);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_requests_all_complete_with_consistent_counters() {
    let (pool, handle, _clock) = pool(4, 64);
    let tasks: Vec<_> = (0..50u64)
        .map(|session| {
            let handle = handle.clone();
            tokio::spawn(async move {
                let mut sum = 0;
                for k in 0..8u64 {
                    // At most 50 requests are in flight (one per task) < 64 slots: none is
                    // rejected.
                    sum += handle
                        .run(LONG, move || session * 8 + k)
                        .await
                        .unwrap()
                        .value;
                }
                sum
            })
        })
        .collect();
    let mut total = 0;
    for task in tasks {
        total += task.await.unwrap();
    }
    assert_eq!(total, (0..400u64).sum::<u64>());
    let m = pool.metrics();
    assert_eq!((m.submitted, m.completed, m.rejected), (400, 400, 0));
    assert_eq!(m.queue_depth, 0);
    assert!(m.max_queue_depth <= 50);
    pool.shutdown();
}

/// Submits `job` as a task and waits until it sits in the queue (`depth` queued jobs in all).
async fn queue_job(
    pool: &InferencePool,
    handle: &PoolHandle,
    clock: &SharedClock,
    priority: Priority,
    depth: usize,
    job: impl FnOnce() -> u32 + Send + 'static,
) -> tokio::task::JoinHandle<Result<Done<u32>, Error>> {
    let handle = handle.clone();
    let task = tokio::spawn(async move { handle.run_with(priority, LONG, job).await });
    eventually(clock, || pool.metrics().queue_depth == depth).await;
    task
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_high_priority_job_runs_before_the_normal_jobs_queued_ahead_of_it() {
    // The 1.5 ms Smart Turn analyses of a few hundred calls ending their turns together
    // queued ahead of the VAD windows of every call (one per 20 ms each) and overflowed the
    // queue; the windows now wait in a line of their own and are taken first.
    let (pool, handle, clock) = pool(1, 8);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let job = |id: u32| {
        let order = std::sync::Arc::clone(&order);
        move || {
            order.lock().unwrap().push(id);
            id
        }
    };
    let mut tasks = Vec::new();
    for (id, priority) in [
        (1, Priority::Normal),
        (2, Priority::Normal),
        (3, Priority::High),
        (4, Priority::Normal),
        (5, Priority::High),
    ] {
        let depth = tasks.len() + 1;
        tasks.push(queue_job(&pool, &handle, &clock, priority, depth, job(id)).await);
    }
    release.send(()).unwrap();
    first.await.unwrap();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        *order.lock().unwrap(),
        [3, 5, 1, 2, 4],
        "high jobs first, each line in submission order"
    );
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_two_lines_are_bounded_separately() {
    // A flood of normal jobs must not make a VAD window bounce off a full queue (a rejected
    // window loses the call's VAD stream), and the reverse.
    let (pool, handle, clock) = pool(1, 1);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let normal = queue_job(&pool, &handle, &clock, Priority::Normal, 1, || 1).await;
    let rejected = handle.run_with(Priority::Normal, LONG, || 2).await;
    assert!(
        matches!(rejected, Err(Error::PoolSaturated { capacity: 1 })),
        "the normal line is full: {rejected:?}"
    );
    let high = queue_job(&pool, &handle, &clock, Priority::High, 2, || 3).await;
    let rejected = handle.run_with(Priority::High, LONG, || 4).await;
    assert!(
        matches!(rejected, Err(Error::PoolSaturated { capacity: 1 })),
        "so is the high line: {rejected:?}"
    );
    release.send(()).unwrap();
    first.await.unwrap();
    assert_eq!(normal.await.unwrap().unwrap().value, 1);
    assert_eq!(high.await.unwrap().unwrap().value, 3);
    let m = pool.metrics();
    assert_eq!((m.submitted, m.rejected, m.completed), (3, 2, 3));
    assert_eq!(
        m.rejected_high, 1,
        "one of the two rejections was a high job"
    );
    pool.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_of_high_priority_jobs_is_counted_apart_from_the_normal_ones() {
    // What a VAD window pays for sharing threads with the long jobs is its own counter.
    let (pool, handle, clock) = pool(1, 8);
    let (started, release, first) = blocker(&handle);
    wait_started(&started);
    let normal = queue_job(&pool, &handle, &clock, Priority::Normal, 1, || 1).await;
    let high = queue_job(&pool, &handle, &clock, Priority::High, 2, || 2).await;
    // Both wait behind the held thread for a measurable time.
    clock.sleep(Duration::from_millis(40)).await;
    release.send(()).unwrap();
    first.await.unwrap();
    normal.await.unwrap().unwrap();
    high.await.unwrap().unwrap();
    let m = pool.metrics();
    assert_eq!(m.completed, 3);
    assert_eq!(m.high_started, 1, "only the high job counts");
    assert!(
        m.high_wait_total >= Duration::from_millis(30),
        "{:?}",
        m.high_wait_total
    );
    assert!(m.high_wait_max >= m.high_wait_total);
    // The normal job was queued first and started last, behind the held thread and the high job:
    // its wait (the total minus the high job's; the blocker's own wait is zero or close) is at
    // least the high job's, and it waited out the 40 ms too.
    let normal_wait = m.wait_total.saturating_sub(m.high_wait_total);
    assert!(
        normal_wait >= m.high_wait_total && normal_wait >= Duration::from_millis(30),
        "the normal job waited longer still: normal {normal_wait:?} vs high {:?}",
        m.high_wait_total
    );
    pool.shutdown();
}
