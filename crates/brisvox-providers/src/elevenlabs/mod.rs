//! ElevenLabs streaming text-to-speech over one multi-context WebSocket per session (feature
//! `provider-elevenlabs`; protocol facts in `docs/providers/elevenlabs.md`, fetched 2026-10-08).
//!
//! # Shape
//! [`ElevenLabsTts`] implements [`TtsProvider`]. `connect` opens (or takes the socket that
//! [`warm`](TtsProvider::warm) parked) a
//! `wss://api.elevenlabs.io/v1/text-to-speech/{voice}/multi-stream-input` connection and spawns
//! one task that owns it. The voice, model and output format are fixed by the URL, so they are
//! fixed per session. The task maps the engine's [`ContextId`](crate::ContextId)s onto
//! ElevenLabs contexts (one server context per utterance, ids `bv<session>-<n>`, never reused)
//! and turns server messages into [`TtsEvent`](crate::TtsEvent)s. The multi-context endpoint is
//! the only one with a documented cancel; the single-context endpoint can only be stopped by
//! closing the socket.
//!
//! # Wire behaviour
//! - A server context starts with `{"text":" ","context_id":..}` (carrying the optional voice
//!   settings and `chunk_length_schedule`, which the docs allow on that first message only).
//! - `TtsInput::Text` becomes `{"text":"<text> ","context_id":..,"flush":true}`. Every text is
//!   flushed: the default buffering waits for 120 characters before the first audio, which is
//!   bad for latency, and brisvox already sends phrase- or sentence-sized chunks. The docs
//!   ask each chunk to end with a single space; one is appended unless the text ends in
//!   whitespace. A chunk must therefore not end in the middle of a word (the segmenter's job).
//!   Text without a single letter or digit (blank, `...`, `-`, emoji only) is dropped: nothing
//!   would be spoken, and a server that answers it with no audio and no alignment would
//!   otherwise look stalled and end the session after `io_timeout`.
//! - `TtsInput::Flush` becomes `{"context_id":..,"flush":true}` then
//!   `{"context_id":..,"close_context":true}`, and [`TtsEvent::Flushed`](crate::TtsEvent::Flushed) follows the server's
//!   `isFinal` for that context.
//! - `audio` (base64) is decoded into [`Bytes`](bytes::Bytes) carved from large reusable blocks.
//! - The alignment (`alignment`, or `normalizedAlignment` with
//!   [`ElevenLabsConfig::use_normalized_alignment`]) is per character with times relative to
//!   the message's own audio. Characters are grouped into whitespace-separated words, a word
//!   starting at its first character, and every [`TtsEvent::Word`](crate::TtsEvent::Word) offset is the audio
//!   delivered before the message plus the character's time: exact in the adapter's own byte
//!   count, continuous across messages, flushes and server contexts, and never going backwards.
//!   Live (2026-10-13) the alignment of a message arrived in the same message as its audio.
//!   With `sync_alignment=true` the times instead run from the start of the server context, so
//!   the origin is the audio delivered before that context opened; `false` is the default.
//!
//! # No completion marker: how the adapter knows what the server still owes
//! The docs define no per-flush "done" message (UNVERIFIED whether `isFinal` follows a flush).
//! The adapter therefore counts the non-whitespace characters sent and the characters the
//! alignment has covered: the plain `alignment` once one was seen, the normalized one only while
//! there is no plain one (the normalized text is spelled out and can be longer than the input,
//! so its count must not stand in for the plain one). While the alignment covers less, the
//! server owes output and a stall deadline ([`ElevenLabsConfig::io_timeout`]) is armed, restarted
//! by every message of the context. A word left open at the end of a message is only ended
//! once the alignment covers everything sent, so a message that ends inside a word never splits
//! it. Without any alignment the first response after the last text counts as complete
//! ([`ElevenLabsConfig::completion_by_alignment`] `= false` forces that mode). A slow LLM or a
//! long tool call between chunks never trips the deadline, because nothing is owed then.
//!
//! If the alignment omits characters (UNVERIFIED whether it maps 1:1 onto the input), a server
//! that went quiet after responding with only a few characters missing (at most 4, or a
//! quarter of the text) is not treated as stalled: coverage is no longer measured for that
//! context and the first response after each later text counts as complete. A larger shortfall,
//! or no response at all, ends the session with a `Timeout`. The price: a real stall at the very
//! end of a text is tolerated and the tail is lost.
//!
//! After `Flush`, `Flushed` is delivered on `isFinal`; if the server does not send one (UNVERIFIED)
//! it follows [`ElevenLabsConfig::final_grace`] after the last message once all text is
//! aligned (five times as long when nothing was ever aligned, because the first audio chunk is
//! then the only sign of progress). A solicited `isFinal` ends the context; later messages for
//! it are dropped. An `isFinal` the adapter did not ask for (UNVERIFIED whether the server
//! sends one per flush or only on its inactivity timeout) closes the server context, with a
//! `close_context` so that it cannot linger, when nothing is owed, and is ignored while text is
//! still owed.
//!
//! # Cancel (barge-in)
//! The cancel channel is polled before input and before the socket, and also while a socket
//! write or a connect is in progress. A cancel (1) forgets the context, so every later server
//! message for it is dropped locally, (2) removes its not yet delivered events from the
//! outgoing queue, (3) hands [`TtsEvent::Cancelled`](crate::TtsEvent::Cancelled) to the consumer at once (before any I/O),
//! and (4) sends `{"context_id":..,"close_context":true}` without a preceding flush, behind
//! whatever write is in flight. The docs do not say whether audio keeps coming after
//! `close_context`; brisvox never waits to find out. Text and flushes that arrive later for a
//! cancelled id are ignored (the id is remembered for 64 cancels; do not reuse ids).
//!
//! # Backpressure, bounds and overflow
//! Channels are bounded ([`TTS_CHANNEL_CAPACITY`](crate::tts::TTS_CHANNEL_CAPACITY), [`TTS_CANCEL_CAPACITY`](crate::tts::TTS_CANCEL_CAPACITY)). Events wait in an
//! internal queue that is drained to the events channel without blocking the task, so cancel
//! and input stay responsive when the consumer is slow. The socket is not read while that queue
//! holds 64 or more events (TCP backpressure, nothing is dropped); one server message adds at
//! most its words and one audio chunk beyond that. A message is at most 1 MiB and its
//! alignment at most 16384 entries (8192 words); a larger one is a protocol error that ends the
//! session, so a misbehaving endpoint cannot make a session hold more than a few MB. At most 4 engine contexts are kept (the
//! server allows 5 per connection); a context the engine never flushes or cancels is evicted
//! (oldest first, its server context closed) when a 5th would be added, and if the engine uses
//! its id again it starts afresh. After the engine's `Flush` of a context has produced its
//! `Flushed` (the last 64 are remembered, like cancelled ids), text and flushes for that id are
//! ignored, whether or not the server's `isFinal` had arrived when they came.
//!
//! # Idle, keepalive, reconnect
//! - The URL asks for the maximum `inactivity_timeout` (180 s). For every open context that
//!   has been quiet for [`ElevenLabsConfig::keepalive_interval`] the adapter sends
//!   `{"context_id":..,"text":""}`, which the docs say resets that context's timer. Whether it
//!   also resets the socket's timer is UNVERIFIED.
//! - If the socket closes while the server owes nothing, the session does not fail: the next
//!   `Text` reconnects lazily (one connect, bounded by [`ElevenLabsConfig::io_timeout`], not
//!   taking the socket parked for the next `connect`) and opens new server contexts. A close or
//!   error while output is owed is `Error::Disconnected` (or `Rejected` for an abnormal close
//!   code) and ends the session.
//! - A server context closed by the server (`isFinal` nobody asked for, for example its
//!   inactivity timeout) while nothing is owed only closes that server context; the engine
//!   context lives on and its next text opens a new one. The same message while text is
//!   outstanding is a protocol error: that text was never spoken.
//! - Dropping `input` closes every open server context, waits for the `isFinal`s (or the grace),
//!   delivers what remains and ends `events`. No `Flushed` is made up for contexts the engine
//!   never flushed.
//!
//! # Timeouts
//! Connect, every send, the close and every wait for owed server output carry the explicit
//! [`ElevenLabsConfig::io_timeout`] on the injected clock. A stalled wait is
//! `Error::Core(Timeout)` and ends the session.
//!
//! # Warm-up
//! The voice is part of the URL, so a socket can only be pre-opened for a known voice and
//! rate: set [`ElevenLabsConfig::warm_target`]. `warm()` then parks one socket for it (TCP, TLS
//! and the upgrade done); `connect` for the same voice and rate takes it. Without a target
//! `warm()` is a no-op.
//!
//! # Secrets
//! The API key is a [`Secret`], sent only in the `xi-api-key` handshake header (marked
//! sensitive), never in the URL, logs, errors or `Debug` output.

mod context;
mod session;
mod wire;

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use brisvox_core::clock::wait_for;
use brisvox_core::frame::SampleRate;
use brisvox_core::{Clock, Error as CoreError, SharedClock};
use futures::{FutureExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

pub use brisvox_core::Secret;

use crate::error::{Error, Result};
use crate::net::connect_ws;
use crate::tts::{TtsConfig, TtsProvider, TtsSession};
use wire::FrameSettings;

/// Provider name used in errors.
pub(crate) const PROVIDER: &str = "elevenlabs";

/// The default endpoint (base URL; the path and query are added per session).
pub const DEFAULT_URL: &str = "wss://api.elevenlabs.io";

/// The default model: low latency (~75 ms), 32 languages including Turkish. `eleven_flash_v2`
/// is English only; `eleven_v3` / `eleven_v4` are not supported on this endpoint.
pub const DEFAULT_MODEL: &str = "eleven_flash_v2_5";

/// Largest `inactivity_timeout` the API accepts (seconds).
pub const MAX_INACTIVITY_TIMEOUT_SECS: u16 = 180;

/// A parked or live ElevenLabs socket.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted. A base64 audio chunk is a few KiB to a few tens of KiB;
/// 1 MiB (about 24 s of 16 kHz PCM16) leaves a wide margin while bounding what one misbehaving
/// endpoint can make a session hold: the message itself, the decoder scratch for its audio and
/// (with the alignment cap in `session`) the words queued from it.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Sessions opened by this process (makes context ids unique across sessions).
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Audio encoding requested from ElevenLabs (`output_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OutputEncoding {
    /// Raw signed 16-bit PCM, `pcm_<rate>`: what [`TtsEvent`](crate::TtsEvent) documents.
    /// That the bytes are little-endian mono without a header is UNVERIFIED (the docs say
    /// only "PCM"); the live test checks it. 44.1 kHz needs a Pro plan.
    #[default]
    Pcm16,
    /// G.711 mu-law (`ulaw_8000`) for telephony. Requires an 8000 Hz session. The audio events
    /// then carry mu-law bytes, one per sample, not PCM16: the caller must not treat them as
    /// PCM16.
    Mulaw,
    /// G.711 A-law (`alaw_8000`); same rules as [`OutputEncoding::Mulaw`].
    Alaw,
}

impl OutputEncoding {
    /// The `output_format` query value for `rate`.
    ///
    /// # Errors
    /// A description when mu-law / A-law is asked for at a rate other than 8 kHz.
    fn format_name(self, rate: SampleRate) -> std::result::Result<String, &'static str> {
        match self {
            Self::Pcm16 => Ok(format!("pcm_{}", rate.hz())),
            Self::Mulaw | Self::Alaw if rate != SampleRate::Hz8000 => {
                Err("mu-law and A-law output need an 8000 Hz session")
            }
            Self::Mulaw => Ok("ulaw_8000".to_owned()),
            Self::Alaw => Ok("alaw_8000".to_owned()),
        }
    }

    /// Bytes per sample.
    pub(crate) fn bytes_per_sample(self) -> u64 {
        match self {
            Self::Pcm16 => 2,
            Self::Mulaw | Self::Alaw => 1,
        }
    }
}

/// `apply_text_normalization` (values from the HTTP convert page; assumed identical on the
/// WebSocket: UNVERIFIED).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextNormalization {
    /// The system decides (the server default).
    Auto,
    /// Always normalize (numbers spelled out, and so on).
    On,
    /// Never normalize.
    Off,
}

impl TextNormalization {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// Voice settings, sent once with the first message of every server context. `None` fields
/// are left to the server's defaults (stability 0.5, similarity 0.75, style 0, speaker boost
/// on, speed 1).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VoiceSettings {
    /// 0.0..=1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stability: Option<f32>,
    /// 0.0..=1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity_boost: Option<f32>,
    /// 0.0..=1.0; V2+ models only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<f32>,
    /// V2+ models only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_speaker_boost: Option<bool>,
    /// 0.7..=1.2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
}

impl VoiceSettings {
    fn validate(&self) -> std::result::Result<(), &'static str> {
        let unit = |v: Option<f32>| v.is_none_or(|v| (0.0..=1.0).contains(&v));
        if !unit(self.stability) || !unit(self.similarity_boost) || !unit(self.style) {
            return Err("voice settings stability, similarity_boost and style must be 0.0..=1.0");
        }
        if !self.speed.is_none_or(|v| (0.7..=1.2).contains(&v)) {
            return Err("voice settings speed must be 0.7..=1.2");
        }
        Ok(())
    }
}

/// How the ElevenLabs adapter is configured. Build with [`ElevenLabsConfig::new`], then set
/// fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ElevenLabsConfig {
    /// API key, sent in the `xi-api-key` handshake header.
    pub api_key: Secret<String>,
    /// WebSocket base URL (scheme and host, for example another residency host). Plain `ws://`
    /// is accepted only for a loopback host (tests, a local proxy) unless
    /// [`allow_insecure_ws`](Self::allow_insecure_ws) is set: the API key is sent in a header
    /// of the handshake.
    pub url: String,
    /// Allow `ws://` to a host that is not loopback. The API key then travels unencrypted:
    /// only for a trusted network path (for example a TLS-terminating sidecar on a private
    /// network).
    pub allow_insecure_ws: bool,
    /// Model id (a token of letters, digits, `.`, `_` and `-`).
    pub model_id: String,
    /// `language_code` (ISO 639-1, for example `"tr"`), or `None` to infer it from the text.
    /// Honoured only by models that support it; behaviour on the WebSocket is UNVERIFIED.
    pub language_code: Option<String>,
    /// Requested audio encoding.
    pub encoding: OutputEncoding,
    /// `inactivity_timeout` in seconds (1..=180): how long the server keeps a quiet context.
    pub inactivity_timeout_secs: u16,
    /// `sync_alignment` (server default `false`). Measured live (2026-10-13): off, the
    /// alignment times are relative to the message's own audio; on, they run from the start of
    /// the server context (each message continues the previous one's times). The adapter
    /// handles both; off is the default because it anchors every word to the audio of its own
    /// message.
    pub sync_alignment: bool,
    /// `apply_text_normalization`, or `None` for the server default.
    pub apply_text_normalization: Option<TextNormalization>,
    /// `enable_logging`, or `None` for the server default (`true`).
    pub enable_logging: Option<bool>,
    /// Voice settings for every context, or `None` for the voice's own.
    pub voice_settings: Option<VoiceSettings>,
    /// `generation_config.chunk_length_schedule` (each 50..=500). Brisvox flushes every text
    /// chunk, so this only matters for text that has not reached a flush; `None` is the
    /// server default.
    pub chunk_length_schedule: Option<Vec<u16>>,
    /// Report the words of `normalizedAlignment` (what is spoken: numbers spelled out)
    /// instead of `alignment` (the input text, which is what the LLM text matching of played text
    /// wants).
    pub use_normalized_alignment: bool,
    /// Decide what the server still owes from the alignment coverage (see the module docs).
    /// `false` times only the first response after each text, which cannot trip over an
    /// alignment that does not match the characters sent.
    pub completion_by_alignment: bool,
    /// A context quiet for this long gets a keepalive message.
    pub keepalive_interval: Duration,
    /// After `Flush`, how long without any message (and with all text aligned) before the
    /// context counts as finished although no `isFinal` came (the docs do not promise one).
    pub final_grace: Duration,
    /// Deadline of the connect, of every send and close, and of every wait for output the
    /// server owes.
    pub io_timeout: Duration,
    /// Voice and rate `warm()` opens a socket for (the URL carries the voice); `None`: `warm()`
    /// does nothing.
    pub warm_target: Option<(Arc<str>, SampleRate)>,
    /// A socket parked by [`warm`](TtsProvider::warm) older than this is discarded.
    pub warm_max_age: Duration,
    /// Time source for every deadline.
    pub clock: SharedClock,
    /// Cancelling this ends every session. A socket parked by `warm` is dropped the next time
    /// the provider looks at it (`warm` or `connect`), or when the provider is dropped: no
    /// task watches it while it waits.
    pub shutdown: CancellationToken,
}

impl ElevenLabsConfig {
    /// Defaults: Flash v2.5, 16-bit PCM, maximum inactivity timeout, keepalive every 10 s,
    /// 10 s deadlines.
    pub fn new(api_key: Secret<String>, clock: SharedClock) -> Self {
        Self {
            api_key,
            url: DEFAULT_URL.to_owned(),
            allow_insecure_ws: false,
            model_id: DEFAULT_MODEL.to_owned(),
            language_code: None,
            encoding: OutputEncoding::Pcm16,
            inactivity_timeout_secs: MAX_INACTIVITY_TIMEOUT_SECS,
            sync_alignment: false,
            apply_text_normalization: None,
            enable_logging: None,
            voice_settings: None,
            chunk_length_schedule: None,
            use_normalized_alignment: false,
            completion_by_alignment: true,
            keepalive_interval: Duration::from_secs(10),
            final_grace: Duration::from_secs(1),
            io_timeout: Duration::from_secs(10),
            warm_target: None,
            warm_max_age: Duration::from_secs(60),
            clock,
            shutdown: CancellationToken::new(),
        }
    }
}

/// A socket opened ahead of time by `warm()`.
struct Warm {
    socket: Socket,
    opened: Instant,
    /// The URL it was opened for (voice, model and format are in it).
    url: String,
}

/// What a session needs besides the socket.
struct Template {
    /// Full handshake URL (no secrets).
    url: String,
    settings: FrameSettings,
    rate: SampleRate,
    bytes_per_sample: u64,
}

pub(crate) struct Inner {
    pub(crate) config: ElevenLabsConfig,
    tls: Arc<rustls::ClientConfig>,
    key_header: HeaderValue,
    warm: Mutex<Option<Warm>>,
}

/// The ElevenLabs streaming TTS provider.
#[derive(Clone)]
pub struct ElevenLabsTts {
    inner: Arc<Inner>,
}

impl fmt::Debug for ElevenLabsTts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElevenLabsTts")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

/// A token that is safe in a URL path or query value without escaping.
fn is_token(text: &str, allow_dot: bool) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || (allow_dot && c == '.'))
}

impl ElevenLabsTts {
    /// Builds the provider. Does not touch the network.
    ///
    /// # Errors
    /// [`Error::Protocol`] if the configuration is invalid (an API key that cannot be an HTTP
    /// header value, a model, language or voice that is not a plain token, an inactivity
    /// timeout outside 1..=180, a chunk schedule outside 50..=500, voice settings out of range,
    /// a zero deadline, a URL that is not `wss://` or loopback `ws://`) or the TLS config cannot
    /// be built. The messages never contain the key.
    pub fn new(config: ElevenLabsConfig) -> Result<Self> {
        let invalid = |detail: &str| Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid configuration: {detail}"),
        };
        if config.io_timeout.is_zero() {
            return Err(invalid("io_timeout is zero"));
        }
        if config.keepalive_interval.is_zero() {
            return Err(invalid("keepalive_interval is zero"));
        }
        if config.final_grace.is_zero() {
            return Err(invalid("final_grace is zero"));
        }
        if !(1..=MAX_INACTIVITY_TIMEOUT_SECS).contains(&config.inactivity_timeout_secs) {
            return Err(invalid("inactivity_timeout_secs must be 1..=180"));
        }
        if !is_token(&config.model_id, true) {
            return Err(invalid("model_id is not a plain token"));
        }
        if let Some(language) = &config.language_code
            && !is_token(language, false)
        {
            return Err(invalid("language_code is not a plain token"));
        }
        if let Some(schedule) = &config.chunk_length_schedule
            && (schedule.is_empty() || schedule.iter().any(|v| !(50..=500).contains(v)))
        {
            return Err(invalid(
                "chunk_length_schedule must hold values in 50..=500",
            ));
        }
        if let Some(settings) = &config.voice_settings {
            settings.validate().map_err(invalid)?;
        }
        if let Some((voice, _)) = &config.warm_target
            && !is_token(voice, false)
        {
            return Err(invalid("the warm target voice is not a plain token"));
        }
        check_url(&config.url, config.allow_insecure_ws).map_err(invalid)?;
        let mut key_header = HeaderValue::from_str(config.api_key.expose())
            .map_err(|_| invalid("the API key is not a valid header value"))?;
        key_header.set_sensitive(true);
        Ok(Self {
            inner: Arc::new(Inner {
                tls: crate::net::tls_config()?,
                key_header,
                warm: Mutex::new(None),
                config,
            }),
        })
    }
}

impl TtsProvider for ElevenLabsTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    /// Opens a session: takes the socket parked by [`warm`](Self::warm) if it is for the same
    /// voice and rate, fresh and still open, otherwise connects (TCP, TLS and the WebSocket
    /// upgrade, all inside [`ElevenLabsConfig::io_timeout`]).
    ///
    /// # Errors
    /// `Rejected { 401 | 403 | 429 | .. }` for a refused handshake, `Core(Timeout)`,
    /// `Disconnected` for a network failure, `Protocol` for an invalid [`TtsConfig`] (a voice
    /// that is not a plain token, or mu-law/A-law at a rate other than 8 kHz) or a TLS failure.
    fn connect(&self, config: TtsConfig) -> impl Future<Output = Result<TtsSession>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let template = inner.template(&config.voice, config.sample_rate)?;
            let socket = inner.acquire(&template.url).await?;
            let id = SESSIONS.fetch_add(1, Ordering::Relaxed);
            Ok(session::spawn(inner, socket, template, id))
        }
    }

    /// Opens a socket for [`ElevenLabsConfig::warm_target`] and parks it for the next `connect`
    /// (at most one; no task is spawned, so nothing runs while it waits). Idempotent: a parked
    /// socket that is still fresh and open is kept. No keepalive is sent, so a socket older than
    /// [`ElevenLabsConfig::warm_max_age`] is discarded and `connect` opens a new one. Without a
    /// warm target there is nothing to open (the voice is part of the URL) and this is a no-op.
    ///
    /// # Errors
    /// The same as `connect`'s handshake errors.
    fn warm(&self) -> impl Future<Output = Result<()>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            if inner.config.shutdown.is_cancelled() {
                // Shut down: nothing to warm, and a socket parked earlier is released.
                *lock(&inner.warm) = None;
                return Ok(());
            }
            let Some((voice, rate)) = inner.config.warm_target.clone() else {
                return Ok(());
            };
            let template = inner.template(&voice, rate)?;
            if inner.warm_is_ready(&template.url) {
                return Ok(());
            }
            let socket = inner.open(&template.url).await?;
            let opened = inner.config.clock.now();
            *lock(&inner.warm) = Some(Warm {
                socket,
                opened,
                url: template.url,
            });
            Ok(())
        }
    }
}

impl Inner {
    /// The session settings and handshake URL for `voice` at `rate`.
    fn template(&self, voice: &str, rate: SampleRate) -> Result<Template> {
        let invalid = |detail: &str| Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid session configuration: {detail}"),
        };
        if !is_token(voice, false) {
            return Err(invalid("the voice id is empty or not a plain token"));
        }
        let config = &self.config;
        let format = config.encoding.format_name(rate).map_err(invalid)?;
        let mut url = format!(
            "{}/v1/text-to-speech/{voice}/multi-stream-input?model_id={}&output_format={format}\
             &inactivity_timeout={}&sync_alignment={}",
            config.url.trim_end_matches('/'),
            config.model_id,
            config.inactivity_timeout_secs,
            config.sync_alignment,
        );
        if let Some(language) = &config.language_code {
            url.push_str("&language_code=");
            url.push_str(language);
        }
        if let Some(mode) = config.apply_text_normalization {
            url.push_str("&apply_text_normalization=");
            url.push_str(mode.wire_name());
        }
        if let Some(logging) = config.enable_logging {
            url.push_str(if logging {
                "&enable_logging=true"
            } else {
                "&enable_logging=false"
            });
        }
        Ok(Template {
            url,
            settings: FrameSettings {
                voice_settings: config.voice_settings.clone(),
                chunk_length_schedule: config.chunk_length_schedule.clone(),
            },
            rate,
            bytes_per_sample: config.encoding.bytes_per_sample(),
        })
    }

    /// A usable socket for `url`: the parked one, or a new one.
    pub(crate) async fn acquire(&self, url: &str) -> Result<Socket> {
        match self.take_warm(url) {
            Some(socket) => Ok(socket),
            None => self.open(url).await,
        }
    }

    /// Opens a new socket within the deadline.
    pub(crate) async fn open(&self, url: &str) -> Result<Socket> {
        let request = self.request(url)?;
        let connector = Connector::Rustls(Arc::clone(&self.tls));
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let connect = connect_ws(request, Some(config), Some(connector), &*self.config.clock);
        match with_timeout(
            &*self.config.clock,
            self.config.io_timeout,
            "elevenlabs connect",
            connect,
        )
        .await?
        {
            Ok((socket, _response)) => Ok(socket),
            Err(e) => Err(map_ws_error(e)),
        }
    }

    /// The handshake request: the URL, the key in a sensitive header.
    fn request(
        &self,
        url: &str,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let mut request = url.into_client_request().map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "invalid websocket url".to_owned(),
        })?;
        request
            .headers_mut()
            .insert("xi-api-key", self.key_header.clone());
        Ok(request)
    }

    /// Takes the parked socket if it is for `url`, fresh and still open. A parked socket for
    /// another URL stays parked.
    fn take_warm(&self, url: &str) -> Option<Socket> {
        let mut slot = lock(&self.warm);
        if slot.as_ref().is_none_or(|warm| warm.url != url) {
            return None;
        }
        let warm = slot.take()?;
        drop(slot);
        if self.config.shutdown.is_cancelled() {
            return None;
        }
        let age = self
            .config
            .clock
            .now()
            .saturating_duration_since(warm.opened);
        let mut socket = warm.socket;
        (age <= self.config.warm_max_age && still_open(&mut socket)).then_some(socket)
    }

    /// Whether a fresh, open socket for `url` is parked (a stale or closed one is dropped).
    fn warm_is_ready(&self, url: &str) -> bool {
        let mut slot = lock(&self.warm);
        let cancelled = self.config.shutdown.is_cancelled();
        let ready = !cancelled
            && slot.as_mut().is_some_and(|warm| {
                let age = self
                    .config
                    .clock
                    .now()
                    .saturating_duration_since(warm.opened);
                warm.url == url && age <= self.config.warm_max_age && still_open(&mut warm.socket)
            });
        if !ready {
            *slot = None;
        }
        ready
    }
}

/// Accepts `wss://` and, for loopback hosts (or with the explicit opt-in), `ws://`; no path,
/// query or credentials (the adapter adds them).
///
/// # Errors
/// A static description of what is wrong; it never contains the URL.
fn check_url(url: &str, allow_insecure: bool) -> std::result::Result<(), &'static str> {
    let uri: tokio_tungstenite::tungstenite::http::Uri =
        url.parse().map_err(|_| "the websocket url is not valid")?;
    if uri.query().is_some() || !uri.path().trim_matches('/').is_empty() {
        return Err("the websocket url must be a base url without path or query");
    }
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err("the websocket url must not carry credentials");
    }
    match uri.scheme_str() {
        Some("wss") => Ok(()),
        Some("ws") => {
            let host = uri.host().unwrap_or("");
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback || allow_insecure {
                Ok(())
            } else {
                Err(
                    "plain ws:// to a non-loopback host would send the API key unencrypted (use wss://, or set allow_insecure_ws)",
                )
            }
        }
        _ => Err("the websocket url must start with wss:// (or ws:// for loopback)"),
    }
}

/// A parked socket is open if polling it yields nothing (no close frame, no error, no data).
fn still_open(socket: &mut Socket) -> bool {
    socket.next().now_or_never().is_none()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned slot only ever holds an `Option` that is replaced wholesale.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Awaits `future` for at most `after` on `clock`.
///
/// # Errors
/// `Core(Timeout { what, after })` when the deadline passes first.
pub(crate) async fn with_timeout<F: Future>(
    clock: &dyn Clock,
    after: Duration,
    what: &'static str,
    future: F,
) -> Result<F::Output> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = wait_for(clock, after) => Err(Error::Core(CoreError::Timeout { what, after })),
    }
}

/// Maps a WebSocket-layer error. An HTTP refusal of the upgrade carries the status; connection
/// loss is `Disconnected`; TLS and protocol violations are `Protocol` (not retryable by
/// reconnecting). The messages contain no request data, so no credentials.
pub(crate) fn map_ws_error(error: WsError) -> Error {
    match error {
        WsError::Http(response) => Error::Rejected {
            provider: PROVIDER,
            status: response.status().as_u16(),
        },
        WsError::ConnectionClosed
        | WsError::AlreadyClosed
        | WsError::Io(_)
        | WsError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
            | ProtocolError::SendAfterClosing
            | ProtocolError::ReceivedAfterClosing,
        ) => Error::Disconnected { provider: PROVIDER },
        WsError::Tls(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("tls: {e}"),
        },
        other => Error::Protocol {
            provider: PROVIDER,
            detail: format!("websocket: {other}"),
        },
    }
}

/// Maps a close frame the server sent. Ordinary closes (normal, going away, no status,
/// abnormal, restart, try again) are a disconnect; any other code (1008 policy violation,
/// 1011 internal error, application codes) is a refusal that carries the code, because the
/// docs name none for authentication or limit failures (UNVERIFIED).
pub(crate) fn map_close_code(code: u16) -> Error {
    match code {
        1000 | 1001 | 1005 | 1006 | 1012 | 1013 => Error::Disconnected { provider: PROVIDER },
        other => Error::Rejected {
            provider: PROVIDER,
            status: other,
        },
    }
}

/// The server context id for the n-th server context of a session.
pub(crate) fn server_context_id(session: u64, n: u64) -> String {
    format!("bv{session}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_must_be_bare_secure_bases() {
        assert!(check_url("wss://api.elevenlabs.io", false).is_ok());
        assert!(check_url("wss://api.eu.residency.elevenlabs.io/", false).is_ok());
        assert!(check_url("ws://127.0.0.1:9", false).is_ok());
        assert!(check_url("ws://localhost:9", false).is_ok());
        assert!(check_url("ws://[::1]:9", false).is_ok());
        assert!(check_url("ws://example.com", false).is_err());
        assert!(check_url("ws://example.com", true).is_ok());
        assert!(check_url("https://api.elevenlabs.io", false).is_err());
        assert!(check_url("wss://api.elevenlabs.io/v1/x", false).is_err());
        assert!(check_url("wss://api.elevenlabs.io?a=b", false).is_err());
        assert!(check_url("wss://user:pw@api.elevenlabs.io", false).is_err());
    }

    #[test]
    fn tokens_reject_url_metacharacters() {
        assert!(is_token("eleven_flash_v2_5", true));
        assert!(is_token("21m00Tcm4TlvDq8ikWAM", false));
        for bad in ["", "a/b", "a?b", "a b", "a&b", "a#b", "a%2f", "a.b", "../x"] {
            assert!(!is_token(bad, false), "{bad:?}");
        }
        assert!(is_token("a.b", true));
    }

    #[test]
    fn output_format_follows_encoding_and_rate() {
        assert_eq!(
            OutputEncoding::Pcm16.format_name(SampleRate::Hz24000),
            Ok("pcm_24000".to_owned())
        );
        assert_eq!(
            OutputEncoding::Mulaw.format_name(SampleRate::Hz8000),
            Ok("ulaw_8000".to_owned())
        );
        assert_eq!(
            OutputEncoding::Alaw.format_name(SampleRate::Hz8000),
            Ok("alaw_8000".to_owned())
        );
        assert!(
            OutputEncoding::Mulaw
                .format_name(SampleRate::Hz16000)
                .is_err()
        );
        assert!(
            OutputEncoding::Alaw
                .format_name(SampleRate::Hz24000)
                .is_err()
        );
    }

    #[test]
    fn close_codes_map_to_disconnect_or_rejection() {
        assert!(matches!(map_close_code(1000), Error::Disconnected { .. }));
        assert!(matches!(map_close_code(1006), Error::Disconnected { .. }));
        assert!(matches!(
            map_close_code(1008),
            Error::Rejected { status: 1008, .. }
        ));
        assert!(matches!(
            map_close_code(4401),
            Error::Rejected { status: 4401, .. }
        ));
    }

    #[test]
    fn voice_settings_are_range_checked() {
        assert!(VoiceSettings::default().validate().is_ok());
        let v = |f: fn(&mut VoiceSettings)| {
            let mut s = VoiceSettings::default();
            f(&mut s);
            s.validate()
        };
        assert!(v(|s| s.stability = Some(1.1)).is_err());
        assert!(v(|s| s.similarity_boost = Some(-0.1)).is_err());
        assert!(v(|s| s.speed = Some(0.5)).is_err());
        assert!(v(|s| s.speed = Some(1.2)).is_ok());
    }
}
