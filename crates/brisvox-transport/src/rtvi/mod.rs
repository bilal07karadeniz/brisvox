//! RTVI compatibility: the Pipecat client SDKs (`@pipecat-ai/client-js` with
//! `@pipecat-ai/websocket-transport`) talk to a brisvox server. Protocol facts:
//! `docs/providers/rtvi.md` (RTVI 2.1.0, fetched and fact-checked 2026-10-08); how to connect a
//! JS app and the manual check: `docs/RTVI.md`.
//!
//! An [`RtviServer`] listens on a WebSocket path and turns each connection into a call
//! ([`RtviCall`]) for the engine, on the same server infrastructure as the generic
//! [`WebSocketServer`] and the Twilio adapter: the accept loop,
//! the call limit and 503 answers, the pings and idle timeout, the write timeout that drops a
//! client that does not take output, and the shutdown. Terminate TLS (`wss://`) in front of it.
//!
//! # Wire format
//! Every WebSocket message in both directions is ONE BINARY frame holding ONE protobuf
//! `pipecat.Frame` ([`proto`]). The server never sends a text frame (the JS client throws on
//! one). RTVI messages travel as JSON inside `Frame.message` (`MessageFrame.data`); audio travels
//! as `Frame.audio` (`AudioRawFrame`): PCM16 little-endian, mono, no WAV header. The server never
//! sends `InterruptionFrame` (field 5), which the JS client's schema does not know.
//!
//! # Handshake
//! 1. The client connects and sends `client-ready` (with an `id`). Nothing else is expected
//!    before it: audio frames before it are dropped, other RTVI messages are answered with an
//!    `error-response`, a text frame closes the connection (code 1003). [`ServerConfig`]'s
//!    `start_timeout` bounds the wait (the client never gives up on its own).
//! 2. The call is handed to the engine ([`RtviServer::accept_call`]): it starts with
//!    [`TransportEvent::Started`](crate::TransportEvent::Started) whose [`CallInfo`](crate::CallInfo) carries
//!    `transport=rtvi`, the two rates (`input_sample_rate`, `output_sample_rate`, see
//!    [`CallInfo::sample_rate`](crate::CallInfo::sample_rate)), `rtvi_client_version`, `client.library` and the other `about`
//!    strings of the client, and `peer_addr`.
//! 3. The server answers `bot-ready` echoing the request `id`, with `data.version` `2.1.0` (a
//!    1.x client gets its own version back) and `data.about`. A version Pipecat would not accept
//!    (not three integers, or major other than 2 or 1) gets an `error-response` first and the
//!    call continues, as in Pipecat.
//!
//! Audio from the client may start right after `client-ready`, before `bot-ready` arrived.
//!
//! # Audio
//! - Client to server: PCM16 mono. The frame's `sample_rate` is authoritative (the JS client
//!   sends its `recorderSampleRate`, 16 kHz by default); the call's input rate in `Started` is
//!   [`RtviConfig::input_rate`] (default 16 kHz) and a frame at another supported rate is passed
//!   on with its own rate. The engine does not resample, so the stages behind the transport
//!   (STT, VAD) receive a mismatched rate as it is: the first such frame of a call is reported
//!   once as a fault (`TransportEvent::Error`), and the client's `recorderSampleRate` must be
//!   fixed. An unsupported rate, a stereo frame or an odd byte count is a fault; an audio frame
//!   above `max_audio_bytes` closes the call (1009).
//! - Server to client: PCM16 mono at [`RtviConfig::output_rate`] (default 24 kHz): the JS client
//!   plays at its own `playerSampleRate` (24 kHz by default) and IGNORES the frame's
//!   `sample_rate`, so the two must agree. Like the other adapters this one never resamples: bot audio at another rate is dropped and reported once per call as a fault.
//!
//! # Interruption, marks and playback
//! RTVI has no clear and no playback acknowledgement: the WebSocket transport cannot flush what
//! the browser already holds, and nothing tells the server what was played. So:
//! - [`TransportCommand::Clear`](crate::TransportCommand::Clear) drops the audio queued in the
//!   adapter and (with [`RtviConfig::auto_events`]) tells the client `bot-interrupted`. Audio the
//!   client already received keeps playing; the core releases audio at real time, which keeps
//!   that tail short.
//! - Engine marks are acknowledged by an *estimate* (`playout`, private): the audio sent so far ends at
//!   `max(ends, now) + duration` on the injected clock, and a mark is "played" when the estimate
//!   reaches it. The same estimate is reported as [`WireEvent::Lead`](crate::WireEvent::Lead)
//!   (at most every [`RtviConfig::lead_interval`]). Network delay and the client's buffering make
//!   the real playout later than the estimate.
//!
//! # Events to the client
//! The engine drives the UI through the [`RtviSender`] of the call: `send` an [`RtviEvent`]
//! (speaking states, transcriptions, LLM/TTS text, metrics, function calls, errors, server
//! messages), [`respond`](RtviSender::respond) to a client request. With
//! [`RtviConfig::auto_events`] the adapter also sends the events it can derive from the transport
//! itself: `bot-started-speaking` with the first audio of a burst, `bot-stopped-speaking` when the
//! playout estimate runs out (or on a barge-in), and `bot-interrupted` on every clear. The
//! transport has no source for `user-started-speaking` and the others (they come from the VAD and
//! STT stages), so those are the engine's to send (`brisvox_engine::rtvi::RtviBridge` sends
//! them all).
//!
//! # Events from the client
//! `dtmf` becomes [`TransportEvent::Dtmf`](crate::TransportEvent::Dtmf) and `disconnect-bot` a
//! hang-up ([`StopReason::PeerHangup`](crate::StopReason::PeerHangup)); a closed socket is a
//! hang-up or a disconnect as in the other adapters. `client-message`, `send-text` and
//! `llm-function-call-result` carry application data that the core's event enum has no variant
//! for (adding one would break every exhaustive match in the engine), so they are a **gap in the
//! core**: they arrive on the call's [`RtviInbox`] as [`RtviRequest`]s. A `client-message` must
//! be answered (`respond` or `respond_error`), or the client's `sendClientRequest()` never
//! resolves; when nobody reads the inbox, or it is full, the adapter answers with an
//! `error-response` itself. `send-text` and `llm-function-call-result` have no reply in the
//! protocol, so a refused one is only counted ([`RtviStats::requests_unanswered`]). [`Transport::accept`] hands out the bare
//! [`CallLink`] and drops the sender and inbox: a plain voice call without RTVI events, whose
//! client requests are all answered with errors.
//!
//! # Ordering
//! RTVI messages queued through one [`RtviSender`] arrive in order. They are NOT ordered against
//! the audio or against the events derived by the adapter (separate queues, the RTVI messages are
//! served first; see [`RtviSender`]).
//!
//! # Bounds and faults
//! Every queue is bounded (`docs/ARCHITECTURE.md`): input audio to the engine drops the oldest
//! frame, control events wait up to `send_timeout` (then the call is closed as overloaded), the
//! events to the client wait for the app up to `send_timeout` ([`RtviSender::send`] fails with a
//! timeout), the inbox drops the newest request and answers it with an error. A message that
//! does not parse, a bad payload, a text frame or an unusable audio frame is a non-fatal fault
//! (reported to the engine; more than 16 close the call); before `client-ready`, more than 8
//! messages other than `client-ready` close the connection (1008). Fatal: a frame above the size
//! limits (an audio frame is also checked against `max_audio_bytes` on its own), an idle
//! connection, an engine that takes no input, a write that blocks past `write_timeout`.
//!
//! # Request validation
//! [`RtviConfig::token`] and [`RtviConfig::sessions`] (the credential travels in the
//! `?token=` query parameter the JS transport appends, in `Authorization: Bearer`, or in the
//! `brisvox-token.<token>` WebSocket subprotocol; see [`crate::auth`]) and
//! [`RtviConfig::allowed_origins`] (the browser's `Origin` header) are optional checks at the
//! upgrade; the protocol itself has no authentication (`docs/providers/rtvi.md`, Auth). Requests to any other path get HTTP 404.
//!
//! [`ServerConfig`]: crate::websocket::ServerConfig

mod connection;
pub(crate) mod playout;
pub mod proto;
pub mod protocol;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::admission::SharedGate;
use crate::auth::{Audience, Authenticator, SessionSecrets};
use crate::call::CallLink;
use crate::error::{Error, Result};
use crate::transport::Transport;
use crate::websocket::{ServeFn, ServerConfig, WebSocketServer, timeout};

pub use protocol::{
    BotOutput, ClientMessage, ErrorData, FunctionCallInProgress, FunctionCallStarted,
    FunctionCallStopped, MetricEntry, Metrics, PROTOCOL_VERSION, RtviEvent, SpokenStatus, TextData,
    UserTranscription,
};

use connection::RtviShared;
use protocol::Reply;

/// Capacity of the queue of events and replies from the app to the writer.
const OUTBOUND_CAPACITY: usize = 64;

/// Capacity of the queue of client requests to the app ([`RtviInbox`]).
const INBOX_CAPACITY: usize = 32;

/// Settings specific to RTVI (the server limits and timeouts are a [`ServerConfig`]).
#[derive(Debug, Clone)]
pub struct RtviConfig {
    /// The path of the WebSocket URL the client connects to (`connect({ wsUrl })`). The client
    /// accepts any path. Other paths are answered with 404; a trailing `/` is accepted on the
    /// request whether or not it is configured.
    pub path: String,
    /// The rate the call reports for the client's audio. The JS client's `recorderSampleRate`
    /// defaults to 16 kHz. A frame at another rate is passed on with its own rate.
    pub input_rate: SampleRate,
    /// The rate the bot's audio must have: the client's `playerSampleRate`, 24 kHz by default.
    pub output_rate: SampleRate,
    /// If set, the upgrade request must carry this static token: `?token=<this>` (the JS
    /// transport's `token` connection parameter), `Authorization: Bearer <this>` or the
    /// subprotocol `brisvox-token.<this>`; else the answer is HTTP 401 (missing) or 403 (wrong).
    /// Shared and long-lived: for development; production uses [`sessions`](Self::sessions).
    /// Never logged (`Secret` prints as redacted).
    pub token: Option<Secret<String>>,
    /// The secret(s) session tokens (`aud` `brisvox-rtvi`, see [`crate::auth`]) are signed with.
    /// Accepted in the same three places as the static token; a token opens one call, and its
    /// `sid`, `agent` and `meta` are in the call's metadata (`session_id`, `session.*`). With
    /// neither this nor [`token`](Self::token) the upgrade needs no credential.
    pub sessions: Option<SessionSecrets>,
    /// If not empty, the upgrade request must carry one of these in its `Origin` header (compared
    /// ignoring case), or the answer is HTTP 403. Empty allows every origin.
    pub allowed_origins: Vec<String>,
    /// Send the RTVI events the adapter can derive itself: `bot-started-speaking`,
    /// `bot-stopped-speaking` and `bot-interrupted` (see the module docs). Turn it off when the
    /// engine sends them from its own source. This is each call's starting value: the engine can
    /// switch a call's derivation off later ([`RtviSender::set_auto_events`]; the engine's RTVI
    /// bridge does so when it sends the speaking events itself).
    pub auto_events: bool,
    /// Longest interval between [`WireEvent::Lead`](crate::WireEvent::Lead) reports while audio
    /// is sent; zero disables the reports.
    pub lead_interval: Duration,
}

impl Default for RtviConfig {
    fn default() -> Self {
        Self {
            path: "/rtvi".to_owned(),
            input_rate: SampleRate::Hz16000,
            output_rate: SampleRate::Hz24000,
            token: None,
            sessions: None,
            allowed_origins: Vec::new(),
            auto_events: true,
            lead_interval: Duration::from_millis(100),
        }
    }
}

impl RtviConfig {
    /// Checks the configuration; [`RtviServer::bind`] and [`RtviServer::serve`] call it.
    ///
    /// The rules: `path` starts with `/` and has no query string; a token is not empty; an
    /// allowed origin is not empty.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if !self.path.starts_with('/') || self.path.contains(['?', '#']) {
            return bad("rtvi path must start with / and have no query string");
        }
        if self.token.as_ref().is_some_and(|t| t.expose().is_empty()) {
            return bad("rtvi token must not be empty");
        }
        if self.allowed_origins.iter().any(String::is_empty) {
            return bad("rtvi allowed_origins must not contain an empty origin");
        }
        Ok(())
    }
}

/// A request from the client that the core's event enum cannot carry (see the module docs).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RtviRequest {
    /// `client-message` `{ t, d }`: answer with [`RtviSender::respond`] or
    /// [`RtviSender::respond_error`] using `id`.
    ClientMessage {
        /// The request id to echo.
        id: String,
        /// The message name.
        t: String,
        /// The payload, if any.
        d: Option<Value>,
    },
    /// `send-text`: text typed by the user, no reply is defined.
    SendText {
        /// The text.
        content: String,
        /// Interrupt the bot and answer now (default `true`).
        run_immediately: bool,
        /// `false` asks for a text-only answer (default `true`).
        audio_response: bool,
    },
    /// `llm-function-call-result`: the client's answer to a function call it handles.
    FunctionCallResult {
        /// The function name.
        function_name: String,
        /// Identifies the call.
        tool_call_id: String,
        /// The arguments the call was made with.
        arguments: Value,
        /// The result.
        result: Value,
    },
}

/// The engine's end of a call's RTVI messages: send events and replies to the client. Cheap to
/// clone.
///
/// # Ordering
/// Messages from one sender (and its clones) reach the client in the order they were queued.
/// There is NO ordering against the call's audio and the events the adapter derives itself
/// (`bot-started-speaking`, `bot-interrupted`, `bot-stopped-speaking`): they travel on separate
/// queues and the writer serves the RTVI messages first. An event queued right after audio can
/// reach the client before that audio, and a `bot-output` sent when the TTS text is produced can
/// precede the auto-derived `bot-started-speaking` (Pipecat's observer sends the latter first).
/// A client that needs a strict order should turn [`RtviConfig::auto_events`] off and send the
/// speaking events itself, at the moments it chooses.
#[derive(Clone)]
pub struct RtviSender {
    tx: mpsc::Sender<String>,
    clock: SharedClock,
    send_timeout: Duration,
    max_bytes: usize,
    /// The call's [`RtviConfig::auto_events`] switch, shared with the connection's writer.
    auto_events: Arc<AtomicBool>,
}

impl std::fmt::Debug for RtviSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtviSender")
            .field("closed", &self.tx.is_closed())
            .finish_non_exhaustive()
    }
}

impl RtviSender {
    async fn queue(&self, json: String) -> Result<()> {
        if json.len() > self.max_bytes {
            return Err(Error::Protocol(format!(
                "rtvi message of {} bytes exceeds the {} byte limit",
                json.len(),
                self.max_bytes
            )));
        }
        match timeout(&*self.clock, self.send_timeout, self.tx.send(json)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Error::Core(brisvox_core::Error::ChannelClosed(
                "rtvi events",
            ))),
            Err(_) => Err(Error::Core(brisvox_core::Error::Timeout {
                what: "rtvi event queue",
                after: self.send_timeout,
            })),
        }
    }

    /// Sends an RTVI event to the client. Waits for queue space (the queue holds 64 messages) for
    /// at most the server's `send_timeout`.
    ///
    /// # Errors
    /// [`Error::Protocol`] if the message is larger than the server's `max_control_bytes`,
    /// [`Error::Core`] (timeout) if the queue stayed full, [`Error::Core`] (channel closed) if the
    /// call is over.
    pub async fn send(&self, event: RtviEvent) -> Result<()> {
        self.queue(protocol::render_event(&event)).await
    }

    /// Answers a `client-message` with `server-response` `{ t, d }`, echoing the request `id`.
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub async fn respond(&self, id: &str, t: &str, d: Option<Value>) -> Result<()> {
        let reply = Reply::ServerResponse { t: t.to_owned(), d };
        self.queue(protocol::render_reply(&reply, id)).await
    }

    /// Answers a request with `error-response`, echoing the request `id`.
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub async fn respond_error(&self, id: &str, error: &str) -> Result<()> {
        let reply = Reply::ErrorResponse {
            error: error.to_owned(),
        };
        self.queue(protocol::render_reply(&reply, id)).await
    }

    /// Sends a `user-transcription` stamped with the injected clock's wall time (ISO-8601 UTC with
    /// milliseconds, `2026-10-08T10:00:00.000+00:00`; the protocol leaves the format open).
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub async fn user_transcription(&self, text: &str, is_final: bool) -> Result<()> {
        self.send(RtviEvent::UserTranscription(UserTranscription {
            text: text.to_owned(),
            user_id: String::new(),
            timestamp: format_timestamp(self.clock.wall()),
            is_final,
        }))
        .await
    }

    /// `true` once the call is over (the connection dropped its end).
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Whether the adapter derives `bot-started-speaking`, `bot-stopped-speaking` and
    /// `bot-interrupted` for this call (it starts as [`RtviConfig::auto_events`]).
    pub fn auto_events(&self) -> bool {
        self.auto_events.load(Ordering::Relaxed)
    }

    /// Switches this call's derived speaking events on or off, from the next one on. A sender
    /// that sends those events itself turns them off so the client does not get them twice.
    pub fn set_auto_events(&self, on: bool) {
        self.auto_events.store(on, Ordering::Relaxed);
    }
}

/// The engine's end of a call's client requests ([`RtviRequest`]).
#[derive(Debug)]
pub struct RtviInbox {
    rx: mpsc::Receiver<RtviRequest>,
}

impl RtviInbox {
    /// The next request from the client; `None` once the call is over and the queue is drained.
    /// Cancel-safe.
    pub async fn recv(&mut self) -> Option<RtviRequest> {
        self.rx.recv().await
    }
}

/// One RTVI call: the link for the transport session plus the RTVI side channels.
pub struct RtviCall {
    /// The core's end of the call: hand it to
    /// [`TransportSession::open`](crate::TransportSession::open).
    pub link: CallLink,
    /// Sends RTVI events and replies to the client.
    pub sender: RtviSender,
    /// Receives the client's `client-message`, `send-text` and `llm-function-call-result`.
    pub inbox: RtviInbox,
}

impl std::fmt::Debug for RtviCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtviCall")
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

/// Counters shared by every call of an [`RtviServer`]. Relaxed atomics: statistics, not
/// synchronization.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) calls: AtomicU64,
    pub(crate) audio_in: AtomicU64,
    pub(crate) audio_out: AtomicU64,
    pub(crate) audio_out_bytes: AtomicU64,
    pub(crate) messages_in: AtomicU64,
    pub(crate) messages_out: AtomicU64,
    pub(crate) requests_unanswered: AtomicU64,
    pub(crate) bot_audio_dropped: AtomicU64,
    pub(crate) faults: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> RtviStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        RtviStats {
            calls: get(&self.calls),
            audio_in: get(&self.audio_in),
            audio_out: get(&self.audio_out),
            audio_out_bytes: get(&self.audio_out_bytes),
            messages_in: get(&self.messages_in),
            messages_out: get(&self.messages_out),
            requests_unanswered: get(&self.requests_unanswered),
            bot_audio_dropped: get(&self.bot_audio_dropped),
            faults: get(&self.faults),
        }
    }
}

/// A snapshot of the adapter's counters, summed over every call since the server started
/// ([`RtviServer::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RtviStats {
    /// Calls that completed the handshake and were handed to the engine.
    pub calls: u64,
    /// Client audio frames passed to the engine.
    pub audio_in: u64,
    /// Audio frames sent to clients.
    pub audio_out: u64,
    /// PCM bytes sent to clients.
    pub audio_out_bytes: u64,
    /// RTVI messages accepted from clients (valid envelope).
    pub messages_in: u64,
    /// RTVI messages sent to clients, handshake included.
    pub messages_out: u64,
    /// Client requests that could not be handed to the app because the inbox was full or not
    /// read. A `client-message` among them was answered with an `error-response`; `send-text` and
    /// `llm-function-call-result` have no reply in the protocol and were dropped.
    pub requests_unanswered: u64,
    /// Bot audio frames dropped for having the wrong rate.
    pub bot_audio_dropped: u64,
    /// Non-fatal protocol faults.
    pub faults: u64,
}

/// A server that yields one call per RTVI client connection.
///
/// Dropping it cancels its tasks without waiting; [`shutdown`](Self::shutdown) waits for them.
pub struct RtviServer {
    inner: WebSocketServer,
    calls: mpsc::Receiver<RtviCall>,
    counters: Arc<Counters>,
}

impl RtviServer {
    /// Binds `server.bind_addr` and starts accepting RTVI connections. The server is a child of
    /// `parent`: cancelling it stops the server.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `rtvi` is invalid, [`Error::Io`] if the address cannot be
    /// bound.
    pub async fn bind(
        server: ServerConfig,
        rtvi: RtviConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        server.validate()?;
        rtvi.validate()?;
        let listener = TcpListener::bind(server.bind_addr).await?;
        Self::serve(listener, server, rtvi, clock, parent)
    }

    /// Starts accepting connections on an already bound `listener` (`server.bind_addr` is
    /// ignored). Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `rtvi` is invalid, [`Error::Io`] if the listener's
    /// address cannot be read.
    pub fn serve(
        listener: TcpListener,
        server: ServerConfig,
        rtvi: RtviConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::serve_gated(listener, server, rtvi, clock, parent, None)
    }

    /// [`serve`](Self::serve) behind the host's admission `gate` (none: like `serve`): a
    /// connection within `max_calls` is admitted by the gate before the upgrade and refused with
    /// HTTP 503 otherwise; the gate's ticket is in the call's link
    /// ([`CallLink::take_ticket`]). See [`crate::admission`].
    ///
    /// # Errors
    /// As [`serve`](Self::serve).
    pub fn serve_gated(
        listener: TcpListener,
        server: ServerConfig,
        rtvi: RtviConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        server.validate()?;
        rtvi.validate()?;
        let counters = Arc::new(Counters::default());
        let (calls_tx, calls_rx) = mpsc::channel(server.accept_queue);
        let auth = Authenticator::from_config(
            Audience::Rtvi,
            rtvi.token.as_ref(),
            rtvi.sessions.as_ref(),
            &clock,
        );
        let shared = Arc::new(RtviShared {
            config: rtvi,
            auth,
            counters: Arc::clone(&counters),
            calls: calls_tx,
        });
        let handler: ServeFn = Arc::new(move |stream, peer, admitted, server_shared| {
            Box::pin(connection::serve(
                stream,
                peer,
                admitted,
                server_shared,
                Arc::clone(&shared),
            ))
        });
        let inner = WebSocketServer::serve_with(listener, server, clock, parent, handler, gate)?;
        Ok(Self {
            inner,
            calls: calls_rx,
            counters,
        })
    }

    /// The address the server listens on.
    pub const fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    /// Connections being served right now (handshake and closing included).
    pub fn active_calls(&self) -> usize {
        self.inner.active_calls()
    }

    /// Counters summed over every call since the server started.
    pub fn stats(&self) -> RtviStats {
        self.counters.snapshot()
    }

    /// Waits for the next call that sent `client-ready`, with its RTVI side channels.
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// and no queued call is left.
    pub async fn accept_call(&mut self, cancel: &CancellationToken) -> Result<Option<RtviCall>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(brisvox_core::Error::Cancelled.into()),
            call = self.calls.recv() => Ok(call),
        }
    }

    /// Stops accepting, closes every call (close code 1001) and waits for all tasks to end.
    ///
    /// # Errors
    /// [`Error::Core`] (timeout) if tasks were still running after `timeout`; they stay
    /// cancelled and finish on their own bounded waits.
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.inner.shutdown(timeout).await
    }
}

impl Transport for RtviServer {
    /// Waits for the next call and hands out its [`CallLink`] alone (see the module docs: the
    /// RTVI sender and inbox are dropped; use [`RtviServer::accept_call`] to keep them).
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// and no queued call is left.
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        Ok(self.accept_call(cancel).await?.map(|call| call.link))
    }
}

/// Formats `time` as `YYYY-MM-DDTHH:MM:SS.mmm+00:00` (UTC).
pub fn format_timestamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let millis = since.subsec_millis();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}+00:00",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_configuration_is_valid() {
        RtviConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_configurations_are_refused_with_a_reason() {
        let cases: Vec<(RtviConfig, &str)> = vec![
            (
                RtviConfig {
                    path: "rtvi".to_owned(),
                    ..RtviConfig::default()
                },
                "path",
            ),
            (
                RtviConfig {
                    path: "/a?token=1".to_owned(),
                    ..RtviConfig::default()
                },
                "path",
            ),
            (
                RtviConfig {
                    token: Some(Secret::new(String::new())),
                    ..RtviConfig::default()
                },
                "token",
            ),
            (
                RtviConfig {
                    allowed_origins: vec![String::new()],
                    ..RtviConfig::default()
                },
                "origin",
            ),
        ];
        for (config, needle) in cases {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let config = RtviConfig {
            token: Some(Secret::new("super-secret-token".to_owned())),
            ..RtviConfig::default()
        };
        assert!(!format!("{config:?}").contains("super-secret-token"));
    }

    #[test]
    fn timestamps_are_iso_8601_utc_with_milliseconds() {
        let at = |secs: u64, millis: u32| UNIX_EPOCH + Duration::new(secs, millis * 1_000_000);
        assert_eq!(format_timestamp(at(0, 0)), "1970-01-01T00:00:00.000+00:00");
        // 2026-10-08T10:00:00.123Z
        assert_eq!(
            format_timestamp(at(1_791_453_600, 123)),
            "2026-10-08T10:00:00.123+00:00"
        );
        // A leap day.
        assert_eq!(
            format_timestamp(at(951_782_400, 5)),
            "2000-02-29T00:00:00.005+00:00"
        );
    }
}
