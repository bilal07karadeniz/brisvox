//! The retention sweeper: deletes recordings older than the configured retention.
//!
//! [`sweep_once`] looks at the files directly inside the recording directory (no recursion, no
//! symlink followed) whose names this module generated ([`is_recording_file`]) and removes
//! those whose modification time is `retention` or more before `now`. Nothing else in the
//! directory is touched. Every disk operation has a timeout (on the injected clock).
//!
//! # Bounded work, no starvation
//! A sweep reads the directory in rounds of [`ENTRIES_PER_ROUND`] entries and yields to the
//! runtime between rounds, so a huge directory never holds a worker for long; it continues
//! until the listing is exhausted, so a file is reached however many other entries the
//! directory holds (there is no cursor to lose, and no first-N window that old files beyond it
//! could starve in). Only a listing longer than [`MAX_ENTRIES_PER_SWEEP`] entries stops the
//! sweep early (`incomplete`), which only a directory that is not dedicated to the recordings
//! can reach (`docs/PRIVACY.md`).
//!
//! [`run_sweeper`] repeats it on the injected clock until cancelled.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use brisvox_core::Clock;
use tokio_util::sync::CancellationToken;

use super::files::{is_recording_file, within};

/// Directory entries a sweep looks at between two yields to the runtime.
pub const ENTRIES_PER_ROUND: usize = 4_096;

/// Most directory entries one sweep looks at (a safety ceiling, far above what a recording
/// server accumulates in years).
pub const MAX_ENTRIES_PER_SWEEP: usize = 10_000_000;

/// How much a sweep may look at.
#[derive(Debug, Clone, Copy)]
struct Limits {
    round: usize,
    total: usize,
}

impl Limits {
    const DEFAULT: Self = Self {
        round: ENTRIES_PER_ROUND,
        total: MAX_ENTRIES_PER_SWEEP,
    };
}

/// What a sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Entries looked at.
    pub scanned: u64,
    /// Recording files removed.
    pub removed: u64,
    /// Recording files that could not be examined or removed.
    pub failed: u64,
    /// The sweep stopped at [`MAX_ENTRIES_PER_SWEEP`] or at an error listing the directory.
    pub incomplete: bool,
}

/// Counters of the running sweeper, readable while it runs.
#[derive(Debug, Default)]
pub struct SweepStats {
    sweeps: AtomicU64,
    removed: AtomicU64,
    failed: AtomicU64,
    panics: AtomicU64,
}

impl SweepStats {
    /// Records that the sweeper task panicked.
    pub(super) fn note_panic(&self) {
        self.panics.fetch_add(1, Ordering::Relaxed);
    }

    /// Times the sweeper task panicked (it does not restart: a non-zero value means expired
    /// recordings are no longer deleted).
    pub fn panics(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }

    /// Sweeps completed.
    pub fn sweeps(&self) -> u64 {
        self.sweeps.load(Ordering::Relaxed)
    }

    /// Files removed over all sweeps.
    pub fn removed(&self) -> u64 {
        self.removed.load(Ordering::Relaxed)
    }

    /// Files that could not be examined or removed, over all sweeps.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

/// One sweep of `dir`: removes recordings last modified `retention` or more before `now`.
pub async fn sweep_once(
    clock: &dyn Clock,
    dir: &Path,
    retention: Duration,
    now: SystemTime,
    io_timeout: Duration,
) -> SweepReport {
    sweep_with(clock, dir, retention, now, io_timeout, Limits::DEFAULT).await
}

async fn sweep_with(
    clock: &dyn Clock,
    dir: &Path,
    retention: Duration,
    now: SystemTime,
    io_timeout: Duration,
    limits: Limits,
) -> SweepReport {
    let mut report = SweepReport::default();
    let mut entries = match within(clock, io_timeout, tokio::fs::read_dir(dir)).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(target: "brisvox::privacy", error = %e, "retention sweep cannot list the recording directory");
            report.incomplete = true;
            return report;
        }
    };
    loop {
        if report.scanned >= limits.total as u64 {
            report.incomplete = true;
            break;
        }
        if report.scanned > 0 && report.scanned % (limits.round.max(1) as u64) == 0 {
            tokio::task::yield_now().await;
        }
        let entry = match within(clock, io_timeout, entries.next_entry()).await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(target: "brisvox::privacy", error = %e, "retention sweep stopped listing the recording directory");
                report.incomplete = true;
                break;
            }
        };
        report.scanned += 1;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| is_recording_file(n)) else {
            continue;
        };
        match examine(clock, &entry, now, retention, io_timeout).await {
            Ok(false) => {}
            Ok(true) => match within(clock, io_timeout, tokio::fs::remove_file(entry.path())).await
            {
                Ok(()) => report.removed += 1,
                // Gone already (another sweeper, an operator): the goal is met.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(target: "brisvox::privacy", file = name, error = %e, "retention sweep cannot remove a recording");
                }
            },
            Err(e) => {
                report.failed += 1;
                tracing::warn!(target: "brisvox::privacy", file = name, error = %e, "retention sweep cannot examine a recording");
            }
        }
    }
    report
}

/// Whether `entry` is a regular file (not a link) older than `retention`.
async fn examine(
    clock: &dyn Clock,
    entry: &tokio::fs::DirEntry,
    now: SystemTime,
    retention: Duration,
    io_timeout: Duration,
) -> io::Result<bool> {
    let kind = within(clock, io_timeout, entry.file_type()).await?;
    if !kind.is_file() {
        return Ok(false);
    }
    let modified = within(clock, io_timeout, entry.metadata())
        .await?
        .modified()?;
    // A file from the future (clock set back) is not old.
    let age = now.duration_since(modified).unwrap_or_default();
    Ok(age >= retention)
}

/// Sweeps `dir` now and then every `interval` until `cancel` fires.
pub async fn run_sweeper(
    dir: std::path::PathBuf,
    retention: Duration,
    interval: Duration,
    io_timeout: Duration,
    clock: Arc<dyn Clock>,
    stats: Arc<SweepStats>,
    cancel: CancellationToken,
) {
    loop {
        let report = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            report = sweep_once(&*clock, &dir, retention, clock.wall(), io_timeout) => report,
        };
        stats.sweeps.fetch_add(1, Ordering::Relaxed);
        stats.removed.fetch_add(report.removed, Ordering::Relaxed);
        stats.failed.fetch_add(report.failed, Ordering::Relaxed);
        if report.removed > 0 {
            tracing::info!(target: "brisvox::privacy", removed = report.removed, "retention sweep removed expired recordings");
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = clock.sleep(interval) => {}
        }
    }
}

// These tests run on real time and the real file system (a sweep's timestamps are the disk's).
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::fs::{self, File};
    use std::time::SystemTime;

    use brisvox_core::SystemClock;

    use super::*;
    use crate::privacy::files::{Kind, file_name, file_name_for};

    const TIMEOUT: Duration = Duration::from_secs(5);
    const DAY: Duration = Duration::from_secs(86_400);

    fn clock() -> brisvox_core::SharedClock {
        SystemClock::shared()
    }

    fn wall() -> SystemTime {
        clock().wall()
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("brisvox-retention-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Creates `name` last modified `age` before `now`.
    fn make(dir: &Path, name: &str, now: SystemTime, age: Duration) {
        let file = File::create(dir.join(name)).unwrap();
        file.set_modified(now - age).unwrap();
    }

    #[tokio::test]
    async fn old_recordings_go_and_young_ones_and_foreign_files_stay() {
        let dir = dir("sweep");
        let now = wall();
        let old_t = file_name(1, 1, Kind::Transcript);
        let old_c = file_name(1, 1, Kind::Caller);
        let old_b = file_name(1, 1, Kind::Bot);
        let young = file_name(2, 2, Kind::Transcript);
        make(&dir, &old_t, now, 40 * DAY);
        make(&dir, &old_c, now, 31 * DAY);
        make(&dir, &old_b, now, 30 * DAY); // exactly the retention: expired
        make(&dir, &young, now, 29 * DAY);
        make(&dir, "notes.txt", now, 400 * DAY);
        make(&dir, "call-1-1.transcript.jsonl.bak", now, 400 * DAY);
        fs::create_dir(dir.join("call-9-9.caller.wav")).unwrap(); // a directory by that name

        let report = sweep_once(&*clock(), &dir, 30 * DAY, now, TIMEOUT).await;

        assert_eq!(report.removed, 3, "{report:?}");
        assert_eq!(report.failed, 0);
        assert!(!report.incomplete);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "call-1-1.transcript.jsonl.bak",
                "call-2-2.transcript.jsonl",
                "call-9-9.caller.wav",
                "notes.txt"
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn old_encrypted_recordings_go_and_lookalikes_stay() {
        let dir = dir("age");
        let now = wall();
        let old: Vec<String> = Kind::ALL
            .iter()
            .map(|&k| file_name_for(1, 1, k, true))
            .collect();
        for name in &old {
            make(&dir, name, now, 40 * DAY);
        }
        let young = file_name_for(2, 2, Kind::Caller, true);
        make(&dir, &young, now, 2 * DAY);
        for foreign in [
            "call-1-1.transcript.jsonl.age.bak",
            "call-1-1.transcript.jsonl.age.age",
            "call-1-1.age",
            "notes.age",
            "x-call-1-1.bot.wav.age",
        ] {
            make(&dir, foreign, now, 400 * DAY);
        }

        let report = sweep_once(&*clock(), &dir, 30 * DAY, now, TIMEOUT).await;

        assert_eq!(report.removed, 3, "{report:?}");
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "call-1-1.age",
                "call-1-1.transcript.jsonl.age.age",
                "call-1-1.transcript.jsonl.age.bak",
                "call-2-2.caller.wav.age",
                "notes.age",
                "x-call-1-1.bot.wav.age",
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_file_from_the_future_is_kept() {
        let dir = dir("future");
        let now = wall();
        let name = file_name(5, 5, Kind::Bot);
        let file = File::create(dir.join(&name)).unwrap();
        file.set_modified(now + DAY).unwrap();
        let report = sweep_once(&*clock(), &dir, Duration::from_secs(3_600), now, TIMEOUT).await;
        assert_eq!(report.removed, 0);
        assert!(dir.join(&name).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_missing_directory_is_reported_not_fatal() {
        let mut p = std::env::temp_dir();
        p.push(format!("brisvox-retention-missing-{}", std::process::id()));
        let report = sweep_once(&*clock(), &p, DAY, wall(), TIMEOUT).await;
        assert!(report.incomplete);
        assert_eq!(report.removed, 0);
    }

    #[tokio::test]
    async fn the_sweeper_sweeps_at_once_then_on_its_interval_and_stops_on_cancel() {
        let dir = dir("loop");
        let now = wall();
        make(&dir, &file_name(1, 1, Kind::Transcript), now, 40 * DAY);
        let stats = Arc::new(SweepStats::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_sweeper(
            dir.clone(),
            30 * DAY,
            Duration::from_millis(20),
            TIMEOUT,
            SystemClock::shared(),
            Arc::clone(&stats),
            cancel.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while stats.sweeps() < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(stats.removed(), 1);
        // A file that turns old later is taken by a later sweep.
        make(&dir, &file_name(2, 2, Kind::Bot), wall(), 41 * DAY);
        tokio::time::timeout(Duration::from_secs(10), async {
            while stats.removed() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn old_files_beyond_a_round_are_reached_by_later_rounds() {
        let dir = dir("rounds");
        let now = wall();
        // 12 foreign entries and 6 old recordings, rounds of 4: nothing starves behind the
        // first rounds, whatever order the directory lists them in.
        for i in 0..12 {
            make(&dir, &format!("other-{i}.txt"), now, 400 * DAY);
        }
        for i in 0..6 {
            make(&dir, &file_name(i, i, Kind::Transcript), now, 40 * DAY);
        }
        let limits = Limits {
            round: 4,
            total: 1_000,
        };
        let report = sweep_with(&*clock(), &dir, 30 * DAY, now, TIMEOUT, limits).await;
        assert_eq!(report.removed, 6, "{report:?}");
        assert_eq!(report.scanned, 18);
        assert!(!report.incomplete);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_total_ceiling_stops_a_sweep_and_says_so() {
        let dir = dir("ceiling");
        let now = wall();
        for i in 0..10 {
            make(&dir, &file_name(i, i, Kind::Bot), now, 40 * DAY);
        }
        let limits = Limits { round: 3, total: 5 };
        let report = sweep_with(&*clock(), &dir, 30 * DAY, now, TIMEOUT, limits).await;
        assert_eq!(report.scanned, 5);
        assert_eq!(report.removed, 5);
        assert!(report.incomplete);
        // The next sweep takes what is left.
        let report = sweep_with(&*clock(), &dir, 30 * DAY, now, TIMEOUT, limits).await;
        assert_eq!(report.removed, 5);
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_skipped_and_never_followed() {
        let dir = dir("symlink");
        let outside = dir.join("outside");
        fs::create_dir(&outside).unwrap();
        let now = wall();
        make(&outside, "precious.txt", now, 400 * DAY);
        let target = outside.join("precious.txt");
        let link_name = file_name(1, 1, Kind::Transcript);
        std::os::unix::fs::symlink(&target, dir.join(&link_name)).unwrap();
        let dir_link = file_name(2, 2, Kind::Caller);
        std::os::unix::fs::symlink(&outside, dir.join(&dir_link)).unwrap();

        let report = sweep_once(&*clock(), &dir, DAY, now, TIMEOUT).await;

        assert_eq!(report.removed, 0, "{report:?}");
        assert!(target.exists(), "the link target was removed");
        assert!(dir.join(&link_name).symlink_metadata().is_ok());
        assert!(dir.join(&dir_link).symlink_metadata().is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }
}
