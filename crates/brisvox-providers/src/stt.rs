//! Streaming speech-to-text interface (minimal; refined with the first real adapter in M3).

use std::future::Future;
use std::sync::Arc;

use brisvox_core::frame::SampleRate;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Capacity of an STT session's input and event channels (backpressure on both).
pub const STT_CHANNEL_CAPACITY: usize = 64;

/// How an STT session is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttConfig {
    /// Sample rate of the PCM16 mono audio that will be sent.
    pub sample_rate: SampleRate,
    /// BCP-47 language, or `None` for the provider default.
    pub language: Option<Arc<str>>,
}

/// What the engine sends to an STT session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttInput {
    /// PCM16 LE mono audio.
    Audio(Bytes),
    /// The utterance ended (VAD stop): finalize what has been heard now. The provider answers
    /// with transcripts flagged `from_finalize`, the last of them `speech_final`.
    Finalize,
}

/// The spoken language of a transcript segment as the provider reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct SpokenLanguage {
    /// Lowercase primary language code (`"tr"`, `"en"`); a regional tag from the provider
    /// (`"en-GB"`) is reduced to its primary subtag by the adapter.
    pub code: Arc<str>,
    /// The provider's confidence in `code`, `0.0..=1.0`, when it gives one (AssemblyAI).
    pub confidence: Option<f32>,
}

impl SpokenLanguage {
    /// A language without a confidence. The code is lower-cased and cut at the first `-` or
    /// `_` (`"en-GB"` becomes `"en"`).
    pub fn new(code: &str) -> Self {
        let primary = code.split(['-', '_']).next().unwrap_or("");
        Self {
            code: Arc::from(primary.to_ascii_lowercase().as_str()),
            confidence: None,
        }
    }

    /// The same with the provider's `confidence` (clamped to `0.0..=1.0`; NaN is dropped).
    #[must_use]
    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = (!confidence.is_nan()).then(|| confidence.clamp(0.0, 1.0));
        self
    }
}

/// What an STT session produces.
#[derive(Debug)]
pub enum SttEvent {
    /// A transcript segment.
    Transcript {
        /// Recognised text of this segment.
        text: Arc<str>,
        /// `false` for interim hypotheses that may still change; `true` once this segment is
        /// fixed (an utterance can have several final segments).
        is_final: bool,
        /// This final segment ends the utterance (the provider's endpoint, or the end of a
        /// requested finalize).
        speech_final: bool,
        /// Produced in response to [`SttInput::Finalize`].
        from_finalize: bool,
        /// The language the provider reports for this segment, when it reports one on streaming
        /// transcripts (Deepgram with `language=multi`, AssemblyAI with language detection,
        /// Gladia); `None` for interim hypotheses and for providers that do not detect the
        /// language.
        language: Option<SpokenLanguage>,
    },
    /// The session failed; no further events follow.
    Error(Error),
}

/// An open STT session. Dropping `input` closes it; the provider then closes `events`.
#[derive(Debug)]
pub struct SttSession {
    /// Audio and control in.
    pub input: mpsc::Sender<SttInput>,
    /// Transcripts out.
    pub events: mpsc::Receiver<SttEvent>,
}

/// A streaming STT service.
pub trait SttProvider: Send + Sync + 'static {
    /// Provider name, e.g. `"deepgram"`.
    fn name(&self) -> &'static str;

    /// Opens a session.
    ///
    /// # Errors
    /// Connection or authentication failures.
    fn connect(&self, config: SttConfig) -> impl Future<Output = Result<SttSession>> + Send;

    /// Pre-establishes whatever makes the next `connect` fast (DNS, TLS, pooled connection).
    ///
    /// # Errors
    /// Warm-up failures (callers may ignore them; `connect` retries).
    fn warm(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}
