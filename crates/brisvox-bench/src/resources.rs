//! Process resource readings for load runs: resident memory, CPU time, tokio task count, and how
//! long a woken task waits for a worker.
//!
//! Readings are blocking system calls, so the [`Sampler`] runs on its own OS thread, never on a
//! tokio worker.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use brisvox_core::SharedClock;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::runtime::Handle;
use tokio::time::Instant;

/// Readings kept by a [`Sampler`] (minutes of sampling; later readings only update peaks).
const MAX_READINGS: usize = 20_000;
/// Wake samples kept by a [`WakeProbe`].
const MAX_WAKES: usize = 100_000;

/// One reading of this process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessReading {
    /// Resident set size (Windows: working set).
    pub rss_bytes: u64,
    /// CPU time used by all threads so far (user + system).
    pub cpu_time: Duration,
}

/// Reads this process's memory and CPU time.
pub struct ProcessProbe {
    system: System,
    pid: Option<Pid>,
}

impl Default for ProcessProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessProbe {
    /// A probe for the current process.
    pub fn new() -> Self {
        Self {
            system: System::new(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }

    /// A fresh reading, or `None` if the platform does not report one.
    pub fn read(&mut self) -> Option<ProcessReading> {
        let pid = self.pid?;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        let process = self.system.process(pid)?;
        Some(ProcessReading {
            rss_bytes: process.memory(),
            cpu_time: Duration::from_millis(process.accumulated_cpu_time()),
        })
    }
}

/// What a [`Sampler`] saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Samples {
    /// Peak resident set size.
    pub rss_peak: u64,
    /// Peak number of alive tokio tasks on the sampled runtime.
    pub alive_tasks_peak: usize,
    /// Readings taken.
    pub count: usize,
    /// (time, cumulative CPU time) of the first `MAX_READINGS` readings.
    pub cpu: Vec<(Instant, Duration)>,
}

impl Samples {
    /// Average CPU cores busy between `from` and `to` (from the readings closest inside the
    /// window), or `None` if fewer than two readings fall inside it.
    pub fn cpu_cores_between(&self, from: Instant, to: Instant) -> Option<f64> {
        let inside = || self.cpu.iter().filter(|(t, _)| *t >= from && *t <= to);
        let (t0, c0) = inside().next()?;
        let (t1, c1) = inside().next_back()?;
        let wall = t1.saturating_duration_since(*t0).as_secs_f64();
        (wall > 0.0).then(|| c1.saturating_sub(*c0).as_secs_f64() / wall)
    }
}

/// Samples resident memory, CPU time and a runtime's alive task count periodically on its own
/// thread.
pub struct Sampler {
    stop: mpsc::SyncSender<()>,
    thread: JoinHandle<Samples>,
}

impl Sampler {
    /// Starts sampling `runtime` every `every`; readings are stamped with `clock`.
    ///
    /// # Errors
    /// If the sampler thread cannot be spawned.
    pub fn start(runtime: Handle, clock: SharedClock, every: Duration) -> std::io::Result<Self> {
        // Capacity 1: the only message is the stop signal.
        let (stop, stopped) = mpsc::sync_channel::<()>(1);
        let thread = std::thread::Builder::new()
            .name("bench-sampler".into())
            .spawn(move || {
                let mut probe = ProcessProbe::new();
                let mut samples = Samples::default();
                loop {
                    if let Some(reading) = probe.read() {
                        samples.rss_peak = samples.rss_peak.max(reading.rss_bytes);
                        if samples.cpu.len() < MAX_READINGS {
                            samples.cpu.push((clock.now(), reading.cpu_time));
                        }
                    }
                    let tasks = runtime.metrics().num_alive_tasks();
                    samples.alive_tasks_peak = samples.alive_tasks_peak.max(tasks);
                    samples.count = samples.count.saturating_add(1);
                    match stopped.recv_timeout(every) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // Stopped: the last reading is at most `every` old.
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return samples,
                    }
                }
            })?;
        Ok(Self { stop, thread })
    }

    /// Stops sampling and returns what was seen, or `None` if the sampler thread panicked.
    pub fn finish(self) -> Option<Samples> {
        // A full channel means a stop is already pending.
        let _ = self.stop.try_send(());
        self.thread.join().ok()
    }
}

/// Measures scheduling delay: a plain OS thread stamps a message every period and sends it to a
/// task on the runtime; the task records how long it took to run after the send. Under load
/// this is how long any woken task (a provider event, an input frame) waits for a worker, which
/// the per-turn overhead cannot see when the wait happens before a timeline mark.
pub struct WakeProbe {
    stop: mpsc::SyncSender<()>,
    sender: JoinHandle<usize>,
    receiver: tokio::task::JoinHandle<Vec<Duration>>,
}

impl WakeProbe {
    /// Starts probing `runtime` every `every`.
    ///
    /// # Errors
    /// If the sender thread cannot be spawned.
    pub fn start(runtime: &Handle, clock: SharedClock, every: Duration) -> std::io::Result<Self> {
        // Bounded: a backlog means the receiver is starved; such stamps are dropped (counted).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Instant>(64);
        let receiver = {
            let clock = clock.clone();
            runtime.spawn(async move {
                let mut wakes = Vec::new();
                while let Some(sent) = rx.recv().await {
                    if wakes.len() < MAX_WAKES {
                        wakes.push(clock.now().saturating_duration_since(sent));
                    }
                }
                wakes
            })
        };
        let (stop, stopped) = mpsc::sync_channel::<()>(1);
        let sender = std::thread::Builder::new()
            .name("bench-wake-probe".into())
            .spawn(move || {
                let mut dropped = 0usize;
                loop {
                    if tx.try_send(clock.now()).is_err() {
                        dropped = dropped.saturating_add(1);
                    }
                    match stopped.recv_timeout(every) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return dropped,
                    }
                }
            })?;
        Ok(Self {
            stop,
            sender,
            receiver,
        })
    }

    /// Stops probing; returns the wake delays and the number of stamps dropped because the
    /// receiver fell behind (`usize::MAX` if the sender thread failed). Call from inside the
    /// runtime.
    pub async fn finish(self) -> (Vec<Duration>, usize) {
        let _ = self.stop.try_send(());
        // The stop signal wakes the sender thread at once; join it off the async workers.
        let sender = self.sender;
        let dropped = tokio::task::spawn_blocking(move || sender.join().unwrap_or(usize::MAX))
            .await
            .unwrap_or(usize::MAX);
        let wakes = self.receiver.await.unwrap_or_default();
        (wakes, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brisvox_core::SystemClock;

    #[test]
    fn reads_this_process() {
        let mut probe = ProcessProbe::new();
        let reading = probe.read().unwrap();
        assert!(reading.rss_bytes > 0);
    }

    #[test]
    fn sampler_sees_runtime_tasks_and_cpu() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let clock = SystemClock::shared();
        let start = clock.now();
        let sampler = Sampler::start(
            runtime.handle().clone(),
            clock.clone(),
            Duration::from_millis(1),
        )
        .unwrap();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let task = runtime.spawn(async move {
            let _ = wait.await;
        });
        // Give the sampler time to observe the parked task (a timed wait without sleep()).
        let (_keep_open, never) = mpsc::sync_channel::<()>(1);
        let _ = never.recv_timeout(Duration::from_millis(50));
        release.send(()).unwrap();
        runtime.block_on(task).unwrap();
        let samples = sampler.finish().unwrap();
        assert!(samples.count >= 2, "{samples:?}");
        assert!(samples.alive_tasks_peak >= 1, "{samples:?}");
        assert!(samples.rss_peak > 0);
        assert!(samples.cpu.len() >= 2);
        let cores = samples.cpu_cores_between(start, clock.now()).unwrap();
        assert!(cores >= 0.0);
        // No readings in an empty window.
        assert_eq!(samples.cpu_cores_between(clock.now(), clock.now()), None);
    }

    #[test]
    fn wake_probe_measures_idle_wakeups() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let probe = WakeProbe::start(
            runtime.handle(),
            SystemClock::shared(),
            Duration::from_millis(1),
        )
        .unwrap();
        let (_keep_open, never) = mpsc::sync_channel::<()>(1);
        let _ = never.recv_timeout(Duration::from_millis(50));
        let (wakes, dropped) = runtime.block_on(probe.finish());
        // Timing-free assertions (a starved CI box may delay or drop some stamps).
        assert!(!wakes.is_empty());
        assert_ne!(dropped, usize::MAX, "sender thread failed");
    }
}
