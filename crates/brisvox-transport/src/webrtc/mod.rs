//! The WebRTC transport: browsers talk to the engine over WebRTC audio. Protocol facts
//! and the library choice: `docs/providers/webrtc.md` (fetched 2026-10-08).
//!
//! ```text
//!   browser --POST /whip (SDP offer)--> signaling (HTTP/1.1 on TCP) --201 + SDP answer--> browser
//!   browser <==== ICE-lite + DTLS-SRTP, Opus/48 kHz, one shared UDP port ====> call task (str0m)
//!                                                                  |
//!                                         CallLink (WireEvent / WireOut) --> engine session
//! ```
//!
//! # Signaling (WHIP-style)
//! [`WebRtcServer`] answers five requests on [`WebRtcConfig::whip_path`], after RFC 9725 but
//! with one deliberate deviation:
//! - `POST` with `Content-Type: application/sdp`: the browser's SDP offer. `201 Created`, the SDP
//!   answer as the body and a `Location` header naming the session
//!   (`<whip_path>/<id>`, 128 random bits). `400` for SDP that does not parse or offers no usable
//!   audio, `415` for another content type, `401` without the bearer token (when one is
//!   configured), `503` + `Retry-After` at the call limit, `429` at the per-address limit.
//!   With [`WebRtcConfig::turn`] set, the `201` also carries one `Link: <url>; rel="ice-server";
//!   username=...; credential=...` header per TURN/STUN URL (RFC 9725 section 4.6); `POST` waits
//!   at most 1.5 s for credentials (a provider outage cannot stall an offer).
//! - `GET <whip_path>/ice`: `{"iceServers":[...]}` (the browser's `RTCIceServer` shape) with the
//!   same TURN credentials, `Cache-Control: no-store`, the same bearer check as `POST`; an empty
//!   list when no relay is configured; `429` + `Retry-After` when one client asks too often.
//!   The bundled pages use this endpoint, not the `Link` headers (which come after the offer is
//!   built). Without a bearer token anyone can obtain relay credentials: see [`turn_creds`].
//! - `DELETE <whip_path>/<id>`: hang up (`200`; `404` for an unknown session).
//! - `OPTIONS`: `200` with `Allow` (and `Accept-Post: application/sdp` where `POST` is taken) and,
//!   when
//!   [`WebRtcConfig::cors_origin`] is set, the CORS headers.
//! - `PATCH` (trickle ICE, ICE restart): `405`. Signaling is non-trickle: the offer must carry
//!   its candidates, the answer carries the server's.
//!
//! **Deviation from RFC 9725:** WHIP is ingest only (the client offers `sendonly`, the server
//! answers `recvonly`). A voice agent also sends audio back, so the server answers `sendrecv` to a
//! `sendrecv` offer on the same session (plain JSEP). A `sendonly` offer is answered `recvonly`:
//! the call works but the bot cannot speak (reported once as a fault).
//!
//! **Kept from RFC 9725 4.4:** an answer is never partially successful. The offer must have
//! exactly one media section, a live audio one, with `rtcp-mux`; anything else (audio plus
//! video, a data channel, a second audio section) is refused whole with `400`.
//!
//! A demo page at [`WebRtcConfig::demo_path`] (`getUserMedia` + `RTCPeerConnection`, non-trickle)
//! talks to this endpoint. Microphones need a secure context: `http://localhost` works, anything
//! else needs HTTPS in front of the signaling port (TLS is not terminated here).
//!
//! # Media
//! One `str0m` `Rtc` per call, in ICE-lite mode by default (the server has an address the
//! browser can reach; it only answers connectivity checks), with str0m's default crypto backend
//! (RustCrypto/dimpl; `aws-lc-sys` is built from C for the DTLS certificate and handshake, so the
//! feature needs a C toolchain and cmake, like libopus).
//! Opus (48 kHz RTP clock, mono content) is decoded to PCM16, resampled with
//! [`StreamResampler`](brisvox_audio::StreamResampler) to [`WebRtcConfig::input_rate`] and handed
//! to the engine; bot audio arrives at [`WebRtcConfig::output_rate`], is resampled to 48 kHz, cut
//! into 20 ms frames and encoded. The call's metadata carries both rates
//! (`sample_rate` and `output_sample_rate`, see [`CallInfo::sample_rate`](crate::CallInfo)).
//! Bot audio at another rate is dropped with one fault per call. Packet loss on the
//! way in is concealed by the decoder (at most [`WebRtcConfig::max_concealed_frames`] frames
//! per gap); the encoder adds in-band FEC.
//!
//! The Opus codec sits behind [`opus::OpusFactory`]; the default is libopus. Encoding costs
//! 90-376 us and decoding 25-45 us per 20 ms frame (`docs/providers/webrtc.md` 10.2), over the
//! 50 us budget for tokio workers, so give the server an
//! [`InferencePool`](brisvox_inference::pool::InferencePool) handle in
//! [`WebRtcOptions::codec_pool`] ([`WebRtcOptions::with_pool`]) in production. Without one the
//! codec runs on the call's task: fine for tests, the simulator and a handful of calls, and
//! [`WebRtcServer::serve`] logs a warning when `max_calls` is above
//! [`INLINE_CODEC_MAX_CALLS`] and there is no pool. A call's inbound audio is rate limited
//! ([`WebRtcConfig::max_audio_packets_per_sec`]), which bounds the codec work one client can
//! cause, with or without a pool.
//!
//! # Reachability: ICE-TCP and TURN
//! With [`WebRtcConfig::tcp_addr`] the answer also carries a passive TCP host candidate
//! (RFC 6544, RFC 4571 framing; UDP keeps the higher priority) for clients whose network blocks
//! UDP; the protocol and its bounds are in the `tcp` module and `docs/ARCHITECTURE.md`. With
//! [`WebRtcConfig::turn`] the browser is handed short-lived TURN credentials ([`turn_creds`]).
//! A browser's TURN relay still reaches the server's UDP port; a host without UDP ingress
//! (Railway) sets [`WebRtcConfig::turn_relay`]: the server holds its own TURN allocation over
//! TCP/TLS and advertises the relayed address ([`RelayConfig`], the `relay` module).
//! Per-host recipes: `docs/DEPLOY.md`.
//!
//! # Interruption and marks
//! - [`WireOut::Clear`](crate::WireOut): the buffered bot audio and the encoder history are
//!   dropped, pending mark acknowledgements are forgotten. Packets already on the wire are
//!   played by the browser (there is no remote flush in WebRTC), so a barge-in is heard after
//!   the audio the browser already holds: up to its jitter buffer, tens of milliseconds.
//! - [`WireOut::Mark`](crate::WireOut): browsers do not report playout. The mark is
//!   acknowledged [`WebRtcConfig::mark_delay`] after the audio ahead of it was sent (the partial
//!   frame and the resampler tail are flushed first), an estimate of the network and jitter
//!   buffer delay. It is an approximation, not an echo (`docs/providers/webrtc.md`).
//!
//! # Timeline of the outgoing RTP stream
//! RTP timestamps follow the media: contiguous while audio keeps coming, and a jump to the
//! wall-clock position (with the marker bit) after a pause of more than 60 ms, so the browser's
//! jitter buffer does not mistake silence for lateness.
//!
//! # Bounded everything
//! - Calls: [`WebRtcConfig::max_calls`] at once (handshake included); more get `503`.
//! - Signaling: [`WebRtcConfig::max_http_connections`] connections, [`WebRtcConfig::max_sdp_bytes`]
//!   body, 8 KiB of headers, one request per connection, a deadline on reading and writing.
//! - UDP: one socket for all calls. Each call has an inbound queue of
//!   [`WebRtcConfig::inbound_queue`] datagrams, drop-oldest; datagrams from unknown sources are
//!   dropped (`stats().datagrams_unrouted`). Sending uses `try_send_to` and counts what the
//!   socket refuses; nothing waits on the network.
//! - TURN relay: one connection and allocation per call, setup within
//!   [`RelayConfig::setup_timeout`]; two bounded drop-oldest queues of [`RelayConfig::queue`]
//!   packets; a malformed or oversize frame from the TURN server ends that allocation only.
//! - ICE-TCP: [`WebRtcConfig::max_tcp_connections`] connections, at most
//!   [`WebRtcConfig::max_tcp_connections_per_ip`] per source IP; a connection that names no live
//!   call within [`WebRtcConfig::tcp_idle_timeout`] is closed; frames above
//!   [`WebRtcConfig::tcp_max_frame`] close the connection; each connection has a fixed 4 KiB read
//!   buffer and an outbound queue of [`WebRtcConfig::tcp_queue`] frames (drop-oldest, whole frames
//!   only, never blocking the call task).
//! - Inbound audio per call: [`WebRtcConfig::max_audio_packets_per_sec`] (token bucket); the
//!   excess is dropped before the codec and counted in `stats().audio_packets_rate_limited`.
//! - Calls waiting for the engine: [`WebRtcConfig::accept_queue`]; a call that finds it full ends
//!   at once.
//! - Time: every timer runs on the injected [`Clock`](brisvox_core::Clock); `str0m` is handed
//!   the clock's `Instant` converted to `std`.
//!
//! # Tasks
//! The server owns an HTTP accept loop and a UDP reader; each call owns one task. All are
//! tracked: [`WebRtcServer::shutdown`] cancels and joins them. A panic inside a call (a `str0m`
//! or codec bug) ends that call only and is counted in [`WebRtcStats::call_panics`].

mod demux;
mod http;
mod limits;
pub mod media;
pub mod opus;
mod peer;
mod relay;
mod signaling;
mod tcp;
pub mod turn_creds;
mod ws;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock};
use brisvox_inference::pool::PoolHandle;
use str0m::crypto::CryptoProvider;
use str0m::crypto::dtls::DtlsCert;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::auth::{Audience, Authenticator, SessionSecrets};
use crate::call::CallLink;
use crate::cidr::Cidr;
use crate::error::{Error, Result};
use crate::transport::Transport;

pub use brisvox_core::telemetry::turn_client_log_allowed;
pub use limits::ClientIpHeader;
pub use opus::{LibOpusFactory, OpusFactory, OpusSettings};
pub use relay::RelayConfig;
pub use turn_creds::{IceServer, TurnConfig, TurnProvider};

use demux::Registry;

/// The demo page, with `__WHIP_PATH__` standing for [`WebRtcConfig::whip_path`], `__WS_PATH__` for
/// [`WebRtcConfig::ws_path`] and `__MODULE_PATH__` for [`WebRtcConfig::demo_module_path`].
pub(crate) const DEMO_HTML: &str = include_str!("demo.html");

/// The browser module the demo page imports (`sdk/web/brisvox-call.js`: WebRTC, the WebSocket
/// fallback, microphone and playback). The same file is what a website imports from `@brisvox/web`,
/// so the demo page and a site's button run the same audio code. Served next to the demo page.
pub(crate) const DEMO_MODULE_JS: &str = include_str!("../../../../sdk/web/brisvox-call.js");

/// The file name of the module under [`WebRtcConfig::demo_path`].
pub(crate) const DEMO_MODULE_FILE: &str = "brisvox-call.js";

/// Tuning of a [`WebRtcServer`].
#[derive(Debug, Clone)]
pub struct WebRtcConfig {
    /// Address of the signaling HTTP listener. Port 0 picks a free port.
    pub http_addr: SocketAddr,
    /// Address of the UDP socket all calls share. Port 0 picks a free port.
    pub udp_addr: SocketAddr,
    /// The IP address browsers send media to, advertised as the server's ICE host candidate.
    /// `None`: the IP of [`udp_addr`](Self::udp_addr), which then must not be unspecified
    /// (`0.0.0.0`). Behind NAT or in a container, set the public or host IP here (and forward
    /// the UDP port), or use [`turn_relay`](Self::turn_relay) when the host has no UDP ingress.
    pub candidate_ip: Option<IpAddr>,
    /// Address of the ICE-TCP listener (passive TCP candidate, RFC 6544; for browsers whose
    /// network blocks UDP, and hosts that expose a TCP port but no UDP). `None`: no TCP
    /// candidate. Port 0 picks a free port. Needs a usable [`candidate_ip`](Self::candidate_ip)
    /// like the UDP socket does. See the `tcp` module docs for the protocol and the bounds.
    pub tcp_addr: Option<SocketAddr>,
    /// The port advertised in the TCP candidate when it differs from the listener's (a TCP
    /// proxy or port mapping in front of the server, e.g. Railway's TCP proxy). `None`: the
    /// listener's port.
    pub tcp_candidate_port: Option<u16>,
    /// ICE-TCP connections served at once. At least 1; more are closed unanswered. Default 256.
    pub max_tcp_connections: usize,
    /// ICE-TCP connections one source IP may hold at once; more are closed unanswered. 0: no
    /// limit (a TCP proxy in front of the server makes every peer the proxy's address). Default
    /// 16. Bounded by [`max_tcp_connections`](Self::max_tcp_connections) anyway.
    pub max_tcp_connections_per_ip: usize,
    /// How long an ICE-TCP connection may take to name a live call (its first STUN request)
    /// before it is closed. Default 10 s.
    pub tcp_idle_timeout: Duration,
    /// Largest ICE-TCP frame (RFC 4571 length), in bytes; a larger one closes the connection.
    /// 512 to 65535. Default 2048 (above any packet `str0m` sends or a browser's MTU).
    pub tcp_max_frame: usize,
    /// Frames queued per ICE-TCP connection towards the browser (drop-oldest). At least 8.
    /// Default 20 (400 ms of 20 ms audio frames: on a slow connection the oldest audio is
    /// dropped instead of delayed).
    pub tcp_queue: usize,
    /// TURN credentials handed to browsers (WHIP `Link` headers and `GET <whip_path>/ice`).
    /// `None`: no relay is offered; the page uses its own iceServers or none.
    pub turn: Option<TurnConfig>,
    /// The server's own TURN relay: every call also advertises a TURN relayed address
    /// as a host candidate, so a server without UDP ingress (Railway: HTTP and a TCP proxy)
    /// completes calls through a managed TURN service reached over TCP/TLS 443. Needs
    /// [`turn`](Self::turn) (the same provider and credentials). `None`: no server-side relay.
    /// See [`RelayConfig`] and the `relay` module docs.
    pub turn_relay: Option<RelayConfig>,
    /// ICE-lite: the server never initiates connectivity checks. Right for a server with a
    /// reachable address (WHIP allows it, RFC 9725 section 4.4.5). Default true.
    pub ice_lite: bool,
    /// Calls served at once, handshake included. At least 1.
    pub max_calls: usize,
    /// Calls waiting for [`Transport::accept`]. At least 1. A call that finds it full ends at
    /// once (the engine is not taking calls).
    pub accept_queue: usize,
    /// Calls one client address may hold at once, handshake included (see
    /// [`trusted_proxies`](Self::trusted_proxies) for which address that is behind a reverse
    /// proxy). `None`: no limit. Over it: `429`. Default `Some(8)`.
    pub max_calls_per_ip: Option<usize>,
    /// Reverse proxies whose `X-Forwarded-For` / `Forwarded` headers are believed: when the TCP
    /// peer is in one of these networks, the client address is the last hop of the header that is
    /// not itself in this list (see the module docs of `limits`). From any other peer the headers
    /// are ignored. Empty (the default): the TCP peer is the client, so behind a proxy every
    /// visitor counts as the proxy's address: set this.
    pub trusted_proxies: Vec<Cidr>,
    /// Which forwarding header a trusted proxy writes the client address into (and the only one
    /// believed): `X-Forwarded-For` (default) or `Forwarded`. Set the one your proxy writes; the
    /// other is ignored. Never put `0.0.0.0/0` (or `::/0`) in
    /// [`trusted_proxies`](Self::trusted_proxies): it makes every client its own proxy, and a
    /// client can then choose the address it is limited under.
    pub client_ip_header: ClientIpHeader,
    /// Calls that may be in their handshake (offer answered, ICE and DTLS not complete) at once.
    /// Offers that never connect hold a call slot for [`connect_timeout`](Self::connect_timeout);
    /// this smaller pool keeps them from starving real calls. `None`: a quarter of `max_calls`
    /// (at least 1). Over it: `503` with `Retry-After`.
    pub max_handshaking: Option<usize>,
    /// Offers (`POST`) one client address may make per minute, with a burst of as many: a token
    /// bucket per address (an IPv6 address counts as its `/64`). `None`: no limit. Over it: `429`
    /// with `Retry-After`. Default `Some(5)`.
    pub offers_per_minute_per_ip: Option<u32>,
    /// Signaling connections served at once. At least 1; more are closed unanswered.
    pub max_http_connections: usize,
    /// Signaling connections one source address may hold at once (an IPv6 address counts as its
    /// `/64`); more are closed unanswered, so one address cannot fill
    /// [`max_http_connections`](Self::max_http_connections) with idle connections. Connections
    /// from a [`trusted_proxy`](Self::trusted_proxies) are exempt (they are many visitors').
    /// 0: no limit. Default 8.
    pub max_http_connections_per_ip: usize,
    /// The signaling endpoint. Starts with `/`, no query, no trailing `/`. Default `/whip`.
    pub whip_path: String,
    /// Where the demo page is served; `None` turns it off. Default `Some("/demo")`.
    pub demo_path: Option<String>,
    /// The WebSocket fallback: a WebSocket upgrade on this path of the signaling port
    /// serves the generic WebSocket audio protocol (`docs/WEBSOCKET_PROTOCOL.md`) with the same
    /// authentication, `Origin`/CORS rule and per-address and admission limits as `POST
    /// <whip_path>`; the bundled pages use it when WebRTC media cannot connect. `None` turns it
    /// off (the path is `404`). Starts with `/`, no query, no trailing `/`, and differs from
    /// `whip_path` (and from what is under it) and `demo_path`. Default `Some("/ws")`.
    pub ws_path: Option<String>,
    /// Static bearer token required on `POST` and `DELETE` (RFC 9725 section 4.7): shared,
    /// long-lived, for development. `None`: no static token. Never logged. Production uses
    /// [`sessions`](Self::sessions).
    pub bearer_token: Option<Secret<String>>,
    /// The secret(s) that session tokens (`aud` `brisvox-webrtc`, see [`crate::auth`]) are signed
    /// with. With them, `POST` needs `Authorization: Bearer <session token>` (single use; its
    /// `sid`, `agent` and `meta` reach the call's metadata and `max_s` caps the call), and
    /// `DELETE` needs only the session URL (128 random bits). With neither this nor
    /// [`bearer_token`](Self::bearer_token) the endpoint is open.
    pub sessions: Option<SessionSecrets>,
    /// Value of `Access-Control-Allow-Origin` for pages on another origin (`*` or an origin).
    /// `None`: no CORS headers (the demo page is same-origin). Default `None`. `*` is refused
    /// unless authentication is configured ([`bearer_token`](Self::bearer_token) or
    /// [`sessions`](Self::sessions)): any web page could otherwise start calls.
    pub cors_origin: Option<String>,
    /// Largest SDP offer, in bytes. At least 1024. Default 64 KiB.
    pub max_sdp_bytes: usize,
    /// Time allowed for a signaling request to arrive completely.
    pub request_timeout: Duration,
    /// Time allowed for writing a signaling response.
    pub write_timeout: Duration,
    /// Time from `POST` to a working media path (ICE and DTLS). Then the call is abandoned.
    pub connect_timeout: Duration,
    /// How long ICE may stay `Disconnected` before the call ends (it may recover).
    pub disconnect_grace: Duration,
    /// Longest a control event (hang-up, mark acknowledgement, fault) waits for the engine's
    /// queue; then the call ends. The call's task serves the browser too, so a stuck engine
    /// stalls that call for up to this long.
    pub send_timeout: Duration,
    /// Datagrams queued per call, between the UDP reader and the call's task (drop-oldest).
    /// At least 8.
    pub inbound_queue: usize,
    /// Rate of the caller audio handed to the engine. Default 16 kHz (STT and VAD models).
    pub input_rate: SampleRate,
    /// Rate the engine delivers bot audio at. Default 24 kHz (TTS output).
    pub output_rate: SampleRate,
    /// Opus encoder settings.
    pub opus: OpusSettings,
    /// How many consecutive lost frames the decoder conceals; a longer gap is passed on as is.
    /// Default 3 (60 ms).
    pub max_concealed_frames: u32,
    /// Audio packets held back to restore order (`str0m` reordering size). At least 1. Default 5.
    pub reorder_packets: usize,
    /// Longest an out-of-order or missing packet is waited for. Default 100 ms.
    pub reorder_timeout: Duration,
    /// How long after the audio before a mark was sent the mark is acknowledged (the estimate of
    /// network plus jitter-buffer delay; see the module docs). Default 80 ms.
    pub mark_delay: Duration,
    /// Longest a codec job may take on the pool before the frame is dropped.
    pub codec_timeout: Duration,
    /// Audio packets per second one call may send (a token bucket with a burst of one second).
    /// A browser sends 50 (20 ms packets) or 100 (10 ms); the excess is dropped before it costs
    /// a decode job and counted in [`WebRtcStats::audio_packets_rate_limited`]. At least 50.
    /// Default 200: four times the usual rate, so network bursts and 10 ms packets pass while
    /// one client cannot make the codec do more than that. It bounds the caller's audio rate
    /// towards the engine too.
    pub max_audio_packets_per_sec: u32,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            http_addr: SocketAddr::from(([127, 0, 0, 1], 8088)),
            udp_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            candidate_ip: None,
            tcp_addr: None,
            tcp_candidate_port: None,
            max_tcp_connections: 256,
            max_tcp_connections_per_ip: 16,
            tcp_idle_timeout: Duration::from_secs(10),
            tcp_max_frame: 2048,
            tcp_queue: 20,
            turn: None,
            turn_relay: None,
            ice_lite: true,
            max_calls: 256,
            accept_queue: 16,
            max_calls_per_ip: Some(8),
            trusted_proxies: Vec::new(),
            client_ip_header: ClientIpHeader::default(),
            max_handshaking: None,
            offers_per_minute_per_ip: Some(5),
            max_http_connections: 128,
            max_http_connections_per_ip: 8,
            whip_path: "/whip".to_owned(),
            demo_path: Some("/demo".to_owned()),
            ws_path: Some("/ws".to_owned()),
            bearer_token: None,
            sessions: None,
            cors_origin: None,
            max_sdp_bytes: 64 * 1024,
            request_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(15),
            disconnect_grace: Duration::from_secs(3),
            send_timeout: Duration::from_secs(5),
            inbound_queue: 128,
            input_rate: SampleRate::Hz16000,
            output_rate: SampleRate::Hz24000,
            opus: OpusSettings::default(),
            max_concealed_frames: 3,
            reorder_packets: 5,
            reorder_timeout: Duration::from_millis(100),
            mark_delay: Duration::from_millis(80),
            codec_timeout: Duration::from_millis(200),
            max_audio_packets_per_sec: 200,
        }
    }
}

impl WebRtcConfig {
    /// Where the demo page's script module is served: `<demo_path>/brisvox-call.js`. `None` when
    /// the demo page is off.
    #[must_use]
    pub fn demo_module_path(&self) -> Option<String> {
        self.demo_path
            .as_deref()
            .map(|demo| format!("{}/{DEMO_MODULE_FILE}", demo.trim_end_matches('/')))
    }

    /// Checks the configuration; [`WebRtcServer::bind`] and [`WebRtcServer::serve`] call it.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken: paths start with `/`, have no
    /// query and (for `whip_path`) no trailing `/`, and differ from each other; header values
    /// (`cors_origin`) contain no control characters; the limits and timeouts are positive;
    /// `max_sdp_bytes` is at least 1024 and `inbound_queue` at least 8; the Opus bit rate is
    /// 6000 to 510000, the complexity 0 to 10 and the expected loss 0 to 100; the 20 ms frame is
    /// a whole number of samples at both audio rates; `max_concealed_frames` is at most 6;
    /// `max_audio_packets_per_sec` is at least 50; `max_handshaking` and
    /// `offers_per_minute_per_ip` are at least 1 when set; `cors_origin` `*` comes with
    /// authentication.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if !path_ok(&self.whip_path) || self.whip_path.len() < 2 || self.whip_path.ends_with('/') {
            return bad("webrtc whip_path must start with / and have no query or trailing /");
        }
        if let Some(demo) = &self.demo_path {
            if !path_ok(demo) {
                return bad("webrtc demo_path must start with / and have no query");
            }
            if demo.trim_end_matches('/') == self.whip_path {
                return bad("webrtc demo_path must differ from whip_path");
            }
            if demo.contains(['"', '\'', '\\', '<', '>', '`']) {
                return bad(
                    "webrtc demo_path must not contain quotes, backslashes, backticks or angle brackets",
                );
            }
            // The path is written into the page's script and a module URL: no whitespace of any
            // kind (U+2028 and U+2029 end a line in JavaScript) and no control characters.
            if demo.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return bad("webrtc demo_path must not contain whitespace or control characters");
            }
            // The page's module is served at `<demo_path>/brisvox-call.js`: it must not shadow the
            // signaling endpoint, a session under it, or the WebSocket path.
            let module = self.demo_module_path().unwrap_or_default();
            if module == self.whip_path || module.starts_with(&format!("{}/", self.whip_path)) {
                return bad("webrtc demo_path must not put the demo module under whip_path");
            }
            if self.ws_path.as_deref() == Some(module.as_str()) {
                return bad("webrtc demo_path must not put the demo module on ws_path");
            }
        }
        if let Some(ws) = &self.ws_path {
            let whip_tree = format!("{}/", self.whip_path);
            if !path_ok(ws)
                || ws.len() < 2
                || ws.ends_with('/')
                || ws.contains(['"', '\'', '\\', '<', '>'])
            {
                return bad("webrtc ws_path must start with / and have no query or trailing /");
            }
            if *ws == self.whip_path || ws.starts_with(&whip_tree) {
                return bad("webrtc ws_path must differ from whip_path and not lie under it");
            }
            if self
                .demo_path
                .as_deref()
                .is_some_and(|demo| demo.trim_end_matches('/') == ws)
            {
                return bad("webrtc ws_path must differ from demo_path");
            }
        }
        if let Some(origin) = &self.cors_origin
            && (origin.is_empty() || origin.chars().any(char::is_control))
        {
            return bad("webrtc cors_origin must be a non-empty header value");
        }
        if self.cors_origin.as_deref() == Some("*")
            && self.bearer_token.is_none()
            && self.sessions.is_none()
        {
            return bad(
                "webrtc cors_origin * needs authentication (bearer_token or sessions): any web page could start calls",
            );
        }
        if let Some(token) = &self.bearer_token
            && (token.expose().is_empty() || token.expose().chars().any(char::is_whitespace))
        {
            return bad("webrtc bearer_token must be non-empty and contain no whitespace");
        }
        if self.max_calls == 0 || self.accept_queue == 0 || self.max_http_connections == 0 {
            return bad(
                "webrtc max_calls, accept_queue and max_http_connections must be at least 1",
            );
        }
        if self.max_calls_per_ip == Some(0) {
            return bad("webrtc max_calls_per_ip must be at least 1");
        }
        if self.max_handshaking == Some(0) {
            return bad("webrtc max_handshaking must be at least 1");
        }
        if self.offers_per_minute_per_ip == Some(0) {
            return bad("webrtc offers_per_minute_per_ip must be at least 1");
        }
        if self.max_sdp_bytes < 1024 {
            return bad("webrtc max_sdp_bytes must be at least 1024");
        }
        if self.inbound_queue < 8 {
            return bad("webrtc inbound_queue must be at least 8");
        }
        if self.reorder_packets == 0 {
            return bad("webrtc reorder_packets must be at least 1");
        }
        let timeouts = [
            ("request_timeout", self.request_timeout),
            ("write_timeout", self.write_timeout),
            ("connect_timeout", self.connect_timeout),
            ("disconnect_grace", self.disconnect_grace),
            ("send_timeout", self.send_timeout),
            ("reorder_timeout", self.reorder_timeout),
            ("codec_timeout", self.codec_timeout),
        ];
        if let Some((name, _)) = timeouts.iter().find(|(_, d)| d.is_zero()) {
            return bad(&format!("webrtc {name} must be longer than zero"));
        }
        if !(6_000..=510_000).contains(&self.opus.bitrate_bps) {
            return bad("webrtc opus bitrate_bps must be 6000 to 510000");
        }
        if !(0..=10).contains(&self.opus.complexity) {
            return bad("webrtc opus complexity must be 0 to 10");
        }
        if !(0..=100).contains(&self.opus.expected_loss_percent) {
            return bad("webrtc opus expected_loss_percent must be 0 to 100");
        }
        for rate in [self.input_rate, self.output_rate] {
            if rate.hz() % 50 != 0 {
                return bad("webrtc audio rates must hold a whole number of samples per 20 ms");
            }
        }
        if self.max_concealed_frames > 6 {
            return bad("webrtc max_concealed_frames must be at most 6");
        }
        if self.max_audio_packets_per_sec < 50 {
            return bad("webrtc max_audio_packets_per_sec must be at least 50");
        }
        // Far below `Semaphore::MAX_PERMITS`, which would panic the accept loop.
        if !(1..=65_536).contains(&self.max_tcp_connections) {
            return bad("webrtc max_tcp_connections must be 1 to 65536");
        }
        if self.tcp_idle_timeout.is_zero() {
            return bad("webrtc tcp_idle_timeout must be longer than zero");
        }
        if !(512..=65_535).contains(&self.tcp_max_frame) {
            return bad("webrtc tcp_max_frame must be 512 to 65535");
        }
        if self.tcp_queue < 8 {
            return bad("webrtc tcp_queue must be at least 8");
        }
        if self.tcp_candidate_port == Some(0) {
            return bad("webrtc tcp_candidate_port must not be 0");
        }
        if let Some(turn) = &self.turn {
            turn.validate()?;
        }
        if let Some(relay) = &self.turn_relay {
            relay.validate()?;
            if self.turn.is_none() {
                return bad(
                    "webrtc turn_relay needs turn (the provider that issues the credentials)",
                );
            }
            if !self.ice_lite {
                return bad(
                    "webrtc turn_relay needs ice_lite (the relayed address is a host candidate)",
                );
            }
        }
        Ok(())
    }
}

fn path_ok(path: &str) -> bool {
    path.starts_with('/') && !path.contains(['?', '#', ' ']) && !path.chars().any(char::is_control)
}

/// What a [`WebRtcServer`] needs besides its configuration.
#[derive(Clone)]
pub struct WebRtcOptions {
    /// Creates the Opus encoder and decoder of every call. Default: libopus.
    pub opus: Arc<dyn OpusFactory>,
    /// Where codec jobs run. `Some`: the inference pool (production). `None`: on the call's own
    /// task, i.e. on a tokio worker (tests, the simulator, few calls: encoding costs
    /// 90-376 us a frame, over the 50 us budget). With `None` and more than
    /// [`INLINE_CODEC_MAX_CALLS`] allowed calls, [`WebRtcServer::serve`] logs a warning.
    pub codec_pool: Option<PoolHandle>,
}

/// Above this many allowed calls ([`WebRtcConfig::max_calls`]) a server without a codec pool
/// logs a warning at start: with many talking calls the inline codec work would stall the tokio
/// workers (100 calls cost roughly one core-second per second).
pub const INLINE_CODEC_MAX_CALLS: usize = 8;

impl WebRtcOptions {
    /// The default options with codec work on `pool` (the production setup).
    pub fn with_pool(pool: PoolHandle) -> Self {
        Self {
            codec_pool: Some(pool),
            ..Self::default()
        }
    }

    /// Is this a setup where inline codec work could stall the runtime: no pool, and room for
    /// more calls than [`INLINE_CODEC_MAX_CALLS`]?
    pub fn inline_codec_is_risky(&self, config: &WebRtcConfig) -> bool {
        self.codec_pool.is_none() && config.max_calls > INLINE_CODEC_MAX_CALLS
    }
}

impl Default for WebRtcOptions {
    fn default() -> Self {
        Self {
            opus: Arc::new(LibOpusFactory),
            codec_pool: None,
        }
    }
}

impl std::fmt::Debug for WebRtcOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcOptions")
            .field("opus", &self.opus)
            .field("codec_pool", &self.codec_pool.is_some())
            .finish()
    }
}

/// Counters of one server since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebRtcStats {
    /// Offers accepted (`201`).
    pub sessions_created: u64,
    /// Calls that reached a working media path and were handed to the engine.
    pub calls_started: u64,
    /// Offers refused (`4xx`, `5xx`), including `503` at the call limit.
    pub offers_refused: u64,
    /// Offers refused for a missing, invalid, expired or already used credential (`401`, `413`).
    pub offers_unauthorized: u64,
    /// Offers refused by the per-address offer rate (`429`).
    pub offers_rate_limited: u64,
    /// Offers refused by the per-address call cap (`429`).
    pub offers_ip_limited: u64,
    /// Offers refused because too many calls were still in their handshake (`503`).
    pub offers_handshake_limited: u64,
    /// Offers let through untracked because the offer-rate table was full.
    pub rate_table_full: u64,
    /// Calls alive now, ICE and DTLS in progress included.
    pub sessions_active: u64,
    /// Datagrams read from the UDP socket.
    pub datagrams_in: u64,
    /// Datagrams from unknown sources that were not a STUN request for a live call.
    pub datagrams_unrouted: u64,
    /// Datagrams `str0m` could not parse or did not accept.
    pub datagrams_rejected: u64,
    /// Datagrams dropped from calls' full inbound queues (live: includes the calls running now).
    pub datagrams_dropped_overflow: u64,
    /// Datagrams sent.
    pub datagrams_out: u64,
    /// Datagrams the socket refused (its buffer was full).
    pub datagrams_out_dropped: u64,
    /// Caller audio frames handed to the engine.
    pub audio_frames_in: u64,
    /// Bot audio frames received from the engine and sent (encoded).
    pub audio_frames_out: u64,
    /// Opus packets written to RTP.
    pub rtp_packets_out: u64,
    /// Lost frames the decoder concealed.
    pub frames_concealed: u64,
    /// Audio packets dropped because a call sent more than
    /// [`WebRtcConfig::max_audio_packets_per_sec`].
    pub audio_packets_rate_limited: u64,
    /// Packets that did not decode.
    pub decode_errors: u64,
    /// Frames that did not encode.
    pub encode_errors: u64,
    /// Codec jobs the pool refused or did not finish in time (the frame was dropped).
    pub codec_dropped: u64,
    /// Bot audio dropped: wrong rate, a receive-only call, or not connected yet.
    pub audio_out_dropped: u64,
    /// Signaling requests that were malformed or too slow.
    pub bad_requests: u64,
    /// WebSocket upgrades on [`WebRtcConfig::ws_path`] that were answered `101`.
    pub ws_upgrades: u64,
    /// WebSocket upgrades refused with an HTTP answer (bad request, origin, credential, rate or
    /// capacity; the credential and capacity ones are also in the offer counters above).
    pub ws_refused: u64,
    /// Calls that ended in a panic.
    pub call_panics: u64,
    /// ICE-TCP connections accepted.
    pub tcp_connections: u64,
    /// ICE-TCP connections open now.
    pub tcp_connections_active: u64,
    /// ICE-TCP connections closed unanswered at the connection limit.
    pub tcp_connections_refused: u64,
    /// ICE-TCP connections closed because they never named a live call in time.
    pub tcp_unrouted_closed: u64,
    /// ICE-TCP connections closed for a frame above the limit.
    pub tcp_protocol_errors: u64,
    /// ICE-TCP frames read.
    pub tcp_frames_in: u64,
    /// ICE-TCP frames queued towards browsers.
    pub tcp_frames_out: u64,
    /// ICE-TCP frames evicted from a full per-connection queue (drop-oldest), counted when the
    /// connection ends.
    pub tcp_frames_out_dropped: u64,
    /// TURN allocations made for calls (server-side relay, [`WebRtcConfig::turn_relay`]).
    pub relay_allocations: u64,
    /// Calls whose relay could not be set up (credentials, connect, TLS, allocate, permissions or
    /// the setup timeout); they went on without it, or were refused in relay-only mode.
    pub relay_setup_failures: u64,
    /// TURN allocations alive now.
    pub relay_active: u64,
    /// Packets received through relays (unwrapped Data indications and ChannelData).
    pub relay_packets_in: u64,
    /// Packets sent through relays.
    pub relay_packets_out: u64,
    /// Packets lost on the relay path: evicted from a full queue, sent to a peer without a
    /// permission, or sent after the allocation ended.
    pub relay_packets_dropped: u64,
    /// `CreatePermission` requests the TURN server refused.
    pub relay_permission_failures: u64,
    /// Relay connections closed for a malformed, unknown or oversize frame from the TURN server.
    pub relay_protocol_errors: u64,
    /// Allocations that ended on their own (connection lost, refresh failed, protocol error)
    /// while the call was still running.
    pub relay_lost: u64,
    /// Longest a packet waited in a relay's outbound queue before its write, in microseconds
    /// (the maximum over all relays since the server started).
    pub relay_queue_dwell_max_us: u64,
    /// Packets that waited longer than 40 ms in a relay's outbound queue.
    pub relay_queue_slow: u64,
    /// Longest write to a TURN connection, in microseconds (maximum since the server started).
    pub relay_write_max_us: u64,
    /// Writes to a TURN connection that took longer than 20 ms.
    pub relay_slow_writes: u64,
}

/// The counters behind [`WebRtcStats`].
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) sessions_created: AtomicU64,
    pub(crate) calls_started: AtomicU64,
    pub(crate) offers_refused: AtomicU64,
    pub(crate) offers_unauthorized: AtomicU64,
    pub(crate) offers_rate_limited: AtomicU64,
    pub(crate) offers_ip_limited: AtomicU64,
    pub(crate) offers_handshake_limited: AtomicU64,
    pub(crate) rate_table_full: AtomicU64,
    pub(crate) sessions_active: AtomicU64,
    pub(crate) datagrams_in: AtomicU64,
    pub(crate) datagrams_unrouted: AtomicU64,
    pub(crate) datagrams_rejected: AtomicU64,
    pub(crate) datagrams_dropped_overflow: AtomicU64,
    pub(crate) datagrams_out: AtomicU64,
    pub(crate) datagrams_out_dropped: AtomicU64,
    pub(crate) audio_frames_in: AtomicU64,
    pub(crate) audio_frames_out: AtomicU64,
    pub(crate) rtp_packets_out: AtomicU64,
    pub(crate) frames_concealed: AtomicU64,
    pub(crate) audio_packets_rate_limited: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
    pub(crate) encode_errors: AtomicU64,
    pub(crate) codec_dropped: AtomicU64,
    pub(crate) audio_out_dropped: AtomicU64,
    pub(crate) bad_requests: AtomicU64,
    pub(crate) ws_upgrades: AtomicU64,
    pub(crate) ws_refused: AtomicU64,
    pub(crate) call_panics: AtomicU64,
    pub(crate) tcp_connections: AtomicU64,
    pub(crate) tcp_connections_active: AtomicU64,
    pub(crate) tcp_connections_refused: AtomicU64,
    pub(crate) tcp_unrouted_closed: AtomicU64,
    pub(crate) tcp_protocol_errors: AtomicU64,
    pub(crate) tcp_frames_in: AtomicU64,
    pub(crate) tcp_frames_out: AtomicU64,
    pub(crate) tcp_frames_out_dropped: AtomicU64,
    pub(crate) relay_allocations: AtomicU64,
    pub(crate) relay_setup_failures: AtomicU64,
    pub(crate) relay_active: AtomicU64,
    pub(crate) relay_packets_in: AtomicU64,
    pub(crate) relay_packets_out: AtomicU64,
    pub(crate) relay_packets_dropped: AtomicU64,
    pub(crate) relay_permission_failures: AtomicU64,
    pub(crate) relay_protocol_errors: AtomicU64,
    pub(crate) relay_lost: AtomicU64,
    pub(crate) relay_queue_dwell_max_us: AtomicU64,
    pub(crate) relay_queue_slow: AtomicU64,
    pub(crate) relay_write_max_us: AtomicU64,
    pub(crate) relay_slow_writes: AtomicU64,
}

impl Counters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// Raises `counter` to `value` if that is larger (a high-water mark).
    pub(crate) fn max(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    fn snapshot(&self) -> WebRtcStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        WebRtcStats {
            sessions_created: get(&self.sessions_created),
            calls_started: get(&self.calls_started),
            offers_refused: get(&self.offers_refused),
            offers_unauthorized: get(&self.offers_unauthorized),
            offers_rate_limited: get(&self.offers_rate_limited),
            offers_ip_limited: get(&self.offers_ip_limited),
            offers_handshake_limited: get(&self.offers_handshake_limited),
            rate_table_full: get(&self.rate_table_full),
            sessions_active: get(&self.sessions_active),
            datagrams_in: get(&self.datagrams_in),
            datagrams_unrouted: get(&self.datagrams_unrouted),
            datagrams_rejected: get(&self.datagrams_rejected),
            datagrams_dropped_overflow: get(&self.datagrams_dropped_overflow),
            datagrams_out: get(&self.datagrams_out),
            datagrams_out_dropped: get(&self.datagrams_out_dropped),
            audio_frames_in: get(&self.audio_frames_in),
            audio_frames_out: get(&self.audio_frames_out),
            rtp_packets_out: get(&self.rtp_packets_out),
            frames_concealed: get(&self.frames_concealed),
            audio_packets_rate_limited: get(&self.audio_packets_rate_limited),
            decode_errors: get(&self.decode_errors),
            encode_errors: get(&self.encode_errors),
            codec_dropped: get(&self.codec_dropped),
            audio_out_dropped: get(&self.audio_out_dropped),
            bad_requests: get(&self.bad_requests),
            ws_upgrades: get(&self.ws_upgrades),
            ws_refused: get(&self.ws_refused),
            call_panics: get(&self.call_panics),
            tcp_connections: get(&self.tcp_connections),
            tcp_connections_active: get(&self.tcp_connections_active),
            tcp_connections_refused: get(&self.tcp_connections_refused),
            tcp_unrouted_closed: get(&self.tcp_unrouted_closed),
            tcp_protocol_errors: get(&self.tcp_protocol_errors),
            tcp_frames_in: get(&self.tcp_frames_in),
            tcp_frames_out: get(&self.tcp_frames_out),
            tcp_frames_out_dropped: get(&self.tcp_frames_out_dropped),
            relay_allocations: get(&self.relay_allocations),
            relay_setup_failures: get(&self.relay_setup_failures),
            relay_active: get(&self.relay_active),
            relay_packets_in: get(&self.relay_packets_in),
            relay_packets_out: get(&self.relay_packets_out),
            relay_packets_dropped: get(&self.relay_packets_dropped),
            relay_permission_failures: get(&self.relay_permission_failures),
            relay_protocol_errors: get(&self.relay_protocol_errors),
            relay_lost: get(&self.relay_lost),
            relay_queue_dwell_max_us: get(&self.relay_queue_dwell_max_us),
            relay_queue_slow: get(&self.relay_queue_slow),
            relay_write_max_us: get(&self.relay_write_max_us),
            relay_slow_writes: get(&self.relay_slow_writes),
        }
    }
}

/// State shared by the signaling handlers, the UDP reader and every call.
pub(crate) struct Shared {
    pub(crate) config: WebRtcConfig,
    pub(crate) clock: SharedClock,
    pub(crate) cancel: CancellationToken,
    pub(crate) calls: mpsc::Sender<CallLink>,
    pub(crate) socket: Arc<UdpSocket>,
    /// The address advertised as the server's host candidate.
    pub(crate) candidate: SocketAddr,
    /// The advertised ICE-TCP (passive) candidate, when the TCP listener is on.
    pub(crate) tcp_candidate: Option<SocketAddr>,
    /// TURN credential issuer (browser relay), when configured.
    pub(crate) turn: Option<turn_creds::TurnService>,
    /// The server's own TURN client, when configured.
    pub(crate) relay: Option<relay::RelayService>,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) stats: Counters,
    pub(crate) permits: Arc<Semaphore>,
    /// Calls still in their handshake (a sub-pool of `permits`).
    pub(crate) handshake_permits: Arc<Semaphore>,
    pub(crate) auth: Authenticator,
    pub(crate) offer_rate: Option<Mutex<limits::OfferRateLimiter>>,
    pub(crate) crypto: Arc<CryptoProvider>,
    pub(crate) cert: DtlsCert,
    pub(crate) options: WebRtcOptions,
    pub(crate) tracker: TaskTracker,
    pub(crate) next_call: AtomicU64,
    pub(crate) random: ring::rand::SystemRandom,
    /// The WebSocket fallback on the signaling port, when [`WebRtcConfig::ws_path`] is set.
    pub(crate) ws: Option<ws::Fallback>,
}

/// A WebRTC server that yields one call per browser session.
///
/// Dropping it cancels its tasks without waiting; [`shutdown`](Self::shutdown) waits for them.
pub struct WebRtcServer {
    shared: Arc<Shared>,
    http_addr: SocketAddr,
    udp_addr: SocketAddr,
    tcp_addr: Option<SocketAddr>,
    calls: mpsc::Receiver<CallLink>,
    cancel: CancellationToken,
}

impl WebRtcServer {
    /// Binds the signaling and the media sockets and starts serving, with the default
    /// [`WebRtcOptions`] (libopus, codec work on the call tasks, which is only right for tests
    /// and few calls: production uses [`bind_with`](Self::bind_with) and
    /// [`WebRtcOptions::with_pool`]). The server is a child of `parent`: cancelling it stops the
    /// server.
    ///
    /// # Errors
    /// [`Error::Config`] if `config` is invalid or the candidate address cannot be advertised,
    /// [`Error::Io`] if an address cannot be bound, [`Error::WebRtc`] if the DTLS certificate
    /// cannot be generated.
    pub async fn bind(
        config: WebRtcConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::bind_with(config, WebRtcOptions::default(), clock, parent).await
    }

    /// [`bind`](Self::bind) with explicit [`WebRtcOptions`].
    ///
    /// # Errors
    /// As [`bind`](Self::bind).
    pub async fn bind_with(
        config: WebRtcConfig,
        options: WebRtcOptions,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        config.validate()?;
        let http = TcpListener::bind(config.http_addr).await?;
        let udp = UdpSocket::bind(config.udp_addr).await?;
        Self::serve(http, udp, config, options, clock, parent).await
    }

    /// Serves on sockets that are already bound (`config.http_addr` and `config.udp_addr` are
    /// ignored; the ICE-TCP listener, if `config.tcp_addr` is set, is bound here). Must run
    /// inside a tokio runtime.
    ///
    /// # Errors
    /// As [`bind`](Self::bind), except the bind errors.
    pub async fn serve(
        http: TcpListener,
        udp: UdpSocket,
        config: WebRtcConfig,
        options: WebRtcOptions,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        config.validate()?;
        if options.inline_codec_is_risky(&config) {
            tracing::warn!(
                max_calls = config.max_calls,
                "webrtc: no codec pool and room for many calls: Opus runs on tokio workers \
                 (90-376 us per frame); set WebRtcOptions::codec_pool for production"
            );
        }
        let http_addr = http.local_addr()?;
        let udp_addr = udp.local_addr()?;
        let ip = config.candidate_ip.unwrap_or_else(|| udp_addr.ip());
        // Relay-only mode advertises no UDP host candidate, so no public address is needed.
        let relay_only = config.turn_relay.as_ref().is_some_and(|relay| relay.only);
        if ip.is_unspecified() && !relay_only {
            return Err(Error::Config(
                "webrtc candidate_ip is required when the UDP socket is bound to an unspecified address"
                    .to_owned(),
            ));
        }
        let candidate = SocketAddr::new(ip, udp_addr.port());
        // The ICE-TCP listener binds here (`serve` takes the other sockets ready-made).
        let tcp = match config.tcp_addr {
            Some(addr) => Some(TcpListener::bind(addr).await?),
            None => None,
        };
        let tcp_addr = tcp.as_ref().map(TcpListener::local_addr).transpose()?;
        if relay_only && tcp_addr.is_some() && ip.is_unspecified() {
            return Err(Error::Config(
                "webrtc candidate_ip is required with the ICE-TCP listener (its candidate needs an address)"
                    .to_owned(),
            ));
        }
        let tcp_candidate = tcp_addr.map(|addr| {
            SocketAddr::new(ip, config.tcp_candidate_port.unwrap_or_else(|| addr.port()))
        });
        let turn = config
            .turn
            .clone()
            .map(|turn| turn_creds::TurnService::new(turn, clock.clone()))
            .transpose()?;
        let relay = config
            .turn_relay
            .clone()
            .map(relay::RelayService::new)
            .transpose()?;
        // The certificate takes tens of milliseconds to generate (ECDSA keygen and X.509): once
        // per server, on a blocking thread, shared by every call.
        let (crypto, cert) = tokio::task::spawn_blocking(|| {
            let provider = str0m::crypto::from_feature_flags();
            let cert = provider.dtls_provider.generate_certificate();
            (Arc::new(provider), cert)
        })
        .await
        .map_err(|e| Error::WebRtc(format!("crypto provider setup failed: {e}")))?;
        let cert =
            cert.ok_or_else(|| Error::WebRtc("could not generate a DTLS certificate".to_owned()))?;

        let cancel = parent.child_token();
        let handshake_pool = config
            .max_handshaking
            .unwrap_or_else(|| (config.max_calls / 4).max(1))
            .min(config.max_calls);
        let (calls_tx, calls_rx) = mpsc::channel(config.accept_queue);
        let ws = config.ws_path.clone().map(|path| ws::Fallback {
            path,
            ws: Arc::new(crate::websocket::Shared::hosted(
                clock.clone(),
                ws::Fallback::server_config(&config),
                calls_tx.clone(),
                cancel.clone(),
            )),
        });
        let shared = Arc::new(Shared {
            permits: Arc::new(Semaphore::new(config.max_calls)),
            handshake_permits: Arc::new(Semaphore::new(handshake_pool)),
            auth: Authenticator::from_config(
                Audience::Webrtc,
                config.bearer_token.as_ref(),
                config.sessions.as_ref(),
                &clock,
            ),
            offer_rate: config
                .offers_per_minute_per_ip
                .map(|n| Mutex::new(limits::OfferRateLimiter::new(n))),
            config,
            clock,
            cancel: cancel.clone(),
            calls: calls_tx,
            socket: Arc::new(udp),
            candidate,
            tcp_candidate,
            turn,
            relay,
            registry: Mutex::new(Registry::default()),
            stats: Counters::default(),
            crypto,
            cert,
            options,
            tracker: TaskTracker::new(),
            next_call: AtomicU64::new(1),
            random: ring::rand::SystemRandom::new(),
            ws,
        });
        shared.tracker.spawn(demux::udp_loop(Arc::clone(&shared)));
        shared
            .tracker
            .spawn(signaling::accept_loop(http, Arc::clone(&shared)));
        if let Some(listener) = tcp {
            shared
                .tracker
                .spawn(tcp::accept_loop(listener, Arc::clone(&shared)));
        }
        Ok(Self {
            shared,
            http_addr,
            udp_addr,
            tcp_addr,
            calls: calls_rx,
            cancel,
        })
    }

    /// The address of the signaling listener.
    pub const fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The address of the shared UDP socket.
    pub const fn udp_addr(&self) -> SocketAddr {
        self.udp_addr
    }

    /// The address of the ICE-TCP listener, when [`WebRtcConfig::tcp_addr`] is set.
    pub const fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    /// The address advertised to browsers as the server's ICE host candidate.
    pub fn candidate_addr(&self) -> SocketAddr {
        self.shared.candidate
    }

    /// Calls alive now (ICE and DTLS in progress included).
    pub fn active_calls(&self) -> usize {
        self.shared
            .config
            .max_calls
            .saturating_sub(self.shared.permits.available_permits())
    }

    /// Counters since the server started. Queue overflow is read from the live queues, so it
    /// is current while a call is overloaded, not only after it ends.
    pub fn stats(&self) -> WebRtcStats {
        // The registry lock makes the live sum and the total of ended calls one consistent view.
        let registry = self
            .shared
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stats = self.shared.stats.snapshot();
        stats.datagrams_dropped_overflow = stats
            .datagrams_dropped_overflow
            .saturating_add(registry.live_overflow());
        stats
    }

    /// Stops accepting, ends every call (the browsers see DTLS close) and waits for all tasks.
    ///
    /// # Errors
    /// [`Error::Core`] (timeout) if tasks were still running after `timeout`; they stay
    /// cancelled and finish on their own bounded waits.
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.cancel.cancel();
        self.shared.tracker.close();
        http::timeout(&*self.shared.clock, timeout, self.shared.tracker.wait())
            .await
            .map_err(|_| {
                Error::Core(brisvox_core::Error::Timeout {
                    what: "webrtc shutdown",
                    after: timeout,
                })
            })
    }
}

impl Drop for WebRtcServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Transport for WebRtcServer {
    /// Waits for the next call whose ICE and DTLS completed.
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// (or was cancelled through its parent token) and no queued call is left.
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(brisvox_core::Error::Cancelled.into()),
            // The server itself holds a sender (`Shared::calls`), so the channel never closes
            // by itself: the server's cancellation is the end. Calls queued before it are
            // still handed out; the tasks that could add more are stopping.
            () = self.cancel.cancelled() => Ok(self.calls.try_recv().ok()),
            link = self.calls.recv() => Ok(link),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_configuration_is_valid() {
        WebRtcConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_configurations_are_refused_with_a_reason() {
        let cases: Vec<(WebRtcConfig, &str)> = vec![
            (
                WebRtcConfig {
                    whip_path: "whip".to_owned(),
                    ..WebRtcConfig::default()
                },
                "whip_path",
            ),
            (
                WebRtcConfig {
                    whip_path: "/whip/".to_owned(),
                    ..WebRtcConfig::default()
                },
                "whip_path",
            ),
            (
                WebRtcConfig {
                    whip_path: "/".to_owned(),
                    ..WebRtcConfig::default()
                },
                "whip_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/whip".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/a\"b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                // Whitespace and control characters, U+2028 / U+2029 (line terminators in
                // JavaScript) and U+0085 among them, never reach the page's script.
                WebRtcConfig {
                    demo_path: Some("/a\u{2028}b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/a\u{2029}b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/a\u{85}b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/a\tb".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                WebRtcConfig {
                    demo_path: Some("/a\u{a0}b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "demo_path",
            ),
            (
                // The demo module would land at `/whip/brisvox-call.js`, under the signaling endpoint.
                WebRtcConfig {
                    demo_path: Some("/whip".to_owned()),
                    whip_path: "/x".to_owned(),
                    ws_path: Some("/whip/brisvox-call.js".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("ws".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("/whip".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("/whip/ice".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("/ws/".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("/demo".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    ws_path: Some("/a\"b".to_owned()),
                    ..WebRtcConfig::default()
                },
                "ws_path",
            ),
            (
                WebRtcConfig {
                    cors_origin: Some("a\r\nb".to_owned()),
                    ..WebRtcConfig::default()
                },
                "cors_origin",
            ),
            (
                WebRtcConfig {
                    bearer_token: Some(Secret::new("a b".to_owned())),
                    ..WebRtcConfig::default()
                },
                "bearer_token",
            ),
            (
                WebRtcConfig {
                    max_calls: 0,
                    ..WebRtcConfig::default()
                },
                "max_calls",
            ),
            (
                WebRtcConfig {
                    max_sdp_bytes: 10,
                    ..WebRtcConfig::default()
                },
                "max_sdp_bytes",
            ),
            (
                WebRtcConfig {
                    inbound_queue: 1,
                    ..WebRtcConfig::default()
                },
                "inbound_queue",
            ),
            (
                WebRtcConfig {
                    connect_timeout: Duration::ZERO,
                    ..WebRtcConfig::default()
                },
                "connect_timeout",
            ),
            (
                WebRtcConfig {
                    opus: OpusSettings {
                        bitrate_bps: 100,
                        ..OpusSettings::default()
                    },
                    ..WebRtcConfig::default()
                },
                "bitrate_bps",
            ),
            (
                WebRtcConfig {
                    opus: OpusSettings {
                        complexity: 11,
                        ..OpusSettings::default()
                    },
                    ..WebRtcConfig::default()
                },
                "complexity",
            ),
            (
                WebRtcConfig {
                    max_concealed_frames: 7,
                    ..WebRtcConfig::default()
                },
                "max_concealed_frames",
            ),
            (
                WebRtcConfig {
                    max_audio_packets_per_sec: 49,
                    ..WebRtcConfig::default()
                },
                "max_audio_packets_per_sec",
            ),
            (
                WebRtcConfig {
                    max_tcp_connections: 0,
                    ..WebRtcConfig::default()
                },
                "max_tcp_connections",
            ),
            (
                WebRtcConfig {
                    max_tcp_connections: 65_537,
                    ..WebRtcConfig::default()
                },
                "max_tcp_connections",
            ),
            (
                WebRtcConfig {
                    tcp_max_frame: 100,
                    ..WebRtcConfig::default()
                },
                "tcp_max_frame",
            ),
            (
                WebRtcConfig {
                    tcp_queue: 2,
                    ..WebRtcConfig::default()
                },
                "tcp_queue",
            ),
            (
                WebRtcConfig {
                    tcp_idle_timeout: Duration::ZERO,
                    ..WebRtcConfig::default()
                },
                "tcp_idle_timeout",
            ),
            (
                WebRtcConfig {
                    tcp_candidate_port: Some(0),
                    ..WebRtcConfig::default()
                },
                "tcp_candidate_port",
            ),
            (
                WebRtcConfig {
                    turn: Some(TurnConfig::new(TurnProvider::Coturn {
                        urls: Vec::new(),
                        secret: Secret::new("s".to_owned()),
                    })),
                    ..WebRtcConfig::default()
                },
                "turn",
            ),
        ];
        for (config, needle) in cases {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
    }

    #[test]
    fn the_bearer_token_never_shows_in_debug_output() {
        let config = WebRtcConfig {
            bearer_token: Some(Secret::new("super-secret-token".to_owned())),
            ..WebRtcConfig::default()
        };
        assert!(!format!("{config:?}").contains("super-secret-token"));
    }

    #[test]
    fn inline_codec_work_is_flagged_only_without_a_pool_and_with_many_calls() {
        let few = WebRtcConfig {
            max_calls: INLINE_CODEC_MAX_CALLS,
            ..WebRtcConfig::default()
        };
        let many = WebRtcConfig {
            max_calls: INLINE_CODEC_MAX_CALLS + 1,
            ..WebRtcConfig::default()
        };
        let inline = WebRtcOptions::default();
        assert!(!inline.inline_codec_is_risky(&few));
        assert!(inline.inline_codec_is_risky(&many));
        assert!(
            inline.inline_codec_is_risky(&WebRtcConfig::default()),
            "the default 256 calls with an inline codec is the risky setup"
        );
    }

    #[test]
    fn the_demo_page_has_the_whip_path_placeholder() {
        assert!(DEMO_HTML.contains("__WHIP_PATH__"));
        assert!(DEMO_HTML.contains("__WS_PATH__"));
        assert!(DEMO_HTML.contains("__MODULE_PATH__"));
        assert!(DEMO_HTML.contains("nonce=\"__NONCE__\""));
    }

    #[test]
    fn the_demo_module_is_the_shared_browser_client() {
        assert!(DEMO_MODULE_JS.contains("export class BrisvoxCall"));
        assert!(DEMO_MODULE_JS.contains("brisvox-playback"));
        assert_eq!(
            WebRtcConfig::default().demo_module_path().as_deref(),
            Some("/demo/brisvox-call.js")
        );
        let root = WebRtcConfig {
            demo_path: Some("/".to_owned()),
            ..WebRtcConfig::default()
        };
        assert_eq!(root.demo_module_path().as_deref(), Some("/brisvox-call.js"));
        assert_eq!(
            WebRtcConfig {
                demo_path: None,
                ..WebRtcConfig::default()
            }
            .demo_module_path(),
            None
        );
    }
}
