//! Dedicated CPU pool for inference and heavy DSP.
//!
//! Jobs run on plain OS threads owned by the pool, never on tokio workers, so a 1-30 ms model
//! call cannot stall the async runtime that moves every session's frames.
//!
//! - **Bounded queue**: [`PoolHandle::run`] never waits for queue space. A full queue rejects
//!   the job at once with [`Error::PoolSaturated`] (overflow policy: reject newest), so a
//!   saturated pool sheds load instead of building latency.
//! - **Two lines** ([`Priority`]): short latency-critical jobs ([`PoolHandle::run_with`]
//!   with [`Priority::High`]) wait in a line of their own, each line bounded by the queue
//!   capacity, and a worker always takes them first. Without it the 1 ms Smart Turn analyses of a few hundred calls ending their
//!   turns together queued ahead of ~75 000 VAD windows per second and overflowed the queue.
//! - **Per-request timeout** on the injected [`Clock`]: the caller gets
//!   [`brisvox_core::Error::Timeout`]. A job whose caller timed out **or dropped the request
//!   future** (barge-in, cancelled task) is skipped if it has not started yet; a running job
//!   cannot be interrupted and finishes in the background.
//! - **Panic isolation**: a panicking job fails only its own request ([`Error::JobPanicked`]);
//!   the worker thread survives.
//! - **Metrics**: queue depth, wait time (queued to started) and run time, as lock-free counters
//!   ([`InferencePool::metrics`]) and per request ([`Done::timing`]) for percentiles. A job's
//!   counters are updated before its caller is answered.
//!
//! **Real time only.** Jobs run on OS threads outside the tokio runtime, so a paused-time
//! `SimClock` cannot account for them: the runtime looks idle while a job runs, virtual time
//! jumps, and the timeout fires at once. Simulations must run model stages inline or behind a
//! mock.

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::error::{Error, Result};

/// Pool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// Worker threads (at least 1 is used).
    pub threads: usize,
    /// Jobs that may wait in the queue beyond those running (at least 1 is used), per line: the
    /// normal and the [`Priority::High`] line each hold this many.
    pub queue_capacity: usize,
    /// Thread name prefix (`<name>-<i>`).
    pub name: String,
}

impl Default for PoolConfig {
    /// Half the logical cores (at least one), 1024 queued jobs.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Self {
            threads: (cores / 2).max(1),
            queue_capacity: 1_024,
            name: "brisvox-infer".to_owned(),
        }
    }
}

/// How long a job waited and ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobTiming {
    /// Submitted to started.
    pub wait: Duration,
    /// Started to finished.
    pub run: Duration,
}

/// A finished job's value and timing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done<T> {
    /// What the job returned.
    pub value: T,
    /// Its timing.
    pub timing: JobTiming,
}

/// Counters since the pool started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolMetrics {
    /// Jobs accepted into the queue.
    pub submitted: u64,
    /// Jobs rejected because their line of the queue was full.
    pub rejected: u64,
    /// Of [`Self::rejected`], the [`Priority::High`] jobs (a rejected VAD window loses the call's
    /// VAD stream: this should stay zero).
    pub rejected_high: u64,
    /// Jobs that ran to completion (including ones whose caller had timed out).
    pub completed: u64,
    /// Jobs that panicked.
    pub panicked: u64,
    /// Requests whose caller timed out.
    pub timed_out: u64,
    /// Queued jobs skipped because their caller had already timed out.
    pub skipped: u64,
    /// Jobs queued now (not yet started).
    pub queue_depth: usize,
    /// Highest queue depth seen.
    pub max_queue_depth: usize,
    /// Sum of wait times of started jobs.
    pub wait_total: Duration,
    /// Longest wait.
    pub wait_max: Duration,
    /// [`Priority::High`] jobs (VAD windows) that started, and the sum and the longest of their
    /// waits: what a window pays for sharing the threads with the long jobs.
    pub high_started: u64,
    /// Sum of the waits of the [`Priority::High`] jobs that started.
    pub high_wait_total: Duration,
    /// Longest wait of a [`Priority::High`] job.
    pub high_wait_max: Duration,
    /// Sum of run times.
    pub run_total: Duration,
    /// Longest run.
    pub run_max: Duration,
}

#[derive(Default)]
struct Counters {
    submitted: AtomicU64,
    rejected: AtomicU64,
    rejected_high: AtomicU64,
    completed: AtomicU64,
    panicked: AtomicU64,
    timed_out: AtomicU64,
    skipped: AtomicU64,
    queue_depth: AtomicUsize,
    max_queue_depth: AtomicUsize,
    wait_total_ns: AtomicU64,
    wait_max_ns: AtomicU64,
    high_started: AtomicU64,
    high_wait_total_ns: AtomicU64,
    high_wait_max_ns: AtomicU64,
    run_total_ns: AtomicU64,
    run_max_ns: AtomicU64,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Counters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn record(total: &AtomicU64, max: &AtomicU64, d: Duration) {
        let ns = nanos(d);
        total.fetch_add(ns, Ordering::Relaxed);
        max.fetch_max(ns, Ordering::Relaxed);
    }

    fn snapshot(&self) -> PoolMetrics {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        PoolMetrics {
            submitted: get(&self.submitted),
            rejected: get(&self.rejected),
            rejected_high: get(&self.rejected_high),
            completed: get(&self.completed),
            panicked: get(&self.panicked),
            timed_out: get(&self.timed_out),
            skipped: get(&self.skipped),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            max_queue_depth: self.max_queue_depth.load(Ordering::Relaxed),
            wait_total: Duration::from_nanos(get(&self.wait_total_ns)),
            wait_max: Duration::from_nanos(get(&self.wait_max_ns)),
            high_started: get(&self.high_started),
            high_wait_total: Duration::from_nanos(get(&self.high_wait_total_ns)),
            high_wait_max: Duration::from_nanos(get(&self.high_wait_max_ns)),
            run_total: Duration::from_nanos(get(&self.run_total_ns)),
            run_max: Duration::from_nanos(get(&self.run_max_ns)),
        }
    }
}

/// A job's type-erased closure: runs the user code, records its run in the counters, then
/// replies to the caller (so a caller that got its answer sees up-to-date counters).
type Work = Box<dyn FnOnce(&dyn Clock, &Counters, Duration) + Send>;

/// A queued job: the closure plus bookkeeping.
struct Job {
    work: Work,
    priority: Priority,
    enqueued: Instant,
    abandoned: Arc<AtomicBool>,
}

/// How urgent a job is; see [`PoolHandle::run_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Priority {
    /// Short, frequent, latency-critical jobs (VAD windows: one per call and 20-32 ms of audio,
    /// microseconds to a fraction of a millisecond each). A worker always takes a waiting
    /// high-priority job before a normal one; one that is already running is not interrupted,
    /// so a high job waits at most for the longest normal job running.
    High,
    /// Everything else: models that run for milliseconds (Smart Turn), warm-ups, builds.
    /// First come, first served among themselves; they wait while high jobs are queued (an
    /// overloaded pool sheds them first, through their callers' timeouts).
    #[default]
    Normal,
}

/// The two waiting lines of the pool and what the workers need to know about them.
#[derive(Default)]
struct Lines {
    high: VecDeque<Job>,
    normal: VecDeque<Job>,
    /// Workers waiting on [`Queue::ready`]: a producer only signals when there is one (no
    /// system call for a busy pool).
    idle: usize,
    /// No more jobs will come: workers leave once both lines are empty.
    closed: bool,
}

/// The bounded two-lane job queue shared by the workers and the submitters. Both lines hold up
/// to `capacity` jobs (overflow policy: reject the newest, per line, so a flood of normal jobs
/// can never push a VAD window out and the reverse).
struct Queue {
    lines: Mutex<Lines>,
    ready: Condvar,
    capacity: usize,
}

/// Why [`Queue::push`] did not queue a job.
enum Refused {
    Full,
    Closed,
}

impl Queue {
    fn new(capacity: usize) -> Self {
        Self {
            lines: Mutex::new(Lines::default()),
            ready: Condvar::new(),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lines> {
        // Critical sections cannot panic, so a poisoned lock still guards consistent data.
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, priority: Priority, job: Job) -> std::result::Result<(), Refused> {
        let mut lines = self.lock();
        if lines.closed {
            return Err(Refused::Closed);
        }
        let line = match priority {
            Priority::High => &mut lines.high,
            Priority::Normal => &mut lines.normal,
        };
        if line.len() >= self.capacity {
            return Err(Refused::Full);
        }
        line.push_back(job);
        let wake = lines.idle > 0;
        drop(lines);
        if wake {
            self.ready.notify_one();
        }
        Ok(())
    }

    /// The next job, waiting for one; `None` once the queue is closed and drained.
    fn pop(&self) -> Option<Job> {
        let mut lines = self.lock();
        loop {
            // Strict priority: a waiting high job always goes first. Normal jobs are shed by
            // their callers' timeouts (an abandoned job is skipped) and the queue bound, which
            // is the right order of loss when the pool is overloaded: a Smart Turn verdict can
            // fall back, a lost VAD window loses the call's speech state.
            if let Some(job) = lines.high.pop_front().or_else(|| lines.normal.pop_front()) {
                return Some(job);
            }
            if lines.closed {
                return None;
            }
            lines.idle = lines.idle.saturating_add(1);
            lines = self
                .ready
                .wait(lines)
                .unwrap_or_else(PoisonError::into_inner);
            lines.idle = lines.idle.saturating_sub(1);
        }
    }

    /// No more jobs will come; the workers finish what is queued and leave.
    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }
}

struct Shared {
    queue: Arc<Queue>,
    counters: Arc<Counters>,
    clock: SharedClock,
    capacity: usize,
    threads: usize,
    /// Warm-up keys claimed so far ([`PoolHandle::claim_warm`]); a handful per process. `None`:
    /// claimed (warm or warming); `Some(t)`: a failed warm-up cooling down until `t`
    /// ([`PoolHandle::release_warm_after`]).
    warmed: Mutex<HashMap<String, Option<Instant>>>,
}

impl Drop for Shared {
    /// The last owner is gone (the pool was shut down or dropped; handles only hold weak
    /// references): the workers finish the queued jobs and leave.
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// The pool: owns the worker threads. Hand out [`PoolHandle`]s to sessions.
pub struct InferencePool {
    shared: Arc<Shared>,
    counters: Arc<Counters>,
    threads: Vec<JoinHandle<()>>,
}

/// A cheap, cloneable way to submit jobs. Fails with a closed-channel error once the pool has
/// shut down.
#[derive(Clone)]
pub struct PoolHandle {
    shared: Weak<Shared>,
}

impl std::fmt::Debug for PoolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolHandle").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for InferencePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferencePool")
            .field("threads", &self.threads.len())
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

fn worker(queue: &Queue, counters: &Counters, clock: &dyn Clock) {
    loop {
        let Some(job) = queue.pop() else {
            return; // the pool shut down and the queue is drained
        };
        counters.queue_depth.fetch_sub(1, Ordering::Relaxed);
        if job.abandoned.load(Ordering::Acquire) {
            Counters::bump(&counters.skipped);
            continue;
        }
        let wait = clock.now().saturating_duration_since(job.enqueued);
        Counters::record(&counters.wait_total_ns, &counters.wait_max_ns, wait);
        if job.priority == Priority::High {
            Counters::bump(&counters.high_started);
            Counters::record(
                &counters.high_wait_total_ns,
                &counters.high_wait_max_ns,
                wait,
            );
        }
        (job.work)(clock, counters, wait);
    }
}

impl InferencePool {
    /// Starts `config.threads` worker threads.
    ///
    /// # Errors
    /// [`Error::Spawn`] if a thread cannot be created (threads already started are stopped).
    pub fn new(config: &PoolConfig, clock: SharedClock) -> Result<Self> {
        let capacity = config.queue_capacity.max(1);
        let queue = Arc::new(Queue::new(capacity));
        let counters = Arc::new(Counters::default());
        let mut threads = Vec::with_capacity(config.threads.max(1));
        for i in 0..config.threads.max(1) {
            let (queue_ref, counters, clock) =
                (Arc::clone(&queue), Arc::clone(&counters), clock.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("{}-{i}", config.name))
                .spawn(move || worker(&queue_ref, &counters, &*clock));
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(error) => {
                    // Closing the queue stops the threads already started.
                    queue.close();
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(Error::Spawn(error));
                }
            }
        }
        Ok(Self {
            shared: Arc::new(Shared {
                queue,
                counters: Arc::clone(&counters),
                clock,
                capacity,
                threads: threads.len(),
                warmed: Mutex::new(HashMap::new()),
            }),
            counters,
            threads,
        })
    }

    /// A handle for submitting jobs.
    pub fn handle(&self) -> PoolHandle {
        PoolHandle {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// Current counters.
    pub fn metrics(&self) -> PoolMetrics {
        self.counters.snapshot()
    }

    /// Stops accepting jobs, lets queued and running jobs finish, and joins the threads.
    ///
    /// Blocks the calling thread until running jobs finish: call it from a non-async context
    /// (e.g. after the runtime has shut down) or via `spawn_blocking`.
    pub fn shutdown(self) {
        let Self {
            shared, threads, ..
        } = self;
        // Handles hold only weak references, so this drops the last owner (which closes the
        // queue): workers drain the queue and exit.
        drop(shared);
        for thread in threads {
            let _ = thread.join();
        }
    }
}

/// Flags a request's job as abandoned when the request future goes away without an answer.
struct AbandonOnDrop(Option<Arc<AtomicBool>>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::Release);
        }
    }
}

impl PoolHandle {
    /// The pool's counters (all zero once it has shut down). Reads atomics only: cheap enough
    /// for an admission decision.
    pub fn metrics(&self) -> PoolMetrics {
        self.shared
            .upgrade()
            .map_or_else(PoolMetrics::default, |shared| shared.counters.snapshot())
    }

    /// Worker threads of the pool; `0` once it has shut down.
    pub fn threads(&self) -> usize {
        self.shared.upgrade().map_or(0, |shared| shared.threads)
    }

    /// Claims the warm-up named `key` (a model, say) on this pool: `true` for the first caller,
    /// who should warm and call [`Self::release_warm`] if that failed; `false` while it is
    /// claimed (warmed, or being warmed by another caller), while a failed warm-up of `key`
    /// cools down ([`Self::release_warm_after`]), and once the pool has shut down.
    /// Models whose sessions live per pool thread need warming once per pool, not once per
    /// call: a burst of calls each submitting a warm job per thread would fill the queue and
    /// make the running calls' real jobs fail with [`Error::PoolSaturated`].
    pub fn claim_warm(&self, key: &str) -> bool {
        self.shared.upgrade().is_some_and(|shared| {
            let now = shared.clock.now();
            let mut warmed = shared.warmed.lock().unwrap_or_else(PoisonError::into_inner);
            match warmed.get(key).copied() {
                None => {
                    warmed.insert(key.to_owned(), None);
                    true
                }
                Some(Some(until)) if now >= until => {
                    warmed.insert(key.to_owned(), None);
                    true
                }
                Some(_) => false,
            }
        })
    }

    /// Gives back a claim of [`Self::claim_warm`] (the warm-up failed): the next claim of `key`
    /// succeeds again.
    pub fn release_warm(&self, key: &str) {
        if let Some(shared) = self.shared.upgrade() {
            shared
                .warmed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(key);
        }
    }

    /// Gives back a claim of [`Self::claim_warm`] after `cooldown` on the pool's clock: until
    /// then every claim of `key` fails. For a warm-up that failed because the pool was busy
    /// (saturated, timed out): the calls that start meanwhile must not each push another set of
    /// warm jobs into the busy queue.
    pub fn release_warm_after(&self, key: &str, cooldown: Duration) {
        if let Some(shared) = self.shared.upgrade() {
            // A cooldown too long to represent never ends: the claim stays taken.
            let until = shared.clock.now().checked_add(cooldown);
            shared
                .warmed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.to_owned(), until);
        }
    }

    /// When the cool-down of a failed warm-up of `key` ends ([`Self::release_warm_after`]);
    /// `None` when `key` is free, claimed or kept, and once the pool has shut down.
    pub fn warm_cooling_until(&self, key: &str) -> Option<Instant> {
        let shared = self.shared.upgrade()?;
        let warmed = shared.warmed.lock().unwrap_or_else(PoisonError::into_inner);
        warmed.get(key).copied().flatten()
    }

    /// Jobs waiting in the queue now (not started); `0` once the pool has shut down.
    pub fn queue_depth(&self) -> usize {
        self.shared.upgrade().map_or(0, |shared| {
            shared.counters.queue_depth.load(Ordering::Relaxed)
        })
    }

    /// Sleeps for `duration` on the pool's injected clock; returns at once when the pool has
    /// shut down. For callers that pace a retry against the pool (the VAD worker after a
    /// rejected batch) without a clock of their own.
    pub async fn sleep(&self, duration: Duration) {
        let Some(clock) = self.shared.upgrade().map(|shared| shared.clock.clone()) else {
            return;
        };
        clock.sleep(duration).await;
    }

    /// Runs `job` on the pool and waits for it at most `timeout`.
    ///
    /// # Errors
    /// - [`Error::PoolSaturated`]: the queue is full (the job was not queued).
    /// - [`Error::JobPanicked`]: the job panicked.
    /// - [`brisvox_core::Error::Timeout`]: no result within `timeout` (the job is skipped if
    ///   it has not started, otherwise it finishes and its result is dropped). Dropping the
    ///   returned future has the same effect on the job.
    /// - [`brisvox_core::Error::ChannelClosed`]: the pool has shut down.
    pub async fn run<T, F>(&self, timeout: Duration, job: F) -> Result<Done<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.run_with(Priority::Normal, timeout, job).await
    }

    /// [`Self::run`] with a [`Priority`]: a [`Priority::High`] job waits in a line of its own
    /// (up to the queue capacity, separate from the normal line) and a worker takes it before
    /// the normal jobs queued ahead of it. Use it for short jobs a call needs every 20-32 ms
    /// (VAD windows): behind a burst of multi-millisecond jobs such a window would otherwise
    /// wait for all of them, and the queue holding them would reject it.
    ///
    /// # Errors
    /// As [`Self::run`]; [`Error::PoolSaturated`] when the line of `priority` is full.
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
        let closed = || Error::Core(brisvox_core::Error::ChannelClosed("inference pool"));
        let Some(shared) = self.shared.upgrade() else {
            return Err(closed());
        };
        let clock = shared.clock.clone();
        let (reply, result) = oneshot::channel::<Result<Done<T>>>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let work = move |clock: &dyn Clock, counters: &Counters, wait: Duration| {
            let started = clock.now();
            let outcome = catch_unwind(AssertUnwindSafe(job));
            let run = clock.now().saturating_duration_since(started);
            Counters::record(&counters.run_total_ns, &counters.run_max_ns, run);
            let timing = JobTiming { wait, run };
            let answer = match outcome {
                Ok(value) => {
                    Counters::bump(&counters.completed);
                    Ok(Done { value, timing })
                }
                Err(payload) => {
                    Counters::bump(&counters.panicked);
                    // A payload whose own drop panics must not take the worker down.
                    let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
                    Err(Error::JobPanicked)
                }
            };
            let _ = reply.send(answer);
        };
        let queued = Job {
            work: Box::new(work),
            priority,
            enqueued: clock.now(),
            abandoned: Arc::clone(&abandoned),
        };
        let counters = &shared.counters;
        // Count before sending so a fast worker never decrements (or completes) ahead of us.
        let depth = counters.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        Counters::bump(&counters.submitted);
        match shared.queue.push(priority, queued) {
            Ok(()) => {
                // Concurrent rejected submitters may have inflated `depth` transiently.
                counters.max_queue_depth.fetch_max(
                    depth.min(shared.capacity.saturating_mul(2)),
                    Ordering::Relaxed,
                );
            }
            Err(error) => {
                counters.queue_depth.fetch_sub(1, Ordering::Relaxed);
                counters.submitted.fetch_sub(1, Ordering::Relaxed);
                return Err(match error {
                    Refused::Full => {
                        Counters::bump(&counters.rejected);
                        if priority == Priority::High {
                            Counters::bump(&counters.rejected_high);
                        }
                        Error::PoolSaturated {
                            capacity: shared.capacity,
                        }
                    }
                    Refused::Closed => closed(),
                });
            }
        }
        let counters = Arc::clone(&shared.counters);
        // Do not keep the pool alive while waiting: shutdown must be able to proceed.
        drop(shared);
        // If this future is dropped before the answer (timeout or cancellation), an unstarted
        // job is skipped.
        let mut guard = AbandonOnDrop(Some(abandoned));
        tokio::select! {
            biased;
            outcome = result => {
                guard.0 = None;
                outcome.unwrap_or_else(|_| Err(closed()))
            }
            () = clock.sleep(timeout) => {
                Counters::bump(&counters.timed_out);
                Err(Error::Core(brisvox_core::Error::Timeout {
                    what: "inference job",
                    after: timeout,
                }))
            }
        }
    }
}
