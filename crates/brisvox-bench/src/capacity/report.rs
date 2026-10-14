//! The capacity report: one row per ramp step, the verdict of each, the conclusion, and the two
//! output formats (JSON with every number, markdown for `bench/results/`).

use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BenchError;
use crate::stats::Summary;
use crate::v2v::engine_target::EngineTargetSettings;

/// The `schema` field of the JSON report. Bumped when a field changes meaning or is removed, or a
/// field is added that changes how a result is read. `/2` adds `environment` (the platform and
/// the "not a Linux run" flag), the late-frame counts and `late_share_limit` (a generator is
/// limited by a sustained share of late frames, no longer by the single latest one), and
/// `engine.pool_queue`; every new field has a default, so `/1` files still parse.
pub const SCHEMA: &str = "brisvox-bench/capacity/2";

/// The banner the reports carry when the run was not on Linux.
pub fn not_linux_banner(os: &str) -> String {
    format!(
        "**not a Linux run** ({os}): timers and scheduling differ from the Linux servers the \
         engine targets, so these numbers show trends only; numbers for comparisons (with other \
         engines, or against latency targets) come from a Linux run"
    )
}

/// The machine and build a capacity run was made on, in the JSON so a reader of the file alone
/// can tell a trend recorded on Windows from a Linux result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnvironment {
    /// `std::env::consts::OS` of the load generator's host (the server runs on the same host for
    /// `--sim`; for `--url` it may not).
    pub os: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    /// Logical cores of the host (0: unknown).
    pub logical_cores: usize,
    /// Whether the benchmark was a release build.
    pub release_build: bool,
    /// `true` unless the host was Linux: the numbers show trends only.
    pub not_linux_run: bool,
    /// The banner of the markdown report (`not_linux_run` only).
    pub banner: Option<String>,
}

impl RunEnvironment {
    /// The environment of this process.
    pub fn current() -> Self {
        Self::of(
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
            !cfg!(debug_assertions),
        )
    }

    /// An environment from its parts.
    pub fn of(os: &str, arch: &str, logical_cores: usize, release_build: bool) -> Self {
        let not_linux_run = os != "linux";
        Self {
            os: os.to_owned(),
            arch: arch.to_owned(),
            logical_cores,
            release_build,
            not_linux_run,
            banner: not_linux_run.then(|| not_linux_banner(os)),
        }
    }
}

/// How a ramp step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every call clean, no turn lost, p95 V2V within the SLO.
    Pass,
    /// The server missed the SLO or lost calls or turns.
    Fail,
    /// The load generator itself fell behind (frames went out late), so the numbers describe the
    /// generator, not the server: the step is neither a pass nor a fail.
    GeneratorLimited,
}

impl Verdict {
    /// The name used in tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "FAIL",
            Self::GeneratorLimited => "generator limited",
        }
    }
}

/// A latency distribution over answered turns, microseconds, nearest-rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dist {
    /// Samples.
    pub count: usize,
    /// Median.
    pub p50_us: u64,
    /// 90th percentile.
    pub p90_us: u64,
    /// 95th percentile.
    pub p95_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// Largest sample.
    pub max_us: u64,
}

impl Dist {
    /// The distribution of `samples`, or `None` if there are none.
    pub fn of(samples: Vec<Duration>) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let s = Summary::of(samples);
        let us = |d: Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        Some(Self {
            count: s.count,
            p50_us: us(s.p50),
            p90_us: us(s.p90),
            p95_us: us(s.p95),
            p99_us: us(s.p99),
            max_us: us(s.max),
        })
    }
}

/// CPU and memory of a process (tree) during a step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Average CPU cores busy over the step (CPU time delta over wall time between the first and
    /// the last reading); `None` with fewer than two readings.
    pub cpu_cores: Option<f64>,
    /// Peak resident set over the step.
    pub rss_peak_bytes: u64,
    /// Peak proportional set (Linux `Pss` of `smaps_rollup`, summed over the tree) over the step:
    /// pages shared between processes counted once in total, where `rss_peak_bytes` counts them
    /// once per process. `None` where the platform does not report it; absent in files written
    /// before PSS was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pss_peak_bytes: Option<u64>,
    /// Most processes counted in one reading (a tree has several with worker processes).
    pub processes: usize,
    /// Readings taken.
    pub readings: usize,
}

/// One ramp step: N calls at once, each replaying the fixture in real time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    /// Calls open at once (the concurrency of the step).
    pub concurrent: usize,
    /// Calls played (`concurrent` x rounds).
    pub calls: usize,
    /// Calls that failed (could not connect, were refused, broke, got a server error).
    pub calls_failed: usize,
    /// The first distinct reasons calls failed.
    pub failures: Vec<String>,
    /// Turns the calls should have produced (calls x utterances).
    pub turns_expected: usize,
    /// Turns answered cleanly.
    pub turns_answered: usize,
    /// Turns whose first audio arrived after the caller began the next utterance.
    pub talked_over: usize,
    /// Turns where the bot was still speaking when the caller stopped.
    pub bot_speaking: usize,
    /// Turns with no audible answer.
    pub no_response: usize,
    /// Share of the expected turns that were not answered cleanly (failed calls included).
    pub bad_rate: f64,
    /// V2V over the answered turns.
    pub v2v: Option<Dist>,
    /// p95 V2V minus the floor, microseconds (negative: faster than the nominal floor).
    pub p95_over_floor_us: Option<i64>,
    /// The largest amount by which the generator sent a frame late.
    pub max_send_lateness_us: u64,
    /// Fixture frames sent more than `lateness_limit_ms` late (their share, not the latest one,
    /// decides whether the generator is the limit).
    #[serde(default)]
    pub late_frames: u64,
    /// Fixture frames sent (the denominator of the late share).
    #[serde(default)]
    pub timed_frames: u64,
    /// Wall time of the step.
    pub wall_ms: u64,
    /// Resident set of the target right before the step (after the cooldown).
    pub rss_before_bytes: Option<u64>,
    /// Proportional set of the target right before the step (see `Usage::pss_peak_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pss_before_bytes: Option<u64>,
    /// The target process (tree), if a pid was given.
    pub target: Option<Usage>,
    /// The load generator's own process.
    pub client: Option<Usage>,
    /// How the step ended.
    pub verdict: Verdict,
    /// Why it did not pass (empty for a pass).
    pub reasons: Vec<String>,
}

/// Why the ramp stopped where it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    /// A step failed: `max_concurrent` is the largest passing step below it.
    Found,
    /// Every step passed: the capacity is at least the top of the ramp.
    NotReached,
    /// The load generator ran out before the server did: a lower bound.
    GeneratorLimited,
    /// The smallest step already failed.
    FirstStepFailed,
}

impl Limit {
    /// How to read the capacity, in words.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Found => "the next step up did not pass",
            Self::NotReached => "every step passed: a lower bound, the limit is above the ramp",
            Self::GeneratorLimited => {
                "the load generator ran out first: a lower bound, not the server's limit"
            }
            Self::FirstStepFailed => "even the smallest step did not pass",
        }
    }
}

/// The result of the ramp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conclusion {
    /// The largest passing step below the first step that failed or was generator limited (the
    /// largest passing step when none did); a later pass after a failure does not raise it.
    /// `None` if no step passed.
    pub max_concurrent: Option<usize>,
    /// The smallest step that failed.
    pub first_failure: Option<usize>,
    /// How to read `max_concurrent`.
    pub limit: Limit,
}

/// The settings the report states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Fixture every call replays.
    pub fixture: String,
    /// Utterances per call.
    pub turns_per_call: usize,
    /// The steps asked for.
    pub steps: Vec<usize>,
    /// Calls each slot plays one after another per step.
    pub rounds: usize,
    /// Calls played alone before the ramp and not counted.
    #[serde(default)]
    pub warmup_calls: usize,
    /// Call starts are spread over this long (slot i starts at spread x i / N).
    pub spread_ms: u64,
    /// Minimum silence between utterances.
    pub min_gap_ms: u64,
    /// Pause between a slot's calls.
    pub pause_ms: u64,
    /// Pause between steps.
    pub cooldown_ms: u64,
    /// The floor the SLO is counted from.
    pub floor_ms: u64,
    /// SLO: allowed p95 V2V above the floor.
    pub slo_over_floor_ms: u64,
    /// SLO: the p95 limit, `floor_ms + slo_over_floor_ms`.
    pub slo_limit_ms: u64,
    /// Share of turns that may be lost or not clean.
    pub max_bad_rate: f64,
    /// A frame sent later than this after its deadline is a late frame.
    pub lateness_limit_ms: u64,
    /// Share of a step's frames that may be late before the generator is the limit.
    #[serde(default)]
    pub late_share_limit: f64,
    /// Bisect between the last pass and the first stop until they are this close (0: off).
    pub resolution: usize,
    /// Whether the ramp went on after a failing step.
    pub keep_going: bool,
    /// Pid of the target process the CPU and memory columns read, if any.
    pub target_pid: Option<u32>,
    /// Whether that reading covers the process tree.
    pub target_tree: bool,
}

/// A whole capacity run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityReport {
    /// [`SCHEMA`].
    pub schema: String,
    /// What was measured ("brisvox", "pipecat", ...).
    pub label: String,
    /// How the server was started, when the benchmark did not start it (`--note`).
    #[serde(default)]
    pub note: Option<String>,
    /// The server URL, reduced to scheme and host.
    pub url: String,
    /// Where it was run (os, arch, cores, build, the "not a Linux run" flag).
    #[serde(default)]
    pub environment: Option<RunEnvironment>,
    /// What was asked.
    pub settings: Settings,
    /// The engine target's mock providers, when the engine server was started by the benchmark.
    pub engine: Option<EngineTargetSettings>,
    /// The warm-up calls, played alone before the ramp (not a step; its failures are shown
    /// because they say the server was not ready).
    #[serde(default)]
    pub warmup: Option<StepReport>,
    /// Every step, by concurrency.
    pub steps: Vec<StepReport>,
    /// The conclusion.
    pub conclusion: Conclusion,
}

/// The conclusion of `steps` (any order): see [`Conclusion::max_concurrent`].
pub fn conclude(steps: &[StepReport]) -> Conclusion {
    let stop = steps
        .iter()
        .filter(|s| s.verdict != Verdict::Pass)
        .min_by_key(|s| s.concurrent);
    let first_failure = steps
        .iter()
        .filter(|s| s.verdict == Verdict::Fail)
        .map(|s| s.concurrent)
        .min();
    let max_concurrent = steps
        .iter()
        .filter(|s| s.verdict == Verdict::Pass)
        .filter(|s| stop.is_none_or(|stop| s.concurrent < stop.concurrent))
        .map(|s| s.concurrent)
        .max();
    let limit = match (stop, max_concurrent) {
        (None, _) => Limit::NotReached,
        (Some(stop), _) if stop.verdict == Verdict::GeneratorLimited => Limit::GeneratorLimited,
        (Some(_), None) => Limit::FirstStepFailed,
        (Some(_), Some(_)) => Limit::Found,
    };
    Conclusion {
        max_concurrent,
        first_failure,
        limit,
    }
}

fn ms_us(us: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // microseconds of a latency
    let ms = us as f64 / 1_000.0;
    format!("{ms:.0}")
}

fn mb(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // a resident set
    let mb = bytes as f64 / (1024.0 * 1024.0);
    format!("{mb:.0}")
}

impl CapacityReport {
    /// The JSON form: every number of every step.
    ///
    /// # Errors
    /// [`BenchError::Config`] if serialization fails (not expected).
    pub fn to_json(&self) -> Result<String, BenchError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| BenchError::Config(format!("capacity report json: {e}")))
    }

    /// The markdown form, headed by `environment` lines (command, machine, the "not a Linux run"
    /// banner when it applies).
    pub fn markdown(&self, environment: &[String]) -> String {
        let s = &self.settings;
        let mut out = format!("# Capacity: {}\n\n", self.label);
        for line in environment {
            let _ = writeln!(out, "- {line}");
        }
        let _ = writeln!(out, "- target: `{}`", self.url);
        if let Some(note) = &self.note {
            let _ = writeln!(out, "- server: {note}");
        }
        let _ = writeln!(
            out,
            "- every call replays `{}` ({} utterances) in real time; {} call(s) per slot, slots \
             start spread over {} ms; steps {:?}; {} warm-up call(s) first, not counted{}",
            s.fixture,
            s.turns_per_call,
            s.rounds,
            s.spread_ms,
            s.steps,
            s.warmup_calls,
            match &self.warmup {
                Some(w) if w.verdict == Verdict::Fail => {
                    format!(" (the warm-up failed: {})", w.reasons.join("; "))
                }
                _ => String::new(),
            }
        );
        let _ = writeln!(
            out,
            "- SLO: p95 V2V at most floor {} ms + {} ms = **{} ms**, no failed call, at most \
             {:.1} % of turns lost or not clean; a healthy step in which more than {:.1} % of the \
             frames went out over {} ms late is generator limited",
            s.floor_ms,
            s.slo_over_floor_ms,
            s.slo_limit_ms,
            s.max_bad_rate * 100.0,
            s.late_share_limit * 100.0,
            s.lateness_limit_ms
        );
        if let Some(engine) = &self.engine {
            let vad = if engine.real_models {
                "real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime)".to_owned()
            } else {
                format!("energy VAD stop {} ms", engine.vad_stop_ms)
            };
            let _ = writeln!(
                out,
                "- engine target: mock providers STT final {} ms, LLM first token {} ms, TTS first \
                 byte {} ms, {vad}, `{}` end of turn, {} pool threads, pool queue {}",
                engine.stt_final_ms,
                engine.llm_first_token_ms,
                engine.tts_first_byte_ms,
                engine.end_of_turn,
                engine.pool_threads,
                if engine.pool_queue == 0 {
                    "not recorded (256)".to_owned()
                } else {
                    engine.pool_queue.to_string()
                }
            );
        }
        match s.target_pid {
            Some(pid) => {
                let _ = writeln!(
                    out,
                    "- CPU / RSS / PSS: read from pid {pid}{} every sample period",
                    if s.target_tree {
                        " and its descendants"
                    } else {
                        ""
                    }
                );
            }
            None => {
                let _ = writeln!(out, "- CPU / RSS of the target: not read (no pid)");
            }
        }
        let c = &self.conclusion;
        let _ = writeln!(
            out,
            "\n**Result: {}**; {}{}.\n",
            c.max_concurrent.map_or_else(
                || "no passing step".to_owned(),
                |n| format!("max {n} concurrent calls within the SLO")
            ),
            c.limit.as_str(),
            c.first_failure
                .map_or_else(String::new, |n| format!(" (first failure at {n})"))
        );
        out.push_str(
            "| concurrent | calls (failed) | answered / expected | talked over / no response / \
             bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU \
             (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) \
             | result |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n",
        );
        for step in &self.steps {
            out.push_str(&step_row(step));
        }
        let mut notes: Vec<String> = Vec::new();
        for step in &self.steps {
            if step.verdict != Verdict::Pass {
                notes.push(format!(
                    "- {} concurrent: {}",
                    step.concurrent,
                    step.reasons.join("; ")
                ));
            }
            for failure in &step.failures {
                notes.push(format!(
                    "- {} concurrent, failed call: {failure}",
                    step.concurrent
                ));
            }
        }
        if !notes.is_empty() {
            out.push_str("\nWhy steps did not pass:\n\n");
            for note in notes {
                let _ = writeln!(out, "{note}");
            }
        }
        out.push_str(
            "\nPercentiles are nearest-rank over the answered turns of the step; with fewer \
             than 20 samples p95 is the largest or second largest sample, and a mock-provider \
             run measures the framework, not the providers (see `docs/LATENCY.md`).\n",
        );
        out
    }
}

fn step_row(step: &StepReport) -> String {
    let dist = |f: fn(&Dist) -> u64| {
        step.v2v
            .as_ref()
            .map_or_else(|| "-".to_owned(), |d| ms_us(f(d)))
    };
    let cores = |u: &Option<Usage>| {
        u.as_ref()
            .and_then(|u| u.cpu_cores)
            .map_or_else(|| "-".to_owned(), |c| format!("{c:.2}"))
    };
    let rss = match (&step.target, step.rss_before_bytes) {
        (Some(t), before) => format!(
            "{} / {}",
            before.map_or_else(|| "-".to_owned(), mb),
            mb(t.rss_peak_bytes)
        ),
        (None, _) => "-".to_owned(),
    };
    let pss = match (&step.target, step.pss_before_bytes) {
        (
            Some(Usage {
                pss_peak_bytes: Some(peak),
                ..
            }),
            before,
        ) => format!(
            "{} / {}",
            before.map_or_else(|| "-".to_owned(), mb),
            mb(*peak)
        ),
        _ => "-".to_owned(),
    };
    let over = step.p95_over_floor_us.map_or_else(
        || "-".to_owned(),
        |us| {
            #[allow(clippy::cast_precision_loss)] // microseconds of a latency
            let ms = us as f64 / 1_000.0;
            format!("{ms:+.0}")
        },
    );
    format!(
        "| {} | {} ({}) | {} / {} | {} / {} / {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
        step.concurrent,
        step.calls,
        step.calls_failed,
        step.turns_answered,
        step.turns_expected,
        step.talked_over,
        step.no_response,
        step.bot_speaking,
        dist(|d| d.p50_us),
        dist(|d| d.p95_us),
        dist(|d| d.p99_us),
        dist(|d| d.max_us),
        over,
        cores(&step.target),
        rss,
        pss,
        cores(&step.client),
        ms_us(step.max_send_lateness_us),
        step.verdict.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn step(concurrent: usize, verdict: Verdict) -> StepReport {
        StepReport {
            concurrent,
            calls: concurrent * 2,
            calls_failed: 0,
            failures: Vec::new(),
            turns_expected: concurrent * 4,
            turns_answered: concurrent * 4,
            talked_over: 0,
            bot_speaking: 0,
            no_response: 0,
            bad_rate: 0.0,
            v2v: Dist::of(vec![
                Duration::from_millis(1_000),
                Duration::from_millis(1_100),
            ]),
            p95_over_floor_us: Some(150_000),
            max_send_lateness_us: 4_000,
            late_frames: 0,
            timed_frames: 1_600,
            wall_ms: 20_000,
            rss_before_bytes: Some(50 << 20),
            pss_before_bytes: Some(30 << 20),
            target: Some(Usage {
                cpu_cores: Some(1.25),
                rss_peak_bytes: 80 << 20,
                pss_peak_bytes: Some(55 << 20),
                processes: 1,
                readings: 40,
            }),
            client: None,
            verdict,
            reasons: if verdict == Verdict::Pass {
                Vec::new()
            } else {
                vec!["p95 too high".to_owned()]
            },
        }
    }

    #[test]
    fn the_conclusion_is_the_largest_pass_below_the_first_stop() {
        let steps = vec![
            step(1, Verdict::Pass),
            step(5, Verdict::Pass),
            step(10, Verdict::Fail),
            // Non-monotone: a later pass does not raise the capacity.
            step(20, Verdict::Pass),
        ];
        let c = conclude(&steps);
        assert_eq!(
            (c.max_concurrent, c.first_failure, c.limit),
            (Some(5), Some(10), Limit::Found)
        );
        let all_pass = conclude(&[step(1, Verdict::Pass), step(5, Verdict::Pass)]);
        assert_eq!(
            (
                all_pass.max_concurrent,
                all_pass.first_failure,
                all_pass.limit
            ),
            (Some(5), None, Limit::NotReached)
        );
        let none = conclude(&[step(1, Verdict::Fail), step(5, Verdict::Pass)]);
        assert_eq!(
            (none.max_concurrent, none.limit),
            (None, Limit::FirstStepFailed)
        );
        let generator = conclude(&[step(1, Verdict::Pass), step(50, Verdict::GeneratorLimited)]);
        assert_eq!(
            (
                generator.max_concurrent,
                generator.first_failure,
                generator.limit
            ),
            (Some(1), None, Limit::GeneratorLimited)
        );
        // A generator limited first step is not "the server failed the smallest step".
        let first = conclude(&[step(1, Verdict::GeneratorLimited)]);
        assert_eq!(
            (first.max_concurrent, first.limit),
            (None, Limit::GeneratorLimited)
        );
        assert_eq!(conclude(&[]).limit, Limit::NotReached);
    }

    #[test]
    fn markdown_states_the_result_and_the_reasons() {
        let report = CapacityReport {
            schema: SCHEMA.to_owned(),
            label: "test".to_owned(),
            note: Some("started by hand".to_owned()),
            url: "ws://127.0.0.1:1".to_owned(),
            settings: Settings {
                fixture: "f".to_owned(),
                turns_per_call: 2,
                steps: vec![1, 5],
                rounds: 2,
                warmup_calls: 0,
                spread_ms: 4_000,
                min_gap_ms: 4_000,
                pause_ms: 200,
                cooldown_ms: 1_000,
                floor_ms: 950,
                slo_over_floor_ms: 250,
                slo_limit_ms: 1_200,
                max_bad_rate: 0.0,
                lateness_limit_ms: 100,
                late_share_limit: 0.01,
                resolution: 0,
                keep_going: false,
                target_pid: Some(42),
                target_tree: true,
            },
            environment: Some(RunEnvironment::of("windows", "x86_64", 32, true)),
            engine: None,
            warmup: None,
            steps: vec![step(1, Verdict::Pass), step(5, Verdict::Fail)],
            conclusion: conclude(&[step(1, Verdict::Pass), step(5, Verdict::Fail)]),
        };
        let md = report.markdown(&["command: `x`".to_owned()]);
        assert!(md.contains("max 1 concurrent calls within the SLO"), "{md}");
        assert!(md.contains("first failure at 5"), "{md}");
        assert!(md.contains("**1200 ms**"), "{md}");
        assert!(md.contains("5 concurrent: p95 too high"), "{md}");
        assert!(md.contains("pid 42 and its descendants"), "{md}");
        assert!(md.contains("- server: started by hand"), "{md}");
        assert!(md.contains("| 1.25 |"), "{md}");
        assert!(md.contains("| 50 / 80 | 30 / 55 |"), "{md}");
        let json = report.to_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["steps"][0]["target"]["pss_peak_bytes"], 55 << 20);
        // The platform is in the JSON itself, not only in the markdown.
        assert_eq!(value["environment"]["os"], "windows");
        assert_eq!(value["environment"]["not_linux_run"], true);
        assert!(
            value["environment"]["banner"]
                .as_str()
                .unwrap()
                .contains("not a Linux run"),
            "{json}"
        );
        let back: CapacityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn a_linux_environment_has_no_banner_and_other_hosts_do() {
        let linux = RunEnvironment::of("linux", "x86_64", 4, true);
        assert!(!linux.not_linux_run && linux.banner.is_none());
        let mac = RunEnvironment::of("macos", "aarch64", 8, false);
        assert!(mac.not_linux_run);
        assert!(mac.banner.unwrap().contains("(macos)"));
        assert_eq!(
            RunEnvironment::current().not_linux_run,
            std::env::consts::OS != "linux"
        );
    }

    #[test]
    fn dist_is_nearest_rank_microseconds() {
        assert_eq!(Dist::of(Vec::new()), None);
        let d = Dist::of((1..=100).map(Duration::from_millis).collect()).unwrap();
        assert_eq!(
            (d.count, d.p50_us, d.p95_us, d.max_us),
            (100, 50_000, 95_000, 100_000)
        );
    }
}
