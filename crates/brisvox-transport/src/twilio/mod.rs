//! The Twilio Media Streams transport: phone calls through a bidirectional
//! `<Connect><Stream>`. Protocol facts: `docs/providers/twilio.md` (fetched 2026-10-08).
//!
//! Twilio is the WebSocket *client*: the TwiML tells it to connect to a `wss://` URL, and a
//! [`TwilioServer`] on that URL turns each connection into a call (a [`CallLink`])
//! for the engine, like the generic [`WebSocketServer`]. It
//! runs on the same server infrastructure: the accept loop, the call limit and 503 answers, the
//! queue of calls waiting for the engine, the pings and idle timeout, the write timeout that drops
//! a client that does not take output, and the shutdown. It is a separate listener with its own
//! [`TwilioConfig`]; terminate TLS (`wss://`) in front of it.
//!
//! ```xml
//! <Response>
//!   <Connect>
//!     <Stream url="wss://agent.example.com/twilio/stream" name="brisvox">
//!       <Parameter name="agent" value="support-tr" />
//!     </Stream>
//!   </Connect>
//! </Response>
//! ```
//!
//! # Audio
//! Both directions are mu-law (G.711), 8 kHz, mono, base64 in JSON `media` messages. Caller audio
//! is decoded to PCM16 8 kHz with [`brisvox_audio::Codec`] and handed to the engine at
//! [`SampleRate::Hz8000`](brisvox_core::frame::SampleRate::Hz8000); bot audio must arrive at
//! 8 kHz and is encoded back. The core is PCM16 throughout, so mu-law is never passed through.
//! Like the generic adapter this one never resamples: bot audio at another rate is
//! dropped and reported once per call as a fault. The call's metadata carries
//! `sample_rate=8000` and `encoding=mulaw` ([`CallInfo::sample_rate`](crate::CallInfo::sample_rate)),
//! plus `transport=twilio`, `stream_sid`, `call_sid`, `account_sid`, `tracks`, `twilio_protocol`,
//! `twilio_version`, `peer_addr` and every TwiML `<Parameter>` as `custom.<name>`.
//!
//! Bot audio goes out in messages of [`TwilioConfig::chunk_bytes`] mu-law bytes (default 160, 20
//! ms, the engine's frame size). Twilio documents no chunk size ("any size").
//!
//! # Interruption and playback tracking
//! - [`TransportCommand::Clear`](crate::TransportCommand::Clear) becomes Twilio's `clear`, which
//!   empties the audio Twilio buffered. Twilio answers a `clear` by echoing every pending `mark`,
//!   and an echo of a flushed mark looks like an echo of a played one. The adapter therefore keeps
//!   the marks it has in flight ([`marks::MarkBook`]) and forgets them all when it sends the
//!   `clear`; the echoes that follow find nothing and are dropped. The core reports the same marks
//!   as [`TransportEvent::MarkCleared`](crate::TransportEvent::MarkCleared).
//! - Engine marks ([`TransportCommand::Mark`](crate::TransportCommand::Mark), normally one at the
//!   end of each utterance) go out as `mark` messages named `e<n>`; the echo becomes
//!   [`TransportEvent::MarkPlayed`](crate::TransportEvent::MarkPlayed). The engine's id never
//!   goes on the wire (Twilio documents no limit on a name), so any id works.
//! - Every [`TwilioConfig::mark_interval`] of audio the adapter also sends a progress mark `p<n>`.
//!   It advances the adapter's played position, bounds how much audio can sit unplayed on
//!   Twilio's side (the docs warn at 10 minutes, error 31931) and feeds [`TwilioStats`]
//!   (`max_echo_latency`, `max_unplayed`) so the real playout delay can be measured.
//! - Every echoed mark (progress or engine) also tells the core how far the caller is behind: the
//!   audio sent after the echoed mark has not played yet. The adapter reports it as
//!   [`WireEvent::Lead`](crate::WireEvent::Lead), which the session forwards as
//!   [`TransportEvent::PlaybackLead`](crate::TransportEvent::PlaybackLead): the engine estimates
//!   what the caller heard mid-utterance as `PlaybackPosition - PlaybackLead`. Using it in the
//!   playout book is the engine's side of the played-text work and is not done by this crate, nor yet
//!   by the engine: until then mid-utterance barge-ins use the engine's fixed lead margin
//!   (`docs/providers/twilio.md`, Marks, clear and played text).
//!
//! # Request validation
//! With [`TwilioConfig::auth_token`] set, the upgrade request must carry a valid
//! `X-Twilio-Signature` (HMAC-SHA1 of the public URL with the account's primary Auth Token, see
//! [`signature`]) or the answer is HTTP 403. Without a token anything that reaches the path is
//! accepted: only do that behind another check. Requests to any other path get HTTP 404.
//! `start.customParameters` arrive after the handshake and are not signed. The signature covers
//! the URL only, and a fixed URL is the same on every call: whoever has seen one signature (a
//! proxy log) can replay it. Set [`TwilioConfig::stream_secret`] to give every call its own URL
//! (`<path>/<callSid>.<exp>.<mac>`, [`stream_token`]): the upgrade is then accepted once, until
//! `exp`, for that call only, and the signature covers the whole per-call URL. A configuration
//! with an `auth_token` needs either the secret or the explicit legacy flag
//! [`TwilioConfig::allow_static_url`].
//!
//! # Hang-up
//! Twilio's `stop` or a close frame is a hang-up (`StopReason::PeerHangup`); a socket that
//! vanishes is a disconnect. When the engine hangs up the adapter closes the socket (code 1000):
//! Twilio only accepts `media`, `mark` and `clear` from the server, so closing is the way to end
//! the stream, after which Twilio continues with the TwiML following `<Connect>`.
//!
//! The close is immediate and does not wait for Twilio to play what it buffered, and whether
//! Twilio still plays buffered audio after the socket closed is not documented (UNVERIFIED).
//! **Rule for the engine:** to say goodbye, send the audio and an utterance-end
//! [`TransportCommand::Mark`](crate::TransportCommand::Mark), wait for its
//! [`TransportEvent::MarkPlayed`](crate::TransportEvent::MarkPlayed) (with a timeout of its own),
//! and only then send [`TransportCommand::Stop`](crate::TransportCommand::Stop).
//!
//! # Streams that end on purpose and come back
//! REST call control (`call_control::twilio`) changes a call by replacing its TwiML, which ends
//! the stream: sending DTMF and a warm transfer do. The call then comes back on a **new** stream
//! (`<Parameter name="resume">`, [`RESUME_PARAMETER`]). The host announces the end first with
//! [`TwilioServer::stream_resumer`] and [`StreamResumer::expect_resume`]; for
//! [`EXPECTED_WITHIN`] after that, a `stop`, a close frame or a vanished socket of that call is
//! taken as the host's doing: the engine gets **no** hang-up event and its session is parked
//! (what it sends meanwhile, up to a minute of audio, is held for the new stream; a barge-in
//! `clear` drops it). A stream of the same `callSid` whose `start` carries the
//! `resume` parameter is handed to the parked session, which goes on with it (the engine sees
//! one call; marks in flight on the old stream are never echoed). A session nobody resumed
//! within the announced time (at most [`MAX_LINGER`]) ends as a disconnect. A `resume` stream
//! that nobody expects is refused. The resumed stream passes the same upgrade check (path and
//! signature) and the host's admission gate as any other; a host at its call limit refuses it
//! like any new connection.
//!
//! # Faults
//! Twilio is the only expected peer, so protocol faults are non-fatal: a message that does not
//! parse, a bad payload, an unknown mark or another stream's `streamSid` is dropped (the first
//! two are reported to the engine as faults; more than 16 on a call close it). Fatal: a `start`
//! with a media format other than mu-law/8000/mono, a frame above the size limits, an idle
//! connection, an engine that takes no input.

mod connection;
pub mod marks;
pub mod protocol;
mod registry;
pub mod signature;
pub mod stream_token;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::admission::SharedGate;
use crate::call::CallLink;
use crate::error::{Error, Result};
use crate::transport::Transport;
use crate::websocket::{Overflow, ServeFn, ServerConfig, WebSocketServer};

pub use marks::{MarkBook, TwilioStats};
pub use registry::{EXPECTED_WITHIN, MAX_LINGER, RESUME_PARAMETER, StreamResumer};

use connection::TwilioShared;
use marks::Counters;
use registry::Registry;

/// Settings specific to Twilio (the server limits and timeouts are a [`ServerConfig`]).
#[derive(Debug, Clone)]
pub struct TwilioConfig {
    /// The path of the stream URL in the TwiML. Other paths are answered with 404. A trailing `/`
    /// is accepted on the request whether or not it is configured.
    pub path: String,
    /// The account's primary Auth Token. `Some` makes the `X-Twilio-Signature` check mandatory
    /// and requires [`public_url`](Self::public_url). Never logged (`Secret` prints as redacted).
    pub auth_token: Option<Secret<String>>,
    /// The URL exactly as written in the TwiML (`wss://host/path`, no query string), for the
    /// signature check. Required with an auth token. Behind a reverse proxy this is the public
    /// URL, not the address the proxy forwards to; its path may differ from [`path`](Self::path)
    /// (a gateway may serve `wss://host/voice/twilio` and forward to `/twilio/stream`), because
    /// `path` only selects the route and the signature is computed over this URL as given. The
    /// proxy must pass the `X-Twilio-Signature` header through.
    pub public_url: Option<String>,
    /// The secret of per-call stream URLs (at least 32 bytes; [`stream_token`]). With it the
    /// stream URL of every call ends in a segment `<callSid>.<exp>.<mac>`: the upgrade is
    /// accepted only if the segment verifies, has not expired and has not been used, and the
    /// `X-Twilio-Signature` (which then covers that whole URL) holds, so a captured upgrade
    /// cannot be replayed. Without it every call has the same URL and the same signature.
    pub stream_secret: Option<Secret<String>>,
    /// Also accept the fixed URL ([`path`](Self::path) without a segment) when `stream_secret`
    /// is set, and, without a `stream_secret`, allow an `auth_token` at all. **Legacy: the
    /// signature of the fixed URL is one constant that anyone who saw it once can replay.** For
    /// deployments whose own TwiML cannot mint per-call URLs. Default `false`.
    pub allow_static_url: bool,
    /// Mu-law bytes per outgoing `media` message (160 is 20 ms). At least 1.
    pub chunk_bytes: usize,
    /// Audio between progress marks; zero disables them.
    pub mark_interval: Duration,
}

impl Default for TwilioConfig {
    fn default() -> Self {
        Self {
            path: "/twilio/stream".to_owned(),
            auth_token: None,
            public_url: None,
            stream_secret: None,
            allow_static_url: false,
            chunk_bytes: 160,
            mark_interval: Duration::from_millis(200),
        }
    }
}

/// Largest `chunk_bytes`: one second of audio.
const MAX_CHUNK_BYTES: usize = 8000;

impl TwilioConfig {
    /// Checks the configuration; [`TwilioServer::bind`] and [`TwilioServer::serve`] call it.
    ///
    /// The rules: `path` starts with `/` and has no query string; `chunk_bytes` is 1 to 8000; an
    /// auth token is not empty and comes with a `public_url` that is a `ws://`, `wss://`,
    /// `http://` or `https://` URL without query string or fragment (its path need not equal
    /// `path`).
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if !self.path.starts_with('/') || self.path.contains(['?', '#']) {
            return bad("twilio path must start with / and have no query string");
        }
        if self.chunk_bytes == 0 || self.chunk_bytes > MAX_CHUNK_BYTES {
            return bad("twilio chunk_bytes must be between 1 and 8000");
        }
        if let Some(secret) = &self.stream_secret
            && secret.expose().len() < stream_token::MIN_SECRET_BYTES
        {
            return bad("twilio stream_secret must be at least 32 bytes");
        }
        let Some(token) = &self.auth_token else {
            return Ok(());
        };
        if self.stream_secret.is_none() && !self.allow_static_url {
            return bad(
                "twilio: set stream_secret (per-call signed stream URLs) or allow_static_url \
                 (legacy: the fixed URL's signature can be replayed)",
            );
        }
        if token.expose().is_empty() {
            return bad("twilio auth_token must not be empty");
        }
        let Some(url) = &self.public_url else {
            return bad("twilio public_url is required with an auth_token");
        };
        if url.contains(['?', '#']) {
            return bad("twilio public_url must have no query string or fragment");
        }
        let candidates = signature::candidate_urls(url);
        let Some(first) = candidates.first() else {
            return bad("twilio public_url is not a URL");
        };
        let scheme_ok = ["wss://", "ws://", "https://", "http://"]
            .iter()
            .any(|scheme| first.starts_with(scheme));
        if !scheme_ok {
            return bad("twilio public_url must start with wss://, ws://, https:// or http://");
        }
        Ok(())
    }
}

/// A server that yields one call per Twilio Media Stream.
///
/// Dropping it cancels its tasks without waiting; [`shutdown`](Self::shutdown) waits for them.
pub struct TwilioServer {
    inner: WebSocketServer,
    counters: Arc<Counters>,
    resumer: StreamResumer,
}

impl TwilioServer {
    /// Binds `server.bind_addr` and starts accepting Twilio connections. The server is a child of
    /// `parent`: cancelling it stops the server.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `twilio` is invalid, [`Error::Io`] if the address cannot
    /// be bound.
    pub async fn bind(
        server: ServerConfig,
        twilio: TwilioConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        server.validate()?;
        twilio.validate()?;
        let listener = TcpListener::bind(server.bind_addr).await?;
        Self::serve(listener, server, twilio, clock, parent)
    }

    /// Starts accepting connections on an already bound `listener` (`server.bind_addr` is
    /// ignored). Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`Error::Config`] if `server` or `twilio` is invalid, [`Error::Io`] if the listener's
    /// address cannot be read.
    pub fn serve(
        listener: TcpListener,
        server: ServerConfig,
        twilio: TwilioConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::serve_gated(listener, server, twilio, clock, parent, None)
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
        twilio: TwilioConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        server.validate()?;
        twilio.validate()?;
        let counters = Arc::new(Counters::default());
        let signed_urls = match (&twilio.auth_token, &twilio.public_url) {
            (Some(_), Some(url)) => signature::candidate_urls(url),
            _ => Vec::new(),
        };
        let tokens = match &twilio.stream_secret {
            Some(secret) => Some(
                stream_token::StreamTokenBook::new(secret, clock.clone())
                    .map_err(|e| Error::Config(format!("twilio: {e}")))?,
            ),
            None => None,
        };
        let registry = Arc::new(Registry::default());
        let resumer = StreamResumer::new(Arc::clone(&registry), clock.clone());
        let shared = Arc::new(TwilioShared {
            config: twilio,
            counters: Arc::clone(&counters),
            signed_urls,
            registry,
            tokens,
        });
        let serving = Arc::clone(&shared);
        let handler: ServeFn = Arc::new(move |stream, peer, admitted, server_shared| {
            Box::pin(connection::serve(
                stream,
                peer,
                admitted,
                server_shared,
                Arc::clone(&serving),
            ))
        });
        // A call whose session waits for its next stream keeps its own permit and ticket: that
        // stream must get in even when the server is full.
        let waiting = Arc::clone(&shared);
        let overflow = Overflow {
            wanted: Arc::new(move || waiting.registry.awaiting_resume()),
            serve: Arc::new(move |stream, peer, slot, server_shared| {
                Box::pin(connection::serve_overflow(
                    stream,
                    peer,
                    slot,
                    server_shared,
                    Arc::clone(&shared),
                ))
            }),
        };
        let inner = WebSocketServer::serve_with_overflow(
            listener,
            server,
            clock,
            parent,
            handler,
            gate,
            Some(overflow),
        )?;
        Ok(Self {
            inner,
            counters,
            resumer,
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

    /// The host's way to announce that a call's stream ends on purpose and the call comes back
    /// on a new one (DTMF and warm transfers through call control, see
    /// [`StreamResumer::expect_resume`]).
    pub fn stream_resumer(&self) -> StreamResumer {
        self.resumer.clone()
    }

    /// Counters summed over every call since the server started.
    pub fn stats(&self) -> TwilioStats {
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

impl Transport for TwilioServer {
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

    fn with_token(url: Option<&str>) -> TwilioConfig {
        TwilioConfig {
            auth_token: Some(Secret::new("token".to_owned())),
            public_url: url.map(str::to_owned),
            allow_static_url: true,
            ..TwilioConfig::default()
        }
    }

    #[test]
    fn a_signed_listener_needs_per_call_urls_or_the_legacy_flag() {
        let signed = |secret: Option<&str>, legacy: bool| TwilioConfig {
            auth_token: Some(Secret::new("token".to_owned())),
            public_url: Some("wss://h.example/twilio/stream".to_owned()),
            stream_secret: secret.map(|s| Secret::new(s.to_owned())),
            allow_static_url: legacy,
            ..TwilioConfig::default()
        };
        let long = "0123456789abcdef0123456789abcdef";
        let error = signed(None, false).validate().unwrap_err().to_string();
        assert!(error.contains("stream_secret"), "{error}");
        assert!(error.contains("replayed"), "{error}");
        signed(None, true).validate().unwrap();
        signed(Some(long), false).validate().unwrap();
        signed(Some(long), true).validate().unwrap();
        let error = signed(Some("short"), true).validate().unwrap_err();
        assert!(error.to_string().contains("32 bytes"), "{error}");
        // Without an auth token nothing is signed and nothing is required.
        TwilioConfig::default().validate().unwrap();
    }

    #[test]
    fn the_default_configuration_is_valid() {
        TwilioConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_configurations_are_refused_with_a_reason() {
        let cases: Vec<(TwilioConfig, &str)> = vec![
            (
                TwilioConfig {
                    path: "stream".to_owned(),
                    ..TwilioConfig::default()
                },
                "path",
            ),
            (
                TwilioConfig {
                    path: "/a?b=1".to_owned(),
                    ..TwilioConfig::default()
                },
                "path",
            ),
            (
                TwilioConfig {
                    chunk_bytes: 0,
                    ..TwilioConfig::default()
                },
                "chunk_bytes",
            ),
            (
                TwilioConfig {
                    chunk_bytes: 8001,
                    ..TwilioConfig::default()
                },
                "chunk_bytes",
            ),
            (with_token(None), "public_url is required"),
            (
                with_token(Some("wss://h/twilio/stream?x=1")),
                "no query string",
            ),
            (
                with_token(Some("agent.example.com/twilio/stream")),
                "not a URL",
            ),
            (with_token(Some("ftp://h/twilio/stream")), "must start with"),
            (
                TwilioConfig {
                    auth_token: Some(Secret::new(String::new())),
                    public_url: Some("wss://h/twilio/stream".to_owned()),
                    allow_static_url: true,
                    ..TwilioConfig::default()
                },
                "must not be empty",
            ),
        ];
        for (config, needle) in cases {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
    }

    #[test]
    fn a_matching_public_url_is_valid_with_or_without_the_trailing_slash() {
        with_token(Some("wss://agent.example.com/twilio/stream"))
            .validate()
            .unwrap();
        with_token(Some("wss://agent.example.com:443/twilio/stream/"))
            .validate()
            .unwrap();
        let root = TwilioConfig {
            path: "/".to_owned(),
            auth_token: Some(Secret::new("t".to_owned())),
            public_url: Some("wss://agent.example.com".to_owned()),
            allow_static_url: true,
            ..TwilioConfig::default()
        };
        root.validate().unwrap();
    }

    #[test]
    fn the_public_path_may_differ_from_the_listener_path() {
        // A gateway that rewrites /voice/twilio to /twilio/stream.
        with_token(Some("wss://agent.example.com/voice/twilio"))
            .validate()
            .unwrap();
    }

    #[test]
    fn the_auth_token_never_shows_in_debug_output() {
        let config = TwilioConfig {
            auth_token: Some(Secret::new("super-secret-token".to_owned())),
            public_url: Some("wss://h/twilio/stream".to_owned()),
            ..TwilioConfig::default()
        };
        assert!(!format!("{config:?}").contains("super-secret-token"));
    }
}
