//! [`Offload`]: where heavy work runs.
//!
//! CPU work heavier than ~50 us (VAD inference, Smart Turn, large response assembly, resampling
//! of long buffers) must not run on a tokio worker (CONTRIBUTING.md invariant). Stages call
//! [`Offload::run`] and do not care where the job goes:
//!
//! - [`Offload::Pool`]: the dedicated `brisvox-inference` pool. Real time only: the pool's
//!   threads live outside the runtime, so a paused-time simulation cannot account for them.
//! - [`Offload::Inline`]: the job runs on the calling task, to completion, before `run`
//!   returns. For deterministic simulations under a paused `SimClock`, where "inference" is a
//!   cheap scripted model. A job that blocks stalls its worker: never use it with a real model.
//!
//! Smart Turn analyses (20-25 ms each on ONNX Runtime) can run on an executor of their own
//! ([`start_smart_turn_executor`]) instead of on the pool the VAD windows (60-100 us each,
//! one per call every 32 ms) use; `SessionBuilder::with_smart_turn_offload` routes them there.
//!
//! Both variants have the same contract: a job panic fails only that request
//! (`Error::JobPanicked`), the returned [`Done`] carries the job's timing on the injected clock,
//! and a request future that is dropped before it ran never runs the job (the inline variant
//! runs it at first poll; the pool skips an abandoned unstarted job).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_inference::{
    Done, Error, InferencePool, JobTiming, PoolConfig, PoolHandle, Priority, Result,
};

/// Waiting analyses the Smart Turn executor holds per thread (its queue is this times its
/// threads). About 0.8 s of work at 25 ms an analysis: a verdict that would start later than
/// that is no better than the turn's silence fallback, so the executor refuses it at once
/// ([`Error::PoolSaturated`]; the turn then ends at its fallback deadline) instead of letting
/// it age in a queue until the analysis timeout.
pub const SMART_TURN_QUEUE_PER_THREAD: usize = 32;

/// Name prefix of the Smart Turn executor's threads (`brisvox-turn-<i>`).
pub const SMART_TURN_THREAD_NAME: &str = "brisvox-turn";

/// How many threads the Smart Turn executor gets when nothing says otherwise, for an inference
/// pool of `pool_threads` threads (the server's `cores / 2`): as many as the pool, at least one.
///
/// Measured (docs/LATENCY.md): an analysis runs ~20-25 ms and a call asks for one about
/// every 9 s, so one thread is 55% busy at 200 calls, and with turns that end together the queue
/// in front of it delays the verdict (one thread: p95 1331 ms at 200 calls, over the SLO; two
/// threads: 845 ms). The executor stays at most half busy up to ~(pool threads x 180) calls.
/// Threads in all: the VAD's pool plus this one, `cores` in a server sized by default (the VAD's
/// threads are idle most of the time; the analyses' ONNX Runtime intra-op workers come on top).
pub const fn default_smart_turn_threads(pool_threads: usize) -> usize {
    if pool_threads == 0 { 1 } else { pool_threads }
}

/// Starts the executor the Smart Turn analyses run on: `threads` plain OS threads of
/// their own with a bounded queue of [`SMART_TURN_QUEUE_PER_THREAD`] jobs per thread (overflow
/// policy: reject the newest; a rejected analysis fails like a timed-out one and the turn ends
/// at its fallback deadline, it never waits and never touches the VAD's pool). `None` for
/// `threads == 0`: the analyses then share the VAD's pool, as in the earlier single-pool layout.
///
/// `brisvox-server` and the benchmarks' engine target call this one function, so what the
/// benchmarks measure is what the server runs.
///
/// # Errors
/// [`Error::Spawn`] if a thread cannot be created.
pub fn start_smart_turn_executor(
    threads: usize,
    clock: SharedClock,
) -> Result<Option<InferencePool>> {
    if threads == 0 {
        return Ok(None);
    }
    InferencePool::new(
        &PoolConfig {
            threads,
            queue_capacity: threads.saturating_mul(SMART_TURN_QUEUE_PER_THREAD),
            name: SMART_TURN_THREAD_NAME.to_owned(),
        },
        clock,
    )
    .map(Some)
}

/// Where to run a heavy job. Cheap to clone.
#[derive(Debug, Clone)]
pub enum Offload {
    /// The shared inference pool (production, real-time tests).
    Pool(PoolHandle),
    /// Run on the calling task, timing with this clock (paused-time simulations).
    Inline(SharedClock),
}

/// What [`Offload::warm_each`] and [`Offload::warm`] achieved: how many of the jobs succeeded,
/// how many failed (an error, a panic, a timeout, a saturated or closed pool), and how many were
/// not submitted because the pool was busy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WarmReport {
    /// Jobs that returned `Ok`.
    pub ok: usize,
    /// Jobs that failed in any way.
    pub failed: usize,
    /// Jobs not submitted: the pool's queue was deeper than [`WARM_MAX_QUEUE_PER_THREAD`] per
    /// thread (live work goes first; the claim cools down, see [`WARM_COOLDOWN`]).
    pub skipped: usize,
}

/// How long a warm-up that failed because the pool was busy (its queue too deep, a job refused
/// as saturated or timed out) keeps its claim before another call may try again
/// ([`brisvox_inference::PoolHandle::release_warm_after`], on the pool's clock). Without it every
/// call starting during an overload would push another set of warm jobs into the busy queue.
pub const WARM_COOLDOWN: Duration = Duration::from_secs(30);

/// A warm-up is submitted only while the pool's queue holds at most this many waiting jobs per
/// worker thread: warm jobs queue FIFO like every other job, so on a busy pool they would sit
/// ahead of the running calls' VAD windows.
pub const WARM_MAX_QUEUE_PER_THREAD: usize = 1;

/// The right to run the one warm-up of a model on a pool, from [`Offload::claim_warm`].
///
/// [`Offload::warm`] hands a share of the claim to every job it submits; the claim is settled
/// when the last share is gone, that is when the warm-up's caller and every job (run, skipped
/// or still running after its caller timed out) have finished:
/// - every job succeeded: the claim is kept, later calls on the pool skip the warm-up;
/// - a job failed because the pool was busy (saturated, timed out, not submitted): the claim
///   comes back after [`WARM_COOLDOWN`];
/// - any other failure, or the warm-up was dropped (its call ended) before it finished: the
///   claim comes back at once, the next call warms again.
///
/// A job that is still running when its caller timed out keeps its share, so the claim is not
/// given back while that warm-up still runs (no duplicate warm-ups of the same model).
#[derive(Debug)]
pub struct WarmClaim {
    /// `None` inline (nothing to release).
    state: Option<Arc<ClaimState>>,
}

/// The shared state of a pool claim (see [`WarmClaim`]).
#[derive(Debug)]
struct ClaimState {
    pool: PoolHandle,
    key: String,
    /// Jobs that must succeed for the warm-up to be kept (0 until [`Offload::warm`] set it).
    expected: AtomicUsize,
    ok: AtomicUsize,
    /// A failure that came from a busy pool (cool down instead of releasing at once).
    busy: AtomicBool,
}

impl Drop for ClaimState {
    fn drop(&mut self) {
        let expected = self.expected.load(Ordering::Acquire);
        if expected > 0 && self.ok.load(Ordering::Acquire) >= expected {
            return; // warm for good
        }
        if self.busy.load(Ordering::Acquire) {
            self.pool.release_warm_after(&self.key, WARM_COOLDOWN);
        } else {
            self.pool.release_warm(&self.key);
        }
    }
}

/// Whether a pool error means the pool was busy rather than the job broken.
pub(crate) const fn is_busy(error: &Error) -> bool {
    matches!(
        error,
        Error::PoolSaturated { .. } | Error::Core(brisvox_core::Error::Timeout { .. })
    )
}

impl Offload {
    /// Whether jobs go to the pool.
    pub const fn is_pool(&self) -> bool {
        matches!(self, Self::Pool(_))
    }

    /// Claims the warm-up of the model named `key`: a model whose sessions live per pool thread
    /// needs it once per pool for the whole process, not once per call. On the pool, `Some` for
    /// the first caller only, and `None` while another call holds or kept it, or while a warm-up
    /// that failed on a busy pool cools down ([`WarmClaim`] says when a claim comes back): a
    /// burst of call starts then submits one set of warm jobs, instead of one per call that
    /// would fill the shared queue and make other calls' real jobs fail as saturated. Inline
    /// every call warms (it touches no shared queue).
    ///
    /// Key it on what the warmed state belongs to: the model file for state kept per pool
    /// thread (the ONNX Runtime Silero), the model instance for state kept in the instance (the
    /// ONNX Runtime Smart Turn), see [`brisvox_inference::smart_turn::TurnModel::warm_key`].
    pub fn claim_warm(&self, key: &str) -> Option<WarmClaim> {
        match self {
            Self::Inline(_) => Some(WarmClaim { state: None }),
            Self::Pool(pool) => pool.claim_warm(key).then(|| WarmClaim {
                state: Some(Arc::new(ClaimState {
                    pool: pool.clone(),
                    key: key.to_owned(),
                    expected: AtomicUsize::new(0),
                    ok: AtomicUsize::new(0),
                    busy: AtomicBool::new(false),
                })),
            }),
        }
    }

    /// How many jobs can run at the same time: the pool's worker threads (at least 1), or 1
    /// inline.
    pub fn threads(&self) -> usize {
        match self {
            Self::Pool(pool) => pool.threads().max(1),
            Self::Inline(_) => 1,
        }
    }

    /// Runs `job` once per worker thread, all submitted at the same moment (model warm-up: the
    /// ONNX Runtime Silero keeps one session per THREAD, so a single job would only warm one).
    /// Inline it runs once. Each job is bounded by `timeout`. Best effort: on an idle pool every
    /// thread takes exactly one of the jobs (a load takes tens of milliseconds, so a thread that
    /// is still busy loading cannot take a second one); on a busy pool a thread that was already
    /// warm may take two and leave another cold, which costs that thread's first real job its
    /// cold start and nothing else.
    ///
    /// The returned future owns its jobs: dropping it abandons the ones not started. Without a
    /// claim; [`Self::warm`] is the claimed, gated form the stages use.
    pub async fn warm_each<F>(&self, timeout: Duration, job: F) -> WarmReport
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        self.submit_warm(None, timeout, self.threads(), Arc::new(job))
            .await
    }

    /// The warm-up of `claim`: `jobs` jobs (at least one) submitted at the same moment, each
    /// running `job` once, bounded by `timeout`; inline `job` runs once. On a pool whose queue
    /// holds more than [`WARM_MAX_QUEUE_PER_THREAD`] waiting jobs per thread nothing is submitted
    /// (the running calls' work goes first): the jobs are reported as `skipped` and the claim
    /// cools down. Every job carries a share of `claim`, which settles as [`WarmClaim`] says once
    /// the last job and this future are gone.
    ///
    /// The returned future owns its jobs: dropping it abandons the ones not started.
    pub async fn warm<F>(
        &self,
        claim: WarmClaim,
        timeout: Duration,
        jobs: usize,
        job: F,
    ) -> WarmReport
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        let jobs = jobs.max(1);
        let state = claim.state;
        if let Some(state) = &state {
            state.expected.store(jobs, Ordering::Release);
        }
        if let Self::Pool(pool) = self
            && pool.queue_depth() > self.threads().saturating_mul(WARM_MAX_QUEUE_PER_THREAD)
        {
            if let Some(state) = &state {
                state.busy.store(true, Ordering::Release);
            }
            return WarmReport {
                skipped: jobs,
                ..WarmReport::default()
            };
        }
        self.submit_warm(state, timeout, jobs, Arc::new(job)).await
    }

    /// Submits `jobs` warm jobs at once (one inline), each holding a share of `state`.
    async fn submit_warm<F>(
        &self,
        state: Option<Arc<ClaimState>>,
        timeout: Duration,
        jobs: usize,
        job: Arc<F>,
    ) -> WarmReport
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        let mut report = WarmReport::default();
        let make = |state: Option<Arc<ClaimState>>| {
            let job = Arc::clone(&job);
            move || {
                let result = job();
                if result.is_ok()
                    && let Some(state) = &state
                {
                    state.ok.fetch_add(1, Ordering::AcqRel);
                }
                result
            }
        };
        let count = |report: &mut WarmReport, outcome: Result<Done<Result<()>>>| match outcome {
            Ok(Done { value: Ok(()), .. }) => report.ok += 1,
            Ok(Done { value: Err(_), .. }) => report.failed += 1,
            Err(error) => {
                if is_busy(&error)
                    && let Some(state) = &state
                {
                    state.busy.store(true, Ordering::Release);
                }
                report.failed += 1;
            }
        };
        match self {
            Self::Inline(_) => {
                count(&mut report, self.run(timeout, make(state.clone())).await);
            }
            Self::Pool(pool) => {
                let mut tasks = tokio::task::JoinSet::new();
                for _ in 0..jobs {
                    let (pool, work) = (pool.clone(), make(state.clone()));
                    tasks.spawn(async move { pool.run(timeout, work).await });
                }
                while let Some(joined) = tasks.join_next().await {
                    match joined {
                        Ok(outcome) => count(&mut report, outcome),
                        Err(_) => report.failed += 1,
                    }
                }
            }
        }
        report
    }

    /// Sleeps for `duration` on the injected clock (the pool's, or the inline variant's).
    pub async fn sleep(&self, duration: Duration) {
        match self {
            Self::Pool(pool) => pool.sleep(duration).await,
            Self::Inline(clock) => clock.sleep(duration).await,
        }
    }

    /// Runs `job` and waits for its result.
    ///
    /// `timeout` is enforced by the pool on the injected clock. The inline variant cannot time
    /// out a job that already completed: it ignores `timeout`.
    ///
    /// # Errors
    /// - `Error::PoolSaturated`: the pool's queue is full, the job did not run (pool only).
    /// - `Error::JobPanicked`: the job panicked; nothing else is affected.
    /// - `Error::Core(Timeout)`: no result within `timeout` (pool only).
    /// - `Error::Core(ChannelClosed)`: the pool has shut down (pool only).
    pub async fn run<T, F>(&self, timeout: Duration, job: F) -> Result<Done<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.run_with(Priority::Normal, timeout, job).await
    }

    /// [`Self::run`] with a pool [`Priority`] (the inline variant has no queue and ignores it):
    /// short jobs a call needs every 20-32 ms (VAD windows) go first, so a burst of long jobs
    /// (Smart Turn analyses, builds) cannot make them wait or overflow the queue.
    ///
    /// # Errors
    /// As [`Self::run`].
    pub async fn run_with<T, F>(
        &self,
        priority: Priority,
        timeout: Duration,
        job: F,
    ) -> Result<Done<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        match self {
            Self::Pool(pool) => pool.run_with(priority, timeout, job).await,
            Self::Inline(clock) => {
                let started = clock.now();
                let outcome = catch_unwind(AssertUnwindSafe(job));
                let run = clock.now().saturating_duration_since(started);
                match outcome {
                    Ok(value) => Ok(Done {
                        value,
                        timing: JobTiming {
                            wait: Duration::ZERO,
                            run,
                        },
                    }),
                    Err(payload) => {
                        // A payload whose own drop panics must not take the caller down.
                        let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
                        Err(Error::JobPanicked)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    use brisvox_core::{Clock, SimClock, SystemClock};
    use brisvox_inference::{InferencePool, PoolConfig};
    use tokio::time::{Instant, Sleep};

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    /// Yields (never sleeps: a paused clock must not jump) until `condition` holds, at most
    /// `SPIN_LIMIT` times with a short real pause between, so a pool that never gets there fails
    /// the test instead of hanging it.
    #[allow(clippy::disallowed_methods)] // real time: the pool's threads are not on the clock
    async fn spin_until(what: &str, condition: impl Fn() -> bool) {
        const SPIN_LIMIT: usize = 200_000;
        for _ in 0..SPIN_LIMIT {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_micros(50));
        }
        panic!("gave up waiting for {what}");
    }

    #[tokio::test(start_paused = true)]
    async fn inline_runs_the_job_at_once_and_reports_timing_on_the_clock() {
        let clock = SimClock::shared();
        let offload = Offload::Inline(clock.clone());
        assert!(!offload.is_pool());
        let before = clock.now();
        let done = offload.run(SECOND, || 21 * 2).await.unwrap();
        assert_eq!(done.value, 42);
        assert_eq!(done.timing.wait, Duration::ZERO);
        assert_eq!(done.timing.run, Duration::ZERO, "paused time does not move");
        assert_eq!(clock.now(), before);
    }

    /// A clock that moves by `step` on every `now()` call, so a job's measured run time is
    /// exactly one step (`now()` is read once before and once after the job), and that counts
    /// the sleeps requested from it.
    #[derive(Debug)]
    struct StepClock {
        sim: SimClock,
        step: Duration,
        reads: AtomicU32,
        sleeps: AtomicU32,
    }

    impl StepClock {
        fn new(step: Duration) -> Arc<Self> {
            Arc::new(Self {
                sim: SimClock::new(),
                step,
                reads: AtomicU32::new(0),
                sleeps: AtomicU32::new(0),
            })
        }
    }

    impl Clock for StepClock {
        fn now(&self) -> Instant {
            let reads = self.reads.fetch_add(1, Ordering::SeqCst);
            self.sim.now() + self.step * reads
        }

        fn wall(&self) -> SystemTime {
            self.sim.wall()
        }

        fn sleep_until(&self, deadline: Instant) -> Sleep {
            self.sleeps.fetch_add(1, Ordering::SeqCst);
            self.sim.sleep_until(deadline)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inline_reports_the_job_timing_measured_on_the_injected_clock() {
        let step = Duration::from_millis(7);
        let clock = StepClock::new(step);
        let offload = Offload::Inline(clock.clone());
        let done = offload.run(SECOND, || "x").await.unwrap();
        assert_eq!(done.timing.run, step, "run is the injected clock's advance");
        assert_eq!(done.timing.wait, Duration::ZERO);
        assert_eq!(
            clock.reads.load(Ordering::SeqCst),
            2,
            "read before and after"
        );
        // A panicking job leaves no timing behind but the clock is still the injected one.
        let failed = offload.run(SECOND, || -> u32 { panic!("bug") }).await;
        assert!(matches!(failed, Err(Error::JobPanicked)));
        assert_eq!(clock.reads.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn inline_isolates_a_panicking_job() {
        let offload = Offload::Inline(SimClock::shared());
        let failed = offload.run(SECOND, || -> u32 { panic!("model bug") }).await;
        assert!(matches!(failed, Err(Error::JobPanicked)));
        // The caller and later jobs are unaffected.
        assert_eq!(offload.run(SECOND, || 7).await.unwrap().value, 7);
    }

    #[tokio::test(start_paused = true)]
    async fn inline_job_never_runs_if_the_request_is_dropped_before_it_is_polled() {
        let offload = Offload::Inline(SimClock::shared());
        let ran = Arc::new(AtomicBool::new(false));
        let request = offload.run(SECOND, {
            let ran = Arc::clone(&ran);
            move || ran.store(true, Ordering::SeqCst)
        });
        drop(request);
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn inline_jobs_run_in_submission_order() {
        let offload = Offload::Inline(SimClock::shared());
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..5 {
            let order = Arc::clone(&order);
            offload
                .run(SECOND, move || order.lock().unwrap().push(i))
                .await
                .unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 3, 4]);
    }

    fn pool(threads: usize, queue_capacity: usize) -> InferencePool {
        InferencePool::new(
            &PoolConfig {
                threads,
                queue_capacity,
                name: "offload-test".to_owned(),
            },
            SystemClock::shared(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pool_runs_jobs_off_the_runtime_threads() {
        let pool = pool(1, 8);
        let offload = Offload::Pool(pool.handle());
        assert!(offload.is_pool());
        let caller = std::thread::current().id();
        let done = offload
            .run(SECOND, move || std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(done.value, caller, "job ran on the calling thread");
        let failed = offload.run(SECOND, || -> u32 { panic!("model bug") }).await;
        assert!(matches!(failed, Err(Error::JobPanicked)));
        assert_eq!(offload.run(SECOND, || 5).await.unwrap().value, 5);
        drop(offload);
        tokio::task::spawn_blocking(move || pool.shutdown())
            .await
            .unwrap();
    }

    /// Occupies the pool's only thread with a job that waits for the returned release handle.
    async fn hold_the_thread(
        offload: &Offload,
    ) -> (mpsc::Sender<()>, tokio::task::JoinHandle<Result<Done<()>>>) {
        let (release, hold) = mpsc::channel::<()>();
        let started = Arc::new(AtomicUsize::new(0));
        let job = tokio::spawn({
            let (offload, started) = (offload.clone(), Arc::clone(&started));
            async move {
                // Far longer than any test waits, so it never fires before the timeout the test
                // is about. It does NOT make awaiting the holder safe under paused time: the
                // timeout is a timer like any other, and the runtime jumps to the nearest timer
                // whenever it is idle, however far away, while the pool thread (which is not part
                // of the runtime) finishes the job. Paused-time tests finish the holder with
                // `finish_holder`, which keeps the runtime busy instead of idle.
                offload
                    .run(Duration::from_secs(3_600), move || {
                        started.fetch_add(1, Ordering::SeqCst);
                        let _ = hold.recv();
                    })
                    .await
            }
        });
        spin_until("the holder to start", || started.load(Ordering::SeqCst) > 0).await;
        (release, job)
    }

    /// Releases the held job and waits for its result WITHOUT letting the runtime go idle, so a
    /// paused clock cannot jump to the holder's timeout while the pool thread is still finishing
    /// (the flaky failure `Timeout { what: "inference job", after: 3600s }`).
    async fn finish_holder(
        release: mpsc::Sender<()>,
        holder: tokio::task::JoinHandle<Result<Done<()>>>,
    ) {
        release.send(()).unwrap();
        spin_until("the holder to finish", || holder.is_finished()).await;
        holder.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn pool_enforces_the_timeout_on_the_injected_clock() {
        let pool = pool(1, 4);
        let offload = Offload::Pool(pool.handle());
        let (release, holder) = hold_the_thread(&offload).await;

        // The only thread is busy: this job waits in the queue and times out (real clock).
        let timed_out = offload.run(Duration::from_millis(30), || 1).await;
        assert!(
            matches!(
                timed_out,
                Err(Error::Core(brisvox_core::Error::Timeout { .. }))
            ),
            "{timed_out:?}"
        );
        assert_eq!(pool.metrics().timed_out, 1);

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        // The abandoned job is skipped; the pool keeps serving.
        assert_eq!(offload.run(SECOND, || 9).await.unwrap().value, 9);
        drop(offload);
        tokio::task::spawn_blocking(move || pool.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pool_timeout_runs_on_the_injected_clock_not_on_real_time() {
        let clock = StepClock::new(Duration::ZERO);
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 1,
                queue_capacity: 4,
                name: "offload-clock".to_owned(),
            },
            clock.clone(),
        )
        .unwrap();
        let offload = Offload::Pool(pool.handle());
        let (release, holder) = hold_the_thread(&offload).await;

        // The thread is busy, so this job times out: 5 s of simulated time pass at once (the
        // runtime auto-advances while idle, before the holder job's own one-hour timeout),
        // which a real-time timeout would need 5 s for.
        let sleeps_before = clock.sleeps.load(Ordering::SeqCst);
        let started = clock.sim.now();
        let timed_out = offload.run(Duration::from_secs(5), || 1).await;
        assert!(
            matches!(
                timed_out,
                Err(Error::Core(brisvox_core::Error::Timeout { .. }))
            ),
            "{timed_out:?}"
        );
        assert!(clock.sleeps.load(Ordering::SeqCst) > sleeps_before);
        assert_eq!(
            clock.sim.now().saturating_duration_since(started),
            Duration::from_secs(5)
        );
        assert_eq!(pool.metrics().timed_out, 1);

        finish_holder(release, holder).await;
        drop(offload);
        tokio::task::spawn_blocking(move || pool.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pool_rejects_instead_of_waiting_when_its_queue_is_full() {
        // One thread, one queue slot.
        let pool = pool(1, 1);
        let offload = Offload::Pool(pool.handle());
        let (release, holder) = hold_the_thread(&offload).await;

        let queued = tokio::spawn({
            let offload = offload.clone();
            async move { offload.run(Duration::from_secs(10), || 2).await }
        });
        spin_until("a queued job", || pool.metrics().queue_depth >= 1).await;
        let rejected = offload.run(SECOND, || 3).await;
        assert!(
            matches!(rejected, Err(Error::PoolSaturated { capacity: 1 })),
            "{rejected:?}"
        );
        assert_eq!(pool.metrics().rejected, 1);

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        assert_eq!(queued.await.unwrap().unwrap().value, 2);
        drop(offload);
        tokio::task::spawn_blocking(move || pool.shutdown())
            .await
            .unwrap();
    }

    #[test]
    fn no_smart_turn_threads_means_no_executor() {
        assert!(
            start_smart_turn_executor(0, SystemClock::shared())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_smart_turn_job_runs_while_every_thread_of_the_vad_pool_is_busy() {
        // The VAD pool's only thread is held by a long job (an analysis, in the earlier single-pool layout): the
        // executor's own thread still takes the analysis, and the VAD's pool is not involved.
        let vad_pool = pool(1, 8);
        let vad = Offload::Pool(vad_pool.handle());
        let (release, holder) = hold_the_thread(&vad).await;
        let executor = start_smart_turn_executor(1, SystemClock::shared())
            .unwrap()
            .unwrap();
        let turn = Offload::Pool(executor.handle());
        let name = turn
            .run(SECOND, || std::thread::current().name().map(str::to_owned))
            .await
            .unwrap()
            .value;
        assert_eq!(name.as_deref(), Some("brisvox-turn-0"));
        assert_eq!(
            turn.threads(),
            1,
            "the warm-up sizes itself on the executor"
        );
        assert_eq!(vad.threads(), 1);
        assert_eq!(executor.metrics().completed, 1);
        assert_eq!(vad_pool.metrics().completed, 0);
        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        drop((vad, turn));
        tokio::task::spawn_blocking(move || {
            vad_pool.shutdown();
            executor.shutdown();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_smart_turn_executor_refuses_beyond_its_bounded_queue_and_recovers() {
        // One thread: SMART_TURN_QUEUE_PER_THREAD jobs wait, the next is refused at once (the
        // turn then ends at its fallback deadline) and later analyses are served again.
        let executor = start_smart_turn_executor(1, SystemClock::shared())
            .unwrap()
            .unwrap();
        let turn = Offload::Pool(executor.handle());
        let (release, holder) = hold_the_thread(&turn).await;
        let mut waiting = Vec::new();
        for _ in 0..SMART_TURN_QUEUE_PER_THREAD {
            let turn = turn.clone();
            waiting.push(tokio::spawn(async move {
                turn.run(Duration::from_secs(30), || 1).await
            }));
        }
        spin_until("a full executor queue", || {
            executor.metrics().queue_depth >= SMART_TURN_QUEUE_PER_THREAD
        })
        .await;
        let refused = turn.run(SECOND, || 2).await;
        assert!(
            matches!(refused, Err(Error::PoolSaturated { .. })),
            "{refused:?}"
        );
        assert_eq!(executor.metrics().rejected, 1);
        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        for job in waiting {
            assert_eq!(job.await.unwrap().unwrap().value, 1);
        }
        assert_eq!(turn.run(SECOND, || 3).await.unwrap().value, 3);
        drop(turn);
        tokio::task::spawn_blocking(move || executor.shutdown())
            .await
            .unwrap();
    }
}
