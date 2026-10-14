//! Deepgram Aura streaming text-to-speech over one WebSocket per session (feature
//! `provider-deepgram-aura`; protocol facts in `docs/providers/deepgram-aura.md`, fetched
//! 2026-10-08).
//!
//! # Shape
//! [`DeepgramAuraTts`] implements [`TtsProvider`]. `connect` opens (or takes the socket that
//! [`warm`](TtsProvider::warm) parked for the same voice and rate) a
//! `wss://api.deepgram.com/v1/speak?model=..&encoding=..&sample_rate=..` connection and spawns
//! one task that owns it. Aura has no contexts: a connection has ONE text buffer and ONE audio
//! stream, binary audio frames carry no utterance marker, and the voice and output format are
//! fixed for the life of the socket.
//!
//! # Mapping brisvox contexts onto the single stream
//! The task keeps a FIFO of engine [`ContextId`](crate::ContextId)s and only the head is "on the
//! wire": its text is sent as `Speak` messages (split to at most
//! [`DeepgramAuraConfig::max_speak_chars`] characters at sentence/space boundaries, spaces kept),
//! its audio is whatever binary frames arrive, and it ends when the `Flushed` of its last `Flush`
//! arrives ([`TtsEvent::Flushed`](crate::TtsEvent::Flushed)). Text and flushes for any other
//! context wait in the FIFO and go out when the head has finished (or was cancelled and the
//! server has answered `Clear`). One in-flight utterance at a time is what makes attribution of
//! unlabelled frames by order sound. Consequence for the engine: flush or cancel a context before
//! relying on the next one's audio; a context that is never flushed or cancelled holds the
//! others back (the queue is bounded, input backpressures).
//!
//! # Flushing
//! A `Flush` is sent when the engine flushes the context. Deepgram allows 20 per 60 s and warns
//! that frequent flushes hurt audio quality, so by default ([`FlushPolicy::PerTurn`]) a text
//! chunk alone is sent without one and the audio starts when the server decides to generate
//! (whether it does so without a `Flush` is UNVERIFIED; the Flush page implies some automatic
//! processing). [`FlushPolicy::PerText`] flushes after every `Text` input instead (first audio
//! does not depend on the unverified auto-trigger; costs flush budget and, per the docs,
//! possibly prosody) and arms the stall deadline from the first text, which the PerTurn mode can
//! only do once the turn's flush is out. Under either policy an explicit flush that finds the
//! budget used up waits until a slot frees (bounded by the 60 s window); `PerText` flushes stop
//! four short of the limit so the turn flush always has room. A flush swallowed by the server
//! would never yield `Flushed`: the stall deadline turns that into a `Timeout`. When more than
//! 1800 characters of one context would sit between two flushes, a flush is made before the
//! next `Speak` (the docs' 2000 character limit is "per request"; whether that is a `Speak`, a
//! flush interval or the connection is UNVERIFIED, and the breach is close 1009, which would end
//! the call). That costs one unit of the flush budget per 1800 characters, about two and a
//! half minutes of speech.
//!
//! Silence while text is on the server and no `Flushed` is owed (`PerTurn` between the first
//! `Speak` and the turn's `Flush`, or a send held back by a rate limit) cannot be told from a
//! slow LLM by waiting for output. The session therefore probes the connection: half an
//! `io_timeout` after the last sign of life it sends a WebSocket `Ping`, and the `Pong` (RFC
//! 6455 obliges every server to answer; that Aura's edge does is UNVERIFIED) or any other frame
//! restarts the deadline. No sign of life within [`DeepgramAuraConfig::io_timeout`] is a
//! `Timeout`. A server that is alive but never generates without a `Flush` is NOT detected
//! (UNVERIFIED open item 3 of the provider doc: the live test measures it).
//!
//! # Cancel (barge-in): local first
//! `Clear` is the only cancel and the server answers `Cleared` only after the remaining audio,
//! so the cancel does not wait for it. The shutdown token and the cancel channel are also served
//! while a connect, a send, a pong flush or the close of a replaced connection is pending. The cancel channel is polled before input and before the
//! socket, also while a write or a connect is in progress. A cancel (1) forgets the context so
//! every later frame is dropped, (2) removes its not yet delivered events from the outgoing
//! queue, (3) hands [`TtsEvent::Cancelled`](crate::TtsEvent::Cancelled) to the consumer at once
//! (before any I/O), and (4) sends `Clear` if the server may hold text of it: text was written
//! on the current connection and is unflushed or has a `Flushed` still owed. A context that was
//! cancelled while the socket was being (re)connected, or that was fully answered, gets no
//! `Clear` (the answer to a `Clear` on an empty buffer is UNVERIFIED, so none is provoked).
//! From then until
//! `Cleared` arrives every binary frame and `Flushed` is dropped (they belong to the cancelled
//! utterance) and nothing of the next context is sent (whether text sent between `Clear` and
//! `Cleared` survives is UNVERIFIED, so one round trip is spent to avoid the question). A
//! `Cleared` that does not come within [`DeepgramAuraConfig::io_timeout`] of the last sign of
//! life is treated as lost (the server may not answer a `Clear` on an empty buffer, UNVERIFIED):
//! the session logs a warning and goes on (a `Cleared` that arrives later than that is matched to
//! the next `Clear` in flight, if any: `sequence_id` is UNVERIFIED and not used). A cancel of a context that is queued, finished or
//! unknown still yields `Cancelled`; later input for a cancelled id is ignored (the last 64
//! cancels are remembered; do not reuse ids).
//!
//! # Words
//! `/v1/speak` offers no timestamps of any kind, so this adapter NEVER emits
//! [`TtsEvent::Word`](crate::TtsEvent::Word). The consumer falls back to estimating word
//! times from the text and the audio duration (a char-rate estimate); truncation on barge-in is
//! therefore coarse. `Metadata` carries no timing either. Flux TTS (`/v2/speak`) does not offer
//! word times either (a candidate follow-up adapter for English).
//!
//! # Turkish
//! Aura-2 languages: English, Spanish, German, French, Dutch, Italian, Japanese. There is NO
//! Turkish voice (as of 2026-10-08). The adapter refuses a Turkish voice
//! (`aura-2-..-tr`) at `connect` and a [`DeepgramAuraConfig::language`] of `tr` in
//! [`DeepgramAuraTts::new`] with `Error::Protocol`; use Cartesia (or another provider) for
//! Turkish. See [`supports_language`].
//!
//! # Limits kept by the client
//! At most [`DeepgramAuraConfig::max_flushes_per_window`] flushes and
//! [`DeepgramAuraConfig::chars_per_window`] characters per [`DeepgramAuraConfig::rate_window`]
//! (20, 2400 and 60 s), on the injected clock. A `Speak` that would exceed the character budget
//! waits for old characters to leave the window (the server closes the socket with 1008 if it
//! is exceeded). Connections older than [`DeepgramAuraConfig::max_connection_age`] (the server
//! limit is 60 minutes) are replaced between utterances.
//!
//! # Backpressure, bounds and overflow
//! Channels are bounded ([`TTS_CHANNEL_CAPACITY`](crate::tts::TTS_CHANNEL_CAPACITY), [`TTS_CANCEL_CAPACITY`](crate::tts::TTS_CANCEL_CAPACITY)). Events wait in an
//! internal queue drained to the events channel without blocking the task; the socket is not
//! read while that queue holds 64 or more events (TCP backpressure, nothing is dropped; one
//! server frame adds one event). At most 64 contexts and 128 text pieces wait in the FIFO; beyond
//! that the task stops reading `input` (the sender blocks) until the head finishes. The cancel
//! channel is never gated.
//!
//! # Idle, expiry, reconnect
//! No keepalive message is sent: none is documented for `/v1/speak`, and the idle timeout is
//! UNVERIFIED (the liveness `Ping` above only runs while text is on the server). If the socket
//! closes while the server owes nothing (no text of the head unflushed and no `Flushed` owed,
//! even if the engine has not flushed the context yet), the session does not fail: the next send
//! reconnects lazily (one connect, bounded by [`DeepgramAuraConfig::io_timeout`]; the very
//! first send of an utterance is retried once on a fresh socket if the old one turns out to be
//! dead). A close or error while text sent for an unfinished utterance is lost with it is
//! `Error::Disconnected` and ends the session. Dropping `input` flushes what is open (no
//! `Flushed` is made up for contexts the engine never flushed), delivers the remaining audio,
//! sends `Close` and ends `events`.
//!
//! # Errors
//! Handshake refusals are `Rejected { status }` (401, 403, 429, ..). Close code 1003/1008/1009
//! is `Protocol` (with the code and the server's reason such as `DATA-0001`), 1011 and every
//! other close is `Disconnected`. An unparsable JSON frame is `Protocol`; `Warning` is logged and
//! ignored; unknown message types are ignored.
//!
//! # Timeouts
//! Connect, every send, the close and every wait for owed server output (`Flushed`, `Cleared`)
//! carry the explicit [`DeepgramAuraConfig::io_timeout`] on the injected clock, and so does the
//! silence of a server that holds unflushed text (the probe above). A stalled wait is
//! `Error::Core(Timeout)` and ends the session (except a lost `Cleared`, above).
//!
//! # Secrets
//! The API key is a [`Secret`], sent only as `Authorization: Token <key>` in the handshake
//! (marked sensitive), never in the URL, logs, errors or `Debug` output.

mod limits;
mod session;
mod wire;

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use brisvox_core::clock::wait_for;
use brisvox_core::{Clock, Error as CoreError, SharedClock};
use futures::{FutureExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

pub use brisvox_core::Secret;
pub use wire::{SUPPORTED_LANGUAGES, supports_language};

use crate::error::{Error, Result};
use crate::net::connect_ws;
use crate::tts::{TtsConfig, TtsProvider, TtsSession};
use wire::SessionParams;

/// Provider name used in errors.
pub(crate) const PROVIDER: &str = "deepgram-aura";

/// The default endpoint (v1 streaming TTS).
pub const DEFAULT_URL: &str = "wss://api.deepgram.com/v1/speak";

/// The voice the docs use throughout their examples. [`TtsConfig::voice`] is the model string;
/// this constant is for callers who need a default.
pub const DEFAULT_MODEL: &str = "aura-2-thalia-en";

/// A parked or live Aura socket.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted (audio frames are a few KiB).
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Audio encoding requested from Aura (`encoding` query parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OutputEncoding {
    /// Signed 16-bit little-endian PCM (`linear16`): what [`TtsEvent`](crate::TtsEvent)
    /// documents. Rates 8000, 16000, 24000, 32000, 48000 (brisvox offers 8000, 16000, 24000 and
    /// 48000).
    #[default]
    Pcm16,
    /// G.711 mu-law (`mulaw`) for telephony; 8000 or 16000 Hz. The audio events then carry
    /// mu-law bytes, one per sample, not PCM16: the caller must not treat them as PCM16.
    Mulaw,
    /// G.711 A-law (`alaw`); same rules as [`OutputEncoding::Mulaw`].
    Alaw,
}

impl OutputEncoding {
    /// The wire name.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Pcm16 => "linear16",
            Self::Mulaw => "mulaw",
            Self::Alaw => "alaw",
        }
    }

    /// Bytes per sample.
    pub(crate) fn bytes_per_sample(self) -> u64 {
        match self {
            Self::Pcm16 => 2,
            Self::Mulaw | Self::Alaw => 1,
        }
    }

    /// Sample rates (Hz) the endpoint accepts with this encoding.
    fn allowed_rates(self) -> &'static [u32] {
        match self {
            Self::Pcm16 => &[8_000, 16_000, 24_000, 32_000, 48_000],
            Self::Mulaw | Self::Alaw => &[8_000, 16_000],
        }
    }
}

/// When the session sends `Flush` (see the module docs, "Flushing").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FlushPolicy {
    /// Flush only when the engine flushes a context (once per bot turn; plus one per 1800
    /// characters of a very long turn). Default: Deepgram limits flushes to 20 per 60 s and
    /// warns about their effect on quality. Assumes the server starts generating without a
    /// `Flush` (UNVERIFIED): if it does not, first audio only comes with the turn's flush.
    /// Silence is still bounded by a liveness `Ping` (module docs, "Flushing").
    #[default]
    PerTurn,
    /// Also flush after every `Text` input, as long as the flush budget has room beyond a
    /// reserve for turn flushes. For a server that does not start generating on its own
    /// (UNVERIFIED), and so that a silent server is noticed from the first text on.
    PerText,
}

/// How the Deepgram Aura adapter is configured. Build with [`DeepgramAuraConfig::new`], then
/// set fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeepgramAuraConfig {
    /// API key, sent as `Authorization: Token <key>` in the handshake.
    pub api_key: Secret<String>,
    /// WebSocket endpoint without query. Plain `ws://` is accepted only for a loopback host
    /// (tests, a local proxy) unless [`allow_insecure_ws`](Self::allow_insecure_ws) is set: the
    /// API key is sent in a header of the handshake.
    pub url: String,
    /// Allow `ws://` to a host that is not loopback. The API key then travels unencrypted:
    /// only for a trusted network path.
    pub allow_insecure_ws: bool,
    /// Requested audio encoding.
    pub encoding: OutputEncoding,
    /// Language tag of the calls this provider serves (for example `"en"`), or `None`. Only
    /// used to refuse an unsupported language early: `"tr"` is refused by
    /// [`DeepgramAuraTts::new`] because Aura has no Turkish voice.
    pub language: Option<String>,
    /// Speaking rate (`speed` query parameter, 0.7..=1.5; "not yet supported in all
    /// languages"), or `None` for the server default (1.0).
    pub speed: Option<f32>,
    /// When to send `Flush`.
    pub flush_policy: FlushPolicy,
    /// Longest `Speak` text in characters (the docs say 2000 per request, the scope is
    /// UNVERIFIED, hence the margin). Longer text is split.
    pub max_speak_chars: usize,
    /// Character budget per [`rate_window`](Self::rate_window) (docs: 2400 per minute).
    pub chars_per_window: usize,
    /// Flush budget per [`rate_window`](Self::rate_window) (docs: 20 per 60 s).
    pub max_flushes_per_window: usize,
    /// Length of the window both budgets are measured over (docs: 60 s).
    pub rate_window: Duration,
    /// A connection older than this is replaced between utterances (docs: 60 minutes per
    /// connection).
    pub max_connection_age: Duration,
    /// Deadline of the connect, of every send and close, and of every wait for output the
    /// server owes.
    pub io_timeout: Duration,
    /// A socket parked by [`warm`](TtsProvider::warm) older than this is discarded.
    pub warm_max_age: Duration,
    /// What [`warm`](TtsProvider::warm) connects for when no session has been opened yet
    /// (Aura fixes voice and rate per socket, so a parked socket can only serve a matching
    /// `connect`). After a `connect`, its configuration is used instead. `None` and no
    /// session yet: `warm` does nothing.
    pub warm_target: Option<TtsConfig>,
    /// Time source for every deadline.
    pub clock: SharedClock,
    /// Cancelling this ends every session. A socket parked by `warm` is dropped the next time
    /// the provider looks at it (`warm` or `connect`), or when the provider is dropped.
    pub shutdown: CancellationToken,
}

impl DeepgramAuraConfig {
    /// Defaults: 16-bit PCM, flush once per turn, the documented limits, 10 s deadlines.
    pub fn new(api_key: Secret<String>, clock: SharedClock) -> Self {
        Self {
            api_key,
            url: DEFAULT_URL.to_owned(),
            allow_insecure_ws: false,
            encoding: OutputEncoding::Pcm16,
            language: None,
            speed: None,
            flush_policy: FlushPolicy::PerTurn,
            max_speak_chars: 1000,
            chars_per_window: 2400,
            max_flushes_per_window: 20,
            rate_window: Duration::from_secs(60),
            max_connection_age: Duration::from_secs(55 * 60),
            io_timeout: Duration::from_secs(10),
            warm_max_age: Duration::from_secs(60),
            warm_target: None,
            clock,
            shutdown: CancellationToken::new(),
        }
    }
}

/// A socket opened ahead of time by `warm()`.
struct Warm {
    socket: Socket,
    opened: Instant,
    params: SessionParams,
}

struct Inner {
    config: DeepgramAuraConfig,
    tls: Arc<rustls::ClientConfig>,
    auth_header: HeaderValue,
    warm: Mutex<Option<Warm>>,
    /// Parameters of the latest `connect` (what `warm` prepares for).
    last: Mutex<Option<SessionParams>>,
}

/// The Deepgram Aura streaming TTS provider.
#[derive(Clone)]
pub struct DeepgramAuraTts {
    inner: Arc<Inner>,
}

impl fmt::Debug for DeepgramAuraTts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepgramAuraTts")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl DeepgramAuraTts {
    /// Builds the provider. Does not touch the network.
    ///
    /// # Errors
    /// [`Error::Protocol`] if the configuration is invalid (an API key that cannot be an HTTP
    /// header value, a Turkish or otherwise unsupported `language` or `warm_target` voice, a
    /// `speed` outside 0.7..=1.5, a zero deadline or window, a zero limit, `max_speak_chars`
    /// above the character budget, a URL that is not `wss://` or loopback `ws://`) or the TLS
    /// config cannot be built. The messages never contain the key.
    pub fn new(config: DeepgramAuraConfig) -> Result<Self> {
        let invalid = |detail: &str| Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid configuration: {detail}"),
        };
        if config.io_timeout.is_zero() {
            return Err(invalid("io_timeout is zero"));
        }
        if config.rate_window.is_zero() || config.max_connection_age.is_zero() {
            return Err(invalid("rate_window or max_connection_age is zero"));
        }
        if config.max_speak_chars == 0
            || config.chars_per_window == 0
            || config.max_flushes_per_window == 0
        {
            return Err(invalid("a limit is zero"));
        }
        if config.max_speak_chars > config.chars_per_window {
            return Err(invalid("max_speak_chars is above chars_per_window"));
        }
        if let Some(speed) = config.speed
            && !(0.7..=1.5).contains(&speed)
        {
            return Err(invalid("speed must be within 0.7..=1.5"));
        }
        if let Some(language) = &config.language
            && !supports_language(language)
        {
            let detail = if language.to_ascii_lowercase().starts_with("tr") {
                "Deepgram Aura has no Turkish voice (not supported as of 2026-10-08); use \
                 another TTS provider for Turkish calls"
            } else {
                "Deepgram Aura has no voice for this language (see SUPPORTED_LANGUAGES)"
            };
            return Err(invalid(detail));
        }
        check_url(&config.url, config.allow_insecure_ws).map_err(invalid)?;
        if let Some(target) = &config.warm_target {
            wire::validate_voice(&target.voice)?;
        }
        let mut auth_header = HeaderValue::from_str(&format!("Token {}", config.api_key.expose()))
            .map_err(|_| invalid("the API key is not a valid header value"))?;
        auth_header.set_sensitive(true);
        Ok(Self {
            inner: Arc::new(Inner {
                tls: crate::net::tls_config()?,
                auth_header,
                warm: Mutex::new(None),
                last: Mutex::new(None),
                config,
            }),
        })
    }
}

impl TtsProvider for DeepgramAuraTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    /// Opens a session: takes the socket parked by [`warm`](Self::warm) if it is fresh, open
    /// and was opened for the same voice and rate, otherwise connects (TCP, TLS and the
    /// WebSocket upgrade, all inside [`DeepgramAuraConfig::io_timeout`]).
    ///
    /// # Errors
    /// `Rejected { 401 | 403 | 429 | .. }` for a refused handshake, `Core(Timeout)`,
    /// `Disconnected` for a network failure, `Protocol` for an invalid [`TtsConfig`] (an empty
    /// or malformed voice, a Turkish or otherwise unsupported language, a sample rate or
    /// encoding combination Aura does not offer) or a TLS failure.
    fn connect(&self, config: TtsConfig) -> impl Future<Output = Result<TtsSession>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let params = inner.params(&config)?;
            *lock(&inner.last) = Some(params.clone());
            let socket = inner.acquire(&params).await?;
            Ok(session::spawn(inner, socket, params))
        }
    }

    /// Opens a socket for the latest session's voice and rate (or
    /// [`DeepgramAuraConfig::warm_target`]) and parks it for the next `connect` (at most one; no
    /// task is spawned, so nothing runs while it waits). Idempotent: a parked socket that is
    /// still fresh and open is kept. No keepalive is sent, so a socket older than
    /// [`DeepgramAuraConfig::warm_max_age`] is discarded.
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
            let params = match lock(&inner.last).clone() {
                Some(params) => params,
                None => match inner.config.warm_target.as_ref() {
                    Some(target) => inner.params(target)?,
                    None => return Ok(()),
                },
            };
            if inner.warm_is_ready(&params) {
                return Ok(());
            }
            let socket = inner.open(&params).await?;
            let opened = inner.config.clock.now();
            *lock(&inner.warm) = Some(Warm {
                socket,
                opened,
                params,
            });
            Ok(())
        }
    }
}

impl Inner {
    /// The connection parameters for `config`, validated.
    fn params(&self, config: &TtsConfig) -> Result<SessionParams> {
        wire::validate_voice(&config.voice)?;
        let encoding = self.config.encoding;
        let hz = config.sample_rate.hz();
        if !encoding.allowed_rates().contains(&hz) {
            return Err(Error::Protocol {
                provider: PROVIDER,
                detail: format!(
                    "invalid session configuration: {} output is not offered at {hz} Hz \
                     (allowed: {:?})",
                    encoding.wire_name(),
                    encoding.allowed_rates()
                ),
            });
        }
        Ok(SessionParams {
            model: config.voice.to_string(),
            encoding,
            sample_rate: hz,
            speed: self.config.speed,
        })
    }

    /// A usable socket: the parked one if it matches, or a new one.
    async fn acquire(&self, params: &SessionParams) -> Result<Socket> {
        match self.take_warm(params) {
            Some(socket) => Ok(socket),
            None => self.open(params).await,
        }
    }

    /// Opens a new socket within the deadline.
    async fn open(&self, params: &SessionParams) -> Result<Socket> {
        let request = self.request(params)?;
        let connector = Connector::Rustls(Arc::clone(&self.tls));
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let connect = connect_ws(request, Some(config), Some(connector), &*self.config.clock);
        match with_timeout(
            &*self.config.clock,
            self.config.io_timeout,
            "deepgram-aura connect",
            connect,
        )
        .await?
        {
            Ok((socket, _response)) => Ok(socket),
            Err(e) => Err(map_ws_error(e)),
        }
    }

    /// The handshake request: URL with the query, the key in a sensitive header.
    fn request(
        &self,
        params: &SessionParams,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let separator = if self.config.url.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!("{}{separator}{}", self.config.url, params.query());
        let mut request = url.into_client_request().map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "invalid websocket url".to_owned(),
        })?;
        request
            .headers_mut()
            .insert("Authorization", self.auth_header.clone());
        Ok(request)
    }

    /// Takes the parked socket if it is fresh, still open and opened for `params`.
    fn take_warm(&self, params: &SessionParams) -> Option<Socket> {
        let warm = lock(&self.warm).take()?;
        if self.config.shutdown.is_cancelled() || warm.params != *params {
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

    /// Whether a fresh, open socket for `params` is parked (a stale, closed or mismatched one is
    /// dropped).
    fn warm_is_ready(&self, params: &SessionParams) -> bool {
        let mut slot = lock(&self.warm);
        let cancelled = self.config.shutdown.is_cancelled();
        let ready = !cancelled
            && slot.as_mut().is_some_and(|warm| {
                let age = self
                    .config
                    .clock
                    .now()
                    .saturating_duration_since(warm.opened);
                warm.params == *params
                    && age <= self.config.warm_max_age
                    && still_open(&mut warm.socket)
            });
        if !ready {
            *slot = None;
        }
        ready
    }
}

/// Accepts `wss://` and, for loopback hosts (or with the explicit opt-in), `ws://`.
///
/// # Errors
/// A static description of what is wrong; it never contains the URL.
fn check_url(url: &str, allow_insecure: bool) -> std::result::Result<(), &'static str> {
    let uri: tokio_tungstenite::tungstenite::http::Uri =
        url.parse().map_err(|_| "the websocket url is not valid")?;
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
                    "plain ws:// to a non-loopback host would send the API key unencrypted \
                     (use wss://, or set allow_insecure_ws)",
                )
            }
        }
        _ => Err("the websocket url must start with wss:// (or ws:// for loopback)"),
    }
}

/// A parked socket is open if polling it yields nothing but benign messages. The server sends
/// `Metadata` on open (timing UNVERIFIED), so text, pings and pongs are consumed and skipped.
fn still_open(socket: &mut Socket) -> bool {
    for _ in 0..8 {
        match socket.next().now_or_never() {
            None => return true,
            Some(Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_)))) => {}
            Some(_) => return false,
        }
    }
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_must_be_secure_or_loopback() {
        assert!(check_url("wss://api.deepgram.com/v1/speak", false).is_ok());
        assert!(check_url("ws://127.0.0.1:9/v1/speak", false).is_ok());
        assert!(check_url("ws://localhost:9", false).is_ok());
        assert!(check_url("ws://[::1]:9", false).is_ok());
        assert!(check_url("ws://example.com", false).is_err());
        assert!(check_url("ws://example.com", true).is_ok());
        assert!(check_url("https://api.deepgram.com", false).is_err());
    }

    /// The documented limits (docs/providers/deepgram-aura.md, "Limits") are what production
    /// runs with: every test overrides them with tiny windows, so the defaults are pinned here.
    #[test]
    fn the_defaults_follow_the_documented_limits() {
        let clock = brisvox_core::SystemClock::shared();
        let config = DeepgramAuraConfig::new(Secret::new("key".to_owned()), clock);
        // At most 20 flushes per 60 s, 2400 characters per minute.
        assert_eq!(config.max_flushes_per_window, 20);
        assert_eq!(config.chars_per_window, 2400);
        assert_eq!(config.rate_window, Duration::from_secs(60));
        // Requests of at most 2000 characters (UNVERIFIED scope): a `Speak` stays well below.
        assert_eq!(config.max_speak_chars, 1000);
        assert!(config.max_speak_chars < 2000);
        // 60 minutes per connection: replaced before that.
        assert_eq!(config.max_connection_age, Duration::from_secs(55 * 60));
        assert!(config.max_connection_age < Duration::from_secs(60 * 60));
        assert_eq!(config.io_timeout, Duration::from_secs(10));
        assert_eq!(config.warm_max_age, Duration::from_secs(60));
        assert_eq!(config.flush_policy, FlushPolicy::PerTurn);
        assert_eq!(config.encoding, OutputEncoding::Pcm16);
        assert!(!config.allow_insecure_ws);
        assert_eq!(config.url, DEFAULT_URL);
        // The defaults are accepted by the constructor.
        assert!(DeepgramAuraTts::new(config).is_ok());
    }

    #[test]
    fn the_rate_table_follows_the_docs() {
        for rate in [8_000, 16_000, 24_000, 32_000, 48_000] {
            assert!(OutputEncoding::Pcm16.allowed_rates().contains(&rate));
        }
        assert!(!OutputEncoding::Pcm16.allowed_rates().contains(&22_050));
        assert!(!OutputEncoding::Pcm16.allowed_rates().contains(&44_100));
        assert!(!OutputEncoding::Mulaw.allowed_rates().contains(&24_000));
    }
}
