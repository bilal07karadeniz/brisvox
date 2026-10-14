//! The production voice pipeline of brisvox (design in `docs/PIPELINE.md`).
//!
//! This crate composes the building blocks of the other crates into one session pipeline:
//! transport pump, input chain, STT, turn driver, LLM runner, segmenter, TTS and transport out.
//! The reference pipeline in `brisvox-sim` stays the untouched golden baseline; this crate never
//! is a dependency of the simulator.
//!
//! What lives here so far (the foundation the stages are built on):
//! - [`EngineConfig`]: rates, queue shapes, timeouts and policies of one session.
//! - [`TaskGuard`]: a spawned helper task that is aborted when its owner goes away.
//! - [`GuardedSender`]: a helper task's way into the pipeline; it stops sending frames of an
//!   epoch that an interrupt already ended.
//! - [`EventSlot`]: the typed, bounded mailbox that travels with a `SystemFrame::Wake`, so a
//!   helper task can hand a single-threaded stage a timer tick or a model verdict.
//! - [`Offload`]: runs heavy work on the inference pool in production and inline in
//!   deterministic simulations.
//! - [`PlayoutBook`]: what was synthesized and what was actually played, per `(epoch, round)`,
//!   from which the LLM context is cut to the words the caller heard.
//!
//! The input chain:
//! - [`Pump`]: the one task that reads the transport session and turns its events into frames,
//!   marks and positions into the [`PlayoutBook`]; it never stops draining.
//! - [`AudioIn`]: converts caller audio to 16 kHz, forwards it at once and tees it to the VAD.
//! - the VAD worker ([`VadSetup`], [`EnergyModel`]): batches the tee into one job at a time on
//!   the [`Offload`] and emits backdated `UserStartedSpeaking` / `UserStoppedSpeaking`.
//!
//! The LLM stage:
//! - [`LlmRunner`]: one round task per turn that builds the request through the
//!   [`RequestBuilder`] (the context window), streams the response, runs the tool loop
//!   and reacts to a barge-in; [`convert`] holds the pure converters to the providers' types and
//!   [`TruncationSource`] is where it learns what the caller heard.
//!
//! The output chain:
//! - [`SegmenterStage`]: cuts the LLM's text into chunks for TTS and sends them as raw slices of
//!   the response, so the text recorded as sent is a prefix of the message in the context.
//! - [`TtsStage`]: one provider context per text-bearing round; normalization, word timestamps,
//!   resampling to the call's output rate ([`OutputFramer`], [`CallRatesCell`]) and exactly one
//!   `TtsEnded` per response.
//! - [`TransportOut`]: the tail; frames to transport commands, the epoch `Clear` rule, the end
//!   mark and its timeout. Not a pacer: the transport session paces.
//!
//! Played-text truncation:
//! - [`PlayoutTruncation`]: the production [`TruncationSource`] over the [`PlayoutBook`]: a settle
//!   wait on the injected clock for the transport's final position, then the text heard at
//!   `min(released, total) - lead` (the client's reported lead, else `lead_margin`).
//!   [`LlmRunnerBuilder::with_playout`] wires it; [`clear_hook`] tells the book about every
//!   `Clear` the turn driver issues.
//!
//! Events and typed input:
//! - [`EngineEvents`]: the call's conversation events (speaking, transcripts, LLM/TTS text,
//!   barge-ins, turn latencies), observed on the stages' inputs without an extra hop and
//!   delivered drop-oldest ([`events`]).
//! - [`text_turns`]: typed user messages into the turn driver ([`text_input`]).
//! - [`rtvi`]: the bridge from both to a Pipecat RTVI client.
//!
//! Tracing target for engine diagnostics: `brisvox::engine`.

#![forbid(unsafe_code)]

pub mod agent_input;
pub mod audio_in;
pub mod config;
pub mod convert;
pub mod error;
pub mod event_slot;
pub mod events;
pub mod filler_clip;
pub mod guard;
pub mod llm;
mod llm_runner;
mod llm_spec;
mod llm_turn;
pub mod offload;
pub mod playout;
pub mod pump;
pub mod rates;
pub mod request;
#[cfg(feature = "rtvi")]
pub mod rtvi;
pub mod s2s;
pub mod segmenter;
pub mod session;
#[cfg(feature = "sim")]
pub mod sim;
pub mod speculation;
pub mod stt;
pub mod text_input;
pub mod timer;
pub mod transport_out;
pub mod truncation;
pub mod tts;
pub mod tts_audio;
pub mod turn_driver;
pub mod vad;

pub use agent_input::{
    BotLine, ConversationEdit, EditReceiver, EditSender, InputError, SayReceiver, SaySender,
    conversation_edits, say_input,
};
pub use audio_in::{AudioIn, AudioInStats, AudioInStatsSnapshot, CallerAudioTap};
pub use config::{
    EarlyAnalysisConfig, EngineConfig, EngineRates, InterruptionConfig, LlmConfig, QueueConfigs,
    SpeculationConfig, Timeouts,
};
pub use convert::ConvertError;
pub use error::{Error, Result};
pub use event_slot::{EventSlot, PushOutcome, SlotOverflow, wake_frame};
pub use events::{
    DEFAULT_EVENT_CAPACITY, EngineEvent, EngineEvents, EventStream, MAX_EVENT_CAPACITY,
};
pub use filler_clip::{ClipError, ClipWord, FillerClip, FillerClipStore};
pub use guard::{Delivery, GuardedSender, JoinOutcome, TaskGuard};
pub use llm::{
    DEFAULT_FALLBACK, DEFAULT_FILLER, LlmStats, LlmStatsSnapshot, MAX_PROVIDER_DATA_BYTES,
};
pub use llm_runner::{FlowProbe, LlmRunner, LlmRunnerBuilder};
pub use offload::{Offload, WARM_COOLDOWN, WARM_MAX_QUEUE_PER_THREAD, WarmClaim, WarmReport};
pub use playout::{
    BookStats, DEFAULT_BOOK_CAPACITY, Heard, PlayoutBook, ResolvedMark, RoundKey, RoundSnapshot,
    clear_hook,
};
pub use pump::{Pump, PumpConfig, PumpEnd, PumpHandle, PumpLinks, PumpStats};
pub use rates::CallRatesCell;
pub use request::{
    BuiltRequest, MAX_SUMMARY_BACKOFF, RequestBuilder, RequestStats, RequestStatsSnapshot,
    SUMMARY_DEGRADED_AFTER, WindowEstimator,
};
pub use s2s::{
    S2sConnect, S2sOptions, S2sStage, S2sStageConfig, S2sStats, S2sStatsSnapshot, S2sTurnMode,
    S2sWiring, Unused,
};
pub use segmenter::{
    RAW_CAP_FACTOR, SegmenterStage, SegmenterStageConfig, SegmenterStats, SegmenterStatsSnapshot,
};
pub use session::{
    Call, CallStats, FrameTap, PumpWiring, SessionBuilder, SessionHandles, SessionPipeline,
    SourceWrap, StageCounters, build_session_pipeline, run_call,
};
pub use speculation::{
    SpeculationStats, SpeculationStatsSnapshot, candidate_frame, normalize, withdrawal_frame,
};
pub use stt::{SttStage, SttStageConfig, SttStats, SttStatsSnapshot};
pub use text_input::{MAX_TEXT_CAPACITY, TextTurn, TextTurnReceiver, TextTurnSender, text_turns};
pub use timer::DeadlineTimer;
pub use transport_out::{
    TransportOut, TransportOutConfig, TransportOutStats, TransportOutStatsSnapshot,
};
pub use truncation::{
    BoxFuture, Cut, CutRequest, KeepAll, PlayoutTruncation, PlayoutTruncationConfig,
    TruncationSource,
};
pub use tts::{TtsStage, TtsStageConfig, TtsStats, TtsStatsSnapshot, WHITESPACE_LEAD_CAP};
pub use tts_audio::OutputFramer;
pub use turn_driver::{
    ANALYSIS_DEFER_MAX, ClearFuture, ClearHook, MachineSnapshot, TurnDriver, TurnDriverConfig,
    TurnDriverStats, TurnDriverStatsSnapshot, TurnProbe, smart_turn_warm_jobs,
};
pub use vad::{EnergyModel, VadSetup, VadStats, VadStatsSnapshot, VadWorkerConfig};
