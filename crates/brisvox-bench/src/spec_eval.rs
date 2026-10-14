//! Speculative LLM start, measured on the fixtures with a paused clock.
//!
//! The engine of the production assembly ([`brisvox_engine::sim::EngineFactory`]) answers the
//! real-speech fixtures over `brisvox_sim` mock providers on tokio's paused clock, so a run is
//! deterministic per seed and takes no wall time. The same calls are played with speculation off
//! and on, for the adaptive end-of-turn policy (a Smart Turn model that answers "complete") and
//! for fixed silences, and with the STT's last interim lagging the final by a word (the mock's
//! own interims: the worst case, every speculation started from the partial misses) or converged
//! to it (the best case), and with speculation at the STT final only (`on_partial` off). For each
//! combination the report has the voice-to-voice time
//! (`UserSpeechEnd` to `FirstAudioOut`, server side), the hit rate, the requests per turn and the
//! tokens wasted on discarded speculations.
//!
//! Latencies are log-normal (sigma 0.3) around the V2V benchmark's medians, one seed per call.
//! Every turn of every combination uses the same seeds, so the "off" and "on" columns see the
//! same latencies. Everything is also played with a fast LLM (first token 100 ms, below the STT
//! final): there a discarded run streams tokens into its buffer before the miss is known, which
//! the slower LLM (350 ms, after the miss) never does, so the wasted-token counters are
//! measured instead of assumed zero.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock, TurnEvent};
use brisvox_engine::SpeculationStatsSnapshot;
use brisvox_engine::sim::EngineFactory;
use brisvox_sim::fixture::FixtureSegment;
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::LlmResponder;
use brisvox_sim::session::SimSession;

use crate::BenchError;
use crate::fixture_load::pad_gaps;
use crate::sim_load::{LLM_FIRST_TOKEN_MEDIAN_MS, STT_FINAL_MEDIAN_MS, TTS_FIRST_BYTE_MEDIAN_MS};
use crate::stats::Summary;
use crate::v2v::engine_target::{CallScript, SttInterims};

const MS: Duration = Duration::from_millis(1);

/// First-token median of the fast-LLM variant: below the STT final (150 ms), so a discarded
/// speculative run streams into its buffer before the miss is known.
pub const FAST_LLM_FIRST_TOKEN_MS: u64 = 100;

/// An end-of-turn policy of the measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The adaptive policy with a Smart Turn model that always answers "complete".
    Adaptive,
    /// A fixed silence after the VAD stop.
    Fixed(Duration),
}

impl Policy {
    fn name(self) -> String {
        match self {
            Self::Adaptive => "adaptive".to_owned(),
            Self::Fixed(silence) => format!("fixed {} ms", silence.as_millis()),
        }
    }
}

/// How speculation was set up in a combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speculation {
    /// Off (the baseline).
    Off,
    /// On, with the STT's interims as given.
    On(SttInterims),
    /// On, but only at the STT final (`on_partial` off): no request from an interim, so the
    /// interims do not matter.
    OnFinal,
    /// On from the partial like [`Self::On`], but with the stable-partial gate of the given
    /// length (`SpeculationConfig::partial_stable_for`).
    OnGated(SttInterims, Duration),
}

impl Speculation {
    fn name(self) -> String {
        match self {
            Self::Off => "off".to_owned(),
            Self::On(interims) => format!("on, {} interims", interims.as_str()),
            Self::OnFinal => "on, final only".to_owned(),
            Self::OnGated(interims, gate) => format!(
                "on, {} interims, gate {} ms",
                interims.as_str(),
                gate.as_millis()
            ),
        }
    }
}

/// What the caller does in a combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// The fixtures as recorded: every utterance is one turn, spoken in one go.
    Continuous,
    /// Every utterance is cut in two at the middle of its audio and the halves are moved
    /// apart by a silence of this length (the caller pauses mid-sentence and keeps talking):
    /// the turn is the whole utterance, and the first half's STT final and partial are
    /// candidates that the rest of the sentence makes useless. The fixed silence of the policy
    /// is longer than the pause, so the turn goes on.
    Resumed(Duration),
}

impl Scenario {
    fn name(self) -> String {
        match self {
            Self::Continuous => "continuous".to_owned(),
            Self::Resumed(pause) => format!("resumed after {} ms", pause.as_millis()),
        }
    }
}

/// What to measure.
#[derive(Debug, Clone)]
pub struct SpecEvalConfig {
    /// Fixture names in `fixtures/`.
    pub fixtures: Vec<String>,
    /// Seeds (calls per fixture and combination).
    pub seeds: u64,
    /// Silence between the utterances, at least (as the V2V benchmark pads them).
    pub gap: Duration,
    /// How long a call stays open after its audio, for the bot to finish.
    pub linger: Duration,
    /// The policies.
    pub policies: Vec<Policy>,
    /// Median LLM first-token latencies to play everything with, in milliseconds.
    pub llm_first_token_ms: Vec<u64>,
    /// Stable-partial gates to play the partial modes with, besides no gate.
    pub gates: Vec<Duration>,
    /// Mid-sentence pauses of the resumed-speech scenario; none: that scenario is skipped.
    pub resume_pauses: Vec<Duration>,
    /// The policies of the resumed-speech scenario. Their silence must outlast the pause plus
    /// the quiet the cut lands next to (up to about 220 ms in the committed fixtures), or the
    /// turn ends inside the pause and the call is reported as failed; the default is 900 ms.
    pub resumed_policies: Vec<Policy>,
}

impl SpecEvalConfig {
    /// The three committed fixtures, 20 seeds, the adaptive policy and fixed silences of 450 and
    /// 700 ms.
    pub fn new() -> Self {
        Self {
            fixtures: ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"]
                .map(str::to_owned)
                .to_vec(),
            seeds: 20,
            gap: Duration::from_secs(4),
            linger: Duration::from_secs(6),
            policies: vec![
                Policy::Adaptive,
                Policy::Fixed(450 * MS),
                Policy::Fixed(700 * MS),
            ],
            llm_first_token_ms: vec![LLM_FIRST_TOKEN_MEDIAN_MS, FAST_LLM_FIRST_TOKEN_MS],
            gates: vec![100 * MS, 200 * MS],
            resume_pauses: vec![350 * MS, 400 * MS, 600 * MS],
            resumed_policies: vec![Policy::Fixed(900 * MS)],
        }
    }
}

impl Default for SpecEvalConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One combination's result.
#[derive(Debug, Clone)]
pub struct SpecRow {
    /// What the caller does.
    pub scenario: Scenario,
    /// The end-of-turn policy.
    pub policy: Policy,
    /// Speculation and the STT's interims.
    pub speculation: Speculation,
    /// Turns answered (a turn with a user-speech end and a first audio).
    pub turns: usize,
    /// Turns the fixtures had.
    pub turns_expected: usize,
    /// Calls that failed to run or did not end cleanly.
    pub failed: usize,
    /// Voice-to-voice, server side.
    pub v2v: Summary,
    /// Mean voice-to-voice in milliseconds.
    pub v2v_mean_ms: f64,
    /// Median LLM first-token latency of the combination in milliseconds.
    pub llm_first_token_ms: u64,
    /// LLM requests the provider received, over all calls.
    pub requests: usize,
    /// Estimated prompt tokens of those requests (4 characters per token, as the mock reports).
    pub prompt_tokens: u64,
    /// The engine's speculation counters, summed over the calls.
    pub counters: SpeculationStatsSnapshot,
}

impl SpecRow {
    #[allow(clippy::cast_precision_loss)] // counts far below 2^52
    fn per_turn(&self, n: u64) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            n as f64 / self.turns as f64
        }
    }
}

fn log_normal(median_ms: u64) -> LatencyProfile {
    LatencyProfile::LogNormal {
        median: Duration::from_millis(median_ms),
        sigma: 0.3,
    }
}

fn sum(into: &mut SpeculationStatsSnapshot, s: &SpeculationStatsSnapshot) {
    macro_rules! add {
        ($($f:ident),*) => { $(into.$f = into.$f.saturating_add(s.$f);)* };
    }
    add!(
        started,
        kept,
        hits,
        misses,
        context_changes,
        expired,
        superseded,
        withdrawn,
        abandoned,
        skipped_busy,
        skipped_flow,
        skipped_unsafe,
        failures,
        tool_phases,
        held_side_effects,
        wasted_output_tokens,
        wasted_input_tokens,
        head_start_ms
    );
}

/// Plays one call of `fixture` under `policy` and `speculation`.
async fn one_call(
    case: &Case,
    seed: u64,
    policy: Policy,
    speculation: Speculation,
    llm_first_token_ms: u64,
    linger: Duration,
) -> Result<OneCall, String> {
    let clock: SharedClock = Arc::new(SimClock::new());
    let session = SimSession::new(clock);
    let base = seed.wrapping_mul(4);
    let _ = session.stt.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::Fixed(100 * MS),
        log_normal(STT_FINAL_MEDIAN_MS),
        base.wrapping_add(1),
    );
    let _ = session.llm.clone().with_latency(
        log_normal(llm_first_token_ms),
        LatencyProfile::Fixed(30 * MS),
        base.wrapping_add(2),
    );
    let _ = session.tts.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        log_normal(TTS_FIRST_BYTE_MEDIAN_MS),
        LatencyProfile::Fixed(20 * MS),
        base.wrapping_add(3),
    );
    let fixture = &case.fixture;
    session.llm.set_responder(case.responder.clone());
    let (on, on_partial, interims, gate) = match speculation {
        Speculation::Off => (false, false, SttInterims::Lagging, Duration::ZERO),
        Speculation::On(interims) => (true, true, interims, Duration::ZERO),
        Speculation::OnFinal => (true, false, SttInterims::Lagging, Duration::ZERO),
        Speculation::OnGated(interims, gate) => (true, true, interims, gate),
    };
    interims.configure(&session.stt);
    let factory = match policy {
        Policy::Adaptive => EngineFactory::new().with_smart_turn(0.95),
        Policy::Fixed(silence) => EngineFactory::new().with_fixed_silence(silence),
    }
    .with_config(|config| {
        config.speculation = if on {
            brisvox_engine::SpeculationConfig {
                on_partial,
                partial_stable_for: gate,
                ..brisvox_engine::SpeculationConfig::ENABLED
            }
        } else {
            brisvox_engine::SpeculationConfig::OFF
        };
    });
    let run = session
        .run_with(&factory, fixture, linger)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = &run.outcome {
        return Err(format!("pipeline: {e}"));
    }
    let v2v: Vec<Duration> = run
        .turns
        .iter()
        .filter_map(|t| t.between(TurnEvent::UserSpeechEnd, TurnEvent::FirstAudioOut))
        .collect();
    let counters = factory
        .handles()
        .first()
        .map(|h| h.speculation.snapshot())
        .unwrap_or_default();
    let requests = session.llm.requests();
    let prompt_chars: usize = requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .map(|m| m.content.chars().count())
        .sum();
    Ok(OneCall {
        v2v,
        requests: requests.len(),
        prompt_tokens: u64::try_from(prompt_chars / 4).unwrap_or(u64::MAX),
        counters,
    })
}

/// A call to play: audio and the transcripts the mock STT hears (the fixture), the answers the
/// mock LLM gives, and the turns the call must produce.
struct Case {
    fixture: Fixture,
    responder: LlmResponder,
    turns: usize,
}

/// `fixture` with every segment cut in two at the middle of its audio, the halves `pause` apart
/// (zeros), and the transcript split after half of the words. A segment of one word, or too
/// short to cut, is kept as it is.
fn resumed_variant(fixture: &Fixture, pause: Duration) -> Fixture {
    let rate = u128::from(fixture.sample_rate.hz());
    let pause_samples =
        u64::try_from(pause.as_micros().saturating_mul(rate) / 1_000_000).unwrap_or(0);
    let total = u64::try_from(fixture.pcm.len() / 2).unwrap_or(u64::MAX);
    let bytes = |from: u64, to: u64| {
        let (a, b) = (
            usize::try_from(from.saturating_mul(2)).unwrap_or(usize::MAX),
            usize::try_from(to.saturating_mul(2)).unwrap_or(usize::MAX),
        );
        fixture.pcm.get(a..b).unwrap_or_default()
    };
    let mut pcm: Vec<u8> = Vec::with_capacity(fixture.pcm.len());
    let mut segments = Vec::with_capacity(fixture.segments.len().saturating_mul(2));
    let mut copied = 0u64; // original samples copied so far
    let mut shift = 0u64; // samples inserted so far
    for segment in &fixture.segments {
        let words: Vec<&str> = segment.text.split_whitespace().collect();
        let mid = segment
            .start
            .saturating_add(segment.end.saturating_sub(segment.start) / 2);
        let half = words.len() / 2;
        if half == 0 || mid <= segment.start || mid >= segment.end {
            pcm.extend_from_slice(bytes(copied, segment.end));
            copied = segment.end;
            segments.push(FixtureSegment {
                start: segment.start.saturating_add(shift),
                end: segment.end.saturating_add(shift),
                ..segment.clone()
            });
            continue;
        }
        let (first, second) = words.split_at(half);
        pcm.extend_from_slice(bytes(copied, mid));
        copied = mid;
        segments.push(FixtureSegment {
            start: segment.start.saturating_add(shift),
            end: mid.saturating_add(shift),
            text: first.join(" "),
            ..segment.clone()
        });
        let silence = usize::try_from(pause_samples.saturating_mul(2)).unwrap_or(0);
        pcm.resize(pcm.len().saturating_add(silence), 0);
        shift = shift.saturating_add(pause_samples);
        pcm.extend_from_slice(bytes(copied, segment.end));
        copied = segment.end;
        segments.push(FixtureSegment {
            start: mid.saturating_add(shift),
            end: segment.end.saturating_add(shift),
            text: second.join(" "),
            ..segment.clone()
        });
    }
    pcm.extend_from_slice(bytes(copied, total));
    Fixture {
        name: format!("{}-resumed-{}ms", fixture.name, pause.as_millis()),
        sample_rate: fixture.sample_rate,
        pcm: pcm.into(),
        segments,
    }
}

/// The modes of every scenario: off, from the partial at the VAD stop (with each gate and
/// without), and at the STT final only.
fn modes(gates: &[Duration]) -> Vec<Speculation> {
    let mut modes = vec![
        Speculation::Off,
        Speculation::On(SttInterims::Lagging),
        Speculation::On(SttInterims::Converged),
        Speculation::OnFinal,
    ];
    for &gate in gates {
        modes.push(Speculation::OnGated(SttInterims::Lagging, gate));
        modes.push(Speculation::OnGated(SttInterims::Converged, gate));
    }
    modes
}

/// What one call measured.
struct OneCall {
    v2v: Vec<Duration>,
    requests: usize,
    prompt_tokens: u64,
    counters: SpeculationStatsSnapshot,
}

/// Runs every combination.
///
/// # Errors
/// [`BenchError::Config`] for a fixture that cannot be loaded or no seeds, [`BenchError::Io`]
/// if the runtime cannot be created.
pub fn run(config: &SpecEvalConfig) -> Result<Vec<SpecRow>, BenchError> {
    if config.seeds == 0
        || config.fixtures.is_empty()
        || config.policies.is_empty()
        || config.llm_first_token_ms.is_empty()
    {
        return Err(BenchError::Config(
            "need at least one seed, one fixture, one policy and one LLM latency".into(),
        ));
    }
    let mut fixtures = Vec::with_capacity(config.fixtures.len());
    for name in &config.fixtures {
        let fixture = Fixture::load(&fixtures_dir(), name)
            .map_err(|e| BenchError::Config(format!("fixture {name}: {e}")))?;
        fixtures.push(pad_gaps(&fixture, config.gap));
    }
    let continuous: Vec<Case> = fixtures
        .iter()
        .map(|fixture| Case {
            fixture: fixture.clone(),
            responder: CallScript::of(fixture).responder(),
            turns: fixture.segments.len(),
        })
        .collect();
    // The caller's turn is the whole utterance: the answers are those of the original one.
    let resumed = |pause: Duration| -> Vec<Case> {
        fixtures
            .iter()
            .map(|fixture| Case {
                fixture: resumed_variant(fixture, pause),
                responder: CallScript::of(fixture).responder(),
                turns: fixture.segments.len(),
            })
            .collect()
    };
    let mut scenarios: Vec<(Scenario, Vec<Case>, &[Policy])> =
        vec![(Scenario::Continuous, continuous, &config.policies)];
    for &pause in &config.resume_pauses {
        scenarios.push((
            Scenario::Resumed(pause),
            resumed(pause),
            &config.resumed_policies,
        ));
    }
    let modes = modes(&config.gates);
    let mut rows = Vec::new();
    for (scenario, cases, policies) in &scenarios {
        for &llm_first_token_ms in &config.llm_first_token_ms {
            for &policy in *policies {
                for &speculation in &modes {
                    // A paused clock needs a current-thread runtime of its own.
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .start_paused(true)
                        .build()?;
                    let row = runtime.block_on(async {
                        let mut row = SpecRow {
                            scenario: *scenario,
                            policy,
                            speculation,
                            turns: 0,
                            turns_expected: 0,
                            failed: 0,
                            v2v: Summary::of(Vec::new()),
                            v2v_mean_ms: 0.0,
                            llm_first_token_ms,
                            requests: 0,
                            prompt_tokens: 0,
                            counters: SpeculationStatsSnapshot::default(),
                        };
                        let mut samples = Vec::new();
                        for case in cases {
                            for seed in 1..=config.seeds {
                                row.turns_expected += case.turns;
                                let call = one_call(
                                    case,
                                    seed,
                                    policy,
                                    speculation,
                                    llm_first_token_ms,
                                    config.linger,
                                )
                                .await;
                                match call {
                                    Ok(call) => {
                                        if call.v2v.len() == case.turns {
                                            samples.extend(call.v2v);
                                        } else {
                                            row.failed += 1;
                                        }
                                        row.requests += call.requests;
                                        row.prompt_tokens =
                                            row.prompt_tokens.saturating_add(call.prompt_tokens);
                                        sum(&mut row.counters, &call.counters);
                                    }
                                    Err(_) => row.failed += 1,
                                }
                            }
                        }
                        row.turns = samples.len();
                        #[allow(clippy::cast_precision_loss)] // a few thousand samples
                        if !samples.is_empty() {
                            row.v2v_mean_ms =
                                samples.iter().map(Duration::as_secs_f64).sum::<f64>() * 1_000.0
                                    / samples.len() as f64;
                        }
                        row.v2v = Summary::of(samples);
                        row
                    });
                    rows.push(row);
                }
            }
        }
    }
    Ok(rows)
}

/// Problems that make a run invalid (a combination with failed calls or unanswered turns).
pub fn problems(rows: &[SpecRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.failed > 0 || r.turns != r.turns_expected)
        .map(|r| {
            format!(
                "{} / {} / {} / LLM {} ms: {} failed calls, {} of {} turns answered",
                r.scenario.name(),
                r.policy.name(),
                r.speculation.name(),
                r.llm_first_token_ms,
                r.failed,
                r.turns,
                r.turns_expected
            )
        })
        .collect()
}

fn ms(d: Duration) -> String {
    format!("{:.0}", d.as_secs_f64() * 1_000.0)
}

/// The markdown report, with `environment` lines on top.
pub fn markdown(rows: &[SpecRow], config: &SpecEvalConfig, environment: &[String]) -> String {
    let mut out = String::from("# Speculative LLM start on the fixtures\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    let _ = writeln!(
        out,
        "\nThe production engine (`EngineFactory`) over mock providers on a paused clock; {} seeds \
         x {} fixtures. Latencies are log-normal (sigma 0.3) around STT final {} ms, TTS first \
         byte {} ms and the LLM first token of the row ({} ms by default, and a fast LLM of {} ms \
         that answers before the STT final), one seed per call and the same seeds in every row. \
         V2V is server side (`UserSpeechEnd` to `FirstAudioOut`), in ms. `lagging` interims: the \
         mock's own word prefixes, the last one a word short of the final (every partial \
         misses). `converged`: the full text is also an interim before the VAD stop (every \
         partial hits). Real STTs are between the two. `final only`: speculation at the STT \
         final, never from a partial. delta = p50 against the same policy and LLM latency with \
         speculation off.\n\nCost columns. `extra requests per turn` = (started - committed) / \
         turns, from the engine's counters: the speculative runs that were not used, each a full \
         prompt sent for nothing. It can reach 1.00 even when every turn commits (fixed 700 ms \
         with lagging interims starts a run at the partial, supersedes it at the final and \
         commits the second). `turns committed` = committed / turns. `run hit rate` = committed / \
         (committed + discarded), counted per run, not per turn. `wasted output tokens` are the \
         tokens a discarded run had buffered (provider-reported, else 4 bytes per token): nonzero \
         only when the run streamed before the miss was known. `wasted input tokens` are what the \
         provider reported for discarded runs: the mock reports usage when its stream reaches \
         the end, so a run cancelled before that reports none (0 for the slow LLM) and a run \
         that had finished by then does (the fast LLM). `prompt tokens per turn` is the estimate \
         (4 characters per token) over every request the \
         provider received, discarded ones included.\n\nStable-partial gate. `gate N ms` \
         rows offer the partial at the VAD stop only after the caller stayed silent, and the \
         partial text did not change, for N ms (`SpeculationConfig::partial_stable_for`); the \
         STT final of the stop supersedes the wait. Scenarios. `continuous`: the fixtures as \
         recorded. `resumed after P ms`: every utterance is cut in two at the middle of its \
         audio, the halves P ms apart, so the caller pauses mid-sentence and goes on; the turn \
         is the whole utterance (a fixed silence of 900 ms: longer than the pause plus the quiet the cut lands \
         next to), and the answers are the same. The VAD reports the stop 300 ms into the pause and the resumption about 40 ms \
         after the speech restarts, so the engine sees the resumption (P - 300 + 40) ms after \
         the stop. A candidate started by the first half is wasted: `extra requests per turn` \
         counts it, `withdrawn` in the second table the candidates taken back by the driver. \
         The first half's STT final (150 ms after the stop, median) is also a candidate and the \
         gate does not cover it.\n",
        config.seeds,
        config.fixtures.len(),
        STT_FINAL_MEDIAN_MS,
        TTS_FIRST_BYTE_MEDIAN_MS,
        LLM_FIRST_TOKEN_MEDIAN_MS,
        FAST_LLM_FIRST_TOKEN_MS,
    );
    out.push_str(concat!(
        "| scenario | LLM first token ms | policy | speculation | turns | V2V p50 | p90 | p99 | mean ",
        "| delta p50 | LLM requests per turn | extra requests per turn | turns committed ",
        "| run hit rate | mean head start ms | wasted output tokens per turn ",
        "| wasted input tokens per turn | prompt tokens per turn |\n",
        "|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    ));
    for row in rows {
        let base = rows.iter().find(|r| {
            r.scenario == row.scenario
                && r.policy == row.policy
                && r.llm_first_token_ms == row.llm_first_token_ms
                && r.speculation == Speculation::Off
        });
        let delta = match (base, row.speculation) {
            (Some(_), Speculation::Off) | (None, _) => "-".to_owned(),
            (Some(b), _) => {
                let d = row.v2v.p50.as_secs_f64() - b.v2v.p50.as_secs_f64();
                format!("{:+.0}", d * 1_000.0)
            }
        };
        let c = &row.counters;
        let decided = c.hits.saturating_add(c.discarded());
        #[allow(clippy::cast_precision_loss)] // counts far below 2^52
        let hit_rate = if decided == 0 {
            "-".to_owned()
        } else {
            format!("{:.0}%", 100.0 * c.hits as f64 / decided as f64)
        };
        #[allow(clippy::cast_precision_loss)] // counts far below 2^52
        let committed = if c.started == 0 || row.turns == 0 {
            "-".to_owned()
        } else {
            format!("{:.0}%", 100.0 * c.hits as f64 / row.turns as f64)
        };
        #[allow(clippy::cast_precision_loss)] // counts far below 2^52
        let head = if c.hits == 0 {
            "-".to_owned()
        } else {
            format!("{:.0}", c.head_start_ms as f64 / c.hits as f64)
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {}/{} | {} | {} | {} | {:.0} | {delta} | {:.2} | {:.2} \
             | {committed} | {hit_rate} | {head} | {:.2} | {:.1} | {:.0} |",
            row.scenario.name(),
            row.llm_first_token_ms,
            row.policy.name(),
            row.speculation.name(),
            row.turns,
            row.turns_expected,
            ms(row.v2v.p50),
            ms(row.v2v.p90),
            ms(row.v2v.p99),
            row.v2v_mean_ms,
            row.per_turn(u64::try_from(row.requests).unwrap_or(u64::MAX)),
            row.per_turn(c.started.saturating_sub(c.hits)),
            row.per_turn(c.wasted_output_tokens),
            row.per_turn(c.wasted_input_tokens),
            row.per_turn(row.prompt_tokens),
        );
    }
    out.push_str(concat!(
        "\nWhat became of the speculative runs (counts over all turns of the row):\n\n",
        "| scenario | LLM first token ms | policy | speculation | started | kept | committed ",
        "| missed | superseded | withdrawn | abandoned | other discarded | skipped (busy) ",
        "| failed |\n",
        "|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    ));
    for row in rows {
        let c = &row.counters;
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.scenario.name(),
            row.llm_first_token_ms,
            row.policy.name(),
            row.speculation.name(),
            c.started,
            c.kept,
            c.hits,
            c.misses,
            c.superseded,
            c.withdrawn,
            c.abandoned,
            c.context_changes.saturating_add(c.expired),
            c.skipped_busy,
            c.failures,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row of the default (slow) LLM.
    fn row(rows: &[SpecRow], policy: Policy, speculation: Speculation) -> &SpecRow {
        row_at(rows, policy, speculation, LLM_FIRST_TOKEN_MEDIAN_MS)
    }

    /// The row of a scenario with the default (slow) LLM.
    fn scenario_row(
        rows: &[SpecRow],
        scenario: Scenario,
        policy: Policy,
        speculation: Speculation,
    ) -> &SpecRow {
        rows.iter()
            .find(|r| {
                r.scenario == scenario
                    && r.policy == policy
                    && r.speculation == speculation
                    && r.llm_first_token_ms == LLM_FIRST_TOKEN_MEDIAN_MS
            })
            .unwrap()
    }

    fn row_at(
        rows: &[SpecRow],
        policy: Policy,
        speculation: Speculation,
        llm_first_token_ms: u64,
    ) -> &SpecRow {
        rows.iter()
            .find(|r| {
                r.scenario == Scenario::Continuous
                    && r.policy == policy
                    && r.speculation == speculation
                    && r.llm_first_token_ms == llm_first_token_ms
            })
            .unwrap()
    }

    /// The measurement behind the speculation default: what speculation gains and costs per policy, on a few
    /// seeds of one fixture (paused clock: no wall time).
    #[test]
    fn speculation_gains_the_stt_final_when_the_partial_converges_and_wastes_a_request_when_not() {
        let config = SpecEvalConfig {
            fixtures: vec!["en_two_utterances".to_owned()],
            seeds: 4,
            policies: vec![Policy::Adaptive, Policy::Fixed(700 * MS)],
            gates: Vec::new(),
            resume_pauses: Vec::new(),
            ..SpecEvalConfig::new()
        };
        let rows = run(&config).unwrap();
        assert_eq!(problems(&rows), Vec::<String>::new());
        let turns = 8; // 4 seeds x 2 utterances
        let lagging = Speculation::On(SttInterims::Lagging);
        let converged = Speculation::On(SttInterims::Converged);
        let off = row(&rows, Policy::Adaptive, Speculation::Off);
        assert_eq!(off.turns, turns);
        assert_eq!((off.requests, off.counters.started), (turns, 0));

        // A partial that converged to the final: every turn commits its speculation, one
        // request per turn, and the answer starts about one STT final earlier (150 ms median).
        let hit = row(&rows, Policy::Adaptive, converged);
        assert_eq!((hit.requests, hit.counters.hits), (turns, turns as u64));
        assert_eq!(hit.counters.discarded(), 0);
        assert!(
            hit.v2v.p50 + 100 * MS < off.v2v.p50,
            "{:?} vs {:?}",
            hit.v2v.p50,
            off.v2v.p50
        );

        // A partial a word short of the final: every speculation misses, so every turn costs
        // a second request and gains nothing under the adaptive policy.
        let miss = row(&rows, Policy::Adaptive, lagging);
        assert_eq!(
            (miss.counters.hits, miss.counters.misses),
            (0, turns as u64)
        );
        assert_eq!(miss.requests, 2 * turns);
        assert!(miss.v2v.p50 + 20 * MS > off.v2v.p50, "no gain from a miss");

        // At the STT final only, the adaptive policy never speculates (the verdict ends the
        // turn at the final itself), so it behaves like off: no gain, no cost.
        let final_only = row(&rows, Policy::Adaptive, Speculation::OnFinal);
        assert_eq!(
            (final_only.counters.started, final_only.requests),
            (0, turns)
        );

        // A fixed silence longer than the finals gate leaves the time between the final and
        // the end of the turn: speculating at the final uses it, without a partial.
        let fixed_off = row(&rows, Policy::Fixed(700 * MS), Speculation::Off);
        let fixed_final = row(&rows, Policy::Fixed(700 * MS), Speculation::OnFinal);
        assert_eq!(fixed_final.counters.hits, turns as u64);
        assert_eq!(fixed_final.requests, turns);
        assert!(fixed_final.v2v.p50 + 150 * MS < fixed_off.v2v.p50);
        // The slow LLM's first token (350 ms) comes after the miss is known (STT final, 150 ms):
        // a discarded run is cancelled before it streams anything.
        assert_eq!(miss.counters.wasted_output_tokens, 0);
    }

    /// The token waste is measured, not assumed: with a first token below the STT final, a
    /// discarded run has streamed into its buffer before the miss is known.
    #[test]
    fn a_fast_llm_wastes_output_tokens_on_a_miss_and_a_slow_one_does_not() {
        let config = SpecEvalConfig {
            fixtures: vec!["en_two_utterances".to_owned()],
            seeds: 4,
            policies: vec![Policy::Adaptive],
            gates: Vec::new(),
            resume_pauses: Vec::new(),
            ..SpecEvalConfig::new()
        };
        let rows = run(&config).unwrap();
        assert_eq!(problems(&rows), Vec::<String>::new());
        let lagging = Speculation::On(SttInterims::Lagging);
        let fast = row_at(&rows, Policy::Adaptive, lagging, FAST_LLM_FIRST_TOKEN_MS);
        let slow = row(&rows, Policy::Adaptive, lagging);
        assert!(fast.counters.misses > 0);
        assert!(
            fast.counters.wasted_output_tokens > 0,
            "a run that streamed before the miss wasted tokens: {:?}",
            fast.counters
        );
        assert_eq!(slow.counters.wasted_output_tokens, 0);
        // The extra requests carry their prompts: the estimate counts them.
        let off = row_at(
            &rows,
            Policy::Adaptive,
            Speculation::Off,
            FAST_LLM_FIRST_TOKEN_MS,
        );
        assert!(fast.prompt_tokens > off.prompt_tokens);
    }

    /// A caller who pauses mid-sentence and goes on. Without the gate, the partial at
    /// the VAD stop starts a request that the rest of the sentence makes useless; with a gate
    /// longer than the time to the resumption, the request is not started (what remains is the
    /// first half's STT final, which the STT marked stable and the gate does not hold). The gate
    /// costs the head start of a partial that did converge. The zero-waste proof for the partial
    /// is the 350 ms pause (final only wastes the same); the driver test
    /// `a_caller_who_resumes_inside_the_gate_costs_no_candidate_at_all` shows zero requests of
    /// any kind when no final is involved.
    #[test]
    fn the_gate_saves_the_request_of_a_caller_who_resumes_and_costs_head_start_otherwise() {
        let config = SpecEvalConfig {
            fixtures: vec!["en_two_utterances".to_owned()],
            seeds: 4,
            policies: vec![Policy::Adaptive],
            llm_first_token_ms: vec![LLM_FIRST_TOKEN_MEDIAN_MS],
            gates: vec![200 * MS],
            resume_pauses: vec![350 * MS, 400 * MS],
            resumed_policies: vec![Policy::Fixed(700 * MS)],
            ..SpecEvalConfig::new()
        };
        let rows = run(&config).unwrap();
        // The pause did not split the turns: every utterance is answered once.
        assert_eq!(problems(&rows), Vec::<String>::new());
        let turns = 8; // 4 seeds x 2 utterances
        let resumed = Scenario::Resumed(400 * MS);
        let fixed = Policy::Fixed(700 * MS);
        let converged = Speculation::On(SttInterims::Converged);
        let gated = Speculation::OnGated(SttInterims::Converged, 200 * MS);
        let off = scenario_row(&rows, resumed, fixed, Speculation::Off);
        assert_eq!(off.requests, turns);
        let extra = |r: &SpecRow| r.counters.started.saturating_sub(r.counters.hits);

        // Without the gate every turn pays for the partial of the first half, withdrawn when the
        // caller goes on.
        let plain = scenario_row(&rows, resumed, fixed, converged);
        assert!(
            plain.counters.withdrawn >= turns as u64 && extra(plain) >= turns as u64,
            "{:?}",
            plain.counters
        );
        // With the gate (200 ms, the resumption is seen about 140 ms after the stop) the partial
        // is never offered: at most the final-based candidate of a turn whose STT final beat
        // the resumption is wasted.
        let held = scenario_row(&rows, resumed, fixed, gated);
        assert!(
            extra(held) * 2 <= extra(plain)
                && held.counters.withdrawn * 2 <= plain.counters.withdrawn,
            "gated {:?} vs plain {:?}",
            held.counters,
            plain.counters
        );
        assert_eq!(held.turns, turns);

        // Zero waste from the partial: the resumption (about 90 ms after the stop at a 350 ms
        // pause) comes before the gate (200 ms) and before the STT final of the first half in
        // nearly every turn, so with the gate the only wasted request is the final's, exactly as
        // with `on_partial = false`: the partial adds no request and no withdrawn run to it.
        // (At 400 ms the final beats the resumption in some turns and the final-based candidate
        // is not gated, so the exact equality is stated for 350 ms.)
        let short = Scenario::Resumed(350 * MS);
        let final_only = scenario_row(&rows, short, fixed, Speculation::OnFinal);
        let short_plain = scenario_row(&rows, short, fixed, converged);
        let short_gated = scenario_row(&rows, short, fixed, gated);
        assert!(
            short_plain.counters.withdrawn >= turns as u64,
            "{:?}",
            short_plain.counters
        );
        assert!(
            extra(short_gated) <= extra(final_only)
                && short_gated.counters.withdrawn <= final_only.counters.withdrawn,
            "gated {:?} vs final only {:?}",
            short_gated.counters,
            final_only.counters
        );
        // Interims that lag behind the speech still cost requests (a word is missing at the
        // gate), far fewer than without the gate.
        let lagging = Speculation::On(SttInterims::Lagging);
        let lagging_gated = Speculation::OnGated(SttInterims::Lagging, 200 * MS);
        assert!(
            extra(scenario_row(&rows, short, fixed, lagging_gated)) * 3
                <= extra(scenario_row(&rows, short, fixed, lagging)),
        );

        // The price: on continuous speech a 200 ms gate is as long as the STT final takes (150
        // ms median), so the head start of a converged partial is gone (a final-only
        // configuration), while without the gate it is about one STT final.
        let adaptive = Policy::Adaptive;
        let continuous = Scenario::Continuous;
        let base = scenario_row(&rows, continuous, adaptive, Speculation::Off);
        let fast = scenario_row(&rows, continuous, adaptive, converged);
        let slow = scenario_row(&rows, continuous, adaptive, gated);
        assert!(fast.v2v.p50 + 100 * MS < base.v2v.p50);
        assert!(slow.v2v.p50 > fast.v2v.p50 + 60 * MS);
        assert_eq!(
            extra(slow),
            0,
            "a converged partial that waited is still a hit: {:?}",
            slow.counters
        );
    }

    #[test]
    fn the_report_names_every_row() {
        let config = SpecEvalConfig {
            fixtures: vec!["en_two_utterances".to_owned()],
            seeds: 1,
            policies: vec![Policy::Adaptive],
            gates: vec![200 * MS],
            resume_pauses: vec![400 * MS],
            ..SpecEvalConfig::new()
        };
        let rows = run(&config).unwrap();
        let md = markdown(&rows, &config, &["command: `test`".to_owned()]);
        for name in [
            "off",
            "on, lagging interims",
            "on, converged interims",
            "on, final only",
            "on, lagging interims, gate 200 ms",
            "on, converged interims, gate 200 ms",
        ] {
            assert!(
                md.contains(&format!("| continuous | 350 | adaptive | {name} |"))
                    && md.contains(&format!("| continuous | 100 | adaptive | {name} |")),
                "{name}: {md}"
            );
        }
        assert!(
            md.contains("| resumed after 400 ms | 350 | fixed 900 ms | on, final only |"),
            "{md}"
        );
        assert!(md.contains("- command: `test`"));
    }
}
