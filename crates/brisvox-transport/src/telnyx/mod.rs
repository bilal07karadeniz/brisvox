//! The Telnyx media streaming transport: phone calls through a bidirectional stream.
//! Protocol facts: `docs/providers/telnyx.md` (fetched 2026-10-08).
//!
//! Telnyx is the WebSocket *client*: it connects to a `wss://` URL given when the stream is
//! started, and a [`TelnyxServer`] on that URL turns each connection into a call (a
//! [`CallLink`]) for the engine, like the generic
//! [`WebSocketServer`] and the
//! `TwilioServer`. It runs on the same server infrastructure: the
//! accept loop, the call limit and 503 answers, the queue of calls waiting for the engine, the
//! pings and idle timeout, the write timeout that drops a client that does not take output, and
//! the shutdown. It is a separate listener with its own [`TelnyxConfig`]; terminate TLS
//! (`wss://`) in front of it.
//!
//! # Setting up a call
//! The stream must be started in `rtp` bidirectional mode (Telnyx's default is `mp3`, which takes
//! files, not streamed audio). With Call Control, after answering the call (this is a derived
//! example, not a Telnyx sample; the key names are from the `streaming_start` reference):
//!
//! ```text
//! POST https://api.telnyx.com/v2/calls/{call_control_id}/actions/streaming_start
//! Authorization: Bearer <API key>
//! {
//!   "stream_url": "wss://agent.example.com/telnyx/stream",
//!   "stream_track": "inbound_track",
//!   "stream_codec": "PCMU",
//!   "stream_bidirectional_mode": "rtp",
//!   "stream_bidirectional_codec": "PCMU",
//!   "stream_auth_token": "<random secret, TelnyxConfig::auth_token>",
//!   "custom_parameters": [ { "name": "agent", "value": "support-tr" } ]
//! }
//! ```
//!
//! With TeXML (`stream_auth_token` has no TeXML attribute, so authenticate with a secret in the
//! URL path, [`TelnyxConfig::path`], and set `enableReconnect="false"`; `SECRET` stands for a long
//! random string):
//!
//! ```xml
//! <Response>
//!   <Connect>
//!     <Stream url="wss://agent.example.com/telnyx/stream/SECRET" bidirectionalMode="rtp"
//!             codec="PCMU" bidirectionalCodec="PCMU" enableReconnect="false">
//!       <Parameter name="agent" value="support-tr" />
//!     </Stream>
//!   </Connect>
//! </Response>
//! ```
//!
//! `stream_codec` (TeXML `codec`) sets the codec of the audio Telnyx streams to us; left out it is
//! `default`, the call leg's own codec, and a leg negotiated as `G722` or `OPUS` would then be
//! refused at `start` (see "Audio"). Always set it to `PCMU` or `PCMA`; Telnyx transcodes, "which
//! may cause a degradation in quality". `stream_bidirectional_codec` must equal
//! [`TelnyxConfig::codec`], and `stream_track` must be `inbound_track` (frames of another track
//! are ignored, and `both_tracks` is not supported: whether chunk numbers are shared between the
//! tracks is not documented).
//!
//! # Audio
//! Both directions are G.711, 8 kHz, mono, base64 in JSON `media` messages: `PCMU` (mu-law) or
//! `PCMA` (A-law). The inbound codec is read from `start.media_format` and may differ from the
//! outbound [`TelnyxConfig::codec`]. Caller audio is decoded to PCM16 8 kHz with
//! [`brisvox_audio::Codec`] and handed to the engine at
//! [`SampleRate::Hz8000`](brisvox_core::frame::SampleRate::Hz8000); bot audio must arrive at
//! 8 kHz and is encoded back. The core is PCM16 throughout, so G.711 is never passed through. Like
//! the generic adapter this one never resamples: bot audio at another rate is dropped and
//! reported once per call as a fault. A `start` with another encoding (`L16`, `G722`, `OPUS`,
//! `AMR-WB`), rate or channel count is refused: L16's byte order is undocumented and the others
//! need codecs that are not in the tree. The call's metadata carries `sample_rate=8000`,
//! `encoding=mulaw|alaw`, `outbound_encoding`, `transport=telnyx`, `stream_id`,
//! `call_control_id`, `call_session_id`, `user_id`, `from`, `to`, `tags`, `client_state`,
//! `telnyx_version`, `peer_addr` and every custom parameter as `custom.<name>`.
//!
//! Bot audio goes out in messages of [`TelnyxConfig::chunk_bytes`] G.711 bytes (default 160, 20
//! ms, the engine's frame size). Telnyx accepts 20 ms to 30 s per message, so a tail shorter than
//! 20 ms is not sent on its own: it waits for the next audio, and is padded with silence to 20 ms
//! when an engine mark follows it (the end of an utterance).
//!
//! # Inbound order
//! Telnyx does not guarantee the order of its frames. Caller audio is released in `media.chunk`
//! order by a small bounded buffer ([`TelnyxConfig::reorder_window`] frames,
//! [`TelnyxConfig::reorder_timeout`] for a lost chunk, counted from the arrival of the first frame
//! behind the gap); late frames and duplicates are dropped. A chunk that arrives with an unusable
//! payload still counts as arrived. Chunk numbers far from the one due (a corrupt number, a
//! restarted counter) cannot be reordered: they are dropped, and when several come in a row the
//! buffer follows the new numbering and reports a fault, so the caller is never silently deaf.
//! `dtmf` events wait up to `reorder_timeout` and are delivered in `sequence_number` order (the
//! documented per-connection counter; `occurred_at` is RFC 3339 with a variable number of fraction
//! digits and no better a key); with `reorder_window` 0 they are delivered as they arrive.
//!
//! # Interruption and playback tracking
//! Identical to the Twilio adapter, with Telnyx's own wording on top:
//! - [`TransportCommand::Clear`](crate::TransportCommand::Clear) becomes `{"event":"clear"}`,
//!   which "immediately stops the media playing and clears the media queue". Telnyx answers by
//!   echoing every queued mark with the same JSON as a played one, so the adapter keeps the marks
//!   it has in flight ([`MarkBook`]) and forgets them all when it sends the `clear`; the echoes
//!   that follow find nothing and are dropped. The core reports the same marks as
//!   [`TransportEvent::MarkCleared`](crate::TransportEvent::MarkCleared).
//! - Engine marks go out as `mark` messages named `e<n>`; the echo becomes
//!   [`TransportEvent::MarkPlayed`](crate::TransportEvent::MarkPlayed). Telnyx documents that a
//!   mark submitted while nothing is queued is echoed at once; the engine's ids never go on the
//!   wire. Mark echoes are matched by name, not by arrival order (no ordering is guaranteed).
//! - Every [`TelnyxConfig::mark_interval`] of audio a progress mark `p<n>` also goes out; each echo
//!   is reported as [`WireEvent::Lead`](crate::WireEvent::Lead), and feeds [`TelnyxStats`]
//!   (`max_echo_latency`, `max_unplayed`).
//!
//! # Request validation
//! Telnyx signs no WebSocket upgrade. The only mechanism is the optional `stream_auth_token`
//! (settable with `streaming_start` and `dial`, not with `answer` or TeXML): Telnyx sends it as the
//! upgrade header `x-telnyx-streaming-auth-token` and in `connected.x-telnyx-streaming-auth-token`.
//! With [`TelnyxConfig::auth_token`] set, a header that differs is refused with HTTP 401 at the
//! upgrade; a missing header is accepted until `start`, which must then follow a `connected`
//! carrying the right token (otherwise the connection is closed with 1008 and the engine never
//! sees the call). Comparison is constant-time and the token never appears in logs or `Debug`.
//! Without a token anything that reaches the path is accepted: only do that behind another check,
//! a secret in the URL path. Requests to any other path get HTTP 404. `start.custom_parameters`
//! arrive after the handshake and are not signed.
//!
//! The path is compared in constant time as well (it may be the only secret, with TeXML).
//!
//! One `call_control_id` streams once: a second connection for a call that is streaming or has
//! streamed (Telnyx's TeXML reconnect) is closed with 1008 and never reaches the engine. A call
//! that was refused before the engine took it (1013, or a refused media format) is not remembered
//! as streamed, so a retry is accepted.
//!
//! # Hang-up
//! Closing the socket is not documented as a hang-up on Telnyx (and TeXML may reconnect it), and
//! there is no server-to-Telnyx `stop`. The host supplies a [`HangupHook`] that calls the REST
//! `POST /v2/calls/{call_control_id}/actions/hangup` (it holds the API key; brisvox does not).
//! The adapter calls it, with [`TelnyxConfig::hangup_timeout`], whenever it ends the connection
//! itself (the engine hung up, a fatal fault, an idle connection, the server shuts down, the socket
//! vanished or Telnyx sent a bare close frame, which is not documented as ending the call) and
//! then closes the socket (code 1000). It also calls it for a call it refuses after `start` (an
//! unsupported media format, an engine that takes no calls), because the caller would otherwise
//! sit on a silent open call; not for a refusal on authentication or a duplicate `call_control_id`.
//! It does not call it after Telnyx's own `stop` ("sent when the call ends"). The hook must
//! treat "already ended" as success.
//!
//! # Stopping the stream on purpose
//! The host sometimes ends the stream itself while the call goes on: Telnyx refuses a `transfer`
//! or a conference command with error `90045` while media streaming is active, and the fix is
//! `streaming_stop` followed by a retry of the command. Telnyx then closes the socket (whether it
//! sends `stop` first is UNVERIFIED), which would look like a caller hang-up or a vanished peer:
//! the adapter would tell the engine the caller hung up (ending the session and the tool call in
//! flight) or even ask the host's hook to hang the call up. So the host announces the stop first,
//! with [`TelnyxServer::stream_stopper`] and [`StreamStopper::expect_stop`] (additive API, the
//! `call_control_id` names the call), then issues `streaming_stop`. For [`STOP_EXPECTED_WITHIN`]
//! after the announcement, a `stop` frame, a close frame or a vanished socket of that call is
//! taken as the host's own doing: the hook is **not** called, the engine gets **no** hang-up event
//! (its session stays up; audio it sends is discarded), and the call's key stays claimed so a
//! TeXML reconnect is still refused. The session is released when the engine ends it, the server
//! shuts down or [`STOPPED_LINGER`] has passed (then the engine sees the peer disconnect). An
//! announcement that is not followed by a stop expires by itself, so a real hang-up after a failed
//! `streaming_stop` is still reported.
//!
//! **Rule for the engine:** to say goodbye, send the audio and an utterance-end
//! [`TransportCommand::Mark`](crate::TransportCommand::Mark), wait for its
//! [`TransportEvent::MarkPlayed`](crate::TransportEvent::MarkPlayed) (with a timeout of its own),
//! and only then send [`TransportCommand::Stop`](crate::TransportCommand::Stop).
//!
//! # Faults
//! Telnyx is the only expected peer, so protocol faults are non-fatal: a message that does not
//! parse, a bad payload or an `error` frame is reported to the engine as a fault, and an unknown
//! mark or another stream's `stream_id` is dropped (and counted). More than 16 faults in the input
//! on a call close it; Telnyx's own `error` frames are reported but do not count toward that
//! limit. Fatal: a `start` with an unsupported media format, a frame above the size limits, an
//! idle connection, an engine that takes no input.

mod connection;
pub mod protocol;
mod registry;
mod reorder;
mod stats;

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::{Secret, SharedClock};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::admission::SharedGate;
use crate::call::CallLink;
use crate::error::{Error, Result};
use crate::transport::Transport;
use crate::websocket::{ServeFn, ServerConfig, WebSocketServer};

pub use crate::telephony::{Echo, MarkBook};
pub use protocol::TelnyxCodec;
pub use stats::TelnyxStats;

use connection::TelnyxShared;
use stats::Counters;

/// How long after [`StreamStopper::expect_stop`] the end of the stream counts as the host's own
/// doing (the REST call, Telnyx's processing and the close of the socket all fit in it).
pub const STOP_EXPECTED_WITHIN: Duration = Duration::from_secs(30);

/// How long the engine's session of a call whose stream the host stopped on purpose is kept alive
/// without a socket, waiting for the engine to end it (a retried transfer fits in it).
pub const STOPPED_LINGER: Duration = Duration::from_secs(120);

/// The host's way to announce that it is about to stop a call's stream (`streaming_stop`) while
/// the call goes on; see the module docs, "Stopping the stream on purpose". Cheap to clone; get it
/// from [`TelnyxServer::stream_stopper`].
#[derive(Clone)]
pub struct StreamStopper {
    registry: Arc<registry::Registry>,
    clock: SharedClock,
}

impl StreamStopper {
    /// Announces that the host stops the stream of the call `call_control_id` (a call that
    /// streams without one is named by its `stream_id`). Call it **before** `streaming_stop`.
    /// Returns whether such a call is streaming on this server; `false` means nothing was
    /// recorded (the call is unknown or over).
    pub fn expect_stop(&self, call_control_id: &str) -> bool {
        let until = deadline_after(self.clock.now(), STOP_EXPECTED_WITHIN);
        self.registry.expect_stop(call_control_id, until)
    }
}

impl fmt::Debug for StreamStopper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamStopper(..)")
    }
}

/// What the host's [`HangupHook`] is asked to hang up.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HangupRequest {
    /// The call to hang up: `POST /v2/calls/{call_control_id}/actions/hangup`.
    pub call_control_id: String,
    /// `start.call_session_id`, if Telnyx sent it.
    pub call_session_id: Option<String>,
    /// The stream the call was on.
    pub stream_id: String,
    /// `start.client_state`, if any (the hang-up request may echo it back in its `client_state`).
    pub client_state: Option<String>,
}

/// The future a [`HangupHook`] returns: `Err` carries a short reason for the logs and counters.
pub type HangupFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// The host's way to hang up a Telnyx call (REST `hangup`; "call already ended", Telnyx error
/// `90018`, is success as far as the adapter is concerned). Called at most once per call, bounded
/// by [`TelnyxConfig::hangup_timeout`]; the hook should carry its own timeout on the HTTP request
/// as well, since the adapter drops it when the deadline passes.
#[derive(Clone)]
pub struct HangupHook(Arc<dyn Fn(HangupRequest) -> HangupFuture + Send + Sync>);

impl HangupHook {
    /// Wraps an async function.
    pub fn new<F, Fut>(hook: F) -> Self
    where
        F: Fn(HangupRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        Self(Arc::new(move |request| Box::pin(hook(request))))
    }

    pub(crate) fn call(&self, request: HangupRequest) -> HangupFuture {
        (self.0)(request)
    }
}

impl fmt::Debug for HangupHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HangupHook(..)")
    }
}

/// Settings specific to Telnyx (the server limits and timeouts are a [`ServerConfig`]).
#[derive(Debug, Clone)]
pub struct TelnyxConfig {
    /// The path of the stream URL given to Telnyx. Other paths are answered with 404. A trailing
    /// `/` is accepted on the request whether or not it is configured.
    pub path: String,
    /// The `stream_auth_token` given to Telnyx. `Some` makes the check described in the module
    /// docs mandatory. At most 4000 bytes, the limit Telnyx enforces. Never logged (`Secret`
    /// prints as redacted).
    pub auth_token: Option<Secret<String>>,
    /// The codec of bot audio: must equal `stream_bidirectional_codec` of the stream. The inbound
    /// codec is whatever `start.media_format` says (PCMU or PCMA).
    pub codec: TelnyxCodec,
    /// G.711 bytes per outgoing `media` message (160 is 20 ms, Telnyx's minimum). 160 to 8000.
    pub chunk_bytes: usize,
    /// Audio between progress marks; zero disables them.
    pub mark_interval: Duration,
    /// Inbound frames the reorder buffer holds while waiting for a missing chunk; zero turns
    /// reordering off (frames are delivered in arrival order). At most 64.
    pub reorder_window: usize,
    /// How long a held inbound frame waits for the missing chunk before it is released anyway
    /// (the chunk counts as lost). Longer than zero when the window is not.
    pub reorder_timeout: Duration,
    /// The host's hang-up (REST). `None`: the adapter only closes the socket, which Telnyx does
    /// not document as a hang-up.
    pub hangup: Option<HangupHook>,
    /// Longest the [`hangup`](Self::hangup) hook may take.
    pub hangup_timeout: Duration,
}

impl Default for TelnyxConfig {
    fn default() -> Self {
        Self {
            path: "/telnyx/stream".to_owned(),
            auth_token: None,
            codec: TelnyxCodec::Pcmu,
            chunk_bytes: 160,
            mark_interval: Duration::from_millis(200),
            reorder_window: 8,
            reorder_timeout: Duration::from_millis(60),
            hangup: None,
            hangup_timeout: Duration::from_secs(5),
        }
    }
}

/// Smallest `chunk_bytes`: 20 ms of G.711, the least Telnyx accepts per message.
pub const MIN_CHUNK_BYTES: usize = 160;

/// Largest `chunk_bytes`: one second of audio.
const MAX_CHUNK_BYTES: usize = 8000;

/// Largest `reorder_window`.
pub const MAX_REORDER_WINDOW: usize = 64;

/// Longest `stream_auth_token` Telnyx accepts.
const MAX_TOKEN_BYTES: usize = 4000;

impl TelnyxConfig {
    /// Checks the configuration; [`TelnyxServer::bind`] and [`TelnyxServer::serve`] call it.
    ///
    /// The rules: `path` starts with `/` and has no query string; `chunk_bytes` is 160 to 8000;
    /// `reorder_window` is at most 64 and comes with a `reorder_timeout` above zero;
    /// `hangup_timeout` is above zero; an auth token is not empty and at most 4000 bytes.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if !self.path.starts_with('/') || self.path.contains(['?', '#']) {
            return bad("telnyx path must start with / and have no query string");
        }
        if self.chunk_bytes < MIN_CHUNK_BYTES || self.chunk_bytes > MAX_CHUNK_BYTES {
            return bad("telnyx chunk_bytes must be between 160 and 8000");
        }
        if self.reorder_window > MAX_REORDER_WINDOW {
            return bad("telnyx reorder_window must be at most 64");
        }
        if self.reorder_window > 0 && self.reorder_timeout.is_zero() {
            return bad("telnyx reorder_timeout must be longer than zero");
        }
        if self.hangup_timeout.is_zero() {
            return bad("telnyx hangup_timeout must be longer than zero");
        }
        if let Some(token) = &self.auth_token {
            if token.expose().is_empty() {
                return bad("telnyx auth_token must not be empty");
            }
            if token.expose().len() > MAX_TOKEN_BYTES {
                return bad("telnyx auth_token must be at most 4000 bytes");
            }
        }
        Ok(())
    }
}

/// A server that yields one call per Telnyx media stream.
///
/// Dropping it cancels its tasks without waiting; [`shutdown`](Self::shutdown) waits for them.
pub struct TelnyxServer {
    inner: WebSocketServer,
    counters: Arc<Counters>,
    stopper: StreamStopper,
}

impl TelnyxServer {
    /// Binds `server.bind_addr` and starts accepting Telnyx connections. The server is a child of
    /// `parent`: cancelling it stops the server.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `telnyx` is invalid, [`Error::Io`] if the address cannot
    /// be bound.
    pub async fn bind(
        server: ServerConfig,
        telnyx: TelnyxConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        server.validate()?;
        telnyx.validate()?;
        let listener = TcpListener::bind(server.bind_addr).await?;
        Self::serve(listener, server, telnyx, clock, parent)
    }

    /// Starts accepting connections on an already bound `listener` (`server.bind_addr` is
    /// ignored). Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `telnyx` is invalid, [`Error::Io`] if the listener's
    /// address cannot be read.
    pub fn serve(
        listener: TcpListener,
        server: ServerConfig,
        telnyx: TelnyxConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::serve_gated(listener, server, telnyx, clock, parent, None)
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
        telnyx: TelnyxConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        server.validate()?;
        telnyx.validate()?;
        let counters = Arc::new(Counters::default());
        let stopper = StreamStopper {
            registry: Arc::new(registry::Registry::default()),
            clock: Arc::clone(&clock),
        };
        let shared = Arc::new(TelnyxShared {
            config: telnyx,
            counters: Arc::clone(&counters),
            registry: Arc::clone(&stopper.registry),
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
            counters,
            stopper,
        })
    }

    /// The address the server listens on.
    pub const fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    /// The handle the host announces a deliberate `streaming_stop` with (see the module docs).
    pub fn stream_stopper(&self) -> StreamStopper {
        self.stopper.clone()
    }

    /// Connections being served right now (handshake and closing included).
    pub fn active_calls(&self) -> usize {
        self.inner.active_calls()
    }

    /// Counters summed over every call since the server started.
    pub fn stats(&self) -> TelnyxStats {
        self.counters.snapshot()
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

impl Transport for TelnyxServer {
    /// Waits for the next call that completed the upgrade and sent a valid `start`.
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// and no queued call is left.
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        self.inner.accept(cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_configuration_is_valid() {
        TelnyxConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_configurations_are_refused_with_a_reason() {
        let base = TelnyxConfig::default;
        let cases: Vec<(TelnyxConfig, &str)> = vec![
            (
                TelnyxConfig {
                    path: "stream".to_owned(),
                    ..base()
                },
                "path",
            ),
            (
                TelnyxConfig {
                    path: "/a?b=1".to_owned(),
                    ..base()
                },
                "path",
            ),
            (
                TelnyxConfig {
                    chunk_bytes: 159,
                    ..base()
                },
                "chunk_bytes",
            ),
            (
                TelnyxConfig {
                    chunk_bytes: 8001,
                    ..base()
                },
                "chunk_bytes",
            ),
            (
                TelnyxConfig {
                    reorder_window: 65,
                    ..base()
                },
                "reorder_window",
            ),
            (
                TelnyxConfig {
                    reorder_timeout: Duration::ZERO,
                    ..base()
                },
                "reorder_timeout",
            ),
            (
                TelnyxConfig {
                    hangup_timeout: Duration::ZERO,
                    ..base()
                },
                "hangup_timeout",
            ),
            (
                TelnyxConfig {
                    auth_token: Some(Secret::new(String::new())),
                    ..base()
                },
                "must not be empty",
            ),
            (
                TelnyxConfig {
                    auth_token: Some(Secret::new("x".repeat(4001))),
                    ..base()
                },
                "at most 4000",
            ),
        ];
        for (config, needle) in cases {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
        // A disabled reorder buffer needs no timeout.
        TelnyxConfig {
            reorder_window: 0,
            reorder_timeout: Duration::ZERO,
            ..base()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn the_auth_token_never_shows_in_debug_output() {
        let config = TelnyxConfig {
            auth_token: Some(Secret::new("super-secret-token".to_owned())),
            hangup: Some(HangupHook::new(|_| async { Ok(()) })),
            ..TelnyxConfig::default()
        };
        let text = format!("{config:?}");
        assert!(!text.contains("super-secret-token"));
        assert!(text.contains("HangupHook(..)"));
    }
}
