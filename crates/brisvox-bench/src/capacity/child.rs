//! The engine as a server process of its own, and the parent side that starts one.
//!
//! A capacity run needs the system under test in a process separate from the load generator, or
//! the two share cores and the server's CPU and memory cannot be read: Pipecat is such a process
//! (`bench/pipecat_baseline/bot.py`), so brisvox is one too. [`serve`] runs the engine over the
//! `brisvox_sim` mock providers behind the real websocket transport
//! ([`EngineServer`]) until the process is stopped; every call plays the same fixture
//! ([`EngineServer::set_default_script`]), which is what the capacity ramp replays.
//! `brisvox-bench engine-server` is that process, `brisvox-bench capacity --sim` starts one as a
//! child ([`EngineChild`]).
//!
//! The child prints one line, `listening ws://HOST:PORT/call`, on stdout once it accepts calls.
//! With `--exit-on-stdin-eof` it also stops when its stdin closes, so a parent that dies cannot
//! leave a server behind.

// `tokio_unstable` is set by `RUSTFLAGS="--cfg tokio_unstable"` for a profiling build only.
#![allow(unexpected_cfgs)]

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_sim::fixture::Fixture;
use tokio_util::sync::CancellationToken;

use crate::BenchError;
use crate::fixture_load::pad_gaps;
use crate::v2v::engine_target::{EngineServer, EngineTargetConfig};
use crate::v2v::real_models::RealModels;

/// The prefix of the line the server prints when it accepts calls.
pub const LISTENING: &str = "listening ";

/// How long the parent waits for the child to print [`LISTENING`].
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stopping server waits for its calls to end.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What the standalone engine server runs.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The engine target: mock providers, VAD, end of turn, pool threads and call limit.
    pub engine: EngineTargetConfig,
    /// The fixture every call plays.
    pub fixture: String,
    /// Where the fixtures are.
    pub fixtures_dir: PathBuf,
    /// Padding between utterances (must equal the load generator's, the mock STT only needs the
    /// transcripts but the report states one number).
    pub min_gap: Duration,
    /// Where to listen.
    pub bind: SocketAddr,
    /// Tokio worker threads; `None`: one per logical core (what the container's cgroup limit
    /// reports on Linux).
    pub workers: Option<usize>,
    /// Stop when stdin reaches end of file.
    pub exit_on_stdin_eof: bool,
    /// Print one `stats` line on stderr per period: calls, the inference pool's queue, wait and
    /// busy share, and the runtime's task counts (profiling a capacity run). `None`: off.
    pub stats_every: Option<Duration>,
    /// The directory of the Smart Turn model file: the calls then run the real Silero VAD and
    /// Smart Turn v3.2 instead of the energy VAD and the stub (`--models DIR`; needs the
    /// `models` feature). `None`: the stub.
    pub models: Option<PathBuf>,
}

/// Runs the engine server until stdin closes (with [`ServeOptions::exit_on_stdin_eof`]) or the
/// process is killed. Blocks; prints [`LISTENING`] and the URL once calls are accepted.
///
/// # Errors
/// [`BenchError::Config`] for a missing fixture or a server that cannot start; [`BenchError::Io`]
/// if the runtime cannot be created.
pub fn serve(options: &ServeOptions) -> Result<(), BenchError> {
    let fixture = Fixture::load(&options.fixtures_dir, &options.fixture)
        .map_err(|e| BenchError::Config(format!("fixture {}: {e}", options.fixture)))?;
    let fixture = pad_gaps(&fixture, options.min_gap);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(workers) = options.workers {
        builder.worker_threads(workers.max(1));
    }
    // The models load before the runtime exists (the file read and the first ONNX Runtime
    // session are blocking), like the server loads them.
    let models = options
        .models
        .as_deref()
        .map(|dir| RealModels::load(dir, SystemClock::shared()))
        .transpose()?;
    if models.is_some() {
        eprintln!("models: Silero VAD (tract) and Smart Turn v3.2 (ONNX Runtime) for every call");
    }
    let runtime = builder.enable_all().build()?;
    let cancel = CancellationToken::new();
    if options.exit_on_stdin_eof {
        watch_stdin(cancel.clone());
    }
    runtime.block_on(async {
        let clock = SystemClock::shared();
        let server =
            EngineServer::start_with(options.engine, options.bind, models, clock, &cancel).await?;
        server.set_default_script(&fixture);
        println!("{LISTENING}ws://{}/call", server.local_addr());
        flush_stdout();
        let server = std::sync::Arc::new(server);
        if let Some(period) = options.stats_every {
            tokio::spawn(stats_loop(Arc::clone(&server), period, cancel.clone()));
        }
        cancel.cancelled().await;
        print_model_totals(&server);
        server.shutdown(SHUTDOWN_TIMEOUT).await
    })
}

/// Prints on stderr what the pool and the Smart Turn model did over the whole run, so the CPU
/// split of a capacity run with the real models is on record: the pool's run time is the VAD
/// windows, the feature extraction and the Smart Turn analyses; the model's own time is the
/// analyses' ONNX Runtime run.
fn print_model_totals(server: &EngineServer) {
    let Some(models) = server.models() else {
        return;
    };
    let pool = server.pool_metrics();
    let turn = models.turn_stats();
    let per = |total: Duration, n: u64| {
        if n == 0 {
            0.0
        } else {
            total.as_secs_f64() * 1e3 / n as f64
        }
    };
    eprintln!(
        "totals pool: {} jobs, run {:.1} s ({:.3} ms/job), wait {:.1} s | smart turn: {} \
         analyses, model run {:.1} s ({:.2} ms/analysis, wall time on the pool thread) | the \
         rest of the pool's run time (VAD windows, features): {:.1} s",
        pool.completed,
        pool.run_total.as_secs_f64(),
        per(pool.run_total, pool.completed),
        pool.wait_total.as_secs_f64(),
        turn.analyses,
        turn.run_total.as_secs_f64(),
        per(turn.run_total, turn.analyses),
        // With a Smart Turn executor the analyses are not in the pool's run time.
        if server.turn_pool_metrics().is_some() {
            pool.run_total
        } else {
            pool.run_total.saturating_sub(turn.run_total)
        }
        .as_secs_f64(),
    );
    let high_mean_us = if pool.high_started == 0 {
        0.0
    } else {
        pool.high_wait_total.as_secs_f64() * 1e6 / pool.high_started as f64
    };
    eprintln!(
        "totals vad windows (priority line): {} started, wait {:.1} us mean, {:.2} ms max, rejected {}",
        pool.high_started,
        high_mean_us,
        pool.high_wait_max.as_secs_f64() * 1e3,
        pool.rejected_high,
    );
    if let Some(turn_pool) = server.turn_pool_metrics() {
        let jobs = turn_pool.completed.max(1) as f64;
        eprintln!(
            "totals smart turn executor: {} jobs, run {:.1} s ({:.2} ms/job), wait {:.2} ms mean, {:.1} ms max, rejected {}, timed_out {}, skipped {}, max queue {}",
            turn_pool.completed,
            turn_pool.run_total.as_secs_f64(),
            turn_pool.run_total.as_secs_f64() * 1e3 / jobs,
            turn_pool.wait_total.as_secs_f64() * 1e3 / jobs,
            turn_pool.wait_max.as_secs_f64() * 1e3,
            turn_pool.rejected,
            turn_pool.timed_out,
            turn_pool.skipped,
            turn_pool.max_queue_depth,
        );
    }
}

/// Prints a `stats` line on stderr every `period` until `cancel`: the pool's counters since the
/// previous line, so a saturated pool (busy share near 100 %, a queue that does not drain, waits
/// of milliseconds) shows next to the number of calls.
async fn stats_loop(server: Arc<EngineServer>, period: Duration, cancel: CancellationToken) {
    let clock = SystemClock::shared();
    let threads = server.config().pool_threads.max(1) as f64;
    let runtime = tokio::runtime::Handle::current();
    let mut last = server.pool_metrics();
    let mut last_turn = server.models().map(RealModels::turn_stats);
    let turn_threads = server
        .config()
        .resolved_turn_threads(server.models().is_some());
    let mut last_at = clock.now();
    #[cfg(tokio_unstable)]
    let mut last_unstable = Unstable::read(&runtime.metrics());
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = clock.sleep(period) => {}
        }
        let (now, now_at) = (server.pool_metrics(), clock.now());
        let secs = now_at.duration_since(last_at).as_secs_f64().max(1e-9);
        let jobs = now.completed.saturating_sub(last.completed);
        let wait = now.wait_total.saturating_sub(last.wait_total);
        let run = now.run_total.saturating_sub(last.run_total);
        let per_job = |d: Duration| {
            if jobs == 0 {
                0.0
            } else {
                d.as_secs_f64() * 1e6 / jobs as f64
            }
        };
        let metrics = runtime.metrics();
        let high = now.high_started.saturating_sub(last.high_started);
        let high_wait = now.high_wait_total.saturating_sub(last.high_wait_total);
        eprintln!(
            "stats calls {} pool: {:.0} jobs/s busy {:.0}% run {:.1} us/job wait {:.0} us/job \
             queue {} (max {}) rejected {} (high {}) timed_out {} skipped {} | vad wait {:.0} \
             us/window (max {:.1} ms) | tokio: {} tasks, global queue {}",
            server.active_calls(),
            jobs as f64 / secs,
            run.as_secs_f64() / secs / threads * 100.0,
            per_job(run),
            per_job(wait),
            now.queue_depth,
            now.max_queue_depth,
            now.rejected.saturating_sub(last.rejected),
            now.rejected_high.saturating_sub(last.rejected_high),
            now.timed_out.saturating_sub(last.timed_out),
            now.skipped.saturating_sub(last.skipped),
            if high == 0 {
                0.0
            } else {
                high_wait.as_secs_f64() * 1e6 / high as f64
            },
            now.high_wait_max.as_secs_f64() * 1e3,
            metrics.num_alive_tasks(),
            metrics.global_queue_depth(),
        );
        if let (Some(models), Some(before)) = (server.models(), last_turn) {
            let turn = models.turn_stats();
            let turn_capacity = if turn_threads > 0 {
                turn_threads as f64
            } else {
                threads
            };
            let analyses = turn.analyses.saturating_sub(before.analyses);
            let model = turn.run_total.saturating_sub(before.run_total);
            eprintln!(
                "   smart turn: {:.1} analyses/s, {:.2} ms/analysis, {:.0}% of the {} threads it runs \
                 on; VAD windows and features: {:.0}% of the pool's {}",
                analyses as f64 / secs,
                if analyses == 0 {
                    0.0
                } else {
                    model.as_secs_f64() * 1e3 / analyses as f64
                },
                model.as_secs_f64() / secs / turn_capacity * 100.0,
                if turn_threads > 0 {
                    turn_threads
                } else {
                    threads as usize
                },
                run.saturating_sub(if turn_threads > 0 {
                    Duration::ZERO
                } else {
                    model
                })
                .as_secs_f64()
                    / secs
                    / threads
                    * 100.0,
                threads as usize,
            );
            last_turn = Some(turn);
            // The container is stopped, not cancelled: the totals so far are the record.
            print_model_totals(&server);
        }
        #[cfg(tokio_unstable)]
        {
            let u = Unstable::read(&metrics);
            let polls = u.polls.saturating_sub(last_unstable.polls);
            let busy = u.busy.saturating_sub(last_unstable.busy);
            eprintln!(
                "   tokio workers: {:.0} polls/s, mean poll {:.2} us, busy {:.0}% of {} workers, \
                 {:.0} parks/s, {:.0} steals/s, {:.0} spawns/s, {} forced yields, {:.0} io events/s",
                polls as f64 / secs,
                if polls == 0 {
                    0.0
                } else {
                    busy.as_secs_f64() * 1e6 / polls as f64
                },
                busy.as_secs_f64() / secs / metrics.num_workers() as f64 * 100.0,
                metrics.num_workers(),
                u.parks.saturating_sub(last_unstable.parks) as f64 / secs,
                u.steals.saturating_sub(last_unstable.steals) as f64 / secs,
                u.spawned.saturating_sub(last_unstable.spawned) as f64 / secs,
                u.forced_yields.saturating_sub(last_unstable.forced_yields),
                u.io_ready.saturating_sub(last_unstable.io_ready) as f64 / secs,
            );
            last_unstable = u;
        }
        last = now;
        last_at = now_at;
    }
}

/// The worker counters tokio keeps with `--cfg tokio_unstable` (a profiling build only), as the
/// difference between two samples.
#[cfg(tokio_unstable)]
#[derive(Default)]
struct Unstable {
    polls: u64,
    busy: Duration,
    parks: u64,
    steals: u64,
    spawned: u64,
    forced_yields: u64,
    io_ready: u64,
}

#[cfg(tokio_unstable)]
impl Unstable {
    fn read(metrics: &tokio::runtime::RuntimeMetrics) -> Self {
        let workers = metrics.num_workers();
        Self {
            polls: (0..workers).map(|w| metrics.worker_poll_count(w)).sum(),
            busy: (0..workers)
                .map(|w| metrics.worker_total_busy_duration(w))
                .sum(),
            parks: (0..workers).map(|w| metrics.worker_park_count(w)).sum(),
            steals: (0..workers).map(|w| metrics.worker_steal_count(w)).sum(),
            spawned: metrics.spawned_tasks_count(),
            forced_yields: metrics.budget_forced_yield_count(),
            io_ready: metrics.io_driver_ready_count(),
        }
    }
}

fn flush_stdout() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

/// Cancels `cancel` when stdin reaches end of file (read on a blocking thread).
fn watch_stdin(cancel: CancellationToken) {
    // The thread is detached: it ends with the process (a blocked stdin read cannot be
    // interrupted).
    let _ = std::thread::Builder::new()
        .name("bench-stdin-watch".into())
        .spawn(move || {
            let mut sink = [0u8; 256];
            let mut stdin = std::io::stdin().lock();
            // Any bytes are ignored; EOF or an error means the parent is gone.
            while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
            cancel.cancel();
        });
}

/// An engine server started as a child process.
pub struct EngineChild {
    child: Child,
    url: String,
}

impl EngineChild {
    /// Starts `exe engine-server <args> --exit-on-stdin-eof` and waits for it to listen.
    /// Blocking: call it off the async workers (`spawn_blocking`).
    ///
    /// # Errors
    /// [`BenchError::Call`] if the process cannot start, exits early, or does not listen within
    /// [`STARTUP_TIMEOUT`].
    pub fn spawn(exe: &std::path::Path, args: &[String]) -> Result<Self, BenchError> {
        let mut child = Command::new(exe)
            .arg("engine-server")
            .args(args)
            .arg("--exit-on-stdin-eof")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| BenchError::Call(format!("cannot start the engine server: {e}")))?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(BenchError::Call("engine server has no stdout".to_owned()));
        };
        // The first line on its own thread, so the wait can time out.
        let (tx, rx) = mpsc::sync_channel::<String>(1);
        let _ = std::thread::Builder::new()
            .name("bench-child-stdout".into())
            .spawn(move || {
                let mut lines = BufReader::new(stdout).lines();
                while let Some(Ok(line)) = lines.next() {
                    if line.starts_with(LISTENING) {
                        let _ = tx.send(line);
                        break;
                    }
                }
                // Keep draining so the child never blocks on a full pipe.
                for _ in lines {}
            });
        match rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(line) => {
                let url = line.trim_start_matches(LISTENING).trim().to_owned();
                Ok(Self { child, url })
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(BenchError::Call(
                    "the engine server did not start listening".to_owned(),
                ))
            }
        }
    }

    /// The websocket URL the child listens on.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stops the child (closing its stdin asks it to stop; it is killed if it does not) and
    /// waits for it. Blocking.
    pub fn stop(mut self) {
        drop(self.child.stdin.take());
        // The child shuts its calls down within `SHUTDOWN_TIMEOUT`; poll for the exit.
        for _ in 0..150 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            pause(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A pause on a plain thread (no clock here: this is process management, off the runtime).
fn pause(duration: Duration) {
    let (_alive, wake) = mpsc::channel::<()>();
    let _ = wake.recv_timeout(duration);
}

impl Drop for EngineChild {
    fn drop(&mut self) {
        // A panic or early return must not leave a server behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
