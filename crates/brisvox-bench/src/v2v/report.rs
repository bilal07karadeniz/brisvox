//! Aggregation and the two output formats of the V2V benchmark: JSON (every sample, machine
//! readable) and a markdown table (what gets committed under `bench/results/`).
//!
//! Percentiles are nearest-rank ([`Summary`]); with fewer than 100 samples p99 is the maximum.
//! Only [`Outcome::Answered`] turns of calls without a failure
//! enter the percentiles; the other outcomes are counted next to them, so a bot that is too slow
//! or silent shows up as such instead of as a missing sample. Because `talked_over` and
//! `no_response` turns are the slowest ones, any of them makes the result unfit to commit
//! ([`BenchReport::problems`]) in every mode: a tail computed without them would flatter the bot.

use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::engine_target::EngineTargetSettings;
use super::measure::{Outcome, TurnResult, micros};
use super::mock_bot::MockBotSettings;
use super::stages::StageReport;
use crate::BenchError;
use crate::stats::{Summary, ms};

/// The `schema` field of the JSON report. Bumped when a field changes meaning or is removed.
///
/// - `v2v/1`: sim mode measured the mock bot, described by `sim`.
/// - `v2v/2`: sim mode measures the engine (`engine`) or, as a self-check, the mock
///   bot (`mock_bot`, the former `sim`).
/// - `v2v/3`: distributions carry `p95_us`; runs carry a `kind` (`latency`, or
///   `barge_in` / `memory` calls, left out of every V2V aggregate); `engine_stages` holds the
///   engine's per-stage times and memory per call.
pub const SCHEMA: &str = "brisvox-bench/v2v/3";

/// The in-process server a sim-mode report measured, for [`BenchReport::build`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSettings {
    /// The engine over mock providers.
    Engine(EngineTargetSettings),
    /// The mock bot.
    MockBot(MockBotSettings),
    /// Nothing in process (live mode).
    None,
}

/// Why a call was played (only latency calls enter the V2V statistics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// A V2V sample: the utterances are spaced so the bot can answer each in peace.
    #[default]
    Latency,
    /// The caller talks over the bot's long answer on purpose, to measure the clear.
    BargeIn,
    /// One of several calls played at once, to measure memory per call.
    Memory,
}

impl CallKind {
    /// The name used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::BargeIn => "barge_in",
            Self::Memory => "memory",
        }
    }
}

/// Which kind of server was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// An in-process server with mock providers: the engine, or the mock bot of this crate.
    Sim,
    /// Any server at a URL, with real providers.
    Live,
}

impl Mode {
    /// The name used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sim => "sim",
            Self::Live => "live",
        }
    }
}

/// A latency distribution (nearest rank), in microseconds in JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dist {
    /// Number of samples.
    pub count: usize,
    /// Median.
    #[serde(rename = "p50_us", with = "micros")]
    pub p50: Duration,
    /// 90th percentile.
    #[serde(rename = "p90_us", with = "micros")]
    pub p90: Duration,
    /// 95th percentile.
    #[serde(rename = "p95_us", with = "micros", default)]
    pub p95: Duration,
    /// 99th percentile.
    #[serde(rename = "p99_us", with = "micros")]
    pub p99: Duration,
    /// Largest sample.
    #[serde(rename = "max_us", with = "micros")]
    pub max: Duration,
}

impl From<Summary> for Dist {
    fn from(s: Summary) -> Self {
        Self {
            count: s.count,
            p50: s.p50,
            p90: s.p90,
            p95: s.p95,
            p99: s.p99,
            max: s.max,
        }
    }
}

impl Dist {
    /// The distribution of `samples`.
    pub fn of(samples: Vec<Duration>) -> Self {
        Summary::of(samples).into()
    }
}

/// Counts and latency of a set of turns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    /// Turns in the set.
    pub turns: usize,
    /// Turns that are clean V2V samples.
    pub answered: usize,
    /// Turns answered only after the user spoke again.
    pub talked_over: usize,
    /// Turns that began while the bot was already speaking.
    pub bot_speaking: usize,
    /// Turns without any bot audio.
    pub no_response: usize,
    /// V2V of the answered turns.
    pub v2v: Dist,
}

impl Aggregate {
    /// Counts `results` and summarizes the latency of the answered ones.
    pub fn of<'a>(results: impl IntoIterator<Item = &'a TurnResult>) -> Self {
        let mut aggregate = Self::default();
        let mut samples = Vec::new();
        for result in results {
            aggregate.turns += 1;
            match result.outcome {
                Outcome::Answered => {
                    aggregate.answered += 1;
                    samples.extend(result.v2v);
                }
                Outcome::TalkedOver => aggregate.talked_over += 1,
                Outcome::BotSpeaking => aggregate.bot_speaking += 1,
                Outcome::NoResponse => aggregate.no_response += 1,
            }
        }
        aggregate.v2v = Dist::of(samples);
        aggregate
    }
}

/// An [`Aggregate`] with a name (a fixture, a language).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedAggregate {
    /// The fixture name or language tag.
    pub name: String,
    /// Its results.
    #[serde(flatten)]
    pub aggregate: Aggregate,
}

/// An [`Aggregate`] of one turn position of one fixture across all runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAggregate {
    /// Fixture name.
    pub fixture: String,
    /// Turn position.
    pub index: usize,
    /// Language of the utterance.
    pub lang: String,
    /// Ground-truth end of speech in the (gap-padded) stream.
    #[serde(rename = "eos_us", with = "micros")]
    pub eos: Duration,
    /// Its results across runs.
    #[serde(flatten)]
    pub aggregate: Aggregate,
}

/// One call of one fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Fixture name.
    pub fixture: String,
    /// Run number, from 0.
    pub run: usize,
    /// The server's id for the call (empty if the call never started).
    pub call_id: String,
    /// Per-turn results (empty if the call never started).
    pub turns: Vec<TurnResult>,
    /// Audio frames sent.
    pub frames_sent: u64,
    /// Binary frames received.
    pub audio_frames_received: u64,
    /// Received frames that were not silent.
    pub audible_frames: u64,
    /// Barge-in clears received.
    pub clears: usize,
    /// Errors the server reported, as `code: message`.
    pub server_errors: Vec<String>,
    /// Largest lateness of a sent frame: how faithful the client's pacing was.
    #[serde(rename = "max_send_lateness_us", with = "micros")]
    pub max_send_lateness: Duration,
    /// Why this run is excluded from the statistics, if it is.
    pub failure: Option<String>,
    /// Why the call was played: anything but [`CallKind::Latency`] stays out of the V2V
    /// statistics.
    #[serde(default)]
    pub kind: CallKind,
}

/// What was run (everything but the server's behaviour).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Runs per fixture.
    pub runs: usize,
    /// Calls played at once (live mode; 1 = one call at a time). Reports written before the
    /// option existed read as 1.
    #[serde(default = "one")]
    pub concurrency: usize,
    /// Fixtures, in order.
    pub fixtures: Vec<String>,
    /// Minimum silence between two utterances after padding, in milliseconds.
    pub min_gap_ms: u64,
    /// Audio frame sent to the server, in milliseconds.
    pub frame_ms: u64,
    /// Peak amplitude below which bot audio is silence.
    pub silence_peak: u16,
    /// Audio this long before EOS marks the turn `bot_speaking`, in milliseconds.
    pub overlap_guard_ms: u64,
    /// How long after the fixture the last turn may still be answered, in milliseconds.
    pub response_timeout_ms: u64,
}

fn one() -> usize {
    1
}

/// The complete result of a benchmark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchReport {
    /// [`SCHEMA`].
    pub schema: String,
    /// Sim or live.
    pub mode: Mode,
    /// The server measured, without credentials, path or query (see [`redact_url`]).
    pub target: String,
    /// What was run.
    pub settings: Settings,
    /// The engine's configuration (sim mode against the engine): provider latencies and the
    /// latency floor the measurement should land above.
    pub engine: Option<EngineTargetSettings>,
    /// The mock bot's behaviour (sim mode against the mock bot): the latency floor the
    /// measurement should land above.
    pub mock_bot: Option<MockBotSettings>,
    /// Every call, failed ones included.
    pub runs: Vec<RunReport>,
    /// Calls excluded from the statistics.
    pub failed_runs: usize,
    /// All turns of all valid calls.
    pub overall: Aggregate,
    /// Per fixture.
    pub per_fixture: Vec<NamedAggregate>,
    /// Per language of the utterance.
    pub per_language: Vec<NamedAggregate>,
    /// Per turn position of each fixture.
    pub per_turn: Vec<TurnAggregate>,
    /// Largest send lateness over all calls.
    #[serde(rename = "max_send_lateness_us", with = "micros")]
    pub max_send_lateness: Duration,
    /// The engine's per-stage times and memory per call (sim mode against the engine).
    #[serde(default)]
    pub engine_stages: Option<StageReport>,
}

/// The URL reduced to scheme, host and port: userinfo, path, query and fragment are removed, as
/// live URLs may carry tokens in any of them. A removed path is shown as `/...`.
pub fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..end).unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let hidden = rest.get(end..).is_some_and(|tail| tail.len() > 1);
    let mut out = String::new();
    if !scheme.is_empty() {
        out.push_str(scheme);
        out.push_str("://");
    }
    out.push_str(host);
    if hidden {
        out.push_str("/...");
    }
    out
}

fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}

impl BenchReport {
    /// Builds the report: aggregates the valid runs (those without a failure, barge-in calls
    /// left out). `engine_stages` is the caller's to fill.
    pub fn build(
        mode: Mode,
        target: String,
        settings: Settings,
        server: ServerSettings,
        runs: Vec<RunReport>,
    ) -> Self {
        let (engine, mock_bot) = match server {
            ServerSettings::Engine(engine) => (Some(engine), None),
            ServerSettings::MockBot(bot) => (None, Some(bot)),
            ServerSettings::None => (None, None),
        };
        let valid: Vec<&RunReport> = runs
            .iter()
            .filter(|r| r.failure.is_none() && r.kind == CallKind::Latency)
            .collect();
        let all_turns = || valid.iter().flat_map(|r| r.turns.iter());
        let per_fixture = unique(valid.iter().map(|r| r.fixture.as_str()))
            .into_iter()
            .map(|name| NamedAggregate {
                name: name.to_owned(),
                aggregate: Aggregate::of(
                    valid
                        .iter()
                        .filter(|r| r.fixture == name)
                        .flat_map(|r| r.turns.iter()),
                ),
            })
            .collect();
        let per_language = unique(all_turns().map(|t| t.lang.as_str()))
            .into_iter()
            .map(|lang| NamedAggregate {
                name: lang.to_owned(),
                aggregate: Aggregate::of(all_turns().filter(|t| t.lang == lang)),
            })
            .collect();
        let mut per_turn = Vec::new();
        for fixture in unique(valid.iter().map(|r| r.fixture.as_str())) {
            let mut indexes: Vec<usize> = valid
                .iter()
                .filter(|r| r.fixture == fixture)
                .flat_map(|r| r.turns.iter().map(|t| t.index))
                .collect();
            indexes.sort_unstable();
            indexes.dedup();
            for index in indexes {
                let results: Vec<&TurnResult> = valid
                    .iter()
                    .filter(|r| r.fixture == fixture)
                    .flat_map(|r| r.turns.iter())
                    .filter(|t| t.index == index)
                    .collect();
                let Some(first) = results.first() else {
                    continue;
                };
                per_turn.push(TurnAggregate {
                    fixture: fixture.to_owned(),
                    index,
                    lang: first.lang.clone(),
                    eos: first.eos,
                    aggregate: Aggregate::of(results.iter().copied()),
                });
            }
        }
        Self {
            schema: SCHEMA.to_owned(),
            mode,
            target,
            settings,
            engine,
            mock_bot,
            failed_runs: runs.iter().filter(|r| r.failure.is_some()).count(),
            overall: Aggregate::of(all_turns()),
            per_fixture,
            per_language,
            per_turn,
            max_send_lateness: runs
                .iter()
                .map(|r| r.max_send_lateness)
                .max()
                .unwrap_or_default(),
            runs,
            engine_stages: None,
        }
    }

    /// What makes this result unfit to commit. A failed call is always a problem, and so is a run
    /// without a single answered turn. So is any `talked_over` or `no_response` turn: those are
    /// the turns the bot answered later than the gap to the next utterance, or not within the
    /// response window, so leaving them out of the percentiles would cut the slowest turns off the
    /// tail. `strict` (always on in sim mode, where the bot is deterministic) also makes
    /// `bot_speaking` turns a problem, so every turn must be a clean sample.
    pub fn problems(&self, strict: bool) -> Vec<String> {
        let mut problems = Vec::new();
        for run in self.runs.iter().filter(|r| r.failure.is_some()) {
            problems.push(format!(
                "{} run {}: {}",
                run.fixture,
                run.run,
                run.failure.as_deref().unwrap_or_default()
            ));
        }
        if self.overall.answered == 0 {
            problems.push("no turn was answered: there is no latency to report".to_owned());
        }
        let barge_in_calls = self
            .runs
            .iter()
            .filter(|r| r.kind == CallKind::BargeIn && r.failure.is_none())
            .count();
        if let Some(stages) = &self.engine_stages
            && barge_in_calls > 0
            && stages.interrupts == 0
        {
            problems.push(format!(
                "{barge_in_calls} barge-in calls, but the engine confirmed no barge-in: there is \
                 no clear to report (widen --barge-in-gap-ms)"
            ));
        }
        if strict && self.overall.answered != self.overall.turns {
            problems.push(format!(
                "{} of {} turns are not clean samples ({} talked over, {} bot speaking, {} \
                 without a response)",
                self.overall.turns - self.overall.answered,
                self.overall.turns,
                self.overall.talked_over,
                self.overall.bot_speaking,
                self.overall.no_response
            ));
        } else if !strict && self.overall.talked_over + self.overall.no_response > 0 {
            problems.push(format!(
                "{} of {} turns were answered only after the user spoke again or not at all ({} \
                 talked over, {} without a response): the percentiles leave them out, so their \
                 tail is not the system's (widen --min-gap-ms or --response-timeout-ms)",
                self.overall.talked_over + self.overall.no_response,
                self.overall.turns,
                self.overall.talked_over,
                self.overall.no_response
            ));
        }
        problems
    }

    /// The engine stages whose p95 missed its engine-overhead budget, one line each. Empty
    /// without engine stages (live mode, the mock-bot target). `brisvox-bench v2v --strict` counts
    /// them as [`problems`](Self::problems); without `--strict` they are only warned about, since a
    /// loaded machine can miss a budget the engine meets.
    pub fn budget_problems(&self) -> Vec<String> {
        let Some(stages) = &self.engine_stages else {
            return Vec::new();
        };
        stages
            .over_budget()
            .into_iter()
            .map(|(stage, p95, budget)| {
                format!(
                    "engine stage {stage}: p95 {} ms over its {} ms budget",
                    ms(p95),
                    ms(budget)
                )
            })
            .collect()
    }

    /// The JSON report (pretty-printed, every sample).
    ///
    /// # Errors
    /// [`BenchError::Config`] if serialization fails (not expected).
    pub fn to_json(&self) -> Result<String, BenchError> {
        serde_json::to_string_pretty(self)
            .map(|mut json| {
                json.push('\n');
                json
            })
            .map_err(|e| BenchError::Config(format!("cannot serialize the report: {e}")))
    }

    /// The markdown report; `environment` lines (command, machine, build) go under the title.
    pub fn markdown(&self, environment: &[String]) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Voice-to-voice latency ({})\n", self.mode.as_str());
        for line in environment {
            let _ = writeln!(out, "- {line}");
        }
        let s = &self.settings;
        let _ = writeln!(
            out,
            "- target: `{}`\n- {} runs of each of {} (utterances spaced at least {} ms apart, \
             {} ms frames, silence below peak {}, response window {} ms after the fixture){}",
            self.target,
            s.runs,
            s.fixtures.join(", "),
            s.min_gap_ms,
            s.frame_ms,
            s.silence_peak,
            s.response_timeout_ms,
            if s.concurrency > 1 {
                format!("\n- {} calls played at once", s.concurrency)
            } else {
                String::new()
            }
        );
        if let Some(engine) = &self.engine {
            let turn = engine.turn_silence_ms.map_or_else(
                || {
                    "the adaptive policy with a Smart Turn model that answers \"complete\" at \
                     every pause (feature extraction real, on the pool)"
                        .to_owned()
                },
                |silence| format!("a fixed {silence} ms of silence after the backdated VAD stop"),
            );
            let _ = writeln!(
                out,
                "- engine: the production pipeline (`brisvox_engine::run_call`) on a \
                 {}-thread inference pool, over `brisvox_sim` mock providers with fixed \
                 latencies: STT final {} ms after `Finalize` (interims every {} ms), LLM first \
                 token {} ms (then a token every {} ms), TTS first byte {} ms (a 40 ms chunk \
                 every {} ms). Energy VAD (RMS >= {}, {} ms to start, stop after {} ms of \
                 silence); end of turn: {}.\n- floor: end of turn {} ms after the end of speech \
                 (VAD stop + STT final, the finals gate{}), latency floor {} ms (end of turn + \
                 LLM first token, with which the answer's first clause leaves + TTS first \
                 byte). The rest of the measurement is the engine's \
                 own time (VAD windows, pool hops, stage queues, the 20 ms frame grid) plus the \
                 websocket transport",
                engine.pool_threads,
                engine.stt_final_ms,
                engine.stt_interim_ms,
                engine.llm_first_token_ms,
                engine.llm_inter_token_ms,
                engine.tts_first_byte_ms,
                engine.tts_chunk_interval_ms,
                engine.vad_threshold_rms,
                engine.vad_start_ms,
                engine.vad_stop_ms,
                turn,
                engine.end_of_turn_floor_ms,
                if engine.turn_silence_ms.is_some() {
                    ", or the fixed silence if longer"
                } else {
                    ""
                },
                engine.floor_ms
            );
            if engine.speculation {
                let _ = writeln!(
                    out,
                    "- speculation: **on**, {}STT interims {}. A committed speculation \
                     starts the LLM before the end of the turn, so a measurement can land below \
                     the floor above by up to the head start (see Speculation), and the engine \
                     stage `turn_end_to_llm_request` has no sample for those turns.",
                    if engine.speculation_gate_ms > 0 {
                        format!("stable-partial gate {} ms, ", engine.speculation_gate_ms)
                    } else {
                        String::new()
                    },
                    engine.stt_interims
                );
            }
        }
        if let Some(sim) = &self.mock_bot {
            let _ = writeln!(
                out,
                "- mock bot: end-of-turn after {} ms of silence, then {} ms (+ up to {} ms \
                 jitter) of simulated provider time, then a {} ms reply. Latency floor \
                 (end-of-turn + provider time) {} ms; the rest of the measurement is the \
                 bot's 20 ms frame granularity plus the websocket transport",
                sim.endpoint_silence_ms,
                sim.response_delay_ms,
                sim.response_jitter_ms,
                sim.reply_ms,
                sim.floor_ms()
            );
        }
        let _ = writeln!(
            out,
            "\n**V2V** = arrival at the client of the first non-silent bot audio after the \
             ground-truth end of the user's speech, minus that end of speech \
             (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 \
             samples p99 is the maximum.\n"
        );
        let _ = writeln!(
            out,
            "## Summary\n\n| scope | turns | answered | talked over | bot speaking | no response \
             | p50 ms | p90 ms | p95 ms | p99 ms | max ms |\n|---|---:|---:|---:|---:|---:|---:|---:|\
             ---:|---:|---:|"
        );
        summary_row(&mut out, "all", &self.overall);
        for f in &self.per_fixture {
            summary_row(&mut out, &format!("fixture {}", f.name), &f.aggregate);
        }
        for l in &self.per_language {
            summary_row(&mut out, &format!("language {}", l.name), &l.aggregate);
        }
        let _ = writeln!(
            out,
            "\n## Per turn\n\n| fixture | turn | lang | EOS at ms | answered/turns | p50 ms | p90 ms \
             | p99 ms | max ms |\n|---|---:|---|---:|---:|---:|---:|---:|---:|"
        );
        for t in &self.per_turn {
            let d = &t.aggregate.v2v;
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {}/{} | {} | {} | {} | {} |",
                t.fixture,
                t.index,
                t.lang,
                ms(t.eos),
                t.aggregate.answered,
                t.aggregate.turns,
                cell(d, d.p50),
                cell(d, d.p90),
                cell(d, d.p99),
                cell(d, d.max)
            );
        }
        if let Some(stages) = &self.engine_stages {
            stages.markdown(&mut out);
        }
        let calls = self.runs.len();
        let _ = writeln!(
            out,
            "\n## Calls\n\n{calls} calls ({} barge-in calls, {} memory calls), {} failed. \
             Largest client send lateness {} ms (a frame sent later than its deadline delays the \
             end of speech the server sees by as much).",
            self.runs
                .iter()
                .filter(|r| r.kind == CallKind::BargeIn)
                .count(),
            self.runs
                .iter()
                .filter(|r| r.kind == CallKind::Memory)
                .count(),
            self.failed_runs,
            ms(self.max_send_lateness)
        );
        let errors: Vec<&str> = self
            .runs
            .iter()
            .flat_map(|r| r.server_errors.iter().map(String::as_str))
            .collect();
        if !errors.is_empty() {
            let _ = writeln!(out, "\nServer errors reported ({}):", errors.len());
            for error in unique(errors.iter().copied()) {
                let _ = writeln!(out, "- {error}");
            }
        }
        for run in self.runs.iter().filter(|r| r.failure.is_some()) {
            let _ = writeln!(
                out,
                "- FAILED {} run {}: {}",
                run.fixture,
                run.run,
                run.failure.as_deref().unwrap_or_default()
            );
        }
        out
    }
}

/// A latency cell: the value, or `-` if there are no samples.
fn cell(dist: &Dist, value: Duration) -> String {
    if dist.count == 0 {
        "-".to_owned()
    } else {
        ms(value)
    }
}

fn summary_row(out: &mut String, scope: &str, a: &Aggregate) {
    let d = &a.v2v;
    let _ = writeln!(
        out,
        "| {scope} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        a.turns,
        a.answered,
        a.talked_over,
        a.bot_speaking,
        a.no_response,
        cell(d, d.p50),
        cell(d, d.p90),
        cell(d, d.p95),
        cell(d, d.p99),
        cell(d, d.max)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn result(index: usize, lang: &str, outcome: Outcome, v2v_ms: Option<u64>) -> TurnResult {
        TurnResult {
            index,
            lang: lang.to_owned(),
            eos: Duration::from_millis(1_000 * (index as u64 + 1)),
            outcome,
            v2v: v2v_ms.map(Duration::from_millis),
        }
    }

    fn run(fixture: &str, n: usize, turns: Vec<TurnResult>, failure: Option<&str>) -> RunReport {
        RunReport {
            fixture: fixture.to_owned(),
            run: n,
            call_id: format!("c{n}"),
            turns,
            frames_sent: 10,
            audio_frames_received: 5,
            audible_frames: 5,
            clears: 0,
            server_errors: Vec::new(),
            max_send_lateness: 3 * MS,
            failure: failure.map(str::to_owned),
            kind: CallKind::Latency,
        }
    }

    fn settings() -> Settings {
        Settings {
            runs: 3,
            concurrency: 1,
            fixtures: vec!["a".to_owned(), "b".to_owned()],
            min_gap_ms: 4_000,
            frame_ms: 20,
            silence_peak: 200,
            overlap_guard_ms: 100,
            response_timeout_ms: 10_000,
        }
    }

    #[test]
    fn barge_in_calls_stay_out_of_the_latency_and_must_show_a_barge_in() {
        let mut barge_in = run(
            "a",
            0,
            vec![
                result(0, "en", Outcome::Answered, Some(900)),
                result(1, "en", Outcome::BotSpeaking, None),
            ],
            None,
        );
        barge_in.kind = CallKind::BargeIn;
        let mut report = BenchReport::build(
            Mode::Sim,
            "ws://127.0.0.1:9".to_owned(),
            settings(),
            ServerSettings::None,
            vec![
                run(
                    "a",
                    0,
                    vec![result(0, "en", Outcome::Answered, Some(812))],
                    None,
                ),
                barge_in,
            ],
        );
        // The barge-in call's turns are in no aggregate, and it is not a failure.
        assert_eq!((report.overall.turns, report.overall.answered), (1, 1));
        assert_eq!(report.overall.v2v.max, 812 * MS);
        assert_eq!(report.failed_runs, 0);
        assert!(
            report.problems(true).is_empty(),
            "{:?}",
            report.problems(true)
        );
        // With the engine's stages: a barge-in call without any confirmed barge-in is a problem.
        let settings = crate::v2v::engine_target::EngineTargetConfig::default().settings();
        report.engine_stages =
            Some(crate::v2v::stages::StageCollector::default().report(&settings));
        let problems = report.problems(true);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no barge-in"), "{problems:?}");
        assert!(
            report.budget_problems().is_empty(),
            "no samples, no verdict"
        );
        // A stage over its budget is a budget problem, named with its p95 and budget.
        let stages = report.engine_stages.as_mut().unwrap();
        let stage = stages
            .stages
            .iter_mut()
            .find(|s| s.budget.is_some())
            .unwrap();
        let (name, budget) = (stage.name.clone(), stage.budget.unwrap());
        stage.dist.p95 = budget + 3 * MS;
        stage.within_budget = Some(false);
        let over = report.budget_problems();
        assert_eq!(over.len(), 1, "{over:?}");
        assert!(
            over[0].starts_with(&format!("engine stage {name}: p95 ")),
            "{over:?}"
        );
        assert!(over[0].contains("over its"), "{over:?}");
        let md = report.markdown(&[]);
        assert!(md.contains("## Engine stages"), "{md}");
        assert!(
            md.contains("2 calls (1 barge-in calls, 0 memory calls)"),
            "{md}"
        );
    }

    #[test]
    fn urls_are_reduced_to_scheme_and_host() {
        assert_eq!(
            redact_url("ws://127.0.0.1:8080/call"),
            "ws://127.0.0.1:8080/..."
        );
        assert_eq!(redact_url("ws://127.0.0.1:8080/"), "ws://127.0.0.1:8080");
        assert_eq!(redact_url("ws://127.0.0.1:8080"), "ws://127.0.0.1:8080");
        assert_eq!(
            redact_url("wss://user:pw@bot.example.com/v1/call/SECRET?token=abc#frag"),
            "wss://bot.example.com/..."
        );
        assert_eq!(
            redact_url("wss://bot.example.com?token=abc"),
            "wss://bot.example.com/..."
        );
        for url in [
            "wss://user:pw@bot.example.com/v1/call/SECRET?token=abc#frag",
            "wss://bot.example.com?token=abc",
        ] {
            let shown = redact_url(url);
            assert!(
                !shown.contains("pw") && !shown.contains("SECRET") && !shown.contains("abc"),
                "{shown}"
            );
        }
    }

    #[test]
    fn aggregation_counts_outcomes_and_percentiles_come_from_answered_turns_only() {
        // 10 answered turns of 100..1000 ms, plus one of every other outcome.
        let mut turns: Vec<TurnResult> = (1..=10)
            .map(|i| result(0, "en", Outcome::Answered, Some(i * 100)))
            .collect();
        turns.push(result(1, "en", Outcome::TalkedOver, Some(9_000)));
        turns.push(result(1, "en", Outcome::BotSpeaking, None));
        turns.push(result(1, "en", Outcome::NoResponse, None));
        let a = Aggregate::of(&turns);
        assert_eq!(
            (
                a.turns,
                a.answered,
                a.talked_over,
                a.bot_speaking,
                a.no_response
            ),
            (13, 10, 1, 1, 1)
        );
        assert_eq!(a.v2v.count, 10);
        assert_eq!(
            (a.v2v.p50, a.v2v.p90, a.v2v.p99, a.v2v.max),
            (500 * MS, 900 * MS, 1_000 * MS, 1_000 * MS)
        );
        assert_eq!(Aggregate::of(&[]), Aggregate::default());
    }

    #[test]
    fn n_runs_are_pooled_per_fixture_language_and_turn_and_failed_runs_are_left_out() {
        let ok = |n, d0, d1| {
            run(
                "a",
                n,
                vec![
                    result(0, "en", Outcome::Answered, Some(d0)),
                    result(1, "tr", Outcome::Answered, Some(d1)),
                ],
                None,
            )
        };
        let runs = vec![
            ok(0, 100, 1_000),
            ok(1, 200, 2_000),
            ok(2, 300, 3_000),
            run(
                "a",
                3,
                vec![result(0, "en", Outcome::Answered, Some(50_000))],
                Some("boom"),
            ),
            run(
                "b",
                0,
                vec![result(0, "en", Outcome::Answered, Some(40))],
                None,
            ),
        ];
        let report = BenchReport::build(
            Mode::Live,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            runs,
        );
        assert_eq!(report.failed_runs, 1);
        assert_eq!(report.overall.turns, 7);
        assert_eq!(
            report.overall.v2v.count, 7,
            "the failed run's 50 s is not a sample"
        );
        assert_eq!(report.overall.v2v.max, 3_000 * MS);
        let names: Vec<&str> = report.per_fixture.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(report.per_fixture[0].aggregate.turns, 6);
        let langs: Vec<&str> = report
            .per_language
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(langs, ["en", "tr"]);
        assert_eq!(report.per_language[1].aggregate.v2v.p50, 2_000 * MS);
        // Per turn position: three samples each for fixture a, from three runs.
        let t0 = &report.per_turn[0];
        assert_eq!(
            (t0.fixture.as_str(), t0.index, t0.lang.as_str()),
            ("a", 0, "en")
        );
        assert_eq!(t0.aggregate.v2v.count, 3);
        assert_eq!(
            (t0.aggregate.v2v.p50, t0.aggregate.v2v.p99),
            (200 * MS, 300 * MS)
        );
        assert_eq!(report.per_turn.len(), 3);
        assert_eq!(report.max_send_lateness, 3 * MS);
    }

    #[test]
    fn problems_name_failed_calls_and_in_strict_mode_unclean_turns() {
        let clean = vec![result(0, "en", Outcome::Answered, Some(100))];
        let report = BenchReport::build(
            Mode::Sim,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            vec![run("a", 0, clean.clone(), None)],
        );
        assert!(report.problems(true).is_empty());
        let mixed = vec![
            result(0, "en", Outcome::Answered, Some(100)),
            result(1, "en", Outcome::NoResponse, None),
        ];
        let report = BenchReport::build(
            Mode::Live,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            vec![
                run("a", 0, mixed, None),
                run("a", 1, clean, Some("the connection broke")),
            ],
        );
        let lenient = report.problems(false);
        assert_eq!(lenient.len(), 2, "{lenient:?}");
        assert!(lenient[0].contains("the connection broke"));
        assert!(lenient[1].contains("1 without a response"), "{lenient:?}");
        assert_eq!(report.problems(true).len(), 2);
        let empty = BenchReport::build(
            Mode::Live,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            vec![],
        );
        assert!(empty.problems(false)[0].contains("no turn was answered"));
    }

    #[test]
    fn live_mode_flags_turns_that_would_cut_the_tail_off_the_percentiles() {
        // 97 fast turns and 3 stalls whose replies landed during the next utterance: the
        // percentiles come from the fast turns only, so the result must not pass as clean.
        let mut turns: Vec<TurnResult> = (0..97)
            .map(|i| result(i, "en", Outcome::Answered, Some(800)))
            .collect();
        turns.extend((97..100).map(|i| result(i, "en", Outcome::TalkedOver, Some(5_000))));
        let report = BenchReport::build(
            Mode::Live,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            vec![run("a", 0, turns, None)],
        );
        assert_eq!(report.overall.v2v.p99, Duration::from_millis(800));
        let problems = report.problems(false);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("3 talked over"), "{problems:?}");
        // A turn that began while the bot was still speaking is no latency sample, but it does not
        // hide a slow reply either: lenient mode lets it pass, strict mode does not.
        let speaking = vec![
            result(0, "en", Outcome::Answered, Some(800)),
            result(1, "en", Outcome::BotSpeaking, None),
        ];
        let report = BenchReport::build(
            Mode::Live,
            "ws://h".to_owned(),
            settings(),
            ServerSettings::None,
            vec![run("a", 0, speaking, None)],
        );
        assert!(report.problems(false).is_empty());
        assert_eq!(report.problems(true).len(), 1);
    }

    #[test]
    fn json_and_markdown_carry_the_numbers_and_no_url_secrets() {
        let report = BenchReport::build(
            Mode::Sim,
            redact_url("ws://127.0.0.1:9/call?token=SECRET"),
            settings(),
            ServerSettings::None,
            vec![run(
                "a",
                0,
                vec![
                    result(0, "en", Outcome::Answered, Some(812)),
                    result(1, "tr", Outcome::NoResponse, None),
                ],
                None,
            )],
        );
        let json = report.to_json().unwrap();
        assert!(
            json.contains("\"schema\": \"brisvox-bench/v2v/3\""),
            "{json}"
        );
        assert!(json.contains("\"mode\": \"sim\""));
        assert!(json.contains("\"v2v_us\": 812000"));
        assert!(json.contains("\"p95_us\""));
        assert!(json.contains("\"p99_us\""));
        assert!(!json.contains("SECRET"));
        let back: BenchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        let md = report.markdown(&["command: `x`".to_owned()]);
        assert!(md.starts_with("# Voice-to-voice latency (sim)"), "{md}");
        assert!(md.contains("- command: `x`"));
        assert!(
            md.contains(
                "| all | 2 | 1 | 0 | 0 | 1 | 812.000 | 812.000 | 812.000 | 812.000 | 812.000 |"
            ),
            "{md}"
        );
        assert!(
            md.contains("| language tr | 1 | 0 | 0 | 0 | 1 | - | - | - | - |"),
            "{md}"
        );
        assert!(
            md.contains("| a | 0 | en | 1000.000 | 1/1 | 812.000 |"),
            "{md}"
        );
        assert!(!md.contains("SECRET"));
    }

    #[test]
    fn the_engine_target_states_its_providers_and_floors() {
        use crate::v2v::engine_target::{EndOfTurn, EngineTargetConfig};
        let clean = vec![result(0, "en", Outcome::Answered, Some(1_000))];
        let settings_of = |config: EngineTargetConfig| {
            BenchReport::build(
                Mode::Sim,
                "ws://127.0.0.1:9".to_owned(),
                settings(),
                ServerSettings::Engine(config.settings()),
                vec![run("a", 0, clean.clone(), None)],
            )
        };
        let report = settings_of(EngineTargetConfig::default());
        assert!(report.mock_bot.is_none());
        assert_eq!(report.engine.as_ref().map(|e| e.floor_ms), Some(950));
        let json = report.to_json().unwrap();
        assert!(json.contains("\"floor_ms\": 950"), "{json}");
        assert!(json.contains("\"end_of_turn_floor_ms\": 450"), "{json}");
        let back: BenchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        let md = report.markdown(&[]);
        assert!(md.contains("STT final 150 ms"), "{md}");
        assert!(md.contains("LLM first token 350 ms"), "{md}");
        assert!(md.contains("TTS first byte 150 ms"), "{md}");
        assert!(md.contains("Smart Turn"), "{md}");
        assert!(
            md.contains("end of turn 450 ms after the end of speech"),
            "{md}"
        );
        assert!(md.contains("latency floor 950 ms"), "{md}");
        assert!(!md.contains("mock bot"), "{md}");
        let fixed = settings_of(EngineTargetConfig {
            end_of_turn: EndOfTurn::FixedSilence(Duration::from_millis(600)),
            ..EngineTargetConfig::default()
        });
        let md = fixed.markdown(&[]);
        assert!(md.contains("a fixed 600 ms of silence"), "{md}");
        assert!(md.contains("latency floor 1100 ms"), "{md}");
    }
}
