//! Provider-neutral speech-to-speech (S2S) interface (`docs/S2S.md`).
//!
//! A speech-to-speech model takes the caller's audio and answers with audio (and, optionally, the
//! text of both), calling tools in between. One session therefore replaces the STT, LLM and TTS
//! stages of the classic pipeline. The interface mirrors [`SttSession`](crate::SttSession) and
//! [`TtsSession`](crate::TtsSession): a bounded input side, a bounded event side, and an
//! adapter task in between that dies with its channels.
//!
//! The types are deliberately small and provider-neutral: OpenAI Realtime is the first adapter
//! (`provider-openai-realtime`), Gemini Live is the next. What a provider cannot do is reported
//! at `connect` (an error) rather than silently emulated.
//!
//! # Audio
//! PCM16 little-endian mono both ways, at the rates of [`S2sConfig`]. The engine converts from and
//! to the call's rate; an adapter that cannot serve a requested rate fails `connect` with
//! [`Error::Protocol`].
//!
//! # Who decides when the caller's turn ends
//! [`S2sTurnDetection::Provider`]: the provider's own VAD ends turns, starts responses and
//! cancels a running response when the caller speaks (it reports [`S2sEvent::SpeechStarted`] and
//! [`S2sEvent::SpeechStopped`]). [`S2sTurnDetection::Manual`]: the provider never decides; the
//! engine sends [`S2sControl::Commit`] and [`S2sControl::CreateResponse`] (driven by the local VAD
//! / turn driver) and [`S2sControl::CancelResponse`] on a barge-in.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::Error;
use crate::llm::ToolSpec;

/// Capacity of a session's audio-in channel (about 1.3 s of 20 ms frames). The engine sends with
/// `try_send` and counts a drop: caller audio never waits for the provider.
pub const S2S_AUDIO_CAPACITY: usize = 64;

/// Capacity of a session's control channel. Control messages are rare (a few per turn); a full
/// channel means a stuck adapter, which the engine treats as a dead session.
pub const S2S_CONTROL_CAPACITY: usize = 32;

/// Capacity of a session's event channel. A full channel backpressures the adapter's socket
/// reader (TCP flow control), bounded by the adapter's own timeouts.
pub const S2S_EVENT_CAPACITY: usize = 128;

/// Most history items a new session is seeded with ([`S2sConfig::history`]). The engine trims to
/// this (newest kept); an adapter refuses a longer list with [`Error::Protocol`].
pub const S2S_MAX_HISTORY_ITEMS: usize = 128;

/// One item of the conversation so far, used to seed a NEW session after a reconnect or a
/// rotation (see [`S2sConfig::history`]). It is the engine's local mirror of what was said and
/// played, not what the old provider session believed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum S2sHistoryItem {
    /// What the caller said (a transcript).
    User(Arc<str>),
    /// What the assistant said (only what was actually played).
    Assistant(Arc<str>),
    /// A tool call the model made earlier. Always followed (later in the list) by its
    /// [`Self::ToolResult`]: the engine never seeds a call without an answer.
    ToolCall {
        /// The id the call had ([`S2sEvent::ToolCall::call_id`] of the old session).
        call_id: Arc<str>,
        /// The tool name.
        name: Arc<str>,
        /// The arguments as JSON object text.
        arguments: Arc<str>,
    },
    /// The result of an earlier [`Self::ToolCall`].
    ToolResult {
        /// The call it answers.
        call_id: Arc<str>,
        /// The result text.
        output: Arc<str>,
    },
    /// A system note (for example a late tool result).
    Note(Arc<str>),
}

/// Who ends the caller's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum S2sTurnDetection {
    /// The provider's VAD ends turns, creates the response and cancels it on barge-in.
    #[default]
    Provider,
    /// The engine decides ([`S2sControl::Commit`], [`S2sControl::CreateResponse`],
    /// [`S2sControl::CancelResponse`]); the provider's VAD is off.
    Manual,
}

/// How an S2S session is opened.
#[derive(Debug, Clone, PartialEq)]
pub struct S2sConfig {
    /// System instructions of the agent.
    pub instructions: Arc<str>,
    /// Voice name, or `None` for the adapter's default.
    pub voice: Option<Arc<str>>,
    /// Tools the model may call (`parameters_schema` is JSON Schema text).
    pub tools: Vec<ToolSpec>,
    /// Who ends the caller's turn.
    pub turn_detection: S2sTurnDetection,
    /// Rate of the audio the engine sends ([`S2sControl`] is separate; audio goes through
    /// [`S2sSession::audio`]).
    pub input_rate: SampleRate,
    /// Rate of the audio the engine wants back in [`S2sEvent::Audio`].
    pub output_rate: SampleRate,
    /// Ask for a transcript of the caller's speech ([`S2sEvent::UserTranscript`]) when the
    /// provider offers one.
    pub input_transcription: bool,
    /// BCP-47 language hint, or `None`.
    pub language: Option<Arc<str>>,
    /// The conversation so far, oldest first; empty for a call's first session. A provider adds
    /// it to the new session's history before the first caller audio so the model continues the
    /// conversation (the engine fills it from its local mirror after a reconnect or rotation).
    /// At most [`S2S_MAX_HISTORY_ITEMS`] items.
    pub history: Vec<S2sHistoryItem>,
}

impl S2sConfig {
    /// A config with provider-side turn detection, 24 kHz both ways, no tools and no
    /// transcription.
    pub fn new(instructions: impl Into<Arc<str>>) -> Self {
        Self {
            instructions: instructions.into(),
            voice: None,
            tools: Vec::new(),
            turn_detection: S2sTurnDetection::Provider,
            input_rate: SampleRate::Hz24000,
            output_rate: SampleRate::Hz24000,
            input_transcription: false,
            language: None,
            history: Vec::new(),
        }
    }
}

/// A command from the engine to a session (everything except audio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S2sControl {
    /// End the caller's turn: turn the buffered audio into a user message. Manual mode only.
    Commit,
    /// Drop the buffered, not yet committed caller audio. Manual mode only.
    ClearInput,
    /// Start a response now. Manual mode, and after a tool result in any mode.
    CreateResponse,
    /// Cancel the running response, if any. Cancelling a response that is not running is not an
    /// error.
    CancelResponse,
    /// The caller heard only the first `audio_end` of the audio of `item`: cut the item (audio and
    /// transcript) in the model's history to that. `item` is the id of an [`S2sEvent::Audio`].
    Truncate {
        /// The assistant item.
        item: Arc<str>,
        /// How much of its audio was played to the caller.
        audio_end: Duration,
    },
    /// The result of the tool call `call_id` (a JSON or text string). Adds it to the history; it
    /// does NOT start a response ([`Self::CreateResponse`] does).
    ToolResult {
        /// The call being answered ([`S2sEvent::ToolCall::call_id`]).
        call_id: Arc<str>,
        /// The result, as text.
        output: Arc<str>,
    },
    /// A system note for the model (for example a tool result that arrived after its call was
    /// interrupted). Added to the history; it does not start a response.
    Note {
        /// The note.
        text: Arc<str>,
    },
}

/// Why a response ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S2sResponseStatus {
    /// Finished normally.
    Completed,
    /// Cancelled (by the caller's barge-in or by [`S2sControl::CancelResponse`]).
    Cancelled,
    /// Cut by a limit (tokens, content filter).
    Incomplete,
    /// Failed; the text says why (never contains credentials).
    Failed(Arc<str>),
}

/// What a session produces.
#[derive(Debug)]
pub enum S2sEvent {
    /// The provider's VAD heard the caller start speaking ([`S2sTurnDetection::Provider`]).
    SpeechStarted,
    /// The provider's VAD heard the caller stop speaking.
    SpeechStopped,
    /// The model began a response.
    ResponseStarted {
        /// The response id.
        response: Arc<str>,
    },
    /// A chunk of the model's speech (PCM16 mono at [`S2sConfig::output_rate`]).
    Audio {
        /// The response it belongs to.
        response: Arc<str>,
        /// The assistant item the audio is part of (what [`S2sControl::Truncate`] cuts).
        item: Arc<str>,
        /// The samples.
        pcm: Bytes,
    },
    /// No more audio follows for `item`.
    AudioDone {
        /// The response.
        response: Arc<str>,
        /// The item.
        item: Arc<str>,
    },
    /// The text of what the model says: a delta, or (`is_final`) the whole transcript of the
    /// item.
    AssistantTranscript {
        /// The response.
        response: Arc<str>,
        /// The item.
        item: Arc<str>,
        /// The text (a delta unless `is_final`).
        text: Arc<str>,
        /// `text` is the complete transcript of the item.
        is_final: bool,
    },
    /// The transcript of a caller turn (provider-side recognition, if enabled).
    UserTranscript {
        /// The text.
        text: Arc<str>,
    },
    /// The model calls a tool. Delivered exactly once per `call_id`.
    ToolCall {
        /// The response that issued the call.
        response: Arc<str>,
        /// The provider's id of the call; [`S2sControl::ToolResult`] answers it.
        call_id: Arc<str>,
        /// The tool name.
        name: Arc<str>,
        /// The arguments as JSON text, exactly as the model produced them.
        arguments: Arc<str>,
    },
    /// A response ended.
    ResponseDone {
        /// The response.
        response: Arc<str>,
        /// How it ended.
        status: S2sResponseStatus,
    },
    /// The provider reported a problem that does not end the session (a rejected command, a rate
    /// limit notice). The text never contains credentials.
    Warning {
        /// The provider's error code, or an empty string.
        code: Arc<str>,
        /// Human-readable detail.
        message: Arc<str>,
    },
    /// The provider announced that this session will be closed in about `time_left` (Gemini
    /// Live `goAway`). The session keeps working until then; the engine replaces it with a new
    /// one, seeded with [`S2sConfig::history`], when the bot is idle.
    SessionEnding {
        /// Time the provider gives before it closes the connection.
        time_left: Duration,
    },
    /// The session failed; no further events follow.
    Error(Error),
}

/// An open S2S session. Dropping `audio` and `control` closes it; the adapter then closes
/// `events`.
#[derive(Debug)]
pub struct S2sSession {
    /// Caller audio in (PCM16 mono at [`S2sConfig::input_rate`]).
    pub audio: mpsc::Sender<Bytes>,
    /// Commands in. The adapter serves this channel before `audio`.
    pub control: mpsc::Sender<S2sControl>,
    /// Events out.
    pub events: mpsc::Receiver<S2sEvent>,
}

/// A speech-to-speech service.
pub trait S2sProvider: Send + Sync + 'static {
    /// Provider name, e.g. `"openai-realtime"`.
    fn name(&self) -> &'static str;

    /// Opens a session and configures it (instructions, tools, voice, turn detection, formats);
    /// returns when the provider acknowledged the configuration.
    ///
    /// # Errors
    /// Connection, authentication or configuration failures (with an explicit timeout).
    fn connect(&self, config: S2sConfig) -> impl Future<Output = Result<S2sSession, Error>> + Send;

    /// The longest a single session lives at this provider (OpenAI Realtime: 60 minutes), or
    /// `None` when there is no known limit. The engine opens a replacement session, seeded with
    /// the conversation, a margin before this elapses and switches over while the bot is idle,
    /// so the call survives the limit.
    fn session_limit(&self) -> Option<Duration> {
        None
    }

    /// Pre-establishes whatever makes the next `connect` fast (DNS, TLS).
    ///
    /// # Errors
    /// Warm-up failures (callers may ignore them; `connect` retries).
    fn warm(&self) -> impl Future<Output = Result<(), Error>> + Send {
        async { Ok(()) }
    }
}
