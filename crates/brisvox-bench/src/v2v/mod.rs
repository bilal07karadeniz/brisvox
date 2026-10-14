//! Voice-to-voice latency benchmark over the websocket protocol.
//!
//! The harness is a *client*: it connects to any server that speaks `docs/WEBSOCKET_PROTOCOL.md`
//! (this engine, the Pipecat baseline behind an adapter, a mock), streams a recorded fixture in
//! real time and measures, for every utterance in it, the time from the ground-truth end of the
//! user's speech (fixture sidecar) to the arrival of the bot's first non-silent audio at
//! the client. That is the V2V definition of `docs/LATENCY.md`. Because it only sees a URL, the
//! same measurement applies to every system under test.
//!
//! - [`client`]: one call: paced streaming, arrival stamps, marks and clears like a phone.
//! - [`measure`]: the pure part: arrivals and ground truth to per-turn results.
//! - [`report`]: p50/p90/p99 over N runs, JSON and markdown.
//! - [`engine_target`]: the sim-mode target, the engine (the production pipeline)
//!   over the `brisvox_sim` mock providers, served in process behind the real websocket
//!   transport.
//! - [`stages`]: the engine's own per-stage times (its turn timeline) and memory per call, next
//!   to the V2V (sim mode against the engine).
//! - [`mock_bot`]: a scripted bot behind the same transport, kept as a self-check of the harness
//!   and the transport (`--target mock-bot`).
//! - [`run_bench`] / [`run_blocking`]: the orchestration the `brisvox-bench v2v` command uses.
//!
//! # Modes
//! - **Sim, engine** ([`Target::Engine`], the default of `--sim`): starts the engine server
//!   ([`engine_target::EngineServer`]) in process and measures against it. Every accepted call
//!   runs [`brisvox_engine::run_call`] with a real inference pool; only the providers are mocks,
//!   with fixed latencies, so every turn has a known floor (end of turn plus provider time,
//!   [`engine_target::EngineTargetSettings::floor_ms`]) and the remainder is the engine's own
//!   time plus the transport. Before each call the harness tells the server which fixture the
//!   call plays (the mock STT needs its transcripts); after it, the harness waits until the
//!   server has torn the call down ([`ENGINE_IDLE_TIMEOUT`]), and errors the engine recorded
//!   fail that call in the report.
//! - **Sim, mock bot** ([`Target::MockBot`]): the in-process mock bot. Its delays are known and
//!   it does nothing else, so the result is the harness plus the transport against its floor (see
//!   [`mock_bot`]).
//! - **Live** ([`Target::Live`]): the same client against a URL. The harness holds no provider
//!   keys and sends no credentials; whatever the server needs to authenticate a client must be in
//!   the URL it is given, which then never appears in a report, error or `Debug` output (only
//!   scheme, host and port are shown, see [`report::redact_url`]).
//!
//! # Runs
//! Each run plays every fixture once, one call at a time (the harness measures latency, not
//! capacity: concurrent calls are the capacity benchmark's job; the one exception is live mode with
//! [`BenchConfig::concurrency`] above 1, which plays that many calls of the plan at once, group
//! by group, to see a real provider under a few simultaneous calls). Runs interleave the fixtures, so slow drift of the
//! server affects all of them alike. Fixtures are padded to a minimum gap between utterances
//! ([`BenchConfig::min_gap`], the same [`pad_gaps`] as the
//! simulated load runs: the recordings leave less silence than a bot needs to answer, and a user
//! who talks over every answer would measure barge-ins, not latency). A pause between calls lets
//! the server finish tearing down the previous one. After [`MAX_CONSECUTIVE_FAILURES`] failed calls in
//! a row the benchmark stops: the server is down.
//!
//! # Barge-in calls and engine stages (sim mode against the engine)
//! After the latency runs, [`BenchConfig::barge_in_runs`] more runs play every fixture with the
//! gaps set to exactly [`BenchConfig::barge_in_gap`] ([`set_gaps`]) while the engine answers at
//! length ([`EngineServer::expect_barge_in`]): the caller talks over the bot, the engine
//! confirms a barge-in and clears the transport. These calls are reported (`barge_in`) but never
//! enter a V2V aggregate; they give the `interrupt_to_clear` stage. Last,
//! [`BenchConfig::memory_calls`] calls of the first fixture are played **at once**
//! ([`EngineServer::expect_memory_calls`]): one call at a time would reuse what the previous call
//! freed, so only calls alive together show a call's footprint. The report's `engine_stages`
//! ([`stages::StageReport`]) holds the engine's per-stage times of the latency and barge-in calls
//! and the process's resident memory: read with the server cold, read again idle and warm right
//! before the memory calls, and sampled every 50 ms while they run (readings are system calls:
//! a thread of their own or a blocking thread, never a tokio worker). RSS per call is the peak
//! over the warm idle reading, divided by the calls alive at once.

pub mod client;
pub mod engine_target;
pub mod measure;
pub mod mock_bot;
pub mod real_models;
pub mod report;
pub mod stages;

use std::path::PathBuf;
use std::time::Duration;

use brisvox_core::clock::wait_for;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use tokio_util::sync::CancellationToken;

use crate::BenchError;
use crate::fixture_load::{FIXTURES, pad_gaps, set_gaps};
use crate::resources::{ProcessProbe, Sampler};
use client::{ClientConfig, run_call};
use engine_target::{EngineServer, EngineTargetConfig, MAX_PENDING_SCRIPTS};
use mock_bot::{MockBotConfig, MockBotServer};
use report::{BenchReport, CallKind, Mode, RunReport, ServerSettings, Settings, redact_url};
use stages::RssReport;

/// Calls that fail in a row before the benchmark gives up.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// How long the harness waits after a call for the engine server to have torn it down.
pub const ENGINE_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Resident memory sampling period while the calls run.
const RSS_EVERY: Duration = Duration::from_millis(50);

/// What to measure.
#[derive(Debug, Clone)]
pub enum Target {
    /// The engine over mock providers, in process (sim mode).
    Engine(EngineTargetConfig),
    /// The in-process mock bot (sim mode, a self-check of the harness and the transport).
    MockBot(MockBotConfig),
    /// A server at this URL (a secret: see the module docs).
    Live(Secret<String>),
}

/// What to run.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// The server under test.
    pub target: Target,
    /// Fixture names (files in [`fixtures_dir`](Self::fixtures_dir)).
    pub fixtures: Vec<String>,
    /// Where the fixtures are.
    pub fixtures_dir: PathBuf,
    /// Runs: calls per fixture. At least 1.
    pub runs: usize,
    /// Minimum silence between two utterances (see the module docs).
    pub min_gap: Duration,
    /// Pause after each call.
    pub pause: Duration,
    /// Calls played at once (live mode only; 1 = one call at a time, the default). The plan's
    /// calls are taken in groups of this size; a group ends when all its calls have ended, then
    /// [`Self::pause`]. The latency of each call is measured as in a single call; the server
    /// sees the concurrent load.
    pub concurrency: usize,
    /// Runs of barge-in calls after the latency runs (sim mode against the engine only, see the
    /// module docs); `None`: as many as [`Self::runs`].
    pub barge_in_runs: Option<usize>,
    /// Silence between two utterances in a barge-in call: the next utterance starts this long
    /// after the previous one ended, while the bot's long answer plays.
    pub barge_in_gap: Duration,
    /// Calls played at once at the end to measure memory per call (sim mode against the engine
    /// only, see the module docs); 0: none; at most [`MAX_PENDING_SCRIPTS`]. The engine server
    /// takes at least that many calls.
    pub memory_calls: usize,
    /// Client settings; its `url` is ignored (the target decides).
    pub client: ClientConfig,
    /// Called after every call, failed ones included, in order (progress output).
    pub on_call: Option<fn(&RunReport)>,
}

impl BenchConfig {
    fn with_target(target: Target) -> Self {
        Self {
            target,
            fixtures: FIXTURES.iter().map(|n| (*n).to_owned()).collect(),
            fixtures_dir: fixtures_dir(),
            runs: 10,
            min_gap: Duration::from_millis(4_000),
            pause: Duration::from_millis(300),
            concurrency: 1,
            barge_in_runs: None,
            barge_in_gap: Duration::from_millis(1_600),
            memory_calls: 8,
            client: ClientConfig::new(""),
            on_call: None,
        }
    }

    /// Sim mode against the engine with the default mock providers, all committed fixtures,
    /// 10 runs.
    pub fn sim() -> Self {
        Self::with_target(Target::Engine(EngineTargetConfig::default()))
    }

    /// Sim mode against the default mock bot, all committed fixtures, 10 runs.
    pub fn mock_bot() -> Self {
        Self::with_target(Target::MockBot(MockBotConfig::default()))
    }

    /// Live mode against `url`, all committed fixtures, 10 runs.
    pub fn live(url: impl Into<String>) -> Self {
        Self::with_target(Target::Live(Secret::new(url.into())))
    }

    /// The [`Settings`] block of the report.
    fn settings(&self) -> Settings {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Settings {
            runs: self.runs,
            concurrency: self.concurrency,
            fixtures: self.fixtures.clone(),
            min_gap_ms: ms(self.min_gap),
            frame_ms: ms(self.client.frame),
            silence_peak: self.client.silence_peak,
            overlap_guard_ms: ms(self.client.overlap_guard),
            response_timeout_ms: ms(self.client.response_timeout),
        }
    }
}

/// The in-process server of sim mode.
enum SimServer {
    Engine(EngineServer),
    MockBot(MockBotServer),
}

impl SimServer {
    async fn shutdown(&self) -> Result<(), BenchError> {
        let timeout = Duration::from_secs(5);
        match self {
            Self::Engine(server) => server.shutdown(timeout).await,
            Self::MockBot(server) => server.shutdown(timeout).await,
        }
    }
}

/// After a call against the engine: waits until the server has torn it down and fails the call
/// in `report` with what the engine recorded, if anything.
async fn settle_engine_call(server: &EngineServer, report: &mut RunReport) {
    let idle = server.wait_idle(ENGINE_IDLE_TIMEOUT).await;
    let mut errors = server.take_errors();
    if !idle {
        errors.push(format!(
            "the engine did not end the call within {} s",
            ENGINE_IDLE_TIMEOUT.as_secs()
        ));
    }
    report.failure = merge_failure(report.failure.take(), &errors);
}

/// The failure of a call against the engine: the client's own failure, if any, followed by what
/// the engine recorded. A client-side failure (a broken connection, say) is often only the symptom
/// of an engine error, so neither may hide the other.
fn merge_failure(client: Option<String>, engine_errors: &[String]) -> Option<String> {
    if engine_errors.is_empty() {
        return client;
    }
    let engine = format!("engine: {}", engine_errors.join("; "));
    Some(match client {
        Some(client) => format!("{client}; {engine}"),
        None => engine,
    })
}

/// Runs the benchmark: `config.runs` calls of each fixture against the target, one at a time.
///
/// Calls that fail are recorded in the report (and excluded from its statistics), not returned as
/// errors.
///
/// # Errors
/// [`BenchError::Config`] for an unusable configuration (no fixtures, zero runs, a missing or
/// inconsistent fixture, no live URL) or an in-process server that cannot be started;
/// [`BenchError::Call`] if the run is cancelled or the in-process server does not stop.
pub async fn run_bench(
    clock: SharedClock,
    config: &BenchConfig,
    cancel: &CancellationToken,
) -> Result<BenchReport, BenchError> {
    if config.runs == 0 {
        return Err(BenchError::Config("runs must be at least 1".to_owned()));
    }
    if config.concurrency == 0 {
        return Err(BenchError::Config(
            "concurrency must be at least 1".to_owned(),
        ));
    }
    if config.concurrency > 1 && !matches!(config.target, Target::Live(_)) {
        return Err(BenchError::Config(
            "concurrency above 1 is for live mode (--url); the sim modes play one call at a time"
                .to_owned(),
        ));
    }
    if config.fixtures.is_empty() {
        return Err(BenchError::Config("no fixtures".to_owned()));
    }
    let mut fixtures: Vec<Fixture> = Vec::new();
    for name in &config.fixtures {
        let loaded = Fixture::load(&config.fixtures_dir, name)
            .map_err(|e| BenchError::Config(format!("fixture {name}: {e}")))?;
        if loaded.segments.is_empty() {
            return Err(BenchError::Config(format!(
                "fixture {name} has no speech segments"
            )));
        }
        fixtures.push(pad_gaps(&loaded, config.min_gap));
    }

    let (mode, server, server_settings, url) = match &config.target {
        Target::Engine(engine_config) => {
            let mut engine_config = *engine_config;
            let memory_calls = config.memory_calls.min(MAX_PENDING_SCRIPTS);
            engine_config.max_calls = engine_config.max_calls.max(memory_calls);
            let server = EngineServer::start(engine_config, clock.clone(), cancel).await?;
            let url = Secret::new(server.url());
            (
                Mode::Sim,
                Some(SimServer::Engine(server)),
                ServerSettings::Engine(engine_config.settings()),
                url,
            )
        }
        Target::MockBot(bot_config) => {
            let bot = MockBotServer::start(bot_config.clone(), clock.clone(), cancel).await?;
            let url = Secret::new(bot.url());
            (
                Mode::Sim,
                Some(SimServer::MockBot(bot)),
                ServerSettings::MockBot(bot_config.settings()),
                url,
            )
        }
        Target::Live(url) => {
            if url.expose().is_empty() {
                return Err(BenchError::Config("no server URL".to_owned()));
            }
            (Mode::Live, None, ServerSettings::None, url.clone())
        }
    };
    let engine = match &server {
        Some(SimServer::Engine(engine)) => Some(engine),
        _ => None,
    };
    let mut client = config.client.clone();
    client.url = url.clone();

    // Memory: the idle, cold server first (engine target only).
    let cold = match engine {
        Some(_) => read_rss().await,
        None => None,
    };
    let mut calls = Calls {
        clock: &clock,
        client: &client,
        cancel,
        server: server.as_ref(),
        on_call: config.on_call,
        runs: Vec::new(),
        consecutive_failures: 0,
        pause: config.pause,
    };
    let mut plan: Vec<(usize, &Fixture, CallKind)> = Vec::new();
    for run in 0..config.runs {
        plan.extend(fixtures.iter().map(|f| (run, f, CallKind::Latency)));
    }
    let barge_in: Vec<Fixture> = fixtures
        .iter()
        .map(|f| set_gaps(f, config.barge_in_gap))
        .collect();
    if engine.is_some() {
        for run in 0..config.barge_in_runs.unwrap_or(config.runs) {
            plan.extend(barge_in.iter().map(|f| (run, f, CallKind::BargeIn)));
        }
    }
    let mut going = true;
    if config.concurrency > 1 {
        for group in plan.chunks(config.concurrency) {
            going = calls.play_group(group).await?;
            if !going {
                break;
            }
        }
    } else {
        for (run, fixture, kind) in plan {
            going = calls.play(run, fixture, kind).await?;
            if !going {
                break;
            }
        }
    }
    let mut rss = None;
    if let (Some(engine), Some(cold), Some(fixture), true) = (
        engine,
        cold,
        fixtures.first(),
        going && config.memory_calls > 0,
    ) {
        rss = calls
            .memory(engine, fixture, config.memory_calls, cold)
            .await;
    }
    let runs = calls.runs;
    let cancelled = cancel.is_cancelled();
    let engine_stages = engine.map(|engine| {
        let mut report = engine.stage_report();
        report.rss = rss;
        report
    });
    if let Some(server) = &server {
        server.shutdown().await?;
    }
    if cancelled {
        return Err(BenchError::Call("cancelled".to_owned()));
    }
    let mut report = BenchReport::build(
        mode,
        redact_url(url.expose()),
        config.settings(),
        server_settings,
        runs,
    );
    report.engine_stages = engine_stages;
    Ok(report)
}

/// The calls of one benchmark, played one at a time.
struct Calls<'a> {
    clock: &'a SharedClock,
    client: &'a ClientConfig,
    cancel: &'a CancellationToken,
    server: Option<&'a SimServer>,
    on_call: Option<fn(&RunReport)>,
    runs: Vec<RunReport>,
    consecutive_failures: usize,
    pause: Duration,
}

impl Calls<'_> {
    /// Plays `calls` memory calls of `fixture` at once against the engine (see the module docs)
    /// and records them; returns the memory reading, `None` if the platform reports none or a
    /// call failed (a failed call holds no memory: the peak would understate a call).
    async fn memory(
        &mut self,
        engine: &EngineServer,
        fixture: &Fixture,
        calls: usize,
        cold: u64,
    ) -> Option<RssReport> {
        let calls = calls.min(MAX_PENDING_SCRIPTS);
        let warm = read_rss().await?;
        let sampler = start_sampler(self.clock);
        engine.expect_memory_calls(fixture, calls);
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..calls {
            let (clock, client, fixture, cancel) = (
                self.clock.clone(),
                self.client.clone(),
                fixture.clone(),
                self.cancel.clone(),
            );
            set.spawn(async move {
                let outcome = run_call(clock, &client, &fixture, &cancel).await;
                report_of(&fixture, 0, CallKind::Memory, outcome)
            });
        }
        let mut reports = Vec::with_capacity(calls);
        while let Some(joined) = set.join_next().await {
            reports.push(joined.unwrap_or_else(|e| RunReport {
                failure: Some(format!("memory call task: {e}")),
                ..report_of(
                    fixture,
                    0,
                    CallKind::Memory,
                    Err(BenchError::Call(String::new())),
                )
            }));
        }
        let peak = finish_sampler(sampler).await;
        let idle = engine.wait_idle(ENGINE_IDLE_TIMEOUT).await;
        let mut errors = engine.take_errors();
        if !idle {
            errors.push("the engine did not end the memory calls in time".to_owned());
        }
        if let Some(last) = reports.last_mut() {
            last.failure = merge_failure(last.failure.take(), &errors);
        }
        let failed = reports.iter().any(|r| r.failure.is_some());
        for (n, mut report) in reports.into_iter().enumerate() {
            report.run = n;
            if let Some(on_call) = self.on_call {
                on_call(&report);
            }
            self.runs.push(report);
        }
        (!failed).then(|| RssReport {
            cold_bytes: cold,
            warm_bytes: warm,
            peak_bytes: peak.unwrap_or(warm).max(warm),
            calls_at_once: calls,
        })
    }

    /// Plays the calls of `group` at once (live mode only) and records them in plan order; then
    /// pauses. Returns whether to go on (not cancelled, the server not down).
    ///
    /// # Errors
    /// A configuration error of the client.
    async fn play_group(
        &mut self,
        group: &[(usize, &Fixture, CallKind)],
    ) -> Result<bool, BenchError> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        let mut set = tokio::task::JoinSet::new();
        for (index, (_, fixture, _)) in group.iter().enumerate() {
            let (clock, client, fixture, cancel) = (
                self.clock.clone(),
                self.client.clone(),
                (*fixture).clone(),
                self.cancel.clone(),
            );
            set.spawn(async move { (index, run_call(clock, &client, &fixture, &cancel).await) });
        }
        let mut outcomes: Vec<Option<Result<client::CallOutcome, BenchError>>> =
            group.iter().map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            if let Ok((index, outcome)) = joined
                && let Some(slot) = outcomes.get_mut(index)
            {
                *slot = Some(outcome);
            }
        }
        for ((run, fixture, kind), outcome) in group.iter().zip(outcomes) {
            let outcome = outcome.unwrap_or_else(|| {
                Err(BenchError::Call("the call task did not finish".to_owned()))
            });
            if let Err(BenchError::Config(_)) = &outcome {
                return outcome.map(|_| false);
            }
            let report = report_of(fixture, *run, *kind, outcome);
            self.consecutive_failures = if report.failure.is_some() {
                self.consecutive_failures + 1
            } else {
                0
            };
            if let Some(on_call) = self.on_call {
                on_call(&report);
            }
            self.runs.push(report);
        }
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Ok(false);
        }
        Ok(tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            () = wait_for(&**self.clock, self.pause) => true,
        })
    }

    /// Plays one call of `fixture` and records it; then pauses. Returns whether to go on (not
    /// cancelled, the server not down).
    ///
    /// # Errors
    /// A configuration error of the client (the in-process server is shut down first).
    async fn play(
        &mut self,
        run: usize,
        fixture: &Fixture,
        kind: CallKind,
    ) -> Result<bool, BenchError> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        let engine = match self.server {
            Some(SimServer::Engine(engine)) => Some(engine),
            _ => None,
        };
        if let Some(engine) = engine {
            if kind == CallKind::BargeIn {
                engine.expect_barge_in(fixture);
            } else {
                engine.expect(fixture);
            }
        }
        let outcome = run_call(self.clock.clone(), self.client, fixture, self.cancel).await;
        if let Err(BenchError::Config(_)) = &outcome {
            if let Some(server) = self.server {
                // The configuration error is the one to report.
                let _ = server.shutdown().await;
            }
            return outcome.map(|_| false);
        }
        let mut report = report_of(fixture, run, kind, outcome);
        if let Some(engine) = engine {
            settle_engine_call(engine, &mut report).await;
        }
        self.consecutive_failures = if report.failure.is_some() {
            self.consecutive_failures + 1
        } else {
            0
        };
        if let Some(on_call) = self.on_call {
            on_call(&report);
        }
        self.runs.push(report);
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Ok(false);
        }
        Ok(tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            () = wait_for(&**self.clock, self.pause) => true,
        })
    }
}

/// The report of one call of `fixture` from what the client saw.
fn report_of(
    fixture: &Fixture,
    run: usize,
    kind: CallKind,
    outcome: Result<client::CallOutcome, BenchError>,
) -> RunReport {
    match outcome {
        Ok(outcome) => RunReport {
            fixture: fixture.name.clone(),
            run,
            call_id: outcome.call_id,
            turns: outcome.turns,
            frames_sent: outcome.frames_sent,
            audio_frames_received: outcome.audio_frames_received,
            audible_frames: outcome.audible_frames,
            clears: outcome.clears,
            server_errors: outcome.server_errors,
            max_send_lateness: outcome.max_send_lateness,
            failure: outcome.failure,
            kind,
        },
        Err(error) => RunReport {
            fixture: fixture.name.clone(),
            run,
            call_id: String::new(),
            turns: Vec::new(),
            frames_sent: 0,
            audio_frames_received: 0,
            audible_frames: 0,
            clears: 0,
            server_errors: Vec::new(),
            max_send_lateness: Duration::ZERO,
            failure: Some(error.to_string()),
            kind,
        },
    }
}

/// The resident set of this process, read on a blocking thread (a system call).
async fn read_rss() -> Option<u64> {
    tokio::task::spawn_blocking(|| ProcessProbe::new().read().map(|r| r.rss_bytes))
        .await
        .ok()
        .flatten()
}

/// Samples the resident set on a thread of its own until [`finish_sampler`].
fn start_sampler(clock: &SharedClock) -> Option<Sampler> {
    Sampler::start(tokio::runtime::Handle::current(), clock.clone(), RSS_EVERY).ok()
}

/// Stops the sampler (joined on a blocking thread) and returns the peak it saw.
async fn finish_sampler(sampler: Option<Sampler>) -> Option<u64> {
    let sampler = sampler?;
    tokio::task::spawn_blocking(move || sampler.finish())
        .await
        .ok()
        .flatten()
        .map(|samples| samples.rss_peak)
}

/// [`run_bench`] on a runtime of its own (two worker threads, so that in sim mode the server and
/// the client do not share a thread; the engine's heavy work runs on its inference pool) and the
/// system clock.
///
/// # Errors
/// As [`run_bench`]; [`BenchError::Io`] if the runtime cannot be created.
pub fn run_blocking(config: &BenchConfig) -> Result<BenchReport, BenchError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let cancel = CancellationToken::new();
    runtime.block_on(run_bench(SystemClock::shared(), config, &cancel))
}

#[cfg(test)]
mod tests {
    use super::merge_failure;

    #[test]
    fn engine_errors_are_kept_next_to_a_client_failure() {
        let engine = vec!["call 3: pipeline: stt failed".to_owned()];
        assert_eq!(
            merge_failure(Some("the connection broke".to_owned()), &engine).as_deref(),
            Some("the connection broke; engine: call 3: pipeline: stt failed")
        );
        assert_eq!(
            merge_failure(None, &engine).as_deref(),
            Some("engine: call 3: pipeline: stt failed")
        );
        assert_eq!(
            merge_failure(Some("timeout".to_owned()), &[]).as_deref(),
            Some("timeout")
        );
        assert_eq!(merge_failure(None, &[]), None);
    }
}
