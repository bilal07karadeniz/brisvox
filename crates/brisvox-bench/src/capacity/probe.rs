//! CPU and memory of a process (tree) by pid, sampled on a thread of its own.
//!
//! The capacity benchmark compares servers that are separate processes: brisvox's engine server
//! (a child of the benchmark) and the Pipecat bot (started by the operator, possibly with several
//! worker processes behind one port, and on Windows behind a venv launcher that starts the real
//! interpreter as a child). Their cost is therefore read for the whole process tree below a pid,
//! with the same call on every platform `sysinfo` supports.
//!
//! Readings are blocking system calls, so [`TreeSampler`] runs on an OS thread, never on a tokio
//! worker, and stamps readings with the injected clock.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use brisvox_core::SharedClock;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::time::Instant;

/// Readings kept per sampler (hours at the default period; later readings only update the peak).
const MAX_READINGS: usize = 20_000;

/// Which processes a probe adds up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The process itself.
    Process,
    /// The process and all its descendants.
    Tree,
}

/// One reading: totals over the probed processes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reading {
    /// Resident set size (Windows: working set), summed. Pages shared between processes (a
    /// library or a model loaded before a fork) are counted once per process.
    pub rss_bytes: u64,
    /// Proportional set size (`Pss` of `/proc/<pid>/smaps_rollup`), summed: a page shared by N
    /// processes counts 1/N in each, so the sum is the memory the tree really holds. `None`
    /// where the platform does not report it (everything but Linux) or the root could not be read.
    pub pss_bytes: Option<u64>,
    /// CPU time used by all threads of the processes alive now (user + system), summed.
    pub cpu_time: Duration,
    /// Processes counted.
    pub processes: usize,
}

/// The `Pss:` figure of a `/proc/<pid>/smaps_rollup` text, in bytes (the file reports kB). The
/// similarly named `Pss_Anon:`, `Pss_File:` and `Pss_Shmem:` lines are not it.
pub fn parse_smaps_rollup_pss(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let kb: u64 = line
            .strip_prefix("Pss:")?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        kb.checked_mul(1024)
    })
}

/// PSS of one process in bytes (Linux; `None` elsewhere, or if the process is gone or not
/// readable). A blocking file read: only call it off the async workers.
#[cfg(target_os = "linux")]
fn read_pss(pid: Pid) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", pid.as_u32())).ok()?;
    parse_smaps_rollup_pss(&text)
}

#[cfg(not(target_os = "linux"))]
fn read_pss(_pid: Pid) -> Option<u64> {
    None
}

/// Reads the totals of a process (tree).
pub struct TreeProbe {
    system: System,
    root: Pid,
    scope: Scope,
}

impl TreeProbe {
    /// A probe for `pid`.
    pub fn new(pid: u32, scope: Scope) -> Self {
        Self {
            system: System::new(),
            root: Pid::from_u32(pid),
            scope,
        }
    }

    /// A probe for this process.
    pub fn current() -> Option<Self> {
        sysinfo::get_current_pid().ok().map(|pid| Self {
            system: System::new(),
            root: pid,
            scope: Scope::Process,
        })
    }

    /// A fresh reading, or `None` if the root process does not exist (any more).
    pub fn read(&mut self) -> Option<Reading> {
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        match self.scope {
            Scope::Process => {
                self.system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[self.root]),
                    true,
                    kind,
                );
            }
            Scope::Tree => {
                self.system
                    .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            }
        }
        let processes = self.system.processes();
        let root = processes.get(&self.root)?;
        // On Linux sysinfo lists every thread (task) as a process of its own whose CPU time and
        // resident set are those of the whole process, so summing them counted CPU about twice
        // and RSS once per thread (a 4 GiB container "used" 5.6 GiB, 3 cores "became" 6): only
        // real processes (`thread_kind() == None`) are members.
        let members: Vec<Pid> = match self.scope {
            Scope::Process => vec![self.root],
            Scope::Tree => descendants(
                self.root,
                processes
                    .iter()
                    .filter(|(_, process)| process.thread_kind().is_none())
                    .filter_map(|(pid, process)| process.parent().map(|parent| (*pid, parent))),
            ),
        };
        let mut reading = Reading {
            rss_bytes: root.memory(),
            cpu_time: Duration::from_millis(root.accumulated_cpu_time()),
            pss_bytes: read_pss(self.root),
            processes: 1,
        };
        for pid in members.iter().filter(|pid| **pid != self.root) {
            if let Some(process) = processes.get(pid) {
                reading.rss_bytes = reading.rss_bytes.saturating_add(process.memory());
                reading.cpu_time = reading
                    .cpu_time
                    .saturating_add(Duration::from_millis(process.accumulated_cpu_time()));
                // A member that exited since the listing adds nothing.
                if let (Some(sum), Some(own)) = (reading.pss_bytes, read_pss(*pid)) {
                    reading.pss_bytes = Some(sum.saturating_add(own));
                }
                reading.processes += 1;
            }
        }
        Some(reading)
    }
}

/// `root` and every process below it, given `(pid, parent)` pairs. A parent loop (a reused pid)
/// cannot make this run forever.
pub fn descendants(root: Pid, pairs: impl Iterator<Item = (Pid, Pid)>) -> Vec<Pid> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, parent) in pairs {
        children.entry(parent).or_default().push(pid);
    }
    let mut seen: HashSet<Pid> = HashSet::from([root]);
    let mut order = vec![root];
    let mut next = 0;
    while let Some(pid) = order.get(next).copied() {
        next += 1;
        for child in children.get(&pid).into_iter().flatten() {
            if seen.insert(*child) {
                order.push(*child);
            }
        }
    }
    order
}

/// Readings taken while a [`TreeSampler`] ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Samples {
    /// `(time, reading)` of the first `MAX_READINGS` readings.
    pub readings: Vec<(Instant, Reading)>,
    /// Peak resident set over all readings.
    pub rss_peak: u64,
    /// Peak proportional set over all readings, if the platform reports it.
    pub pss_peak: Option<u64>,
    /// Readings taken.
    pub count: usize,
}

impl Samples {
    /// Average CPU cores busy between `from` and `to`, from the first and last readings inside
    /// the window; `None` with fewer than two readings or no elapsed time.
    ///
    /// A process that exits inside the window takes its CPU time with it, so a tree whose
    /// children come and go can read low; the servers measured here keep their workers.
    pub fn cpu_cores_between(&self, from: Instant, to: Instant) -> Option<f64> {
        let inside = || {
            self.readings
                .iter()
                .filter(move |(t, _)| *t >= from && *t <= to)
        };
        let (t0, r0) = inside().next()?;
        let (t1, r1) = inside().next_back()?;
        let wall = t1.saturating_duration_since(*t0).as_secs_f64();
        (wall > 0.0).then(|| r1.cpu_time.saturating_sub(r0.cpu_time).as_secs_f64() / wall)
    }

    /// Peak resident set of the readings inside the window.
    pub fn rss_peak_between(&self, from: Instant, to: Instant) -> Option<u64> {
        self.readings
            .iter()
            .filter(|(t, _)| *t >= from && *t <= to)
            .map(|(_, r)| r.rss_bytes)
            .max()
    }

    /// Peak proportional set of the readings inside the window, if any carries one.
    pub fn pss_peak_between(&self, from: Instant, to: Instant) -> Option<u64> {
        self.readings
            .iter()
            .filter(|(t, _)| *t >= from && *t <= to)
            .filter_map(|(_, r)| r.pss_bytes)
            .max()
    }

    /// The last reading at or before `at`, if any.
    pub fn reading_at(&self, at: Instant) -> Option<Reading> {
        self.readings
            .iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .map(|(_, r)| *r)
    }
}

/// Samples a [`TreeProbe`] every period on a thread of its own.
pub struct TreeSampler {
    stop: mpsc::SyncSender<()>,
    thread: JoinHandle<Samples>,
}

impl TreeSampler {
    /// Starts sampling `probe` every `every`, stamping readings with `clock`.
    ///
    /// # Errors
    /// If the sampler thread cannot be spawned.
    pub fn start(
        mut probe: TreeProbe,
        clock: SharedClock,
        every: Duration,
    ) -> std::io::Result<Self> {
        // Capacity 1: the only message is the stop signal.
        let (stop, stopped) = mpsc::sync_channel::<()>(1);
        let thread = std::thread::Builder::new()
            .name("bench-capacity-sampler".into())
            .spawn(move || {
                let mut samples = Samples::default();
                loop {
                    if let Some(reading) = probe.read() {
                        samples.rss_peak = samples.rss_peak.max(reading.rss_bytes);
                        samples.pss_peak = samples.pss_peak.max(reading.pss_bytes);
                        samples.count = samples.count.saturating_add(1);
                        if samples.readings.len() < MAX_READINGS {
                            samples.readings.push((clock.now(), reading));
                        }
                    }
                    match stopped.recv_timeout(every) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return samples,
                    }
                }
            })?;
        Ok(Self { stop, thread })
    }

    /// Stops sampling and returns what was seen, or `None` if the sampler thread panicked. Joins
    /// the thread: call it off the async workers (`spawn_blocking`).
    pub fn finish(self) -> Option<Samples> {
        // A full channel means a stop is already pending.
        let _ = self.stop.try_send(());
        self.thread.join().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brisvox_core::SystemClock;

    fn pid(n: u32) -> Pid {
        Pid::from_u32(n)
    }

    #[test]
    fn descendants_follow_parents_to_any_depth() {
        let pairs = vec![
            (pid(2), pid(1)),
            (pid(3), pid(2)),
            (pid(4), pid(1)),
            (pid(9), pid(8)), // unrelated
        ];
        let mut found: Vec<u32> = descendants(pid(1), pairs.into_iter())
            .into_iter()
            .map(Pid::as_u32)
            .collect();
        found.sort_unstable();
        assert_eq!(found, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_parent_loop_ends() {
        // 1 -> 2 -> 1: a reused pid must not loop.
        let pairs = vec![(pid(2), pid(1)), (pid(1), pid(2))];
        assert_eq!(descendants(pid(1), pairs.into_iter()).len(), 2);
    }

    #[test]
    fn reads_this_process_and_its_tree() {
        let me = std::process::id();
        let process = TreeProbe::new(me, Scope::Process).read().unwrap();
        let tree = TreeProbe::new(me, Scope::Tree).read().unwrap();
        assert!(process.rss_bytes > 0 && process.processes == 1);
        assert!(tree.rss_bytes >= process.rss_bytes && tree.processes >= 1);
        assert!(TreeProbe::current().unwrap().read().is_some());
    }

    /// On Linux `sysinfo` lists threads as processes: they must not be summed (they carry the
    /// whole process's CPU time and RSS), so a process with many threads reads as one process.
    #[test]
    fn threads_are_not_processes() {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let stop_rx = std::sync::Arc::new(std::sync::Mutex::new(stop_rx));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop_rx);
                std::thread::spawn(move || {
                    let _ = stop.lock().map(|rx| rx.recv());
                })
            })
            .collect();
        let tree = TreeProbe::new(std::process::id(), Scope::Tree)
            .read()
            .unwrap();
        drop(stop_tx);
        for thread in threads {
            let _ = thread.join();
        }
        // Other tests of this binary may have up to two child processes alive (the process-tree
        // helper and its child) while this runs; the 6 threads here and the test harness's own
        // must not add to that: with threads counted this reads 8 or more.
        assert!(
            tree.processes <= 4,
            "threads were summed as processes: {} entries, {} B",
            tree.processes,
            tree.rss_bytes,
        );
    }

    /// Environment variable that turns [`process_tree_helper`] into a process of a tree.
    const HELPER_ENV: &str = "BRISVOX_PROBE_TREE_HELPER";

    /// Not a test of its own: when started by [`a_real_process_tree_is_summed_below_its_root`]
    /// with `HELPER_ENV=parent` it starts a copy of itself as `child`, and both block on stdin
    /// until it closes (the parent's stdin pipe closes when the test kills it, the child's
    /// when the parent is gone). Without the variable it passes at once.
    #[test]
    fn process_tree_helper() {
        use std::io::Read as _;
        use std::process::{Command, Stdio};
        let Ok(role) = std::env::var(HELPER_ENV) else {
            return;
        };
        let _child = if role == "parent" {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "capacity::probe::tests::process_tree_helper"])
                .env(HELPER_ENV, "child")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .ok()
        } else {
            None
        };
        let mut sink = [0u8; 16];
        while matches!(std::io::stdin().read(&mut sink), Ok(n) if n > 0) {}
    }

    #[test]
    fn a_real_process_tree_is_summed_below_its_root() {
        use std::process::{Command, Stdio};
        let mut root = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capacity::probe::tests::process_tree_helper"])
            .env(HELPER_ENV, "parent")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = root.id();
        // The child appears a moment after the root: poll the tree for it.
        let mut tree = None;
        for _ in 0..200 {
            let reading = TreeProbe::new(pid, Scope::Tree).read();
            if reading.is_some_and(|r| r.processes >= 2) {
                tree = reading;
                break;
            }
            let (_keep, never) = mpsc::sync_channel::<()>(1);
            let _ = never.recv_timeout(Duration::from_millis(50));
        }
        let alone = TreeProbe::new(pid, Scope::Process).read();
        root.kill().unwrap();
        root.wait().unwrap();
        let tree = tree.expect("the helper process started no child");
        let alone = alone.unwrap();
        assert_eq!(alone.processes, 1, "{alone:?}");
        assert!(tree.processes >= 2, "{tree:?}");
        // The tree holds the child's memory as well as the root's.
        assert!(tree.rss_bytes > alone.rss_bytes, "{tree:?} vs {alone:?}");
    }

    /// A real `smaps_rollup` (kernel 6.6): `Pss:` is the figure, not `Pss_Anon:` and friends.
    const SMAPS_ROLLUP: &str =
        "55d0c8a00000-7ffd5e9ff000 ---p 00000000 00:00 0                          [rollup]
Rss:              148216 kB
Pss:               91032 kB
Pss_Dirty:         70244 kB
Pss_Anon:          64120 kB
Pss_File:          26700 kB
Pss_Shmem:           212 kB
Shared_Clean:      66460 kB
Private_Dirty:     63864 kB
";

    #[test]
    fn parses_the_pss_line_in_bytes() {
        assert_eq!(parse_smaps_rollup_pss(SMAPS_ROLLUP), Some(91_032 * 1024));
        assert_eq!(
            parse_smaps_rollup_pss(
                "Pss:  0 kB
"
            ),
            Some(0)
        );
    }

    #[test]
    fn a_text_without_a_pss_line_or_with_a_bad_one_is_none() {
        assert_eq!(parse_smaps_rollup_pss(""), None);
        assert_eq!(
            parse_smaps_rollup_pss(
                "Rss: 10 kB
Pss_Anon: 5 kB
"
            ),
            None
        );
        assert_eq!(
            parse_smaps_rollup_pss(
                "Pss: lots kB
"
            ),
            None
        );
        assert_eq!(
            parse_smaps_rollup_pss(
                "Pss: 12
"
            ),
            None
        );
        // kB x 1024 must not wrap.
        assert_eq!(
            parse_smaps_rollup_pss(
                "Pss: 18446744073709551615 kB
"
            ),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reads_this_processs_pss() {
        let reading = TreeProbe::current().unwrap().read().unwrap();
        let pss = reading
            .pss_bytes
            .expect("smaps_rollup is readable for the own process");
        assert!(pss > 0 && pss <= reading.rss_bytes, "{reading:?}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn other_platforms_report_no_pss() {
        assert_eq!(
            TreeProbe::current().unwrap().read().unwrap().pss_bytes,
            None
        );
    }

    #[test]
    fn a_missing_process_reads_nothing() {
        assert_eq!(TreeProbe::new(u32::MAX - 1, Scope::Tree).read(), None);
    }

    #[test]
    fn cpu_cores_and_peaks_come_from_the_readings_in_the_window() {
        let clock = SystemClock::shared();
        let t0 = clock.now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let reading = |cpu_ms: u64, rss: u64| Reading {
            rss_bytes: rss,
            pss_bytes: Some(rss / 2),
            cpu_time: Duration::from_millis(cpu_ms),
            processes: 1,
        };
        let samples = Samples {
            readings: vec![
                (at(0), reading(0, 10)),
                (at(1_000), reading(500, 30)),
                (at(2_000), reading(1_500, 20)),
                (at(3_000), reading(1_600, 25)),
            ],
            rss_peak: 30,
            pss_peak: Some(15),
            count: 4,
        };
        // The readings at 1000 ms and 3000 ms: 1100 ms of CPU time over 2 s of wall time.
        let cores = samples.cpu_cores_between(at(1_000), at(3_000)).unwrap();
        assert!((cores - 0.55).abs() < 1e-9, "{cores}");
        assert_eq!(samples.rss_peak_between(at(1_500), at(3_000)), Some(25));
        assert_eq!(samples.pss_peak_between(at(1_500), at(3_000)), Some(12));
        assert_eq!(samples.pss_peak_between(at(5_000), at(6_000)), None);
        assert_eq!(samples.cpu_cores_between(at(1_500), at(1_600)), None);
        assert_eq!(samples.reading_at(at(1_500)).unwrap().rss_bytes, 30);
        assert_eq!(samples.reading_at(t0), Some(reading(0, 10)));
    }

    #[test]
    fn the_sampler_reads_this_process() {
        let clock = SystemClock::shared();
        let sampler = TreeSampler::start(
            TreeProbe::current().unwrap(),
            clock.clone(),
            Duration::from_millis(1),
        )
        .unwrap();
        // A timed wait without sleep(): give it a few periods.
        let (_keep_open, never) = mpsc::sync_channel::<()>(1);
        let _ = never.recv_timeout(Duration::from_millis(50));
        let samples = sampler.finish().unwrap();
        assert!(samples.count >= 2 && samples.rss_peak > 0, "{samples:?}");
    }
}
