//! The sim-mode target: the engine itself behind the real websocket transport, in process.
//!
//! Every call the [`WebSocketServer`] accepts is served by the production assembly:
//! [`brisvox_engine::run_call`] over a [`TransportSession`] (pump, `AudioIn` with the VAD
//! worker, STT stage, turn driver, LLM runner, segmenter, TTS stage, `TransportOut`, the
//! session's output pacer), with the heavy work on a real [`InferencePool`]. Only the three
//! providers are mocks, the `brisvox_sim` ones, with **fixed** latencies, so every turn has the
//! same known floor:
//!
//! - the mock STT answers each `Finalize` with the transcript of the fixture's next utterance
//!   after [`ProviderLatencies::stt_final`];
//! - the mock LLM answers each turn after [`ProviderLatencies::llm_first_token`] with one short
//!   sentence in the language of the utterance (`Okay, got it.` / `Peki, anladım.`);
//! - the mock TTS speaks it (a quiet square wave, peak 1000 of 32768: audible to the client)
//!   after [`ProviderLatencies::tts_first_byte`].
//!
//! What is not production, unless the server is started with [`RealModels`] (`--models`, the
//! `models` feature): the VAD is the simulator's energy detector run by the production VAD
//! worker on the pool ([`brisvox_engine::sim::energy_vad_setup`]), and under
//! [`EndOfTurn::SmartTurn`] the Smart Turn model always answers "complete" ([`FixedTurnModel`];
//! its feature extraction is real and runs on the pool). With [`RealModels`] the VAD is the
//! Silero VAD and the model is Smart Turn v3.2, built like `brisvox-server` builds them. The
//! engine configuration is otherwise [`EngineConfig::DEFAULT`].
//!
//! # The floor
//! The ground-truth end of speech (EOS) is where the energy VAD's stop is backdated to. The VAD
//! reports the stop [`EngineTargetConfig::vad_stop`] later; the STT stage sends `Finalize` then,
//! and the turn driver holds the end of the turn until the final arrives (the finals gate):
//!
//! - [`EndOfTurn::SmartTurn`]: the turn ends at `vad_stop + stt_final` after EOS (the analysis
//!   runs alongside the final and is faster);
//! - [`EndOfTurn::FixedSilence`]`(s)`: at `max(s, vad_stop + stt_final)` (the silence counts from
//!   the backdated stop).
//!
//! The answer is one sentence in one token (`Okay, got it.`). Its opening clause (`Okay,`) is a
//! first chunk of its own (the engine's first chunk ends after one word), and its boundary
//! is decided inside the token (the segmenter's one-character lookahead is the space after the
//! comma), so the first TTS text leaves with the first token, `llm_first_token` after the end of
//! the turn, and the first audio `tts_first_byte` later ([`EngineTargetSettings::floor_ms`]).
//! (Earlier the first chunk needed three words and waited for the response's end, one
//! `llm_inter_token` later: the floor had that gap in it.) Everything a measurement shows above the floor is the
//! engine's own time (VAD windows, pool hops, the stages' queues and the 20 ms frame grid) plus
//! the websocket transport and the harness.
//!
//! # Scripts
//! The mock STT must know what the caller says, but a websocket call carries no fixture name:
//! [`EngineServer::expect`] stores the script of the next call, which the next accepted call
//! takes. The benchmark plays one call at a time, calls `expect` before each and
//! [`EngineServer::wait_idle`] after each, so scripts and calls cannot cross. A call accepted
//! without a script is served with an empty one (it hears nothing) and recorded as an error.
//!
//! The capacity server (`brisvox-bench engine-server`) cannot tell which call comes
//! next, since many are open at once and another process is the client:
//! [`EngineServer::set_default_script`] gives every call that finds no stored script the script
//! of one fixture, which all the ramp's calls replay.
//!
//! [`EngineServer::expect_barge_in`] stores a script whose answers are long
//! ([`ANSWER_LONG_EN`] / [`ANSWER_LONG_TR`], several seconds of speech), for the barge-in calls
//! the benchmark plays to measure the clear.
//!
//! # Stage times
//! Every call runs on a [`TimelineRecorder`] of its own. When the call ends, its finished turns
//! go into the server's [`StageCollector`] ([`EngineServer::stage_report`]): the engine's own
//! per-stage times (see [`super::stages`]).
//!
//! Bounds: at most [`EngineTargetConfig::max_calls`] calls at once (the websocket server refuses
//! more with HTTP 503), [`MAX_ERRORS`] recorded errors, the engine's bounded queues per call.
//! Every task hangs off the server's cancellation token.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::TurnPolicy;
use brisvox_core::{SharedClock, TimelineRecorder, TurnEvent, TurnRecord};
use brisvox_engine::offload::{default_smart_turn_threads, start_smart_turn_executor};
use brisvox_engine::sim::{FixedTurnModel, energy_vad_setup};
use brisvox_engine::speculation::normalize;
use brisvox_engine::{EngineConfig, Offload, SessionBuilder, SpeculationConfig, run_call};
use brisvox_inference::{InferencePool, PoolConfig, PoolHandle};
use brisvox_providers::Role;
use brisvox_sim::fixture::Fixture;
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::{LlmResponder, LlmScript, MockLlm};
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use brisvox_sim::mock_tts::MockTts;
use brisvox_sim::vad::VadConfig;
use brisvox_transport::websocket::{ServerConfig, WebSocketServer};
use brisvox_transport::{CallLink, SessionConfig, Transport, TransportSession};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::real_models::RealModels;
use super::report::CallKind;
use super::stages::{StageCollector, StageReport};
use crate::BenchError;

/// What the mock LLM answers an English utterance.
pub const ANSWER_EN: &str = "Okay, got it.";
/// What the mock LLM answers a Turkish utterance.
pub const ANSWER_TR: &str = "Peki, anladım.";
/// What the mock LLM answers an English utterance in a barge-in call: long enough (about five
/// seconds of mock speech) that the caller's next utterance always talks over it.
pub const ANSWER_LONG_EN: &str = "Okay, got it. Let me tell you a little more about that while \
                                  you listen, it will take a moment.";
/// The Turkish [`ANSWER_LONG_EN`].
pub const ANSWER_LONG_TR: &str = "Peki, anladım. Bununla ilgili size biraz daha bilgi vereyim, \
                                  siz dinlerken biraz sürecek.";
/// Errors kept for [`EngineServer::take_errors`]; later ones are counted only.
pub const MAX_ERRORS: usize = 16;
/// Scripts waiting for calls ([`EngineServer::expect_memory_calls`]); more are not kept.
pub const MAX_PENDING_SCRIPTS: usize = 64;
/// The probability the Smart Turn model of [`EndOfTurn::SmartTurn`] always answers.
const SMART_TURN_COMPLETE: f32 = 0.95;

/// RMS (PCM16 units) at which the mock STT hears speech: the energy VAD's default threshold.
pub const SPEECH_RMS: f64 = 500.0;

const MS: Duration = Duration::from_millis(1);

/// How the engine decides that the user's turn is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOfTurn {
    /// The engine default: the adaptive policy ([`TurnPolicy::adaptive`]) with a Smart Turn
    /// model that answers "complete" at every pause.
    SmartTurn,
    /// A fixed silence after the (backdated) VAD stop ([`TurnPolicy::fixed`]).
    FixedSilence(Duration),
}

/// How close the mock STT's last interim is to the final transcript when the VAD stops. Only
/// speculative generation (which starts the LLM at the VAD stop from the interim) cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SttInterims {
    /// The mock's own interims: growing word prefixes of the final, so the last one is a word
    /// short. A speculation started from it always misses (the worst case).
    #[default]
    Lagging,
    /// The full final text is also an interim, delivered before the VAD stop (a real STT that
    /// has converged by the time the caller pauses: the best case).
    Converged,
}

impl SttInterims {
    /// The name used in reports and flags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lagging => "lagging",
            Self::Converged => "converged",
        }
    }

    /// Sets `stt` to deliver this form of interims, timed from the speech (a frame at or above
    /// [`SPEECH_RMS`]) as a real STT's are, not from the first frame after the last `Finalize`.
    pub fn configure(self, stt: &MockStt) {
        stt.set_converged_interims(self == Self::Converged);
        stt.set_interims_from_speech(SPEECH_RMS);
    }
}

/// The mock providers' latencies; all fixed (see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLatencies {
    /// STT connect (at the call's start, not on the turn's path).
    pub stt_connect: Duration,
    /// Between interim transcripts.
    pub stt_interim: Duration,
    /// From `Finalize` to the final transcript.
    pub stt_final: Duration,
    /// From the request to the first LLM token.
    pub llm_first_token: Duration,
    /// Between LLM tokens.
    pub llm_inter_token: Duration,
    /// TTS connect (at the call's start, not on the turn's path).
    pub tts_connect: Duration,
    /// From the text to the first TTS audio.
    pub tts_first_byte: Duration,
    /// Between TTS audio chunks (40 ms of audio each).
    pub tts_chunk_interval: Duration,
}

impl Default for ProviderLatencies {
    /// The medians of the load simulation (`sim_load`): STT final 150 ms, LLM first token
    /// 350 ms, TTS first byte 150 ms; connects 50 ms, interims every 100 ms, a token every
    /// 30 ms, a TTS chunk every 20 ms.
    fn default() -> Self {
        Self {
            stt_connect: 50 * MS,
            stt_interim: 100 * MS,
            stt_final: Duration::from_millis(crate::sim_load::STT_FINAL_MEDIAN_MS),
            llm_first_token: Duration::from_millis(crate::sim_load::LLM_FIRST_TOKEN_MEDIAN_MS),
            llm_inter_token: 30 * MS,
            tts_connect: 50 * MS,
            tts_first_byte: Duration::from_millis(crate::sim_load::TTS_FIRST_BYTE_MEDIAN_MS),
            tts_chunk_interval: 20 * MS,
        }
    }
}

/// How the engine target is set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineTargetConfig {
    /// The end-of-turn policy.
    pub end_of_turn: EndOfTurn,
    /// RMS level (PCM16 units) at or above which a frame is speech for the energy VAD.
    pub vad_threshold: u16,
    /// Speech that starts a user turn.
    pub vad_start: Duration,
    /// Silence after which the VAD reports a stop.
    pub vad_stop: Duration,
    /// The mock providers' latencies.
    pub latencies: ProviderLatencies,
    /// Threads of the inference pool (VAD windows, Smart Turn, heavy builds). At least 1.
    pub pool_threads: usize,
    /// Jobs that may wait in each line of the inference pool's queue. `0` (the default) sizes it
    /// like `brisvox-server` does: `max(1024, 2 x max_calls)` ([`PoolConfig::default`] is 1024),
    /// so a capacity run measures the engine and not a smaller queue of the benchmark's own;
    /// any other value is used as given. Read it through [`Self::pool_queue_capacity`].
    pub pool_queue: usize,
    /// Threads of the dedicated Smart Turn executor that the real models' analyses run
    /// on, so that they never hold a thread of the pool the VAD windows use. `None` (the
    /// default): what `brisvox-server` runs, [`default_smart_turn_threads`]; `Some(0)`: no
    /// executor, the analyses share the pool with the VAD windows (the engine's earlier layout).
    /// Read it through [`Self::resolved_turn_threads`]: without real models (the Smart Turn
    /// stub) there is no executor whatever this says, so the mock benchmarks stay as measured.
    pub turn_threads: Option<usize>,
    /// Most calls served at once.
    pub max_calls: usize,
    /// Speculative generation ([`SpeculationConfig::ENABLED`]) instead of the engine default.
    pub speculation: bool,
    /// With speculation on: also at the VAD stop from the best partial transcript
    /// ([`SpeculationConfig::on_partial`]); `false` speculates at the STT final only.
    pub speculation_on_partial: bool,
    /// The stable-partial gate of the speculation
    /// (`SpeculationConfig::partial_stable_for`); zero offers the partial at the VAD stop at once.
    pub speculation_gate: Duration,
    /// How close the STT's last interim is to its final (see [`SttInterims`]).
    pub stt_interims: SttInterims,
}

impl Default for EngineTargetConfig {
    /// Smart Turn, the simulator's VAD (RMS 500, 40 ms to start, 300 ms to stop), the default
    /// latencies, two pool threads with the production queue (1024 jobs), four calls.
    fn default() -> Self {
        let vad = VadConfig::default();
        Self {
            end_of_turn: EndOfTurn::SmartTurn,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 500.0
            vad_threshold: vad.threshold.clamp(0.0, f64::from(u16::MAX)) as u16,
            vad_start: vad.start_after,
            vad_stop: vad.stop_after,
            latencies: ProviderLatencies::default(),
            pool_threads: 2,
            pool_queue: 0,
            turn_threads: None,
            max_calls: 4,
            speculation: EngineConfig::DEFAULT.speculation.enabled,
            speculation_on_partial: EngineConfig::DEFAULT.speculation.on_partial,
            speculation_gate: EngineConfig::DEFAULT.speculation.partial_stable_for,
            stt_interims: SttInterims::Lagging,
        }
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl EngineTargetConfig {
    /// The queue capacity the inference pool is started with: [`Self::pool_queue`] when set,
    /// else what `brisvox-server` uses, `max(1024, 2 x max_calls)` (a call has at most one VAD
    /// batch and a Smart Turn analysis or two outstanding, and a rejected VAD batch used to lose
    /// the call's VAD stream).
    pub fn pool_queue_capacity(&self) -> usize {
        if self.pool_queue > 0 {
            return self.pool_queue;
        }
        PoolConfig::default()
            .queue_capacity
            .max(self.max_calls.saturating_mul(2))
    }

    /// The threads of the Smart Turn executor this configuration starts: `0` (none) without real
    /// models or with a fixed-silence end of turn, else [`Self::turn_threads`], defaulting to
    /// the server's [`default_smart_turn_threads`] for [`Self::pool_threads`].
    pub fn resolved_turn_threads(&self, real_models: bool) -> usize {
        if !real_models || !matches!(self.end_of_turn, EndOfTurn::SmartTurn) {
            return 0;
        }
        self.turn_threads
            .unwrap_or_else(|| default_smart_turn_threads(self.pool_threads.max(1)))
    }

    /// The report form of the configuration, with the floors.
    pub fn settings(&self) -> EngineTargetSettings {
        let l = &self.latencies;
        let (end_of_turn, turn_silence_ms) = match self.end_of_turn {
            EndOfTurn::SmartTurn => ("smart_turn".to_owned(), None),
            EndOfTurn::FixedSilence(silence) => ("fixed_silence".to_owned(), Some(ms(silence))),
        };
        let gate = ms(self.vad_stop).saturating_add(ms(l.stt_final));
        let end_of_turn_floor_ms = turn_silence_ms.map_or(gate, |silence| silence.max(gate));
        EngineTargetSettings {
            end_of_turn,
            turn_silence_ms,
            vad_threshold_rms: self.vad_threshold,
            vad_start_ms: ms(self.vad_start),
            vad_stop_ms: ms(self.vad_stop),
            stt_interim_ms: ms(l.stt_interim),
            stt_final_ms: ms(l.stt_final),
            llm_first_token_ms: ms(l.llm_first_token),
            llm_inter_token_ms: ms(l.llm_inter_token),
            tts_first_byte_ms: ms(l.tts_first_byte),
            tts_chunk_interval_ms: ms(l.tts_chunk_interval),
            pool_threads: self.pool_threads.max(1),
            pool_queue: self.pool_queue_capacity(),
            smart_turn_threads: 0,
            speculation: self.speculation,
            speculation_on_partial: self.speculation && self.speculation_on_partial,
            speculation_gate_ms: if self.speculation && self.speculation_on_partial {
                ms(self.speculation_gate)
            } else {
                0
            },
            stt_interims: self.stt_interims.as_str().to_owned(),
            end_of_turn_floor_ms,
            floor_ms: end_of_turn_floor_ms
                .saturating_add(ms(l.llm_first_token))
                .saturating_add(ms(l.tts_first_byte)),
            real_models: false,
        }
    }

    /// The engine configuration of a call: the engine default with this end-of-turn policy and
    /// the energy VAD's hangover known to the barge-in guards.
    pub fn engine_config(&self) -> EngineConfig {
        let mut config = EngineConfig::DEFAULT;
        config.turn = match self.end_of_turn {
            EndOfTurn::SmartTurn => TurnPolicy::adaptive(),
            EndOfTurn::FixedSilence(silence) => TurnPolicy::fixed(silence),
        };
        // The energy VAD reports a stop `vad_stop` after the speech (as the simulator's
        // EngineFactory sets it).
        config.interruption.vad_hangover = self.vad_stop;
        config.speculation = if self.speculation {
            SpeculationConfig {
                on_partial: self.speculation_on_partial,
                partial_stable_for: self.speculation_gate,
                ..SpeculationConfig::ENABLED
            }
        } else {
            SpeculationConfig::OFF
        };
        config
    }

    /// The energy VAD's configuration.
    fn vad(&self) -> VadConfig {
        VadConfig {
            threshold: f64::from(self.vad_threshold),
            start_after: self.vad_start,
            stop_after: self.vad_stop,
        }
    }

    /// The builder of one call over fresh mock providers scripted with `script`, recording its
    /// turns on `timeline`.
    ///
    /// # Errors
    /// The engine's verdict on [`Self::engine_config`] (not expected: it is validated).
    pub fn builder(
        &self,
        clock: &SharedClock,
        script: &CallScript,
        offload: Offload,
        seed: u64,
        timeline: TimelineRecorder,
    ) -> Result<SessionBuilder<MockStt, MockLlm, MockTts>, BenchError> {
        self.builder_with(clock, script, offload, seed, timeline, None)
    }

    /// [`Self::builder`] with the real models when `models` is given: the Silero VAD in
    /// place of the energy VAD (the engine's own VAD worker settings, so `vad_*` of this
    /// configuration do not apply and the barge-in guards keep the engine's default VAD
    /// hangover), and the real Smart Turn model under [`EndOfTurn::SmartTurn`] in place of the
    /// fixed stub.
    ///
    /// # Errors
    /// The engine's verdict on [`Self::engine_config`] (not expected: it is validated).
    pub fn builder_with(
        &self,
        clock: &SharedClock,
        script: &CallScript,
        offload: Offload,
        seed: u64,
        timeline: TimelineRecorder,
        models: Option<&RealModels>,
    ) -> Result<SessionBuilder<MockStt, MockLlm, MockTts>, BenchError> {
        let l = &self.latencies;
        let fixed = LatencyProfile::Fixed;
        let stt = MockStt::new(clock.clone()).with_latency(
            fixed(l.stt_connect),
            fixed(l.stt_interim),
            fixed(l.stt_final),
            seed,
        );
        self.stt_interims.configure(&stt);
        for utterance in &script.utterances {
            stt.push(utterance.clone());
        }
        let llm = MockLlm::new(clock.clone()).with_latency(
            fixed(l.llm_first_token),
            fixed(l.llm_inter_token),
            seed,
        );
        // The answer follows the request, not the order of requests: a speculative request that
        // is discarded must not consume the next turn's answer.
        llm.set_responder(script.responder());
        let tts = MockTts::new(clock.clone()).with_latency(
            fixed(l.tts_connect),
            fixed(l.tts_first_byte),
            fixed(l.tts_chunk_interval),
            seed,
        );
        let mut config = self.engine_config();
        if models.is_some() {
            // The energy VAD's hangover (see `engine_config`) is not the real VAD's.
            config.interruption.vad_hangover = EngineConfig::DEFAULT.interruption.vad_hangover;
        }
        let builder = SessionBuilder::new(
            config,
            clock.clone(),
            timeline,
            offload.clone(),
            Arc::new(stt),
            Arc::new(llm),
            Arc::new(tts),
        )
        .map_err(|e| BenchError::Config(format!("engine configuration: {e}")))?;
        let builder = match models {
            Some(models) => models.with_vad(builder, offload, &config),
            None => builder.with_vad(energy_vad_setup(offload, &self.vad(), &config)),
        };
        Ok(match (self.end_of_turn, models) {
            (EndOfTurn::SmartTurn, Some(models)) => builder.with_smart_turn(models.smart_turn()),
            (EndOfTurn::SmartTurn, None) => {
                builder.with_smart_turn(Arc::new(FixedTurnModel(SMART_TURN_COMPLETE)))
            }
            (EndOfTurn::FixedSilence(_), _) => builder,
        })
    }
}

/// The engine target's configuration as recorded in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTargetSettings {
    /// `smart_turn` or `fixed_silence`.
    pub end_of_turn: String,
    /// The fixed silence, in milliseconds (`fixed_silence` only).
    pub turn_silence_ms: Option<u64>,
    /// RMS level at or above which a frame is speech.
    pub vad_threshold_rms: u16,
    /// Speech that starts a turn, in milliseconds.
    pub vad_start_ms: u64,
    /// Silence after which the VAD reports a stop, in milliseconds.
    pub vad_stop_ms: u64,
    /// Between STT interims, in milliseconds.
    pub stt_interim_ms: u64,
    /// From `Finalize` to the STT final, in milliseconds.
    pub stt_final_ms: u64,
    /// To the first LLM token, in milliseconds.
    pub llm_first_token_ms: u64,
    /// Between LLM tokens, in milliseconds.
    pub llm_inter_token_ms: u64,
    /// To the first TTS audio, in milliseconds.
    pub tts_first_byte_ms: u64,
    /// Between TTS chunks, in milliseconds.
    pub tts_chunk_interval_ms: u64,
    /// Inference pool threads.
    pub pool_threads: usize,
    /// Jobs that may wait in the inference pool's queue (0: not recorded, a report from before
    /// the queue was a setting; it was 256 then).
    #[serde(default)]
    pub pool_queue: usize,
    /// Threads of the dedicated Smart Turn executor; 0: the analyses shared the pool
    /// with the VAD windows (or ran on the stub). Absent in older reports: 0.
    #[serde(default)]
    pub smart_turn_threads: usize,
    /// Whether speculative generation was on.
    #[serde(default)]
    pub speculation: bool,
    /// Whether speculation also started at the VAD stop from the partial transcript (`false`:
    /// at the STT final only, or speculation off).
    #[serde(default)]
    pub speculation_on_partial: bool,
    /// The stable-partial gate in milliseconds (0: none, or speculation not from a partial).
    #[serde(default)]
    pub speculation_gate_ms: u64,
    /// The STT's interims, `lagging` or `converged` ([`SttInterims`]).
    #[serde(default)]
    pub stt_interims: String,
    /// The earliest end of the turn after the end of speech, in milliseconds (module docs).
    pub end_of_turn_floor_ms: u64,
    /// The least V2V the configuration allows: the end-of-turn floor plus the LLM's first token
    /// (the answer's first clause leaves with it, see the module docs) and the TTS's first byte,
    /// in milliseconds.
    pub floor_ms: u64,
    /// The calls ran the real Silero VAD and Smart Turn v3.2 (`--models`), so the
    /// `vad_*` fields above (the energy VAD's) did not apply. Absent in older reports: `false`.
    #[serde(default)]
    pub real_models: bool,
}

/// What the caller of one call says and what the mock LLM answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallScript {
    /// The transcripts, one per utterance, in order.
    pub utterances: Vec<SttUtterance>,
    /// The answers, one per utterance, in order.
    pub answers: Vec<&'static str>,
    /// Why the call is played: decides which stages its turns feed (see [`super::stages`]);
    /// a barge-in call gets the long answers.
    pub kind: CallKind,
}

impl CallScript {
    /// The mock LLM's answer by request: the answer of the utterance whose transcript the
    /// request's last user message is (or a prefix of: a speculative request carries the
    /// interim). No match falls through to the mock's empty fallback (the call then shows no
    /// response, which the benchmark reports).
    pub fn responder(&self) -> LlmResponder {
        let table: Vec<(String, &'static str)> = self
            .utterances
            .iter()
            .zip(&self.answers)
            .map(|(utterance, answer)| (normalize(&utterance.finals.join(" ")), *answer))
            .collect();
        Arc::new(move |request| {
            let user = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| normalize(&m.content))?;
            if user.is_empty() {
                return None;
            }
            let answer = table
                .iter()
                .find(|(text, _)| *text == user)
                .or_else(|| table.iter().find(|(text, _)| text.starts_with(&user)))
                .map(|(_, answer)| *answer)?;
            Some(LlmScript::text([answer]))
        })
    }

    /// The script of `fixture`: its transcripts, and an answer in each utterance's language.
    pub fn of(fixture: &Fixture) -> Self {
        Self {
            utterances: fixture.utterances(),
            answers: fixture
                .segments
                .iter()
                .map(|s| if s.lang == "tr" { ANSWER_TR } else { ANSWER_EN })
                .collect(),
            kind: CallKind::Latency,
        }
    }

    /// The script of a barge-in call of `fixture`: its transcripts, and a long answer
    /// ([`ANSWER_LONG_EN`] / [`ANSWER_LONG_TR`]) in each utterance's language.
    pub fn barge_in(fixture: &Fixture) -> Self {
        Self {
            utterances: fixture.utterances(),
            answers: fixture
                .segments
                .iter()
                .map(|s| {
                    if s.lang == "tr" {
                        ANSWER_LONG_TR
                    } else {
                        ANSWER_LONG_EN
                    }
                })
                .collect(),
            kind: CallKind::BargeIn,
        }
    }
}

/// State shared by the server handle and its tasks.
struct Shared {
    config: EngineTargetConfig,
    /// The real models every call runs, or `None`: the energy VAD and the Smart Turn stub.
    models: Option<RealModels>,
    pool: PoolHandle,
    /// The Smart Turn executor's handle when the real models run on one.
    turn_pool: Option<PoolHandle>,
    /// The scripts of the next accepted calls, in order (at most [`MAX_PENDING_SCRIPTS`]).
    next: Mutex<VecDeque<CallScript>>,
    /// The script of a call that finds [`Self::next`] empty (the capacity server, where every
    /// call plays the same fixture and nobody can say which call comes next), if set.
    default_script: Mutex<Option<CallScript>>,
    /// The first [`MAX_ERRORS`] errors not yet taken, and how many more were dropped.
    errors: Mutex<(Vec<String>, usize)>,
    /// Calls being served.
    active: watch::Sender<usize>,
    /// The stage times of the calls' finished turns.
    stages: Mutex<StageCollector>,
}

impl Shared {
    fn record_error(&self, error: String) {
        let mut errors = self.errors.lock().unwrap_or_else(PoisonError::into_inner);
        if errors.0.len() < MAX_ERRORS {
            errors.0.push(error);
        } else {
            errors.1 = errors.1.saturating_add(1);
        }
    }
}

/// Counts a call as active from its accept until its task ends (panics included).
struct ActiveCall(Arc<Shared>);

impl ActiveCall {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.active.send_modify(|n| *n = n.saturating_add(1));
        Self(Arc::clone(shared))
    }
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.active.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// A running engine server; see the [module docs](self).
pub struct EngineServer {
    addr: std::net::SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    clock: SharedClock,
    shared: Arc<Shared>,
    /// Taken by [`Self::shutdown`].
    pool: Mutex<Option<InferencePool>>,
    /// The Smart Turn executor, taken by [`Self::shutdown`].
    turn_pool: Mutex<Option<InferencePool>>,
}

impl EngineServer {
    /// Starts the inference pool and the server on a free port of 127.0.0.1, as a child of
    /// `parent`.
    ///
    /// # Errors
    /// [`BenchError::Config`] if the pool or the websocket server cannot be started.
    pub async fn start(
        config: EngineTargetConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self, BenchError> {
        Self::start_on(config, ([127, 0, 0, 1], 0).into(), clock, parent).await
    }

    /// [`Self::start`] on `bind_addr` (the standalone capacity server, where another process is
    /// the client).
    ///
    /// # Errors
    /// [`BenchError::Config`] if the pool or the websocket server cannot be started.
    pub async fn start_on(
        config: EngineTargetConfig,
        bind_addr: std::net::SocketAddr,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self, BenchError> {
        Self::start_with(config, bind_addr, None, clock, parent).await
    }

    /// [`Self::start_on`] with the real models for every call when `models` is given.
    ///
    /// # Errors
    /// [`BenchError::Config`] if the pool or the websocket server cannot be started.
    pub async fn start_with(
        config: EngineTargetConfig,
        bind_addr: std::net::SocketAddr,
        models: Option<RealModels>,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self, BenchError> {
        let pool = InferencePool::new(
            &PoolConfig {
                threads: config.pool_threads.max(1),
                queue_capacity: config.pool_queue_capacity(),
                name: "v2v-engine-pool".to_owned(),
            },
            clock.clone(),
        )
        .map_err(|e| BenchError::Config(format!("cannot start the inference pool: {e}")))?;
        // The same executor the server starts, only with the real models: the stub's analyses
        // are microseconds and the mock benchmarks keep their measured shape.
        let turn_pool = match start_smart_turn_executor(
            config.resolved_turn_threads(models.is_some()),
            clock.clone(),
        ) {
            Ok(turn_pool) => turn_pool,
            Err(e) => {
                let _ = tokio::task::spawn_blocking(move || pool.shutdown()).await;
                return Err(BenchError::Config(format!(
                    "cannot start the Smart Turn executor: {e}"
                )));
            }
        };
        let server_config = ServerConfig {
            bind_addr,
            max_calls: config.max_calls.max(1),
            ..ServerConfig::default()
        };
        let cancel = parent.child_token();
        let server = match WebSocketServer::bind(server_config, clock.clone(), &cancel).await {
            Ok(server) => server,
            Err(e) => {
                // Joining the pool's idle threads is quick, but it is a join: off the worker.
                let _ = tokio::task::spawn_blocking(move || pool.shutdown()).await;
                return Err(BenchError::Config(format!(
                    "cannot start the engine server: {e}"
                )));
            }
        };
        let addr = server.local_addr();
        let shared = Arc::new(Shared {
            config,
            models,
            pool: pool.handle(),
            turn_pool: turn_pool.as_ref().map(InferencePool::handle),
            next: Mutex::new(VecDeque::new()),
            default_script: Mutex::new(None),
            errors: Mutex::new((Vec::new(), 0)),
            active: watch::Sender::new(0),
            stages: Mutex::new(StageCollector::default()),
        });
        let tracker = TaskTracker::new();
        tracker.spawn(accept_loop(
            server,
            Arc::clone(&shared),
            clock.clone(),
            cancel.clone(),
            tracker.clone(),
        ));
        Ok(Self {
            addr,
            cancel,
            tracker,
            clock,
            shared,
            pool: Mutex::new(Some(pool)),
            turn_pool: Mutex::new(turn_pool),
        })
    }

    /// The websocket URL of the server.
    pub fn url(&self) -> String {
        format!("ws://{}/call", self.addr)
    }

    /// The configuration the server runs.
    pub fn config(&self) -> &EngineTargetConfig {
        &self.shared.config
    }

    /// Every accepted call that finds no pending script plays `fixture` (the capacity server:
    /// all concurrent calls play the same fixture, so any call can take any script).
    pub fn set_default_script(&self, fixture: &Fixture) {
        *self
            .shared
            .default_script
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(CallScript::of(fixture));
    }

    /// The address the server listens on.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// The next accepted call plays `fixture` (replaces the scripts no call has taken).
    pub fn expect(&self, fixture: &Fixture) {
        self.expect_scripts(std::iter::once(CallScript::of(fixture)));
    }

    /// The next accepted call is a barge-in call of `fixture` ([`CallScript::barge_in`]).
    pub fn expect_barge_in(&self, fixture: &Fixture) {
        self.expect_scripts(std::iter::once(CallScript::barge_in(fixture)));
    }

    /// The next `calls` accepted calls are memory calls of `fixture` (at most
    /// [`MAX_PENDING_SCRIPTS`]), for calls the harness opens at once.
    pub fn expect_memory_calls(&self, fixture: &Fixture, calls: usize) {
        let mut script = CallScript::of(fixture);
        script.kind = CallKind::Memory;
        self.expect_scripts(std::iter::repeat_n(script, calls));
    }

    /// Replaces the pending scripts with `scripts` (the first [`MAX_PENDING_SCRIPTS`]).
    fn expect_scripts(&self, scripts: impl Iterator<Item = CallScript>) {
        let mut next = self
            .shared
            .next
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        next.clear();
        next.extend(scripts.take(MAX_PENDING_SCRIPTS));
    }

    /// The engine's stage times over the calls served so far (memory is the caller's to add).
    pub fn stage_report(&self) -> StageReport {
        self.shared
            .stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .report(&self.shared.config.settings())
    }

    /// The inference pool's counters (zero once it has shut down); the capacity server's
    /// `--stats-every-ms` line is made of them.
    pub fn pool_metrics(&self) -> brisvox_inference::PoolMetrics {
        self.shared.pool.metrics()
    }

    /// The Smart Turn executor's counters, `None` when the analyses share the pool.
    pub fn turn_pool_metrics(&self) -> Option<brisvox_inference::PoolMetrics> {
        self.shared.turn_pool.as_ref().map(PoolHandle::metrics)
    }

    /// The real models every call runs, if the server was started with them.
    pub fn models(&self) -> Option<&RealModels> {
        self.shared.models.as_ref()
    }

    /// Calls being served now.
    pub fn active_calls(&self) -> usize {
        *self.shared.active.borrow()
    }

    /// Waits until no call is being served, at most `timeout`. Returns whether it is idle.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let mut active = self.shared.active.subscribe();
        let idle = tokio::select! {
            biased;
            idle = active.wait_for(|n| *n == 0) => idle.is_ok(),
            () = self.clock.sleep(timeout) => false,
        };
        idle || self.active_calls() == 0
    }

    /// The errors of calls (setup failures, pipeline failures, calls without a script) since the
    /// last time, with a note of the ones that did not fit.
    pub fn take_errors(&self) -> Vec<String> {
        let mut errors = self
            .shared
            .errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (mut taken, dropped) = (std::mem::take(&mut errors.0), errors.1);
        errors.1 = 0;
        if dropped > 0 {
            taken.push(format!("{dropped} more errors"));
        }
        taken
    }

    /// Cancels every call, waits up to `timeout` for the tasks to end, then stops the inference
    /// pool (joined on a blocking thread).
    ///
    /// # Errors
    /// [`BenchError::Call`] if tasks were still running after `timeout` (they stay cancelled;
    /// the pool is then left to wind down by itself).
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), BenchError> {
        self.cancel.cancel();
        self.tracker.close();
        let stopped = tokio::select! {
            biased;
            () = self.tracker.wait() => true,
            () = self.clock.sleep(timeout) => false,
        };
        if !stopped {
            return Err(BenchError::Call(
                "the engine server did not stop in time".to_owned(),
            ));
        }
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let turn_pool = self
            .turn_pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        for pool in pool.into_iter().chain(turn_pool) {
            tokio::task::spawn_blocking(move || pool.shutdown())
                .await
                .map_err(|e| BenchError::Call(format!("inference pool shutdown: {e}")))?;
        }
        Ok(())
    }
}

impl Drop for EngineServer {
    fn drop(&mut self) {
        // The pool's threads end once its last handle is gone and their queue is drained; the
        // `InferencePool` itself is dropped without a join (no blocking in drop).
        self.cancel.cancel();
    }
}

/// Takes calls from the transport until it is shut down or cancelled.
async fn accept_loop(
    mut server: WebSocketServer,
    shared: Arc<Shared>,
    clock: SharedClock,
    cancel: CancellationToken,
    tracker: TaskTracker,
) {
    let mut call = 0u64;
    while let Ok(Some(link)) = server.accept(&cancel).await {
        let active = ActiveCall::new(&shared);
        let pending = shared
            .next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let script = pending.or_else(|| {
            shared
                .default_script
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        });
        tracker.spawn(serve_call(
            clock.clone(),
            link,
            script,
            call,
            active,
            cancel.child_token(),
        ));
        call = call.wrapping_add(1);
    }
    let _ = server.shutdown(Duration::from_secs(2)).await;
}

/// Serves one call; records its error, if any.
async fn serve_call(
    clock: SharedClock,
    link: CallLink,
    script: Option<CallScript>,
    call: u64,
    active: ActiveCall,
    cancel: CancellationToken,
) {
    let shared = Arc::clone(&active.0);
    let script = script.unwrap_or_else(|| {
        shared.record_error(format!("call {call} arrived without a script"));
        CallScript::default()
    });
    if let Err(error) = run_engine_call(&clock, link, &shared, &script, call, &cancel).await {
        shared.record_error(format!("call {call}: {error}"));
    }
    drop(active);
}

/// Opens the transport session, starts the engine on it and waits until the call ends (the
/// caller hangs up) or the server is cancelled (then the call is stopped, bounded by the
/// engine's stop timeout).
async fn run_engine_call(
    clock: &SharedClock,
    link: CallLink,
    shared: &Shared,
    script: &CallScript,
    call: u64,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let session_config = SessionConfig {
        seed: call,
        // A diagnostic, not a setting: `BRISVOX_BENCH_PACER_PHASE=1` brings back the
        // pacer's per-call phase on the first frame (the original behaviour), for a
        // before/after run of the same binary.
        first_frame_phase: std::env::var_os("BRISVOX_BENCH_PACER_PHASE").is_some_and(|v| v == "1"),
        ..SessionConfig::default()
    };
    let session = TransportSession::open(clock.clone(), link, session_config, cancel)
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let timeline = TimelineRecorder::new();
    let builder = shared
        .config
        .builder_with(
            clock,
            script,
            Offload::Pool(shared.pool.clone()),
            call,
            timeline.clone(),
            shared.models.as_ref(),
        )
        .map_err(|e| e.to_string())?;
    let builder = match &shared.turn_pool {
        Some(turn_pool) => builder.with_smart_turn_offload(Offload::Pool(turn_pool.clone())),
        None => builder,
    };
    let mut running = run_call(session, builder, cancel)
        .await
        .map_err(|e| format!("start: {e}"))?;
    let ended = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            let _ = running.stop().await;
            Ok(())
        }
        ended = running.wait() => ended.map_err(|e| format!("pipeline: {e}")),
    };
    // Every stage has stopped: the timeline is complete.
    timeline.finish();
    let turns = timeline.drain_finished();
    if turn_log_enabled() {
        for turn in &turns {
            eprintln!("{}", turn_log_line(call, turn));
        }
    }
    let speculation = running.handles().speculation.snapshot();
    let mut stages = shared.stages.lock().unwrap_or_else(PoisonError::into_inner);
    for turn in &turns {
        stages.add(turn, script.kind);
    }
    stages.add_speculation(&speculation, script.kind);
    drop(stages);
    ended
}

/// Whether `BRISVOX_BENCH_TURN_LOG=1`: every finished turn of every call is printed to stderr
/// as one line (a diagnostic for "where do the milliseconds go", off by default).
fn turn_log_enabled() -> bool {
    std::env::var_os("BRISVOX_BENCH_TURN_LOG").is_some_and(|v| v == "1")
}

/// One line per turn: every recorded event as `name=+ms`, relative to the engine's
/// `UserSpeechEnd` (the VAD's backdated last speech window), in microsecond precision.
fn turn_log_line(call: u64, turn: &TurnRecord) -> String {
    use std::fmt::Write as _;
    let mut line = format!("turn-log call={call} epoch={:?}", turn.epoch());
    let Some(origin) = turn.at(TurnEvent::UserSpeechEnd) else {
        line.push_str(" (no UserSpeechEnd)");
        return line;
    };
    for event in TurnEvent::ALL {
        if let Some(at) = turn.at(event) {
            let us = if at >= origin {
                i128::try_from(at.duration_since(origin).as_micros()).unwrap_or(i128::MAX)
            } else {
                -i128::try_from(origin.duration_since(at).as_micros()).unwrap_or(i128::MAX)
            };
            let _ = write!(
                line,
                " {}={}.{:03}",
                event.name(),
                us / 1000,
                (us % 1000).abs()
            );
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_turn_log_line_gives_every_event_relative_to_the_end_of_speech() {
        use brisvox_core::{Clock, SimClock, TurnEpoch};

        let rec = TimelineRecorder::new();
        let t0 = SimClock::new().now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let e = TurnEpoch::new(0);
        rec.mark(e, TurnEvent::UserSpeechStart, at(0));
        rec.mark(e, TurnEvent::UserSpeechEnd, at(1_000));
        rec.mark_open_turn(TurnEvent::VadStop, at(1_090));
        rec.mark(e, TurnEvent::TurnEnd, at(1_240));
        rec.mark(e, TurnEvent::FirstAudioOut, at(1_800));
        rec.finish();
        let records = rec.drain_finished();
        let line = turn_log_line(7, &records[0]);
        assert!(line.starts_with("turn-log call=7 "), "{line}");
        assert!(line.contains(" user_speech_start=-1000.000"), "{line}");
        assert!(line.contains(" user_speech_end=0.000"), "{line}");
        assert!(line.contains(" vad_stop=90.000"), "{line}");
        assert!(line.contains(" turn_end=240.000"), "{line}");
        assert!(line.contains(" first_audio_out=800.000"), "{line}");
        assert!(
            !line.contains("llm_request"),
            "events that did not happen are absent: {line}"
        );
    }

    #[test]
    fn the_pool_queue_follows_max_calls_like_the_server_unless_set() {
        let sized = |pool_queue, max_calls| {
            EngineTargetConfig {
                pool_queue,
                max_calls,
                ..EngineTargetConfig::default()
            }
            .pool_queue_capacity()
        };
        assert_eq!(sized(0, 4), 1024, "the production default is the floor");
        assert_eq!(sized(0, 512), 1024);
        assert_eq!(sized(0, 2000), 4000, "2 x max_calls above 512 calls");
        assert_eq!(sized(77, 2000), 77, "an explicit --pool-queue wins");
        assert_eq!(
            EngineTargetConfig {
                max_calls: 2000,
                ..EngineTargetConfig::default()
            }
            .settings()
            .pool_queue,
            4000,
            "the report shows the capacity actually used"
        );
    }

    #[test]
    fn the_floor_is_the_end_of_turn_plus_the_providers() {
        let smart = EngineTargetConfig::default().settings();
        // 300 ms VAD stop + 150 ms STT final, then 350 ms + 150 ms.
        assert_eq!(smart.end_of_turn, "smart_turn");
        assert_eq!(smart.turn_silence_ms, None);
        assert_eq!(smart.end_of_turn_floor_ms, 450);
        assert_eq!(smart.floor_ms, 950);
        // A fixed silence longer than the gate sets the end of the turn.
        let long = EngineTargetConfig {
            end_of_turn: EndOfTurn::FixedSilence(600 * MS),
            ..EngineTargetConfig::default()
        }
        .settings();
        assert_eq!(long.end_of_turn_floor_ms, 600);
        assert_eq!(long.floor_ms, 1_100);
        // A shorter one waits for the final all the same.
        let short = EngineTargetConfig {
            end_of_turn: EndOfTurn::FixedSilence(200 * MS),
            ..EngineTargetConfig::default()
        }
        .settings();
        assert_eq!(short.turn_silence_ms, Some(200));
        assert_eq!(short.end_of_turn_floor_ms, 450);
    }

    #[test]
    fn the_engine_configuration_is_the_default_with_the_policy() {
        let config = EngineTargetConfig::default();
        let engine = config.engine_config();
        assert_eq!(engine.turn, TurnPolicy::adaptive());
        assert_eq!(engine.interruption.vad_hangover, 300 * MS);
        assert_eq!(engine.lead_margin, EngineConfig::DEFAULT.lead_margin);
        assert!(engine.validate().is_ok());
        let fixed = EngineTargetConfig {
            end_of_turn: EndOfTurn::FixedSilence(450 * MS),
            ..config
        };
        assert_eq!(fixed.engine_config().turn, TurnPolicy::fixed(450 * MS));
    }

    #[test]
    fn a_script_answers_in_the_language_of_each_utterance() {
        use brisvox_core::frame::SampleRate;
        use brisvox_sim::fixture::FixtureSegment;
        let segment = |lang: &str, text: &str| FixtureSegment {
            start: 0,
            end: 1,
            lang: lang.to_owned(),
            text: text.to_owned(),
            source: "test".to_owned(),
        };
        let fixture = Fixture {
            name: "mixed".to_owned(),
            sample_rate: SampleRate::Hz16000,
            pcm: bytes::Bytes::new(),
            segments: vec![segment("en", "hello there"), segment("tr", "merhaba")],
        };
        let script = CallScript::of(&fixture);
        assert_eq!(script.answers, [ANSWER_EN, ANSWER_TR]);
        assert_eq!(script.utterances, fixture.utterances());
    }

    fn script_of(utterances: &[(&str, &'static str)]) -> CallScript {
        CallScript {
            utterances: utterances
                .iter()
                .map(|(text, _)| SttUtterance::words(text))
                .collect(),
            answers: utterances.iter().map(|(_, answer)| *answer).collect(),
            kind: CallKind::Latency,
        }
    }

    fn answer_to(script: &CallScript, user_text: &str) -> Option<LlmScript> {
        use brisvox_providers::{LlmRequest, Message};
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::System, "be brief"),
                Message::new(Role::User, "an earlier turn"),
                Message::new(Role::Assistant, "an earlier answer"),
                Message::new(Role::User, user_text),
            ],
            tools: Vec::new(),
        };
        let responder = script.responder();
        responder(&request)
    }

    #[test]
    fn the_answer_follows_the_request_not_the_order_of_requests() {
        let script = script_of(&[
            (
                "The major religion in Moldova is Orthodox Christian.",
                ANSWER_EN,
            ),
            (
                "Noel, Hristiyanligin en muhim bayramlarindan biridir.",
                ANSWER_TR,
            ),
        ]);
        let en = LlmScript::text([ANSWER_EN]);
        let tr = LlmScript::text([ANSWER_TR]);
        assert_eq!(
            answer_to(
                &script,
                "the major religion in moldova is orthodox christian"
            ),
            Some(en)
        );
        assert_eq!(
            answer_to(
                &script,
                "Noel, Hristiyanligin en muhim bayramlarindan biridir."
            ),
            Some(tr.clone())
        );
        // A speculative request carries the interim, a prefix of the transcript, and is
        // answered like the turn it speculates on; asking twice changes nothing.
        assert_eq!(
            answer_to(&script, "Noel, Hristiyanligin en"),
            Some(tr.clone())
        );
        assert_eq!(answer_to(&script, "Noel, Hristiyanligin en"), Some(tr));
        // A text that is no turn's, and an empty one, get no answer (the mock's empty
        // fallback: the call then shows no response).
        assert_eq!(answer_to(&script, "something else"), None);
        assert_eq!(answer_to(&script, " "), None);
    }

    #[test]
    fn speculation_is_off_by_default_and_follows_the_flags() {
        let off = EngineTargetConfig::default();
        assert!(!off.engine_config().speculation.enabled);
        let report = off.settings();
        assert!(!report.speculation && !report.speculation_on_partial);
        assert_eq!(report.stt_interims, "lagging");

        let on = EngineTargetConfig {
            speculation: true,
            stt_interims: SttInterims::Converged,
            ..off
        };
        let speculation = on.engine_config().speculation;
        assert!(speculation.enabled && speculation.on_partial);
        assert!(on.engine_config().validate().is_ok());
        let report = on.settings();
        assert!(report.speculation && report.speculation_on_partial);
        assert_eq!(report.stt_interims, "converged");

        let final_only = EngineTargetConfig {
            speculation_on_partial: false,
            ..on
        };
        let speculation = final_only.engine_config().speculation;
        assert!(speculation.enabled && !speculation.on_partial);
        assert!(!final_only.settings().speculation_on_partial);
    }
}
