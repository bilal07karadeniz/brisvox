//! Cartesia streaming text-to-speech over one WebSocket per session (feature
//! `provider-cartesia`; protocol facts in `docs/providers/cartesia.md`, fetched 2026-10-08).
//!
//! # Shape
//! [`CartesiaTts`] implements [`TtsProvider`]. `connect` opens (or takes the socket that
//! [`warm`](TtsProvider::warm) parked) a `wss://api.cartesia.ai/tts/websocket` connection and
//! spawns one task that owns it. The task multiplexes the engine's [`ContextId`](crate::ContextId)s onto Cartesia
//! contexts (one server context per utterance, ids `bv<session>-<n>`) and turns server messages
//! into [`TtsEvent`](crate::TtsEvent)s.
//!
//! # Wire behaviour
//! - `TtsInput::Text` becomes a generation request with `continue: true`, then (default) an
//!   empty-transcript manual flush (`flush: true`); `TtsInput::Flush` becomes the closing
//!   request (`continue: false`, empty transcript) and `TtsEvent::Flushed` follows the server's
//!   `done`. Text is sent verbatim: chunks are concatenated by the server, so the caller keeps
//!   the spaces between them.
//! - Every request repeats model, voice, language, output format and the timestamp flags
//!   unchanged (the contexts page requires it).
//! - `max_buffer_delay_ms` defaults to 0: brisvox already sends sentence-sized chunks, and the
//!   server default of 3000 ms would delay speech for nothing.
//! - Audio is base64-decoded into [`Bytes`](bytes::Bytes) carved from large reusable blocks.
//! - Word timestamps become [`TtsEvent::Word`](crate::TtsEvent::Word) with offsets from the context's first sample.
//!   The docs do not say whether a continuation chunk's times restart at zero; both are handled
//!   (see `context::WordClock`, which also copes with `timestamps` arriving after the chunk's
//!   `flush_done`). A bad word time is clamped, never an error.
//!
//! # Why a flush marker after every text chunk
//! With `continue: true` the server sends nothing to say "this chunk is finished", so a silent
//! server cannot be told from one that waits for more text. The empty `flush: true` request makes
//! the server answer `flush_done` after the chunk's output. That lets the session (a) arm a stall
//! deadline only while output is owed, so a slow LLM or a long tool call between sentences never
//! trips it, and (b) know the audio duration at each chunk boundary, which fixes the word time
//! base. [`CartesiaConfig::flush_after_text`] turns it off: then only the first response to a
//! text chunk and the final `done` are timed, and word times are taken as context-cumulative.
//! Whether the extra flush changes prosody is UNVERIFIED (nothing is buffered with a delay of 0).
//!
//! # Cancel (barge-in)
//! The cancel channel is polled before input and before the socket, and also while a socket
//! write or a connect is in progress. A cancel (1) forgets the context, so every later server
//! message for it is dropped locally, (2) removes its not yet delivered events from the
//! outgoing queue, (3) hands [`TtsEvent::Cancelled`](crate::TtsEvent::Cancelled) to the consumer at once (before any I/O),
//! and (4) sends `{"context_id":..,"cancel":true}` behind whatever write is in flight. The docs
//! say a request that is already generating keeps sending until done; brisvox never waits for
//! that. Text and flushes that arrive later for a cancelled id are ignored (the id is
//! remembered for 64 cancels; do not reuse ids).
//!
//! # Backpressure, bounds and overflow
//! Channels are bounded ([`TTS_CHANNEL_CAPACITY`](crate::tts::TTS_CHANNEL_CAPACITY), [`TTS_CANCEL_CAPACITY`](crate::tts::TTS_CANCEL_CAPACITY)). Events wait in an
//! internal queue that is drained to the events channel without blocking the task, so cancel
//! and input stay responsive when the consumer is slow. The socket is not read while that queue
//! holds 64 or more events (TCP backpressure, nothing is dropped); one server message adds at
//! most its words and one audio chunk beyond that. At most 64 engine contexts are kept; a
//! context the engine never flushes or cancels is evicted (oldest first, its server context
//! closed) when a 65th would be added, and if the engine uses its id again it starts afresh.
//!
//! # Idle, expiry, reconnect
//! - The server closes an idle socket after 5 minutes. If it closes while the server owes
//!   nothing, the session does not fail: the next `Text` reconnects lazily (one connect,
//!   bounded by [`CartesiaConfig::io_timeout`], not taking the socket parked for the next
//!   `connect`). A close or error while output is owed is `Error::Disconnected` and ends the
//!   session. No keepalive is sent: none is documented.
//! - A server context expires 1 s after its last audio and input on it is "not supported". An
//!   engine context therefore outlives its server contexts: after
//!   [`CartesiaConfig::context_expiry`] without output (or after an unsolicited `done`, or an
//!   error on a context that had produced output) the next `Text` opens a new server context
//!   under a new id. The audio already delivered becomes the origin of the new context's word
//!   times, so [`TtsEvent::Word`](crate::TtsEvent::Word) offsets stay continuous. The one `Text` that met an expiry
//!   error is sent again to the new context.
//! - Dropping `input` closes every open server context (`continue: false`), waits for the
//!   `done`s, delivers what remains and ends `events`. No `Flushed` is made up for contexts the
//!   engine never flushed.
//! - A `concurrency_limited` refusal of a context that has produced nothing (a context
//!   cancelled by a barge-in can still count against the plan while the server finishes it) is
//!   retried for that context only, with a growing delay
//!   ([`CartesiaConfig::concurrency_retry_delay`], four times), before it ends the session as
//!   `Rejected 429`. Every other server `error` for a live context ends the session.

//! # Timeouts
//! Connect, every send, the close and every wait for owed server output carry the explicit
//! [`CartesiaConfig::io_timeout`] on the injected clock. A stalled wait is
//! `Error::Core(Timeout)` and ends the session.
//!
//! # Connecting
//! The TCP connection is raced over all resolved addresses ([`crate::net::connect_ws`]), because
//! an IPv6 black hole would otherwise eat the whole handshake deadline
//! (`docs/providers/cartesia.md`, live run).
//!
//! # Secrets
//! The API key is a [`Secret`], sent only in the `X-API-Key` handshake header (marked
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
use wire::RequestTemplate;

/// Provider name used in errors.
pub(crate) const PROVIDER: &str = "cartesia";

/// The default endpoint.
pub const DEFAULT_URL: &str = "wss://api.cartesia.ai/tts/websocket";

/// The API version pinned by default (structured errors need 2026-03-01 or later).
pub const DEFAULT_API_VERSION: &str = "2026-08-14";

/// The default model: the alias of the latest stable Sonic 3.6 snapshot.
pub const DEFAULT_MODEL: &str = "sonic-3.6";

/// Largest `max_buffer_delay_ms` the API accepts.
pub const MAX_BUFFER_DELAY_MS: u16 = 5000;

/// A parked or live Cartesia socket.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted (a base64 audio chunk is a few KiB).
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Sessions opened by this process (makes context ids unique across sessions).
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Audio encoding requested from Cartesia (`output_format.encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OutputEncoding {
    /// Signed 16-bit little-endian PCM (`pcm_s16le`): what [`TtsEvent`](crate::TtsEvent) documents.
    #[default]
    Pcm16,
    /// G.711 mu-law (`pcm_mulaw`) for telephony. Requires an 8000 Hz session. The audio events
    /// then carry mu-law bytes, one per sample, not PCM16: the caller must not treat them as
    /// PCM16.
    Mulaw,
    /// G.711 A-law (`pcm_alaw`); same rules as [`OutputEncoding::Mulaw`].
    Alaw,
}

impl OutputEncoding {
    /// The wire name.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Pcm16 => "pcm_s16le",
            Self::Mulaw => "pcm_mulaw",
            Self::Alaw => "pcm_alaw",
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

/// How the Cartesia adapter is configured. Build with [`CartesiaConfig::new`], then set fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CartesiaConfig {
    /// API key, sent in the `X-API-Key` handshake header.
    pub api_key: Secret<String>,
    /// WebSocket endpoint. Plain `ws://` is accepted only for a loopback host (tests, a local
    /// proxy) unless [`allow_insecure_ws`](Self::allow_insecure_ws) is set: the API key is sent
    /// in a header of the handshake.
    pub url: String,
    /// Allow `ws://` to a host that is not loopback. The API key then travels unencrypted:
    /// only for a trusted network path (for example a TLS-terminating sidecar on a private
    /// network).
    pub allow_insecure_ws: bool,
    /// `cartesia_version` query parameter.
    pub api_version: String,
    /// Model id.
    pub model_id: String,
    /// Language code (for example `"tr"`), or `None` to leave it to the server's default.
    pub language: Option<String>,
    /// Requested audio encoding.
    pub encoding: OutputEncoding,
    /// `max_buffer_delay_ms` (0..=5000). 0: start speaking at once, because brisvox already
    /// sends sentence-sized chunks.
    pub max_buffer_delay_ms: u16,
    /// `use_normalized_timestamps`: `false` reports the words as written (what the LLM text
    /// matching of played text wants), `true` the spoken, normalized form.
    pub use_normalized_timestamps: bool,
    /// Send an empty `flush: true` request after every text chunk (see the module docs).
    pub flush_after_text: bool,
    /// Deadline of the connect, of every send and close, and of every wait for output the
    /// server owes.
    pub io_timeout: Duration,
    /// A socket parked by [`warm`](TtsProvider::warm) older than this is discarded (the server
    /// closes idle sockets after 5 minutes).
    pub warm_max_age: Duration,
    /// The server expires a context this long after its last audio (1 s in the docs). A
    /// context that owes nothing and has been quiet that long is not reused: the next text
    /// opens a new server context and the audio and word offsets carry on. Needs
    /// `flush_after_text` (without markers the adapter cannot tell an idle context from one
    /// that is still generating, so it never decides that).
    pub context_expiry: Duration,
    /// Delay before a context refused with `concurrency_limited` is opened again (a context
    /// cancelled by a barge-in may still count against the plan while the server finishes it).
    /// The delay grows linearly with each attempt; after four attempts the refusal ends the
    /// session (`Rejected 429`).
    pub concurrency_retry_delay: Duration,
    /// Time source for every deadline.
    pub clock: SharedClock,
    /// Cancelling this ends every session. A socket parked by `warm` is dropped the next time
    /// the provider looks at it (`warm` or `connect`), or when the provider is dropped: no
    /// task watches it while it waits.
    pub shutdown: CancellationToken,
}

impl CartesiaConfig {
    /// Defaults: Sonic 3.6, 16-bit PCM, no buffering delay, flush markers on, 10 s deadlines.
    pub fn new(api_key: Secret<String>, clock: SharedClock) -> Self {
        Self {
            api_key,
            url: DEFAULT_URL.to_owned(),
            allow_insecure_ws: false,
            api_version: DEFAULT_API_VERSION.to_owned(),
            model_id: DEFAULT_MODEL.to_owned(),
            language: None,
            encoding: OutputEncoding::Pcm16,
            max_buffer_delay_ms: 0,
            use_normalized_timestamps: false,
            flush_after_text: true,
            io_timeout: Duration::from_secs(10),
            warm_max_age: Duration::from_secs(60),
            context_expiry: Duration::from_secs(1),
            concurrency_retry_delay: Duration::from_millis(300),
            clock,
            shutdown: CancellationToken::new(),
        }
    }
}

/// A socket opened ahead of time by `warm()`.
struct Warm {
    socket: Socket,
    opened: Instant,
}

pub(crate) struct Inner {
    pub(crate) config: CartesiaConfig,
    tls: Arc<rustls::ClientConfig>,
    key_header: HeaderValue,
    warm: Mutex<Option<Warm>>,
}

/// The Cartesia streaming TTS provider.
#[derive(Clone)]
pub struct CartesiaTts {
    inner: Arc<Inner>,
}

impl fmt::Debug for CartesiaTts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CartesiaTts")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl CartesiaTts {
    /// Builds the provider. Does not touch the network.
    ///
    /// # Errors
    /// [`Error::Protocol`] if the configuration is invalid (an API key that cannot be an HTTP
    /// header value, a buffer delay above 5000 ms, a zero deadline, a URL that is not `wss://`
    /// or loopback `ws://`) or the TLS config cannot be built. The messages never contain the key.
    pub fn new(config: CartesiaConfig) -> Result<Self> {
        let invalid = |detail: &str| Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid configuration: {detail}"),
        };
        if config.max_buffer_delay_ms > MAX_BUFFER_DELAY_MS {
            return Err(invalid("max_buffer_delay_ms above 5000"));
        }
        if config.io_timeout.is_zero() {
            return Err(invalid("io_timeout is zero"));
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

/// Cartesia's documented idle limit of a TTS WebSocket.
const IDLE_LIMIT: Duration = Duration::from_secs(300);

impl TtsProvider for CartesiaTts {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    /// The server closes a socket after 5 minutes without traffic
    /// (`docs/providers/cartesia.md`, "Keepalive / timeouts"); a session behind
    /// [`ResilientTts`](crate::resilience::ResilientTts) is replaced before that.
    fn idle_limit(&self) -> Option<Duration> {
        Some(IDLE_LIMIT)
    }

    /// Opens a session: takes the socket parked by [`warm`](Self::warm) if it is fresh and
    /// still open, otherwise connects (TCP, TLS and the WebSocket upgrade, all inside
    /// [`CartesiaConfig::io_timeout`]).
    ///
    /// # Errors
    /// `Rejected { 401 | 403 | 429 | .. }` for a refused handshake, `Core(Timeout)`,
    /// `Disconnected` for a network failure, `Protocol` for an invalid [`TtsConfig`] (an empty
    /// voice, or mu-law/A-law at a rate other than 8 kHz) or a TLS failure.
    fn connect(&self, config: TtsConfig) -> impl Future<Output = Result<TtsSession>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let template = inner.template(&config)?;
            let socket = inner.acquire().await?;
            let id = SESSIONS.fetch_add(1, Ordering::Relaxed);
            Ok(session::spawn(inner, socket, &config, template, id))
        }
    }

    /// Opens a socket and parks it for the next `connect` (at most one; no task is spawned, so
    /// nothing runs while it waits). Idempotent: a parked socket that is still fresh and open
    /// is kept. No keepalive is sent, so a socket older than
    /// [`CartesiaConfig::warm_max_age`] is discarded and `connect` opens a new one.
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
            if inner.warm_is_ready() {
                return Ok(());
            }
            let socket = inner.open().await?;
            let opened = inner.config.clock.now();
            *lock(&inner.warm) = Some(Warm { socket, opened });
            Ok(())
        }
    }
}

impl Inner {
    /// The per-session request fields for `config`.
    fn template(&self, config: &TtsConfig) -> Result<RequestTemplate> {
        let encoding = self.config.encoding;
        let invalid = |detail: &str| Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid session configuration: {detail}"),
        };
        if config.voice.is_empty() {
            return Err(invalid("empty voice id"));
        }
        if encoding != OutputEncoding::Pcm16 && config.sample_rate != SampleRate::Hz8000 {
            return Err(invalid("mu-law and A-law output need an 8000 Hz session"));
        }
        Ok(RequestTemplate {
            model_id: self.config.model_id.clone(),
            voice: config.voice.to_string(),
            language: self.config.language.clone(),
            encoding,
            sample_rate: config.sample_rate.hz(),
            max_buffer_delay_ms: self.config.max_buffer_delay_ms,
            use_normalized_timestamps: self.config.use_normalized_timestamps,
        })
    }

    /// A usable socket: the parked one, or a new one.
    pub(crate) async fn acquire(&self) -> Result<Socket> {
        match self.take_warm() {
            Some(socket) => Ok(socket),
            None => self.open().await,
        }
    }

    /// Opens a new socket within the deadline: name lookup, a raced TCP connect (see
    /// [`connect_ws`]), then TLS and the WebSocket handshake.
    pub(crate) async fn open(&self) -> Result<Socket> {
        let request = self.request()?;
        let connector = Connector::Rustls(Arc::clone(&self.tls));
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let clock = &*self.config.clock;
        let connect = connect_ws(request, Some(config), Some(connector), clock);
        match with_timeout(clock, self.config.io_timeout, "cartesia connect", connect).await? {
            Ok((socket, _response)) => Ok(socket),
            Err(e) => Err(map_ws_error(e)),
        }
    }

    /// The handshake request: URL with the version, the key in a sensitive header.
    fn request(&self) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let separator = if self.config.url.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!(
            "{}{separator}cartesia_version={}",
            self.config.url, self.config.api_version
        );
        let mut request = url.into_client_request().map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "invalid websocket url".to_owned(),
        })?;
        request
            .headers_mut()
            .insert("X-API-Key", self.key_header.clone());
        Ok(request)
    }

    /// Takes the parked socket if it is fresh and still open.
    fn take_warm(&self) -> Option<Socket> {
        let warm = lock(&self.warm).take()?;
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

    /// Whether a fresh, open socket is parked (a stale or closed one is dropped).
    fn warm_is_ready(&self) -> bool {
        let mut slot = lock(&self.warm);
        let cancelled = self.config.shutdown.is_cancelled();
        let ready = !cancelled
            && slot.as_mut().is_some_and(|warm| {
                let age = self
                    .config
                    .clock
                    .now()
                    .saturating_duration_since(warm.opened);
                age <= self.config.warm_max_age && still_open(&mut warm.socket)
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
                    "plain ws:// to a non-loopback host would send the API key unencrypted                      (use wss://, or set allow_insecure_ws)",
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

/// The server context id for the engine's context (exposed for tests of the wire format).
pub(crate) fn server_context_id(session: u64, n: u64) -> String {
    format!("bv{session}-{n}")
}
