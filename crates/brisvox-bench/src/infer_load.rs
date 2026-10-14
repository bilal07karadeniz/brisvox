//! Inference pool load test.
//!
//! N sessions each submit one job per period (32 ms = one Silero VAD window of 512 samples at
//! 16 kHz) to a shared [`InferencePool`], the way a model VAD would. A job burns `job_cost` of
//! real CPU (busy loop, not sleep). Two arrival modes ([`Arrivals`]): session tasks on timers
//! (closed loop: a slow pool shows up as missed windows, not unbounded backlog; bursty on
//! Windows timers) and evenly spin-paced arrivals (open loop: measures the pool itself).

use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_until};
use brisvox_core::{Clock, SharedClock, SystemClock};
use brisvox_inference::{InferencePool, PoolConfig, PoolMetrics};

use crate::BenchError;
use crate::stats::{Summary, ms};

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferLoadConfig {
    /// Concurrent sessions.
    pub sessions: usize,
    /// Time between a session's jobs.
    pub period: Duration,
    /// CPU time each job burns.
    pub job_cost: Duration,
    /// How long sessions keep submitting.
    pub duration: Duration,
    /// Pool threads.
    pub pool_threads: usize,
    /// Pool queue capacity.
    pub queue_capacity: usize,
    /// Per-job timeout.
    pub timeout: Duration,
    /// tokio worker threads driving the sessions.
    pub worker_threads: usize,
    /// How jobs arrive.
    pub arrivals: Arrivals,
}

/// How the load generator issues jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrivals {
    /// Each session is a tokio task that waits for its window on a timer and for its job
    /// (closed loop, like a real VAD stage). Timer granularity makes arrivals bursty on
    /// Windows (~15.6 ms batches).
    SessionTimers,
    /// One OS thread spin-waits for each job's exact arrival time (`period / sessions` apart)
    /// and submits it (open loop): evenly spaced arrivals, so the wait measures the pool.
    Paced,
}

impl InferLoadConfig {
    /// 500 sessions x one job per 32 ms for 10 s on a 4-thread pool.
    pub fn new(job_cost: Duration) -> Self {
        Self {
            sessions: 500,
            period: Duration::from_millis(32),
            job_cost,
            duration: Duration::from_secs(10),
            pool_threads: 4,
            queue_capacity: 1_024,
            timeout: Duration::from_millis(100),
            worker_threads: 2,
            arrivals: Arrivals::SessionTimers,
        }
    }

    /// Offered load in pool threads' worth of CPU (1.0 = every pool thread always busy).
    pub fn offered_load(&self) -> f64 {
        let jobs_per_sec = self.sessions as f64 / self.period.as_secs_f64();
        jobs_per_sec * self.job_cost.as_secs_f64() / self.pool_threads.max(1) as f64
    }
}

/// Results.
#[derive(Debug, Clone, PartialEq)]
pub struct InferLoadReport {
    /// What was run.
    pub config: InferLoadConfig,
    /// Windows the sessions were scheduled to process.
    pub windows: u64,
    /// Windows skipped because the previous job was still pending past the next window.
    pub missed_windows: u64,
    /// Queue wait (submitted to started) of completed jobs.
    pub wait: Summary,
    /// Run time of completed jobs.
    pub run: Summary,
    /// Submit-to-result latency seen by sessions.
    pub latency: Summary,
    /// How late sessions woke for their window: load generator timer granularity (bursty
    /// arrivals when timers fire in batches).
    pub tick_late: Summary,
    /// Pool counters at the end.
    pub pool: PoolMetrics,
    /// Measured pool utilisation (run time / (threads x wall)).
    pub utilisation: f64,
    /// Wall time.
    pub wall: Duration,
}

/// Burns `cost` of CPU on the calling (pool) thread.
fn spin(clock: &dyn Clock, cost: Duration) {
    let until = deadline_after(clock.now(), cost);
    while clock.now() < until {
        std::hint::spin_loop();
    }
}

#[derive(Default)]
struct SessionStats {
    windows: u64,
    missed: u64,
    wait: Vec<Duration>,
    run: Vec<Duration>,
    latency: Vec<Duration>,
    tick_late: Vec<Duration>,
}

async fn session(
    index: usize,
    config: InferLoadConfig,
    clock: SharedClock,
    pool: brisvox_inference::PoolHandle,
    start: tokio::time::Instant,
) -> SessionStats {
    let mut stats = SessionStats::default();
    // Spread the sessions' windows evenly over one period.
    let sessions = u32::try_from(config.sessions.max(1)).unwrap_or(u32::MAX);
    let phase = config
        .period
        .checked_div(sessions)
        .unwrap_or_default()
        .saturating_mul(u32::try_from(index).unwrap_or(u32::MAX));
    let end = deadline_after(start, config.duration);
    let mut next = deadline_after(start, phase);
    while next < end {
        wait_until(&*clock, next).await;
        stats
            .tick_late
            .push(clock.now().saturating_duration_since(next));
        stats.windows += 1;
        let submitted = clock.now();
        let job_clock = clock.clone();
        let cost = config.job_cost;
        if let Ok(done) = pool
            .run(config.timeout, move || spin(&*job_clock, cost))
            .await
        {
            stats.wait.push(done.timing.wait);
            stats.run.push(done.timing.run);
            stats
                .latency
                .push(clock.now().saturating_duration_since(submitted));
        }
        next = deadline_after(next, config.period);
        // Windows that passed while the job was pending are missed (the VAD would batch or
        // drop them).
        let now = clock.now();
        while next < now && next < end {
            stats.missed += 1;
            stats.windows += 1;
            next = deadline_after(next, config.period);
        }
    }
    stats
}

/// Open-loop, evenly spaced arrivals: this (OS) thread spin-waits for each job's slot and
/// spawns its request on the runtime. Burns one core while it runs.
fn paced(
    config: &InferLoadConfig,
    runtime: &tokio::runtime::Runtime,
    clock: &SharedClock,
    handle: &brisvox_inference::PoolHandle,
    start: tokio::time::Instant,
) -> SessionStats {
    #[derive(Default)]
    struct Sample {
        wait: Option<Duration>,
        run: Option<Duration>,
        latency: Option<Duration>,
    }
    let sessions = u32::try_from(config.sessions.max(1)).unwrap_or(u32::MAX);
    let gap = config.period.checked_div(sessions).unwrap_or_default();
    let end = deadline_after(start, config.duration);
    let mut stats = SessionStats::default();
    let mut requests = Vec::new();
    let mut k: u32 = 0;
    loop {
        let at = deadline_after(start, gap.saturating_mul(k));
        if at >= end || gap.is_zero() {
            break;
        }
        while clock.now() < at {
            std::hint::spin_loop();
        }
        stats
            .tick_late
            .push(clock.now().saturating_duration_since(at));
        stats.windows += 1;
        let (handle, clock, cost, timeout) = (
            handle.clone(),
            clock.clone(),
            config.job_cost,
            config.timeout,
        );
        requests.push(runtime.spawn(async move {
            let submitted = clock.now();
            let job_clock = clock.clone();
            match handle.run(timeout, move || spin(&*job_clock, cost)).await {
                Ok(done) => Sample {
                    wait: Some(done.timing.wait),
                    run: Some(done.timing.run),
                    latency: Some(clock.now().saturating_duration_since(submitted)),
                },
                Err(_) => Sample::default(),
            }
        }));
        k = k.saturating_add(1);
    }
    runtime.block_on(async {
        for request in requests {
            if let Ok(sample) = request.await {
                stats.wait.extend(sample.wait);
                stats.run.extend(sample.run);
                stats.latency.extend(sample.latency);
            }
        }
    });
    stats
}

/// Runs the load test.
///
/// # Errors
/// [`BenchError::Io`] if the runtime cannot be built, [`BenchError::Config`] for an invalid
/// configuration or if the pool cannot start.
pub fn run(config: &InferLoadConfig) -> Result<InferLoadReport, BenchError> {
    if config.sessions == 0 || config.period.is_zero() {
        return Err(BenchError::Config(
            "need sessions and a non-zero period".into(),
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads.max(1))
        .enable_time()
        .thread_name("bench-worker")
        .build()?;
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: config.pool_threads,
            queue_capacity: config.queue_capacity,
            name: "bench-infer".into(),
        },
        clock.clone(),
    )
    .map_err(|e| BenchError::Config(format!("inference pool: {e}")))?;
    let handle = pool.handle();
    let start = deadline_after(clock.now(), Duration::from_millis(50));
    let stats: Vec<SessionStats> = if config.arrivals == Arrivals::Paced {
        vec![paced(config, &runtime, &clock, &handle, start)]
    } else {
        runtime.block_on(async {
            let tasks: Vec<_> = (0..config.sessions)
                .map(|i| {
                    tokio::spawn(session(
                        i,
                        config.clone(),
                        clock.clone(),
                        handle.clone(),
                        start,
                    ))
                })
                .collect();
            let mut stats = Vec::with_capacity(tasks.len());
            for task in tasks {
                // A session task cannot hang: every pool call has a timeout and the loop ends at
                // `duration`.
                if let Ok(s) = task.await {
                    stats.push(s);
                }
            }
            stats
        })
    };
    let wall = clock.now().saturating_duration_since(start);
    let metrics = pool.metrics();
    drop(runtime);
    pool.shutdown();
    let mut windows = 0;
    let mut missed = 0;
    let (mut wait, mut run, mut latency) = (Vec::new(), Vec::new(), Vec::new());
    let mut tick_late = Vec::new();
    for s in stats {
        windows += s.windows;
        missed += s.missed;
        wait.extend(s.wait);
        run.extend(s.run);
        latency.extend(s.latency);
        tick_late.extend(s.tick_late);
    }
    let busy = metrics.run_total.as_secs_f64();
    let capacity = wall.as_secs_f64() * config.pool_threads.max(1) as f64;
    Ok(InferLoadReport {
        config: config.clone(),
        windows,
        missed_windows: missed,
        wait: Summary::of(wait),
        run: Summary::of(run),
        latency: Summary::of(latency),
        tick_late: Summary::of(tick_late),
        pool: metrics,
        utilisation: if capacity > 0.0 { busy / capacity } else { 0.0 },
        wall,
    })
}

/// A markdown table of several runs.
pub fn markdown(reports: &[InferLoadReport], environment: &[String]) -> String {
    let mut out = String::from("# Inference pool load test\n\n");
    for line in environment {
        out.push_str(&format!("- {line}\n"));
    }
    out.push_str(
        "\nEach session submits one job per period (32 ms = one Silero VAD window); a job \
         busy-spins `job cost` of CPU. Arrivals: `timers` = each session a tokio task waiting on \
         a timer and on its job (closed loop; bursty on Windows, see tick late), `paced` = one \
         thread spin-paces jobs evenly (open loop; measures the pool itself). Offered load = job \
         CPU per second / pool threads. Wait = queued to started; latency = submit to result. \
         Times in ms.\n\n\
         | arrivals | sessions | pool threads | job cost | offered load | utilisation | jobs done | rejected \
         | timed out | missed windows | wait p50 | wait p99 | wait max | latency p99 | max queue \
         | tick late p50 | tick late p99 |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in reports {
        let c = &r.config;
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.2} | {:.2} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} \
             | {} | {} |\n",
            match c.arrivals {
                Arrivals::SessionTimers => "timers",
                Arrivals::Paced => "paced",
            },
            c.sessions,
            c.pool_threads,
            ms(c.job_cost),
            c.offered_load(),
            r.utilisation,
            r.pool.completed,
            r.pool.rejected,
            r.pool.timed_out,
            r.missed_windows,
            r.windows,
            ms(r.wait.p50),
            ms(r.wait.p99),
            ms(r.wait.max),
            ms(r.latency.p99),
            r.pool.max_queue_depth,
            ms(r.tick_late.p50),
            ms(r.tick_late.p99),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_light_load_completes_every_window_quickly() {
        let mut config = InferLoadConfig::new(Duration::from_micros(100));
        config.sessions = 20;
        config.duration = Duration::from_millis(500);
        config.pool_threads = 2;
        let report = run(&config).unwrap();
        assert!(config.offered_load() < 0.1);
        assert_eq!(report.pool.rejected, 0);
        assert_eq!(report.pool.timed_out, 0);
        assert_eq!(report.pool.panicked, 0);
        assert!(report.windows >= 20 * 15, "{report:?}");
        assert_eq!(
            u64::try_from(report.run.count).unwrap(),
            report.pool.completed
        );
        assert!(report.run.p50 >= Duration::from_micros(100));
        assert!(report.utilisation > 0.0 && report.utilisation < 0.5);
    }

    #[test]
    fn offered_load_formula() {
        let config = InferLoadConfig::new(Duration::from_micros(256));
        // 500 / 0.032 s = 15 625 jobs/s x 0.256 ms = 4 cores / 4 threads.
        assert!((config.offered_load() - 1.0).abs() < 1e-9);
    }
}
