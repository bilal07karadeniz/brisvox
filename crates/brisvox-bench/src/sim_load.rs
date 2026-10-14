//! In-process load simulation.
//!
//! Runs N simulated calls concurrently in one process, in real time ([`SystemClock`]) on a
//! multi-thread tokio runtime: each call is the full reference pipeline of
//! [`SimSession`] (energy VAD, STT, turn manager, LLM, TTS, pacer, transport) driven by a scripted
//! multi-turn caller against mock providers with realistic, seeded latencies.
//!
//! **Internal overhead** of a turn is the time after the end-of-turn decision that is not spent
//! between a provider request and its answer reaching the pipeline:
//!
//! ```text
//! overhead = (FirstAudioOut - TurnEnd) - (LlmFirstSentence - LlmRequest) - (TtsFirstByte - TtsRequest)
//! ```
//!
//! What it covers: the hops `TurnEnd -> LlmRequest` (task spawn, scheduling) and
//! `TtsFirstByte -> FirstAudioOut` (pacer, transport). What it cannot see: a late wake-up inside
//! a provider interval (it counts as provider time) and everything before `TurnEnd` (input
//! frames, VAD, STT). Scheduling pressure on every hop is measured separately by the **wake
//! probe** ([`WakeProbe`]): how long a task woken from outside waits for a worker.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::{SharedClock, SystemClock, TurnEvent, TurnRecord};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::{SessionRun, SimSession};
use tokio::time::Instant;

use crate::BenchError;
use crate::resources::{ProcessProbe, Sampler, WakeProbe};
use crate::stats::{Summary, ms};

const MS: Duration = Duration::from_millis(1);
/// What the bot answers every turn (13 characters = 780 ms of mock TTS audio).
const ANSWER: &str = "Okay, got it.";
/// Median of the mock STT final latency, in ms (log-normal, see `configure`).
pub const STT_FINAL_MEDIAN_MS: u64 = 150;
/// Median of the mock LLM first-token latency, in ms.
pub const LLM_FIRST_TOKEN_MEDIAN_MS: u64 = 350;
/// Median of the mock TTS first-byte latency, in ms.
pub const TTS_FIRST_BYTE_MEDIAN_MS: u64 = 150;
/// Resource sampling period.
const SAMPLE_EVERY: Duration = Duration::from_millis(50);
/// Wake probe period.
const WAKE_EVERY: Duration = Duration::from_millis(5);
/// Upper bound on waiting for detached tasks to wind down after the last session returned.
const SETTLE: Duration = Duration::from_secs(2);

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadConfig {
    /// Concurrent calls.
    pub sessions: usize,
    /// User turns per call.
    pub turns: usize,
    /// Base seed of the mock latency samplers (session `i` uses `seed + i`).
    pub seed: u64,
    /// Length of each user utterance.
    pub utterance: Duration,
    /// Silence after each utterance. Must cover the bot's answer (response + audio) so the
    /// caller never barges in; barge-ins are counted in the report.
    pub gap: Duration,
    /// Call starts are spread evenly over this window (no thundering herd at t = 0).
    pub ramp: Duration,
    /// How long a call stays open after the caller's script ends.
    pub linger: Duration,
    /// tokio worker threads (`None`: one per core).
    pub worker_threads: Option<usize>,
}

impl LoadConfig {
    /// The default call shape for `sessions` concurrent calls: 3 turns of 1.2 s speech and
    /// 3.5 s silence.
    pub fn new(sessions: usize) -> Self {
        Self {
            sessions,
            turns: 3,
            seed: 1,
            utterance: 1_200 * MS,
            gap: 3_500 * MS,
            ramp: Duration::from_secs(1),
            linger: 1_500 * MS,
            worker_threads: None,
        }
    }

    /// The caller's script.
    pub fn scenario(&self) -> Scenario {
        let mut scenario = Scenario::new().silence(200 * MS);
        for _ in 0..self.turns {
            scenario = scenario
                .speak("hello there how are you", self.utterance)
                .silence(self.gap);
        }
        scenario
    }

    /// Wall-clock bound for the whole run, after which unfinished calls count as failed.
    fn deadline(&self) -> Duration {
        let call = self.scenario().duration() + self.linger + self.ramp;
        call.saturating_mul(3) + Duration::from_secs(30)
    }
}

/// Results of one load run.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadReport {
    /// What was run.
    pub config: LoadConfig,
    /// tokio worker threads used.
    pub worker_threads: usize,
    /// Wall time of the run (first call start to last call end).
    pub wall: Duration,
    /// Turns scripted across all calls.
    pub turns_expected: usize,
    /// Turns that produced bot audio.
    pub turns_answered: usize,
    /// Barge-ins (should be 0 with a long enough gap).
    pub interrupts: usize,
    /// Calls that failed, panicked or missed the deadline.
    pub failed_sessions: usize,
    /// Internal overhead per answered turn with all marks (see the module docs).
    pub overhead: Summary,
    /// Turns whose provider intervals exceeded the post-endpoint time (overhead clamped to 0;
    /// overlapping intervals, should not happen with one sentence per answer).
    pub overhead_clamped: usize,
    /// Overhead part 1: end-of-turn decision to LLM request (`LlmRequest - TurnEnd`).
    pub to_llm_request: Summary,
    /// Overhead part 2: TTS first byte to the transport (`FirstAudioOut - TtsFirstByte`).
    pub to_audio_out: Summary,
    /// Provider time per answered turn (LLM to first sentence + TTS to first byte).
    pub providers: Summary,
    /// Server-side voice-to-voice per answered turn (`FirstAudioOut - UserSpeechEnd`).
    pub response: Summary,
    /// Delay between waking a task from outside the runtime and it running (wake probe).
    pub wake: Summary,
    /// Wake probe stamps dropped because the probe task fell behind.
    pub wake_dropped: usize,
    /// CPU time used during the whole run (ramp, plateau, linger, settle).
    pub cpu_time: Option<Duration>,
    /// Average CPU cores busy while every call was active (after the ramp, before the first
    /// call's script ended).
    pub cpu_cores_plateau: Option<f64>,
    /// Resident memory before the run.
    pub rss_before: Option<u64>,
    /// Peak resident memory during the run (0 if the sampler failed).
    pub rss_peak: u64,
    /// Whether the resource sampler thread ran to the end.
    pub sampler_ok: bool,
    /// Alive tokio tasks before the run.
    pub tasks_before: usize,
    /// Peak alive tokio tasks during the run.
    pub tasks_peak: usize,
    /// Alive tokio tasks after every call ended and a settle period (above `tasks_before`:
    /// leaked). Covers `tokio::spawn`ed tasks, not `spawn_blocking` or other thread pools.
    pub tasks_after: usize,
}

impl LoadReport {
    /// Average CPU cores busy over the whole run (CPU time / wall time).
    pub fn cpu_cores(&self) -> Option<f64> {
        let wall = self.wall.as_secs_f64();
        self.cpu_time
            .filter(|_| wall > 0.0)
            .map(|cpu| cpu.as_secs_f64() / wall)
    }

    /// Peak resident memory above the pre-run level, per call (includes fixed process growth,
    /// so it overstates the marginal cost at small N).
    pub fn rss_per_session(&self) -> Option<u64> {
        let calls = u64::try_from(self.config.sessions)
            .ok()
            .filter(|n| *n > 0)?;
        let before = self.rss_before?;
        Some(self.rss_peak.saturating_sub(before) / calls)
    }

    /// Tasks still alive after the run beyond the pre-run count.
    pub fn leaked_tasks(&self) -> usize {
        self.tasks_after.saturating_sub(self.tasks_before)
    }

    /// The harness's own correctness checks; empty when the run is valid.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let n = self.config.sessions;
        if self.failed_sessions > 0 {
            problems.push(format!("{n} calls: {} failed", self.failed_sessions));
        }
        if self.turns_answered < self.turns_expected {
            problems.push(format!(
                "{n} calls: {}/{} turns answered",
                self.turns_answered, self.turns_expected
            ));
        }
        if self.interrupts > 0 {
            problems.push(format!("{n} calls: {} barge-ins", self.interrupts));
        }
        if self.overhead.count < self.turns_answered {
            problems.push(format!(
                "{n} calls: overhead measured for {} of {} answered turns (missing marks)",
                self.overhead.count, self.turns_answered
            ));
        }
        if self.overhead_clamped > 0 {
            problems.push(format!(
                "{n} calls: {} turns with overlapping provider intervals",
                self.overhead_clamped
            ));
        }
        if self.leaked_tasks() > 0 {
            problems.push(format!("{n} calls: {} leaked tasks", self.leaked_tasks()));
        }
        if self.wake_dropped > 0 {
            problems.push(format!(
                "{n} calls: {} wake probe stamps dropped (wake percentiles understated)",
                self.wake_dropped
            ));
        }
        if !self.sampler_ok {
            problems.push(format!("{n} calls: resource sampler failed"));
        }
        problems
    }
}

/// Per-turn samples collected from finished calls.
#[derive(Default)]
pub(crate) struct Collected {
    pub(crate) overhead: Vec<Duration>,
    pub(crate) overhead_clamped: usize,
    pub(crate) to_llm_request: Vec<Duration>,
    pub(crate) to_audio_out: Vec<Duration>,
    pub(crate) providers: Vec<Duration>,
    pub(crate) response: Vec<Duration>,
    pub(crate) answered: usize,
    pub(crate) interrupts: usize,
    pub(crate) failed: usize,
}

impl Collected {
    pub(crate) fn add_run(&mut self, run: &SessionRun) {
        if run.outcome.is_err() {
            self.failed = self.failed.saturating_add(1);
        }
        self.interrupts = self
            .interrupts
            .saturating_add(run.output.iter().filter(|r| r.kind == "interrupt").count());
        for turn in &run.turns {
            self.add_turn(turn);
        }
    }

    pub(crate) fn add_turn(&mut self, turn: &TurnRecord) {
        use TurnEvent as E;
        if turn.at(E::FirstAudioOut).is_none() {
            return;
        }
        self.answered = self.answered.saturating_add(1);
        if let Some(response) = turn.between(E::UserSpeechEnd, E::FirstAudioOut) {
            self.response.push(response);
        }
        if let (Some(post), Some(llm), Some(tts)) = (
            turn.between(E::TurnEnd, E::FirstAudioOut),
            turn.between(E::LlmRequest, E::LlmFirstSentence),
            turn.between(E::TtsRequest, E::TtsFirstByte),
        ) {
            let providers = llm.saturating_add(tts);
            if providers > post {
                self.overhead_clamped = self.overhead_clamped.saturating_add(1);
            }
            self.providers.push(providers);
            self.overhead.push(post.saturating_sub(providers));
        }
        if let Some(d) = turn.between(E::TurnEnd, E::LlmRequest) {
            self.to_llm_request.push(d);
        }
        if let Some(d) = turn.between(E::TtsFirstByte, E::FirstAudioOut) {
            self.to_audio_out.push(d);
        }
    }
}

/// Mock latencies of a typical cloud stack (same shape as the simulator's "realistic"
/// scenarios): STT final ~150 ms, LLM first token ~350 ms, TTS first byte ~150 ms, log-normal.
/// The LLM answers turn `n` with `answers[n]`.
pub(crate) fn configure(session: &SimSession, seed: u64, answers: &[&str]) {
    let log_normal = |median_ms: u64| LatencyProfile::LogNormal {
        median: Duration::from_millis(median_ms),
        sigma: 0.3,
    };
    let _ = session.stt.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::Fixed(100 * MS),
        log_normal(STT_FINAL_MEDIAN_MS),
        seed.wrapping_mul(4).wrapping_add(1),
    );
    let _ = session.llm.clone().with_latency(
        log_normal(LLM_FIRST_TOKEN_MEDIAN_MS),
        LatencyProfile::Fixed(30 * MS),
        seed.wrapping_mul(4).wrapping_add(2),
    );
    let _ = session.tts.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        log_normal(TTS_FIRST_BYTE_MEDIAN_MS),
        LatencyProfile::Fixed(20 * MS),
        seed.wrapping_mul(4).wrapping_add(3),
    );
    for answer in answers {
        session.llm.push(LlmScript::text([*answer]));
    }
}

/// What [`drive`] measured.
struct Driven {
    collected: Collected,
    start: Instant,
    wall: Duration,
    wake: Vec<Duration>,
    wake_dropped: usize,
}

/// Starts every call, waits for all of them (bounded by the config's deadline) and collects
/// their turns.
async fn drive(config: &LoadConfig, clock: SharedClock) -> Result<Driven, BenchError> {
    let wake_probe = WakeProbe::start(
        &tokio::runtime::Handle::current(),
        clock.clone(),
        WAKE_EVERY,
    )?;
    let scenario = Arc::new(config.scenario());
    let start = clock.now();
    let deadline = deadline_after(start, config.deadline());
    let calls = u32::try_from(config.sessions).unwrap_or(u32::MAX);
    let step = config.ramp.checked_div(calls).unwrap_or_default();
    let mut handles = Vec::with_capacity(config.sessions);
    for i in 0..calls {
        let session = SimSession::new(clock.clone());
        configure(
            &session,
            config.seed.wrapping_add(u64::from(i)),
            &vec![ANSWER; config.turns],
        );
        let scenario = Arc::clone(&scenario);
        let clock = clock.clone();
        let linger = config.linger;
        let begin = deadline_after(start, step.saturating_mul(i));
        handles.push(tokio::spawn(async move {
            brisvox_core::clock::wait_until(&*clock, begin).await;
            session.run(&scenario, linger).await
        }));
    }
    let mut collected = Collected::default();
    for mut handle in handles {
        tokio::select! {
            // A finished call is collected even if the deadline has passed meanwhile.
            biased;
            joined = &mut handle => match joined {
                Ok(Ok(run)) => collected.add_run(&run),
                Ok(Err(_)) | Err(_) => collected.failed = collected.failed.saturating_add(1),
            },
            () = clock.sleep_until(deadline) => {
                handle.abort();
                collected.failed = collected.failed.saturating_add(1);
            }
        }
    }
    let wall = clock.now().saturating_duration_since(start);
    let (wake, wake_dropped) = wake_probe.finish().await;
    // Pipeline tasks are detached: give them a moment to finish dropping after their call
    // returned, so the task count measures leaks, not stragglers.
    let settle = deadline_after(clock.now(), SETTLE);
    let metrics = tokio::runtime::Handle::current().metrics();
    while metrics.num_alive_tasks() > 0 && clock.now() < settle {
        clock.sleep(MS).await;
    }
    Ok(Driven {
        collected,
        start,
        wall,
        wake,
        wake_dropped,
    })
}

/// Runs `config` on a fresh multi-thread runtime and reports.
///
/// # Errors
/// [`BenchError::Config`] for zero calls or turns, [`BenchError::Io`] if the runtime or a
/// probe thread cannot be created.
pub fn run(config: &LoadConfig) -> Result<LoadReport, BenchError> {
    if config.sessions == 0 || config.turns == 0 {
        return Err(BenchError::Config(
            "need at least one call and one turn".into(),
        ));
    }
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_time().thread_name("bench-worker");
    if let Some(workers) = config.worker_threads {
        builder.worker_threads(workers.max(1));
    }
    let runtime = builder.build()?;
    let metrics = runtime.metrics();
    let clock = SystemClock::shared();
    let mut probe = ProcessProbe::new();
    let before = probe.read();
    let tasks_before = metrics.num_alive_tasks();
    let sampler = Sampler::start(runtime.handle().clone(), clock.clone(), SAMPLE_EVERY)?;
    let driven = runtime.block_on(drive(config, clock));
    let samples = sampler.finish();
    let after = probe.read();
    let driven = driven?;
    // Plateau: every call has started and none has finished its script.
    let plateau_from = deadline_after(driven.start, config.ramp);
    let plateau_to = deadline_after(driven.start, config.scenario().duration());
    let collected = driven.collected;
    Ok(LoadReport {
        config: config.clone(),
        worker_threads: metrics.num_workers(),
        wall: driven.wall,
        turns_expected: config.sessions.saturating_mul(config.turns),
        turns_answered: collected.answered,
        interrupts: collected.interrupts,
        failed_sessions: collected.failed,
        overhead: Summary::of(collected.overhead),
        overhead_clamped: collected.overhead_clamped,
        to_llm_request: Summary::of(collected.to_llm_request),
        to_audio_out: Summary::of(collected.to_audio_out),
        providers: Summary::of(collected.providers),
        response: Summary::of(collected.response),
        wake: Summary::of(driven.wake),
        wake_dropped: driven.wake_dropped,
        cpu_time: before
            .zip(after)
            .map(|(b, a)| a.cpu_time.saturating_sub(b.cpu_time)),
        cpu_cores_plateau: samples
            .as_ref()
            .and_then(|s| s.cpu_cores_between(plateau_from, plateau_to)),
        rss_before: before.map(|b| b.rss_bytes),
        rss_peak: samples.as_ref().map_or(0, |s| s.rss_peak),
        sampler_ok: samples.is_some(),
        tasks_before,
        tasks_peak: samples.as_ref().map_or(0, |s| s.alive_tasks_peak),
        tasks_after: metrics.num_alive_tasks(),
    })
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn opt(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.decimals$}"))
}

/// A markdown report of several runs (one row per run), with `environment` lines on top.
pub fn markdown(reports: &[LoadReport], environment: &[String]) -> String {
    let mut out = String::from("# In-process load simulation baseline\n\n");
    for line in environment {
        out.push_str(&format!("- {line}\n"));
    }
    if let Some(first) = reports.first() {
        let c = &first.config;
        out.push_str(&format!(
            "- call: {} turns of {} ms speech + {} ms silence, answer \"{ANSWER}\", ramp {} ms, \
             linger {} ms; mock latency medians STT final 150 / LLM first token 350 / TTS first \
             byte 150 ms (log-normal, sigma 0.3)\n",
            c.turns,
            c.utterance.as_millis(),
            c.gap.as_millis(),
            c.ramp.as_millis(),
            c.linger.as_millis()
        ));
    }
    out.push_str(
        "\nOverhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first \
         byte; wake = delay from waking a task to it running (scheduling pressure on every hop). \
         CPU = cores busy on the plateau (all calls active). Definitions and caveats: \
         `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).\n\n\
         | calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 \
         | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak \
         RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:\
         |---:|---:|---:|---:|\n",
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} \
             | {} | {} | {} | {} | {:.1} |\n",
            r.config.sessions,
            r.turns_answered,
            r.turns_expected,
            r.failed_sessions,
            r.interrupts,
            r.overhead.count,
            ms(r.overhead.p50),
            ms(r.overhead.p90),
            ms(r.overhead.p99),
            ms(r.overhead.max),
            ms(r.wake.p50),
            ms(r.wake.p99),
            ms(r.wake.max),
            ms(r.providers.p50),
            ms(r.response.p50),
            ms(r.response.p99),
            opt(r.cpu_cores_plateau, 2),
            mib(r.rss_peak),
            opt(r.rss_per_session().map(|b| b as f64 / 1024.0), 0),
            r.tasks_peak,
            r.leaked_tasks(),
            r.wall.as_secs_f64(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_runs() {
        assert!(matches!(
            run(&LoadConfig::new(0)),
            Err(BenchError::Config(_))
        ));
        let mut no_turns = LoadConfig::new(1);
        no_turns.turns = 0;
        assert!(matches!(run(&no_turns), Err(BenchError::Config(_))));
    }

    #[test]
    fn scenario_has_one_utterance_per_turn() {
        let config = LoadConfig::new(1);
        assert_eq!(config.scenario().speech_segments().len(), config.turns);
    }

    #[test]
    fn problems_flag_every_correctness_signal() {
        let healthy = LoadReport {
            config: LoadConfig::new(2),
            worker_threads: 1,
            wall: Duration::from_secs(1),
            turns_expected: 6,
            turns_answered: 6,
            interrupts: 0,
            failed_sessions: 0,
            overhead: Summary::of(vec![MS; 6]),
            overhead_clamped: 0,
            to_llm_request: Summary::default(),
            to_audio_out: Summary::default(),
            providers: Summary::default(),
            response: Summary::default(),
            wake: Summary::default(),
            wake_dropped: 0,
            cpu_time: None,
            cpu_cores_plateau: None,
            rss_before: None,
            rss_peak: 0,
            sampler_ok: true,
            tasks_before: 0,
            tasks_peak: 10,
            tasks_after: 0,
        };
        assert!(healthy.problems().is_empty());
        let broken = [
            LoadReport {
                failed_sessions: 1,
                ..healthy.clone()
            },
            LoadReport {
                turns_answered: 5,
                ..healthy.clone()
            },
            LoadReport {
                interrupts: 1,
                ..healthy.clone()
            },
            LoadReport {
                overhead: Summary::of(vec![MS; 5]),
                ..healthy.clone()
            },
            LoadReport {
                overhead_clamped: 1,
                ..healthy.clone()
            },
            LoadReport {
                tasks_after: 1,
                ..healthy.clone()
            },
            LoadReport {
                sampler_ok: false,
                ..healthy.clone()
            },
            LoadReport {
                wake_dropped: 1,
                ..healthy.clone()
            },
        ];
        for report in broken {
            assert_eq!(report.problems().len(), 1, "{report:?}");
        }
    }
}
