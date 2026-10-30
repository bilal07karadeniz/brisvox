//! Speech-to-text, LLM and text-to-speech provider traits and their adapters. Each adapter is
//! gated behind a `provider-<name>` cargo feature.
//!
//! Sessions are channel-based: an open STT/TTS session is a bounded input sender plus a bounded
//! event receiver, and an LLM response is a bounded event stream. Any task can drive them, and
//! dropping the handles cancels the provider work.

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-assemblyai")]
pub mod assemblyai;
#[cfg(feature = "provider-deepgram")]
pub mod deepgram;

#[cfg(feature = "provider-cartesia")]
pub mod cartesia;
#[cfg(feature = "provider-deepgram-aura")]
pub mod deepgram_aura;
#[cfg(feature = "provider-elevenlabs")]
pub mod elevenlabs;
pub mod error;
#[cfg(feature = "provider-gemini")]
pub mod gemini;
#[cfg(feature = "provider-gemini-live")]
pub mod gemini_live;
#[cfg(feature = "provider-gladia")]
pub mod gladia;

#[cfg(any(
    feature = "provider-openai",
    feature = "provider-openai-compatible",
    feature = "provider-openai-speech"
))]
// The speech adapter uses a subset of the helpers (no error bodies, no drain, no CPU offload).
#[cfg_attr(
    not(any(feature = "provider-openai", feature = "provider-openai-compatible")),
    allow(dead_code)
)]
mod http_llm;
pub mod llm;
pub mod loopback;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "provider-openai")]
pub mod openai;
#[cfg(feature = "provider-openai-compatible")]
pub mod openai_compatible;
#[cfg(feature = "provider-openai-realtime")]
pub mod openai_realtime;
#[cfg(feature = "provider-openai-realtime-stt")]
pub mod openai_realtime_stt;
#[cfg(feature = "provider-openai-speech")]
pub mod openai_speech;
pub mod resilience;
pub mod s2s;
#[cfg(feature = "provider-sherpa-onnx")]
pub mod sherpa_onnx;
#[cfg(any(feature = "provider-openai", feature = "provider-openai-compatible"))]
mod sse;
pub mod stt;
#[cfg(any(
    feature = "provider-sherpa-onnx",
    feature = "provider-openai-realtime-stt",
    feature = "provider-openai-speech"
))]
mod timer;
pub mod tts;
#[cfg(any(
    feature = "provider-sherpa-onnx",
    feature = "provider-openai-realtime-stt"
))]
mod wsstt;

pub use error::{Error, Result};
pub use llm::{
    FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, ToolCall, ToolSpec,
    Usage,
};
pub use s2s::{
    S2sConfig, S2sControl, S2sEvent, S2sHistoryItem, S2sProvider, S2sResponseStatus, S2sSession,
    S2sTurnDetection,
};
pub use stt::{SpokenLanguage, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
pub use tts::{ContextId, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
