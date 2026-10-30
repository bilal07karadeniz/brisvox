//! Streaming text-to-speech interface (minimal; refined with the first real adapter in M3).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Capacity of a TTS session's input and event channels (backpressure on both).
pub const TTS_CHANNEL_CAPACITY: usize = 64;

/// Capacity of a TTS session's cancel channel (kept separate so a cancel never waits behind
/// queued text).
pub const TTS_CANCEL_CAPACITY: usize = 8;

/// Identifies one utterance (usually one bot turn) inside a long-lived TTS session. Chosen by
/// the engine; echoed on every event so stale output after a cancel can be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextId(pub u64);

/// How a TTS session is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsConfig {
    /// Output sample rate (PCM16 LE mono).
    pub sample_rate: SampleRate,
    /// Provider voice id.
    pub voice: Arc<str>,
    /// BCP-47 language the session speaks (a lowercase primary code such as `"tr"`), for
    /// providers that take a language next to the voice (Cartesia `language`, ElevenLabs
    /// `language_code`, Chatterbox `language_id`); `None` keeps the adapter's own setting.
    pub language: Option<Arc<str>>,
    /// Model id overriding the adapter's configured one for this session (a language may need
    /// another model); `None` keeps the adapter's.
    pub model: Option<Arc<str>>,
}

/// What the engine sends to a TTS session (cancel goes through [`TtsSession::cancel`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsInput {
    /// More text for `context` (continuation of its earlier text).
    Text {
        /// Utterance.
        context: ContextId,
        /// Text to speak.
        text: Arc<str>,
    },
    /// No more text for `context`: synthesize what is buffered, then report
    /// [`TtsEvent::Flushed`].
    Flush {
        /// Utterance.
        context: ContextId,
    },
}

/// What a TTS session produces.
#[derive(Debug)]
pub enum TtsEvent {
    /// PCM16 LE mono audio of `context` at the configured sample rate.
    Audio {
        /// Utterance.
        context: ContextId,
        /// Samples.
        audio: Bytes,
    },
    /// A word of `context` starts `start` after that context's first audio sample (continuous
    /// across text chunks and flushes of the same context).
    Word {
        /// Utterance.
        context: ContextId,
        /// The word.
        word: Arc<str>,
        /// Offset from the context's first audio sample.
        start: Duration,
    },
    /// Everything of `context` up to its `Flush` has been produced.
    Flushed {
        /// Utterance.
        context: ContextId,
    },
    /// `context` was cancelled: no more events for it follow (earlier ones may still be queued).
    Cancelled {
        /// Utterance.
        context: ContextId,
    },
    /// The session failed; no further events follow.
    Error(Error),
}

/// An open TTS session. Dropping `input` closes it; the provider then closes `events`.
#[derive(Debug)]
pub struct TtsSession {
    /// Text in.
    pub input: mpsc::Sender<TtsInput>,
    /// Priority path for barge-in: providers handle it before any queued input.
    pub cancel: mpsc::Sender<ContextId>,
    /// Audio and timestamps out.
    pub events: mpsc::Receiver<TtsEvent>,
}

/// A streaming TTS service.
pub trait TtsProvider: Send + Sync + 'static {
    /// Provider name, e.g. `"cartesia"`.
    fn name(&self) -> &'static str;

    /// Opens a session.
    ///
    /// # Errors
    /// Connection or authentication failures.
    fn connect(&self, config: TtsConfig) -> impl Future<Output = Result<TtsSession>> + Send;

    /// Pre-establishes whatever makes the next `connect` fast (DNS, TLS, pooled connection).
    ///
    /// # Errors
    /// Warm-up failures (callers may ignore them; `connect` retries).
    fn warm(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// How long the provider keeps a session's connection open while nothing is sent on it
    /// (Cartesia: 5 minutes), when it documents such a limit. The resilience layer
    /// ([`crate::resilience::ResilientTts`]) replaces an idle session before it.
    /// Default: `None` (no documented limit, or the adapter keeps the connection alive itself).
    fn idle_limit(&self) -> Option<Duration> {
        None
    }
}
