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
