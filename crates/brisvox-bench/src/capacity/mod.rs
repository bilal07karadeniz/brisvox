//! The capacity benchmark: how many concurrent calls does a server carry within a
//! latency SLO?
//!
//! The harness is the V2V client of [`crate::v2v`] run many times at once. A **ramp** of steps
//! (for example 1, 5, 10, 20, 50 concurrent calls) is played against one websocket URL. In a
//! step every one of the N *slots* starts a call (slot `i` after `spread x i / N`, so the calls
//! do not all speak in the same instant), plays the fixture in real time, and starts the next of
//! its `rounds` calls when the previous one ended: N calls are open at all times from the start
//! of the last slot until the first slot finishes. Every call measures its own V2V per utterance
//! with exactly the client logic of `brisvox-bench v2v` ([`run_call`]); the step pools the
//! answered turns of all calls.
//!
//! A step **passes** when no call failed, no more than `max_bad_rate` of the expected turns were
//! lost or not clean (talked over, bot still speaking, no answer; none by default), and the p95
//! V2V is within the SLO: the **floor** (the latency of an unloaded turn with the providers
//! used, stated by the operator or known for the mock providers) plus an allowance (250 ms by
//! default). A step during which the generator itself sent a sustained share of its frames late
//! (more than `late_share_limit` of them, 1 % by default, each later than `lateness_limit`) and
//! in which the server otherwise looks healthy is **generator limited**: the numbers then
//! describe the load generator, not the server, and the ramp stops there with a lower bound. One
//! late frame (a scheduling hiccup) is not a verdict; and a step with failed calls, lost turns or
//! a p95 over the SLO stays a **fail** even when the generator was late as well, because a
//! stalled server backs the generator's own writes up and so also shows as lateness: the reasons
//! then name both.
//!
//! The reported capacity ([`report::Conclusion::max_concurrent`]) is the largest passing step
//! below the first step that did not pass. With a `resolution` the ramp then bisects between the
//! last pass and the first stop until they are that close.
//!
//! # What is measured, and what is not
//! - The server is a separate process. With a pid ([`CapacityConfig::target_pid`]) its CPU time
//!   and resident memory (summed over the process tree) are sampled every `sample_every` on a
//!   thread of its own ([`probe`]); the generator's own process is sampled the same way so a
//!   saturated generator shows.
//! - brisvox: [`child::serve`] (`brisvox-bench engine-server`, started by `capacity --sim`) runs
//!   the production engine over the `brisvox_sim` mock providers; Pipecat:
//!   `bench/pipecat_baseline/bot.py --mock-providers` runs the real Pipecat pipeline over mocks
//!   with the same latencies and shapes. No provider keys, no provider noise; the numbers are the
//!   frameworks' own cost (and exclude model inference on both sides: the VAD is an energy
//!   detector and the Smart Turn model answers "complete", see `docs/LATENCY.md`).
//! - Time comes from the injected clock; the run is real time (the system clock).

pub mod child;
pub mod probe;
pub mod report;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::wait_for;
use brisvox_core::{Secret, SharedClock};
use brisvox_sim::fixture::Fixture;
use tokio_util::sync::CancellationToken;

use crate::BenchError;
use crate::fixture_load::pad_gaps;
use crate::v2v::client::{ClientConfig, run_call};
use crate::v2v::engine_target::EngineTargetSettings;
use crate::v2v::measure::{Outcome, TurnResult};
use crate::v2v::report::redact_url;
use probe::{Samples, Scope, TreeProbe, TreeSampler};
use report::{
    CapacityReport, Dist, RunEnvironment, SCHEMA, Settings, StepReport, Usage, Verdict, conclude,
};

/// Distinct failure messages kept per step.
pub const MAX_FAILURE_NOTES: usize = 5;

/// The default [`CapacityConfig::late_share_limit`]: a step whose generator sent more than 1 % of
/// its frames late is generator limited.
pub const DEFAULT_LATE_SHARE: f64 = 0.01;

/// The latency objective of a capacity run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slo {
    /// The V2V of an unloaded turn: for the mock providers the sum of the end-of-turn wait and
    /// the providers' fixed latencies, for a live system what a one-call `brisvox-bench v2v` run
    /// shows.
    pub floor: Duration,
    /// How far p95 may rise above the floor.
    pub over_floor: Duration,
}

impl Slo {
    /// The p95 limit: the floor plus the allowance.
    pub fn limit(&self) -> Duration {
        self.floor.saturating_add(self.over_floor)
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub struct CapacityConfig {
    /// The server under test (a secret: it may carry a token, see [`redact_url`]).
    pub url: Secret<String>,
    /// What the report calls the system ("brisvox", "pipecat").
    pub label: String,
    /// A free-text note for the report: how the server was started, when the benchmark did not
    /// start it itself (`--url`).
    pub note: Option<String>,
    /// The fixture every call replays.
    pub fixture: String,
    /// Where the fixtures are.
    pub fixtures_dir: PathBuf,
    /// The ramp: concurrent calls per step (sorted and deduplicated; each at least 1).
    pub steps: Vec<usize>,
    /// Bisect between the last pass and the first stop until they are this close; 0: off.
    pub resolution: usize,
    /// Go on after a step that did not pass (the report still takes the first stop).
    pub keep_going: bool,
    /// Calls each slot plays one after another in a step. At least 1.
    pub rounds: usize,
    /// Calls played one after another before the ramp and not counted: a server that loads
    /// models or compiles code on its first calls (a Python process, a cold cache) would fail the
    /// first step for that alone. 0: none.
    pub warmup_calls: usize,
    /// Slot `i` of `n` starts `spread x i / n` after the step began.
    pub spread: Duration,
    /// Minimum silence between utterances ([`pad_gaps`]).
    pub min_gap: Duration,
    /// Pause between the calls of a slot.
    pub pause: Duration,
    /// Pause between steps, for the server to release what the last step held.
    pub cooldown: Duration,
    /// The objective.
    pub slo: Slo,
    /// Share of the expected turns that may be lost or not clean (0.0: none).
    pub max_bad_rate: f64,
    /// A frame sent later than this after its deadline is a late frame.
    pub lateness_limit: Duration,
    /// Share of the step's frames that may be late before the generator is the limit.
    pub late_share_limit: f64,
    /// The server's process id, to read its CPU and memory.
    pub target_pid: Option<u32>,
    /// Whether the reading adds up the processes below the pid.
    pub target_scope: Scope,
    /// Period of the CPU and memory readings.
    pub sample_every: Duration,
    /// Client settings; its `url` is ignored.
    pub client: ClientConfig,
    /// The engine target's settings to state in the report, when the benchmark started it.
    pub engine: Option<EngineTargetSettings>,
    /// Called after every step, in order, with whether it was the warm-up (progress output).
    pub on_step: Option<fn(&StepReport, bool)>,
}

impl CapacityConfig {
    /// A configuration for `url` with the defaults of the CLI: steps 1, 5, 10, 20; two calls per
    /// slot; starts spread over 4 s; 4 s between utterances; SLO floor + 250 ms; no lost turn.
    pub fn new(url: impl Into<String>, floor: Duration) -> Self {
        Self {
            url: Secret::new(url.into()),
            label: "server".to_owned(),
            note: None,
            fixture: "en_two_utterances".to_owned(),
            fixtures_dir: brisvox_sim::fixture::fixtures_dir(),
            steps: vec![1, 5, 10, 20],
            resolution: 0,
            keep_going: false,
            rounds: 2,
            warmup_calls: 1,
            spread: Duration::from_millis(4_000),
            min_gap: Duration::from_millis(4_000),
            pause: Duration::from_millis(200),
            cooldown: Duration::from_millis(3_000),
            slo: Slo {
                floor,
                over_floor: Duration::from_millis(250),
            },
            max_bad_rate: 0.0,
            lateness_limit: Duration::from_millis(100),
            late_share_limit: DEFAULT_LATE_SHARE,
            target_pid: None,
            target_scope: Scope::Tree,
            sample_every: Duration::from_millis(500),
            client: ClientConfig::new(""),
            engine: None,
            on_step: None,
        }
    }

    fn validate(&self) -> Result<Vec<usize>, BenchError> {
        let bad = |what: &str| Err(BenchError::Config(what.to_owned()));
        let mut steps = self.steps.clone();
        steps.sort_unstable();
        steps.dedup();
        if steps.is_empty() || steps.first() == Some(&0) {
            return bad("capacity: steps must be a non-empty list of numbers of at least 1");
        }
        if self.rounds == 0 {
            return bad("capacity: rounds must be at least 1");
        }
        if !(0.0..=1.0).contains(&self.max_bad_rate) {
            return bad("capacity: max bad rate must be between 0 and 1");
        }
        if !(0.0..=1.0).contains(&self.late_share_limit) {
            return bad("capacity: late share limit must be between 0 and 1");
        }
        if self.url.expose().is_empty() {
            return bad("capacity: no server URL");
        }
        Ok(steps)
    }

    fn settings(&self, steps: Vec<usize>, turns_per_call: usize) -> Settings {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Settings {
            fixture: self.fixture.clone(),
            turns_per_call,
            steps,
            rounds: self.rounds,
            warmup_calls: self.warmup_calls,
            spread_ms: ms(self.spread),
            min_gap_ms: ms(self.min_gap),
            pause_ms: ms(self.pause),
            cooldown_ms: ms(self.cooldown),
            floor_ms: ms(self.slo.floor),
            slo_over_floor_ms: ms(self.slo.over_floor),
            slo_limit_ms: ms(self.slo.limit()),
            max_bad_rate: self.max_bad_rate,
            lateness_limit_ms: ms(self.lateness_limit),
            late_share_limit: self.late_share_limit,
            resolution: self.resolution,
            keep_going: self.keep_going,
            target_pid: self.target_pid,
            target_tree: self.target_scope == Scope::Tree,
        }
    }
}

/// What one call gave a step.
#[derive(Debug, Clone, Default)]
pub struct CallResult {
    /// Why the call is not valid (could not connect, refused, broke, the server sent an error).
    pub failure: Option<String>,
    /// The turns of a valid call.
    pub turns: Vec<TurnResult>,
    /// The largest send lateness of the call.
    pub max_send_lateness: Duration,
    /// Fixture frames the call sent (the frames whose lateness is counted).
    pub fixture_frames: u64,
    /// Of those, the frames sent later than the lateness limit.
    pub late_frames: u64,
}

/// The rules a step is judged by.
#[derive(Debug, Clone, Copy)]
pub struct Judge {
    /// The latency objective.
    pub slo: Slo,
    /// See [`CapacityConfig::max_bad_rate`].
    pub max_bad_rate: f64,
    /// See [`CapacityConfig::lateness_limit`]; the client counts the frames beyond it.
    pub lateness_limit: Duration,
    /// See [`CapacityConfig::late_share_limit`].
    pub late_share_limit: f64,
}

/// The start offset of slot `index` of `slots`: `spread x index / slots`.
pub fn stagger(spread: Duration, index: usize, slots: usize) -> Duration {
    if slots == 0 {
        return Duration::ZERO;
    }
    let nanos = spread
        .as_nanos()
        .saturating_mul(u128::try_from(index).unwrap_or(0))
        / u128::try_from(slots).unwrap_or(1);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Pools the calls of one step into its report (without CPU and memory) and judges it.
pub fn summarize(
    concurrent: usize,
    turns_per_call: usize,
    calls: &[CallResult],
    judge: &Judge,
) -> StepReport {
    let calls_failed = calls.iter().filter(|c| c.failure.is_some()).count();
    let mut failures: Vec<String> = Vec::new();
    for message in calls.iter().filter_map(|c| c.failure.as_ref()) {
        if failures.len() < MAX_FAILURE_NOTES && !failures.contains(message) {
            failures.push(message.clone());
        }
    }
    let (mut answered, mut talked_over, mut bot_speaking, mut no_response) = (0, 0, 0, 0);
    let mut v2v: Vec<Duration> = Vec::new();
    for call in calls.iter().filter(|c| c.failure.is_none()) {
        for turn in &call.turns {
            match turn.outcome {
                Outcome::Answered => {
                    answered += 1;
                    v2v.extend(turn.v2v);
                }
                Outcome::TalkedOver => talked_over += 1,
                Outcome::BotSpeaking => bot_speaking += 1,
                Outcome::NoResponse => no_response += 1,
            }
        }
    }
    let turns_expected = calls.len().saturating_mul(turns_per_call);
    let bad = turns_expected.saturating_sub(answered);
    #[allow(clippy::cast_precision_loss)] // turn counts of a benchmark
    let bad_rate = if turns_expected == 0 {
        0.0
    } else {
        bad as f64 / turns_expected as f64
    };
    let max_send_lateness = calls
        .iter()
        .map(|c| c.max_send_lateness)
        .max()
        .unwrap_or_default();
    let fixture_frames: u64 = calls.iter().map(|c| c.fixture_frames).sum();
    let late_frames: u64 = calls.iter().map(|c| c.late_frames).sum();
    #[allow(clippy::cast_precision_loss)] // frame counts of a benchmark
    let late_share = if fixture_frames == 0 {
        0.0
    } else {
        late_frames as f64 / fixture_frames as f64
    };
    let v2v = Dist::of(v2v);
    let us = |d: Duration| i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
    let p95_over_floor_us = v2v.as_ref().map(|d| {
        i64::try_from(d.p95_us)
            .unwrap_or(i64::MAX)
            .saturating_sub(us(judge.slo.floor))
    });

    let mut reasons: Vec<String> = Vec::new();
    let mut verdict = Verdict::Pass;
    let mut fail = |reason: String| {
        if verdict == Verdict::Pass {
            verdict = Verdict::Fail;
        }
        reasons.push(reason);
    };
    if calls.is_empty() {
        fail("no call was played".to_owned());
    }
    if calls_failed > 0 {
        fail(format!("{calls_failed} of {} calls failed", calls.len()));
    }
    if bad_rate > judge.max_bad_rate {
        fail(format!(
            "{bad} of {turns_expected} turns lost or not clean ({} talked over, {} no response, \
             {} bot speaking; allowed {:.1} %)",
            talked_over,
            no_response,
            bot_speaking,
            judge.max_bad_rate * 100.0
        ));
    }
    match &v2v {
        None if !calls.is_empty() => fail("no turn was answered".to_owned()),
        Some(d) if d.p95_us > u64::try_from(judge.slo.limit().as_micros()).unwrap_or(u64::MAX) => {
            fail(format!(
                "p95 V2V {} ms is above the limit {} ms (floor {} ms + {} ms)",
                d.p95_us / 1_000,
                judge.slo.limit().as_millis(),
                judge.slo.floor.as_millis(),
                judge.slo.over_floor.as_millis()
            ));
        }
        _ => {}
    }
    // A sustained share of late frames, not one outlier, makes the generator the suspect. With
    // a healthy server the step is generator limited; with failures on top the step stays a fail
    // (a stalled server backs up the generator's writes and shows as lateness too) and the
    // reasons say both.
    if late_share > judge.late_share_limit {
        let note = format!(
            "{late_frames} of {fixture_frames} frames ({:.1} %) went out more than {} ms late              (limit {:.1} %, latest {} ms)",
            late_share * 100.0,
            judge.lateness_limit.as_millis(),
            judge.late_share_limit * 100.0,
            max_send_lateness.as_millis(),
        );
        if verdict == Verdict::Pass {
            verdict = Verdict::GeneratorLimited;
            reasons.push(format!("{note}: the load generator is the limit"));
        } else {
            reasons.push(format!(
                "{note}: the generator may have contributed, check its CPU column"
            ));
        }
    }
    StepReport {
        concurrent,
        calls: calls.len(),
        calls_failed,
        failures,
        turns_expected,
        turns_answered: answered,
        talked_over,
        bot_speaking,
        no_response,
        bad_rate,
        v2v,
        p95_over_floor_us,
        max_send_lateness_us: u64::try_from(max_send_lateness.as_micros()).unwrap_or(u64::MAX),
        late_frames,
        timed_frames: fixture_frames,
        wall_ms: 0,
        rss_before_bytes: None,
        pss_before_bytes: None,
        target: None,
        client: None,
        verdict,
        reasons,
    }
}

/// The order in which the ramp visits concurrencies: the steps asked for, then bisection.
#[derive(Debug)]
pub struct Ramp {
    pending: VecDeque<usize>,
    results: Vec<(usize, Verdict)>,
    resolution: usize,
    keep_going: bool,
}

impl Ramp {
    /// A ramp over `steps` (ascending).
    pub fn new(steps: &[usize], resolution: usize, keep_going: bool) -> Self {
        Self {
            pending: steps.iter().copied().collect(),
            results: Vec::new(),
            resolution,
            keep_going,
        }
    }

    /// The next concurrency to play, or `None` when the ramp is done.
    pub fn next_step(&mut self) -> Option<usize> {
        if let Some(n) = self.pending.pop_front() {
            return Some(n);
        }
        if self.resolution == 0 {
            return None;
        }
        let stop = self
            .results
            .iter()
            .filter(|(_, v)| *v != Verdict::Pass)
            .map(|(n, _)| *n)
            .min()?;
        let pass = self
            .results
            .iter()
            .filter(|(n, v)| *v == Verdict::Pass && *n < stop)
            .map(|(n, _)| *n)
            .max()?;
        if stop.saturating_sub(pass) <= self.resolution {
            return None;
        }
        let mid = pass + (stop - pass) / 2;
        (mid > pass && mid < stop && !self.results.iter().any(|(n, _)| *n == mid)).then_some(mid)
    }

    /// Records how step `n` ended; a step that did not pass drops the steps still queued unless
    /// the ramp keeps going.
    pub fn record(&mut self, n: usize, verdict: Verdict) {
        self.results.push((n, verdict));
        if verdict != Verdict::Pass && !self.keep_going {
            self.pending.clear();
        }
    }
}

/// Plays the ramp against `config.url` and reports it.
///
/// A step that fails is a result, not an error.
///
/// # Errors
/// [`BenchError::Config`] for an unusable configuration (no steps, zero rounds, a missing
/// fixture); [`BenchError::Call`] if the run is cancelled.
pub async fn run_capacity(
    clock: SharedClock,
    config: &CapacityConfig,
    cancel: &CancellationToken,
) -> Result<CapacityReport, BenchError> {
    let steps = config.validate()?;
    let loaded = Fixture::load(&config.fixtures_dir, &config.fixture)
        .map_err(|e| BenchError::Config(format!("fixture {}: {e}", config.fixture)))?;
    if loaded.segments.is_empty() {
        return Err(BenchError::Config(format!(
            "fixture {} has no speech segments",
            config.fixture
        )));
    }
    let fixture = Arc::new(pad_gaps(&loaded, config.min_gap));
    let turns_per_call = fixture.segments.len();
    let judge = Judge {
        slo: config.slo,
        max_bad_rate: config.max_bad_rate,
        lateness_limit: config.lateness_limit,
        late_share_limit: config.late_share_limit,
    };
    let mut ramp = Ramp::new(&steps, config.resolution, config.keep_going);
    let mut reports: Vec<StepReport> = Vec::new();
    // One slot, played alone: a warm-up is not a load step.
    let warmup = if config.warmup_calls > 0 {
        let report = run_step(
            &clock,
            config,
            &fixture,
            1,
            config.warmup_calls,
            &judge,
            cancel,
        )
        .await?;
        if let Some(on_step) = config.on_step {
            on_step(&report, true);
        }
        Some(report)
    } else {
        None
    };
    while let Some(n) = ramp.next_step() {
        if !reports.is_empty() || warmup.is_some() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = wait_for(&*clock, config.cooldown) => {}
            }
        }
        if cancel.is_cancelled() {
            return Err(BenchError::Call("cancelled".to_owned()));
        }
        let report = run_step(&clock, config, &fixture, n, config.rounds, &judge, cancel).await?;
        if let Some(on_step) = config.on_step {
            on_step(&report, false);
        }
        ramp.record(n, report.verdict);
        reports.push(report);
    }
    reports.sort_by_key(|r| r.concurrent);
    let conclusion = conclude(&reports);
    Ok(CapacityReport {
        schema: SCHEMA.to_owned(),
        label: config.label.clone(),
        note: config.note.clone(),
        url: redact_url(config.url.expose()),
        environment: Some(RunEnvironment::current()),
        settings: config.settings(steps, turns_per_call),
        engine: config.engine.clone(),
        warmup,
        steps: reports,
        conclusion,
    })
}

/// One reading of the target, taken on a blocking thread.
async fn read_once(pid: u32, scope: Scope) -> Option<probe::Reading> {
    tokio::task::spawn_blocking(move || TreeProbe::new(pid, scope).read())
        .await
        .ok()
        .flatten()
}

/// Stops a sampler (joined on a blocking thread).
async fn finish(sampler: Option<TreeSampler>) -> Option<Samples> {
    let sampler = sampler?;
    tokio::task::spawn_blocking(move || sampler.finish())
        .await
        .ok()
        .flatten()
}

fn usage(
    samples: Option<&Samples>,
    from: tokio::time::Instant,
    to: tokio::time::Instant,
) -> Option<Usage> {
    let samples = samples?;
    Some(Usage {
        cpu_cores: samples.cpu_cores_between(from, to),
        rss_peak_bytes: samples
            .rss_peak_between(from, to)
            .unwrap_or(samples.rss_peak),
        pss_peak_bytes: samples.pss_peak_between(from, to).or(samples.pss_peak),
        processes: samples
            .readings
            .iter()
            .filter(|(t, _)| *t >= from && *t <= to)
            .map(|(_, r)| r.processes)
            .max()
            .unwrap_or(0),
        readings: samples.count,
    })
}

/// Plays one step: `n` slots, each `rounds` calls.
async fn run_step(
    clock: &SharedClock,
    config: &CapacityConfig,
    fixture: &Arc<Fixture>,
    n: usize,
    rounds: usize,
    judge: &Judge,
    cancel: &CancellationToken,
) -> Result<StepReport, BenchError> {
    let before = match config.target_pid {
        Some(pid) => read_once(pid, config.target_scope).await,
        None => None,
    };
    let (rss_before, pss_before) = (
        before.map(|r| r.rss_bytes),
        before.and_then(|r| r.pss_bytes),
    );
    let sampler = |probe: Option<TreeProbe>| {
        probe.and_then(|p| TreeSampler::start(p, clock.clone(), config.sample_every).ok())
    };
    let target_sampler = sampler(
        config
            .target_pid
            .map(|pid| TreeProbe::new(pid, config.target_scope)),
    );
    let client_sampler = sampler(TreeProbe::current());

    let mut client = config.client.clone();
    client.url = config.url.clone();
    client.late_threshold = config.lateness_limit;
    let started = clock.now();
    let mut set = tokio::task::JoinSet::new();
    for slot in 0..n {
        let (clock, client, fixture, cancel) = (
            clock.clone(),
            client.clone(),
            Arc::clone(fixture),
            cancel.clone(),
        );
        let (offset, pause) = (stagger(config.spread, slot, n), config.pause);
        set.spawn(
            async move { play_slot(clock, client, fixture, cancel, offset, rounds, pause).await },
        );
    }
    let mut calls: Vec<CallResult> = Vec::with_capacity(n.saturating_mul(rounds));
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(mut slot_calls) => calls.append(&mut slot_calls),
            Err(e) => calls.push(CallResult {
                failure: Some(format!("slot task: {e}")),
                ..CallResult::default()
            }),
        }
    }
    let ended = clock.now();
    let target_samples = finish(target_sampler).await;
    let client_samples = finish(client_sampler).await;
    if cancel.is_cancelled() {
        return Err(BenchError::Call("cancelled".to_owned()));
    }
    let mut report = summarize(n, fixture.segments.len(), &calls, judge);
    report.wall_ms =
        u64::try_from(ended.saturating_duration_since(started).as_millis()).unwrap_or(u64::MAX);
    report.rss_before_bytes = rss_before;
    report.pss_before_bytes = pss_before;
    report.target = usage(target_samples.as_ref(), started, ended);
    report.client = usage(client_samples.as_ref(), started, ended);
    Ok(report)
}

/// One slot: waits `offset`, then plays `rounds` calls one after another.
async fn play_slot(
    clock: SharedClock,
    client: ClientConfig,
    fixture: Arc<Fixture>,
    cancel: CancellationToken,
    offset: Duration,
    rounds: usize,
    pause: Duration,
) -> Vec<CallResult> {
    let mut calls = Vec::with_capacity(rounds);
    tokio::select! {
        biased;
        () = cancel.cancelled() => return calls,
        () = wait_for(&*clock, offset) => {}
    }
    for round in 0..rounds {
        if cancel.is_cancelled() {
            break;
        }
        calls.push(
            match run_call(clock.clone(), &client, &fixture, &cancel).await {
                Ok(outcome) => {
                    let mut failure = outcome.failure;
                    if failure.is_none() && !outcome.server_errors.is_empty() {
                        failure = Some(format!(
                            "server error: {}",
                            outcome.server_errors.join("; ")
                        ));
                    }
                    CallResult {
                        failure,
                        turns: outcome.turns,
                        max_send_lateness: outcome.max_send_lateness,
                        fixture_frames: outcome.fixture_frames_sent,
                        late_frames: outcome.late_frames,
                    }
                }
                Err(error) => CallResult {
                    failure: Some(error.to_string()),
                    ..CallResult::default()
                },
            },
        );
        if round + 1 < rounds {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = wait_for(&*clock, pause) => {}
            }
        }
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn judge() -> Judge {
        Judge {
            slo: Slo {
                floor: 950 * MS,
                over_floor: 250 * MS,
            },
            max_bad_rate: 0.0,
            lateness_limit: 100 * MS,
            late_share_limit: 0.01,
        }
    }

    fn turn(index: usize, outcome: Outcome, v2v_ms: Option<u64>) -> TurnResult {
        TurnResult {
            index,
            lang: "en".to_owned(),
            eos: Duration::from_secs(7),
            outcome,
            v2v: v2v_ms.map(Duration::from_millis),
        }
    }

    fn call(v2v_ms: &[u64]) -> CallResult {
        CallResult {
            failure: None,
            turns: v2v_ms
                .iter()
                .enumerate()
                .map(|(i, ms)| turn(i, Outcome::Answered, Some(*ms)))
                .collect(),
            max_send_lateness: 5 * MS,
            fixture_frames: 800,
            late_frames: 0,
        }
    }

    fn failed(message: &str) -> CallResult {
        CallResult {
            failure: Some(message.to_owned()),
            ..CallResult::default()
        }
    }

    #[test]
    fn a_clean_step_within_the_slo_passes() {
        let calls = vec![call(&[1_000, 1_050]), call(&[1_100, 1_190])];
        let step = summarize(2, 2, &calls, &judge());
        assert_eq!(step.verdict, Verdict::Pass, "{:?}", step.reasons);
        assert_eq!(
            (step.calls, step.turns_expected, step.turns_answered),
            (2, 4, 4)
        );
        let v2v = step.v2v.unwrap();
        assert_eq!(
            (v2v.count, v2v.p50_us, v2v.p95_us),
            (4, 1_050_000, 1_190_000)
        );
        // p95 minus the floor.
        assert_eq!(step.p95_over_floor_us, Some(240_000));
        assert!(step.reasons.is_empty());
    }

    #[test]
    fn p95_above_the_limit_fails_and_the_limit_is_inclusive() {
        // The limit is 1200 ms: 1200 passes, 1201 fails.
        let at = summarize(1, 2, &[call(&[1_000, 1_200])], &judge());
        assert_eq!(at.verdict, Verdict::Pass, "{:?}", at.reasons);
        let over = summarize(1, 2, &[call(&[1_000, 1_201])], &judge());
        assert_eq!(over.verdict, Verdict::Fail);
        assert!(
            over.reasons[0].contains("p95 V2V 1201 ms is above the limit 1200 ms"),
            "{:?}",
            over.reasons
        );
    }

    #[test]
    fn a_failed_call_fails_the_step_and_its_turns_count_as_lost() {
        let calls = vec![
            call(&[1_000, 1_000]),
            failed("connect timed out"),
            failed("connect timed out"),
        ];
        let step = summarize(3, 2, &calls, &judge());
        assert_eq!(step.verdict, Verdict::Fail);
        assert_eq!(
            (step.calls_failed, step.turns_expected, step.turns_answered),
            (2, 6, 2)
        );
        assert!((step.bad_rate - 4.0 / 6.0).abs() < 1e-12);
        assert_eq!(step.failures, vec!["connect timed out".to_owned()]);
        assert!(
            step.reasons
                .iter()
                .any(|r| r.contains("2 of 3 calls failed")),
            "{:?}",
            step.reasons
        );
    }

    #[test]
    fn lost_turns_fail_unless_the_rate_is_allowed() {
        let mut slow = call(&[1_000, 0]);
        slow.turns[1] = turn(1, Outcome::TalkedOver, Some(4_500));
        let mut silent = call(&[1_000, 0]);
        silent.turns[1] = turn(1, Outcome::NoResponse, None);
        let strict = summarize(2, 2, &[slow.clone(), silent.clone()], &judge());
        assert_eq!(strict.verdict, Verdict::Fail);
        assert_eq!(
            (
                strict.talked_over,
                strict.no_response,
                strict.turns_answered
            ),
            (1, 1, 2)
        );
        // A talked-over turn's V2V never enters the percentiles.
        assert_eq!(strict.v2v.unwrap().max_us, 1_000_000);
        let lenient = Judge {
            max_bad_rate: 0.5,
            ..judge()
        };
        assert_eq!(
            summarize(2, 2, &[slow, silent], &lenient).verdict,
            Verdict::Pass
        );
    }

    /// A call of 800 fixture frames of which `late` went out late, the latest by `max` ms.
    fn late_call(v2v_ms: &[u64], late: u64, max: u64) -> CallResult {
        CallResult {
            max_send_lateness: Duration::from_millis(max),
            late_frames: late,
            ..call(v2v_ms)
        }
    }

    #[test]
    fn a_sustained_share_of_late_frames_makes_a_healthy_step_generator_limited() {
        // 20 of 800 frames (2.5 %) late: over the 1 % limit.
        let step = summarize(40, 2, &[late_call(&[1_000, 1_000], 20, 250)], &judge());
        assert_eq!(step.verdict, Verdict::GeneratorLimited);
        assert!(step.reasons[0].contains("generator"), "{:?}", step.reasons);
        assert!(
            step.reasons[0].contains("20 of 800 frames"),
            "{:?}",
            step.reasons
        );
        assert_eq!((step.late_frames, step.timed_frames), (20, 800));
    }

    #[test]
    fn one_late_frame_is_not_a_verdict() {
        // A single 2 s hiccup among 8000 frames of ten calls: the step is judged on the server.
        let mut calls: Vec<CallResult> = (0..9).map(|_| call(&[1_000, 1_000])).collect();
        calls.push(late_call(&[1_000, 1_000], 1, 2_000));
        let step = summarize(10, 2, &calls, &judge());
        assert_eq!(step.verdict, Verdict::Pass, "{:?}", step.reasons);
        assert_eq!(step.max_send_lateness_us, 2_000_000);
        assert_eq!(step.late_frames, 1);
        // The same outlier on a failing server is a failure, not a lower bound.
        calls.push(failed("connect timed out"));
        let failing = summarize(11, 2, &calls, &judge());
        assert_eq!(failing.verdict, Verdict::Fail);
    }

    #[test]
    fn late_frames_do_not_hide_a_server_failure() {
        // Sustained lateness and 2 of 3 calls failed, p95 over the limit: still a fail, and the
        // reasons name the lateness too.
        let calls = vec![
            late_call(&[2_000, 2_000], 40, 400),
            failed("a write timed out"),
            failed("a write timed out"),
        ];
        let step = summarize(40, 2, &calls, &judge());
        assert_eq!(step.verdict, Verdict::Fail, "{:?}", step.reasons);
        assert!(
            step.reasons.iter().any(|r| r.contains("calls failed")),
            "{:?}",
            step.reasons
        );
        assert!(
            step.reasons.iter().any(|r| r.contains("frames (")),
            "{:?}",
            step.reasons
        );
        // p95 over the SLO alone with late frames is a fail as well.
        let slow = summarize(40, 2, &[late_call(&[2_000, 2_000], 40, 400)], &judge());
        assert_eq!(slow.verdict, Verdict::Fail);
        assert!(slow.reasons.len() >= 2, "{:?}", slow.reasons);
    }

    #[test]
    fn no_answered_turn_and_no_call_both_fail() {
        let none = summarize(1, 2, &[], &judge());
        assert_eq!(none.verdict, Verdict::Fail);
        let mut silent = call(&[0, 0]);
        for t in &mut silent.turns {
            t.outcome = Outcome::NoResponse;
            t.v2v = None;
        }
        let step = summarize(1, 2, &[silent], &judge());
        assert_eq!(step.verdict, Verdict::Fail);
        assert!(
            step.reasons
                .iter()
                .any(|r| r.contains("no turn was answered")),
            "{:?}",
            step.reasons
        );
    }

    #[test]
    fn failure_notes_are_distinct_and_bounded() {
        let calls: Vec<CallResult> = (0..20)
            .map(|i| failed(&format!("error {}", i % 8)))
            .collect();
        let step = summarize(20, 2, &calls, &judge());
        assert_eq!(step.failures.len(), MAX_FAILURE_NOTES);
        let mut sorted = step.failures.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), MAX_FAILURE_NOTES);
    }

    #[test]
    fn slots_start_evenly_across_the_spread() {
        let offsets: Vec<u128> = (0..4)
            .map(|i| stagger(4_000 * MS, i, 4).as_millis())
            .collect();
        assert_eq!(offsets, vec![0, 1_000, 2_000, 3_000]);
        assert_eq!(stagger(4_000 * MS, 0, 1), Duration::ZERO);
        assert_eq!(stagger(4_000 * MS, 3, 0), Duration::ZERO);
    }

    #[test]
    fn the_ramp_stops_at_the_first_stop_and_bisects_to_the_resolution() {
        let mut ramp = Ramp::new(&[1, 5, 10, 20], 0, false);
        assert_eq!(ramp.next_step(), Some(1));
        ramp.record(1, Verdict::Pass);
        assert_eq!(ramp.next_step(), Some(5));
        ramp.record(5, Verdict::Pass);
        assert_eq!(ramp.next_step(), Some(10));
        ramp.record(10, Verdict::Fail);
        // 20 is dropped, and without a resolution there is nothing more.
        assert_eq!(ramp.next_step(), None);

        // With a resolution: bisect 5..10 -> 7, then 5..7 -> 6 (resolution 1), then done.
        let mut ramp = Ramp::new(&[1, 5, 10, 20], 1, false);
        for (n, v) in [(1, Verdict::Pass), (5, Verdict::Pass), (10, Verdict::Fail)] {
            assert_eq!(ramp.next_step(), Some(n));
            ramp.record(n, v);
        }
        assert_eq!(ramp.next_step(), Some(7));
        ramp.record(7, Verdict::Fail);
        assert_eq!(ramp.next_step(), Some(6));
        ramp.record(6, Verdict::Pass);
        assert_eq!(ramp.next_step(), None);

        // A coarser resolution stops earlier: 5..10 is within 5.
        let mut ramp = Ramp::new(&[5, 10], 5, false);
        ramp.next_step();
        ramp.record(5, Verdict::Pass);
        ramp.next_step();
        ramp.record(10, Verdict::Fail);
        assert_eq!(ramp.next_step(), None);
    }

    #[test]
    fn keep_going_plays_every_step_and_a_first_step_failure_has_nothing_to_bisect() {
        let mut ramp = Ramp::new(&[1, 5, 10], 0, true);
        for n in [1, 5, 10] {
            assert_eq!(ramp.next_step(), Some(n));
            ramp.record(n, if n == 5 { Verdict::Fail } else { Verdict::Pass });
        }
        assert_eq!(ramp.next_step(), None);

        let mut ramp = Ramp::new(&[1, 5], 1, false);
        assert_eq!(ramp.next_step(), Some(1));
        ramp.record(1, Verdict::Fail);
        assert_eq!(ramp.next_step(), None);
    }

    #[test]
    fn the_configuration_is_validated() {
        let mut config = CapacityConfig::new("ws://127.0.0.1:1/", 950 * MS);
        assert_eq!(config.validate().unwrap(), vec![1, 5, 10, 20]);
        config.steps = vec![10, 1, 10, 5];
        assert_eq!(config.validate().unwrap(), vec![1, 5, 10]);
        for bad in [
            |c: &mut CapacityConfig| c.steps = vec![],
            |c: &mut CapacityConfig| c.steps = vec![0, 1],
            |c: &mut CapacityConfig| c.rounds = 0,
            |c: &mut CapacityConfig| c.max_bad_rate = 1.5,
            |c: &mut CapacityConfig| c.url = Secret::new(String::new()),
        ] {
            let mut config = CapacityConfig::new("ws://127.0.0.1:1/", 950 * MS);
            bad(&mut config);
            assert!(matches!(config.validate(), Err(BenchError::Config(_))));
        }
    }
}
