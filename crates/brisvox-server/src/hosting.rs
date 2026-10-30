//! The transports the server binary hosts (`docs/OPERATIONS.md`).
//!
//! [`HostSettings::from_env`] says which listeners to bind and which agent definition their
//! calls run; [`Listeners::bind`] binds them and runs one accept loop per listener.
//!
//! # Admission
//! Every listener admits through the process's [`SessionManager`]: its websocket server asks a
//! gate over [`SessionManager::try_admit`] right after the TCP accept, before the upgrade
//! ([`brisvox_transport::admission`]). The gate's ticket is the session's [`SessionSlot`]; it
//! travels with the call through the handshake and the protocol's start, and the accept loop
//! spawns the session into it ([`SessionSlot::spawn_with`]) with the [`AcceptedCall`]. So:
//!
//! - **Full** (`BRISVOX_MAX_SESSIONS` calls live or reserved, across all listeners): the
//!   connection is answered with HTTP 503 and `Retry-After: 1`, no upgrade (SIP: `503` with
//!   `Retry-After`).
//! - **Draining** (after the first shutdown signal): HTTP 503 without `Retry-After` (SIP: `503`
//!   without it); `/readyz` says `draining` at the same moment. Calls already admitted keep
//!   running, their connections untouched: the listeners stay bound through the drain and only
//!   refuse.
//! - Each websocket server keeps its own connection limit (`max_calls`) as a bound on
//!   sockets, at twice the manager's limit: a connection keeps its permit through its close
//!   handshake (up to `close_grace`) after its session ended, so a limit equal to the manager's
//!   would refuse calls the manager has room for. The manager's limit is the one that refuses.
//!   The SIP server's call limit is set the same way (capped at [`MAX_SIP_CALLS`]).
//!
//! # The generic websocket listener
//! The generic protocol (`docs/WEBSOCKET_PROTOCOL.md`) has no authentication of its own, and its
//! connections take the same session reservations as every other listener's. So it is never
//! exposed by default: unset, it listens on loopback ([`DEFAULT_WS_ADDR`]) when no other
//! listener is configured, and is off when one is (a Twilio deployment does not get a second,
//! unauthenticated door). Binding it to a public address takes an explicit `BRISVOX_WS_ADDR`
//! and credentials (next section).
//!
//! # Fail closed (`docs/SECURITY.md`)
//! The generic websocket, RTVI, WebRTC signaling and Telnyx listeners refuse to start on a
//! non-loopback address without authentication: a session secret (`BRISVOX_SESSION_SECRET`,
//! short-lived single-use tokens minted by the application's backend, [`brisvox_transport::auth`])
//! or a static token (`BRISVOX_WS_TOKEN`, `BRISVOX_RTVI_TOKEN`, `BRISVOX_WEBRTC_TOKEN`,
//! `BRISVOX_TELNYX_STREAM_TOKEN`). `BRISVOX_INSECURE_LISTENERS=1` accepts the risk, and is
//! logged loudly for every listener it opens. Loopback binds need nothing (development).
//! Twilio always verifies its signature; SIP has its own source ACL and digest rule.
//!
//! # Shutdown
//! The transports run under [`SessionManager::shutdown_token`]: the drain stage leaves them
//! alone, the cancel stage closes every connection (a live call gets the protocol's shutdown
//! message, a connection still in its handshake gives its reservation back). After the drain,
//! [`Listeners::stop`] ends the accept loops and shuts every transport down, bounded.
//!
//! # WebRTC
//! With the `webrtc` feature, `BRISVOX_WEBRTC_ADDR` hosts the browser transport: WHIP-style
//! HTTP signaling on that TCP address, media on one shared UDP socket
//! (`BRISVOX_WEBRTC_UDP_ADDR`, advertised as `BRISVOX_WEBRTC_CANDIDATE_IP`). Unlike the
//! websocket listeners it has no gate before the handshake: a call reaches the accept loop once
//! its ICE and DTLS completed and is admitted then, so a full server connects the browser and
//! hangs up. Its signaling needs a credential on a non-loopback address (see above): anyone who
//! reaches an open one runs the agent on your provider keys. It limits calls per client address,
//! calls still handshaking and offers per minute per address (`BRISVOX_WEBRTC_MAX_CALLS_PER_IP`,
//! `BRISVOX_WEBRTC_MAX_HANDSHAKING`, `BRISVOX_WEBRTC_OFFERS_PER_MINUTE`), and reads the client
//! address from `X-Forwarded-For` (or `Forwarded` or `X-Real-IP`, `BRISVOX_CLIENT_IP_HEADER`; exactly one) only
//! from the proxies in `BRISVOX_TRUSTED_PROXIES`, and caps the signaling connections one address
//! holds (`BRISVOX_WEBRTC_HTTP_MAX_PER_IP`). The same port also takes a WebSocket upgrade on
//! `BRISVOX_WEBRTC_WS_PATH` (default `/ws`, `off` to disable): the generic websocket
//! protocol over the signaling port, for browsers whose network passes only HTTP(S), with the
//! credentials and limits of `POST /whip`. Its Opus
//! codec runs on the inference pool (`HostSettings::with_codec_pool`; the binary does it).
//!
//! # SIP
//! With the `sip` feature, `BRISVOX_SIP_ADDR` hosts the SIP trunk endpoint: its INVITE
//! path asks the same gate (`SipServer::bind_gated`). An open SIP port is scanned within
//! minutes, so the listener needs `BRISVOX_SIP_ALLOWED_SOURCES` or digest credentials (or
//! both). With `BRISVOX_SIP_TLS_ADDR`, `BRISVOX_SIP_TLS_CERT` and `BRISVOX_SIP_TLS_KEY` it also
//! takes SIP over TLS, and `BRISVOX_SIP_SRTP` (`off` | `optional` | `required`, default
//! `optional` with TLS) decides about SDES-SRTP media, which is only ever negotiated on TLS
//! signaling (`docs/TELEPHONY.md` section 8). It is not a child of the manager's token (cancelling that would stop its signaling
//! endpoint at once, without a BYE): at the cancel stage its accept loop shuts it down
//! gracefully instead, which hangs up every call with a BYE (bounded) and drops the
//! reservations of INVITEs still waiting for their ACK.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use brisvox_core::{Secret, SharedClock};
#[cfg(any(feature = "sip", feature = "webrtc"))]
use brisvox_transport::Cidr;
use brisvox_transport::Transport;
use brisvox_transport::admission::{AdmissionGate, AdmissionTicket, Refusal, SharedGate};
use brisvox_transport::auth::{Audience, Authenticator, SessionSecrets};
use brisvox_transport::rtvi::{RtviConfig, RtviServer};
#[cfg(feature = "sip")]
use brisvox_transport::sip::{InboundAuth, SipConfig, SipServer, SipTlsConfig, SrtpPolicy};
use brisvox_transport::telnyx::{StreamStopper, TelnyxConfig, TelnyxServer};
use brisvox_transport::twilio::{StreamResumer, TwilioConfig, TwilioServer};
#[cfg(feature = "webrtc")]
use brisvox_transport::webrtc::{
    RelayConfig, TurnConfig, TurnProvider, WebRtcConfig, WebRtcOptions, WebRtcServer,
};
use brisvox_transport::websocket::{ServerConfig, WebSocketServer};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{self, AgentConfig, Env, Format};
use crate::runner::AcceptedCall;
use crate::session::{AdmissionError, SessionManager, SessionRunner, SessionSlot};
use crate::settings::{ENV_INSECURE_LISTENERS, addr_var, count_var, duration_var, flag_var, var};
use crate::{Error, Result};

/// Environment variable: the agent definition every hosted call runs (`docs/CONFIG.md`).
pub const ENV_AGENT_FILE: &str = "BRISVOX_AGENT_FILE";
/// Environment variable: the agent definition file used when neither [`ENV_AGENT_FILE`] nor
/// [`ENV_AGENT_DEFINITION`] is set. The container image sets it to `/etc/brisvox/agent.toml`, so a
/// file mounted there works without more, and `BRISVOX_AGENT_DEFINITION` alone works too (a
/// platform that cannot mount a file).
pub const ENV_DEFAULT_AGENT_FILE: &str = "BRISVOX_DEFAULT_AGENT_FILE";
/// Environment variable: an agent definition file for the calls of the generic websocket
/// listener only (`BRISVOX_WS_ADDR`); the other listeners run [`ENV_AGENT_FILE`]'s agent.
pub const ENV_WS_AGENT_FILE: &str = "BRISVOX_WS_AGENT_FILE";
/// Environment variable: the agent definition as text (TOML; YAML with [`ENV_AGENT_FORMAT`]),
/// for hosts that take variables but no file mount. The same loader, limits and messages as the
/// file (source name `$BRISVOX_AGENT_DEFINITION`); secrets still come from the variables the
/// definition names (`api_key_env`). Mutually exclusive with [`ENV_AGENT_FILE`].
pub const ENV_AGENT_DEFINITION: &str = "BRISVOX_AGENT_DEFINITION";
/// Environment variable: the syntax of [`ENV_AGENT_DEFINITION`], `toml` (default) or `yaml`.
/// Never guessed from the text.
pub const ENV_AGENT_FORMAT: &str = "BRISVOX_AGENT_FORMAT";
/// Environment variable: the generic websocket listener (`docs/WEBSOCKET_PROTOCOL.md`), or
/// `off`.
pub const ENV_WS_ADDR: &str = "BRISVOX_WS_ADDR";
/// Environment variable: the secret that signs session tokens (at least 32 bytes). With it the
/// generic websocket, RTVI and WebRTC listeners accept short-lived single-use tokens minted by
/// the application's backend (`docs/SECURITY.md`).
pub const ENV_SESSION_SECRET: &str = "BRISVOX_SESSION_SECRET";
/// Environment variable: the previous session secret, still accepted while old tokens expire
/// (rotation).
pub const ENV_SESSION_SECRET_PREV: &str = "BRISVOX_SESSION_SECRET_PREV";
/// Environment variable: static token of the generic websocket listener (development; sent as
/// `Authorization: Bearer`, the `brisvox-token.<token>` subprotocol or `?token=`).
pub const ENV_WS_TOKEN: &str = "BRISVOX_WS_TOKEN";
/// Environment variable: comma-separated CIDR blocks of the reverse proxies whose
/// `X-Forwarded-For` / `Forwarded` / `X-Real-IP` headers the WebRTC listener believes (`10.0.0.0/8,
/// 172.16.0.0/12`).
pub const ENV_TRUSTED_PROXIES: &str = "BRISVOX_TRUSTED_PROXIES";
/// Environment variable: which header a trusted proxy writes the client address into and the
/// only one believed, `x-forwarded-for` (default), `forwarded` or `x-real-ip` (one address; what
/// Railway's edge writes). `0.0.0.0/0` in
/// [`ENV_TRUSTED_PROXIES`] makes every client its own proxy (it can then choose the address it is
/// limited under): list the proxies' real addresses.
pub const ENV_CLIENT_IP_HEADER: &str = "BRISVOX_CLIENT_IP_HEADER";
/// Environment variable: signaling connections one source address (an IPv6 `/64`) may hold at
/// once, default 8, `0` for no limit; trusted proxies are exempt.
pub const ENV_WEBRTC_HTTP_MAX_PER_IP: &str = "BRISVOX_WEBRTC_HTTP_MAX_PER_IP";
/// Environment variable: the Twilio Media Streams listener; unset or `off` disables it.
pub const ENV_TWILIO_ADDR: &str = "BRISVOX_TWILIO_ADDR";
/// Environment variable: the Twilio account's Auth Token (required with the Twilio listener).
pub const ENV_TWILIO_AUTH_TOKEN: &str = "TWILIO_AUTH_TOKEN";
/// Environment variable: the stream URL as written in the TwiML (required with the Twilio
/// listener; the signature is computed over it).
pub const ENV_TWILIO_PUBLIC_URL: &str = "BRISVOX_TWILIO_PUBLIC_URL";
/// Environment variable: the Telnyx media streaming listener; unset or `off` disables it.
pub const ENV_TELNYX_ADDR: &str = "BRISVOX_TELNYX_ADDR";
/// Environment variable: the secret (at least 32 bytes) of per-call Twilio stream URLs
/// (`docs/providers/twilio.md`, "Per-call stream URLs"). Required with the Twilio listener unless
/// [`ENV_TWILIO_ALLOW_STATIC_URL`] is set.
pub const ENV_TWILIO_STREAM_SECRET: &str = "BRISVOX_TWILIO_STREAM_SECRET";

/// Environment variable: `1` accepts the fixed Twilio stream URL (the legacy mode whose
/// `X-Twilio-Signature` is one constant that can be replayed) instead of, or besides, per-call
/// URLs.
pub const ENV_TWILIO_ALLOW_STATIC_URL: &str = "BRISVOX_TWILIO_ALLOW_STATIC_URL";

/// Environment variable: the Telnyx `stream_auth_token`. Required for a Telnyx listener that is
/// bound to a non-loopback address (unless [`ENV_INSECURE_LISTENERS`] is `1`): Telnyx signs
/// nothing, so the path and this token are the only checks.
pub const ENV_TELNYX_TOKEN: &str = "BRISVOX_TELNYX_STREAM_TOKEN";
/// Environment variable: the RTVI (Pipecat client) listener; unset or `off` disables it.
pub const ENV_RTVI_ADDR: &str = "BRISVOX_RTVI_ADDR";
/// Environment variable: the token RTVI clients must send (`?token=`; optional).
pub const ENV_RTVI_TOKEN: &str = "BRISVOX_RTVI_TOKEN";
/// Environment variable: comma-separated `Origin`s RTVI browsers may connect from (optional;
/// unset allows any).
pub const ENV_RTVI_ORIGINS: &str = "BRISVOX_RTVI_ALLOWED_ORIGINS";

/// Environment variable: the SIP listener (feature `sip`); unset or `off` disables it.
pub const ENV_SIP_ADDR: &str = "BRISVOX_SIP_ADDR";
/// Environment variable: the address the SIP listener advertises in SDP, Contact and Via
/// (required when `BRISVOX_SIP_ADDR` is an unspecified address such as `0.0.0.0`).
pub const ENV_SIP_PUBLIC_IP: &str = "BRISVOX_SIP_PUBLIC_IP";
/// Environment variable: also take SIP over TCP (`true`/`false`, default `false`).
pub const ENV_SIP_TCP: &str = "BRISVOX_SIP_TCP";
/// Environment variable: the UDP port range for RTP, `low-high` (unset: the OS picks).
pub const ENV_SIP_RTP_PORTS: &str = "BRISVOX_SIP_RTP_PORTS";
/// Environment variable: comma-separated networks (`10.0.0.0/8`, `203.0.113.7`) allowed to send
/// SIP.
pub const ENV_SIP_ALLOWED_SOURCES: &str = "BRISVOX_SIP_ALLOWED_SOURCES";
/// Environment variable: the digest user inbound INVITEs must authenticate as.
pub const ENV_SIP_AUTH_USER: &str = "BRISVOX_SIP_AUTH_USER";
/// Environment variable: the digest password (required with `BRISVOX_SIP_AUTH_USER`).
pub const ENV_SIP_AUTH_PASSWORD: &str = "BRISVOX_SIP_AUTH_PASSWORD";
/// Environment variable: the digest realm (default `brisvox`).
pub const ENV_SIP_AUTH_REALM: &str = "BRISVOX_SIP_AUTH_REALM";
/// Environment variable: the SIPS listener (SIP over TLS, conventionally `0.0.0.0:5061`); unset
/// or `off` disables it. Needs `BRISVOX_SIP_TLS_CERT` and `BRISVOX_SIP_TLS_KEY`.
pub const ENV_SIP_TLS_ADDR: &str = "BRISVOX_SIP_TLS_ADDR";
/// Environment variable: path of the PEM certificate chain (leaf first) the SIPS listener presents.
pub const ENV_SIP_TLS_CERT: &str = "BRISVOX_SIP_TLS_CERT";
/// Environment variable: path of the PEM private key of that certificate. Read once at startup.
pub const ENV_SIP_TLS_KEY: &str = "BRISVOX_SIP_TLS_KEY";
/// Environment variable: path of a PEM CA bundle that outbound TLS calls verify the callee
/// against (optional; without it outbound `sips:` calls cannot be verified).
pub const ENV_SIP_TLS_CA: &str = "BRISVOX_SIP_TLS_CA";
/// Environment variable: TLS connections at once (default 1024).
pub const ENV_SIP_TLS_MAX_CONNECTIONS: &str = "BRISVOX_SIP_TLS_MAX_CONNECTIONS";
/// Environment variable: TLS connections at once from one address (default 32).
pub const ENV_SIP_TLS_MAX_PER_IP: &str = "BRISVOX_SIP_TLS_MAX_PER_IP";
/// Environment variable: the longest a client may take for the TLS handshake, in milliseconds
/// (default 10000).
pub const ENV_SIP_TLS_HANDSHAKE_MS: &str = "BRISVOX_SIP_TLS_HANDSHAKE_MS";
/// Environment variable: seconds an established SIPS connection may carry no traffic (a CRLF
/// keepalive counts) before it is closed (default 300; `0` turns the check off).
pub const ENV_SIP_TLS_IDLE_TIMEOUT: &str = "BRISVOX_SIP_TLS_IDLE_TIMEOUT";
/// Environment variable: SRTP authentication failures per second and call that end the call
/// (default 50; `0` turns the limit off).
pub const ENV_SIP_SRTP_AUTH_FAIL_LIMIT: &str = "BRISVOX_SIP_SRTP_AUTH_FAIL_LIMIT";
/// Environment variable: SDES-SRTP policy, `off` | `optional` | `required` (default `optional`
/// with the SIPS listener, `off` without; anything but `off` needs the SIPS listener).
pub const ENV_SIP_SRTP: &str = "BRISVOX_SIP_SRTP";

/// Environment variable: the WebRTC signaling listener (feature `webrtc`; WHIP-style HTTP
/// `POST`, `docs/providers/webrtc.md`); unset or `off` disables it.
pub const ENV_WEBRTC_ADDR: &str = "BRISVOX_WEBRTC_ADDR";
/// Environment variable: the UDP socket every WebRTC call's media shares (default: the
/// signaling address's IP and port number).
pub const ENV_WEBRTC_UDP_ADDR: &str = "BRISVOX_WEBRTC_UDP_ADDR";
/// Environment variable: the IP address advertised to browsers as the server's media address
/// (required when the UDP socket is bound to an unspecified address such as `0.0.0.0`; behind
/// NAT or in a container, the address browsers can reach).
pub const ENV_WEBRTC_CANDIDATE_IP: &str = "BRISVOX_WEBRTC_CANDIDATE_IP";
/// Environment variable: the bearer token `POST`/`DELETE` of the signaling endpoint require
/// (optional; RFC 9725 section 4.7).
pub const ENV_WEBRTC_TOKEN: &str = "BRISVOX_WEBRTC_TOKEN";
/// Environment variable: calls one client address may hold at once, or `off` (default 8).
pub const ENV_WEBRTC_MAX_CALLS_PER_IP: &str = "BRISVOX_WEBRTC_MAX_CALLS_PER_IP";
/// Environment variable: calls that may be in their ICE/DTLS handshake at once (default: a
/// quarter of `BRISVOX_MAX_SESSIONS`).
pub const ENV_WEBRTC_MAX_HANDSHAKING: &str = "BRISVOX_WEBRTC_MAX_HANDSHAKING";
/// Environment variable: offers one client address may make per minute, or `off` (default 5).
pub const ENV_WEBRTC_OFFERS_PER_MINUTE: &str = "BRISVOX_WEBRTC_OFFERS_PER_MINUTE";
/// Environment variable: the `Origin` of pages allowed to call the signaling endpoint from
/// another origin (`Access-Control-Allow-Origin`; `*` or one origin; unset: same origin only).
pub const ENV_WEBRTC_CORS_ORIGIN: &str = "BRISVOX_WEBRTC_CORS_ORIGIN";
/// Environment variable: the ICE-TCP listener: a passive TCP candidate next to the UDP
/// one, for browsers whose network blocks UDP and hosts with a TCP port but no UDP. Unset or
/// `off`: no TCP candidate. Needs `BRISVOX_WEBRTC_CANDIDATE_IP` when bound to an unspecified
/// address.
pub const ENV_WEBRTC_TCP_ADDR: &str = "BRISVOX_WEBRTC_TCP_ADDR";
/// Environment variable: the port advertised in the TCP candidate when it differs from the
/// listener's (a TCP proxy or port mapping in front of the server).
pub const ENV_WEBRTC_TCP_CANDIDATE_PORT: &str = "BRISVOX_WEBRTC_TCP_CANDIDATE_PORT";
/// Environment variable: ICE-TCP connections served at once (1 to 65536; default 256).
pub const ENV_WEBRTC_TCP_MAX_CONNECTIONS: &str = "BRISVOX_WEBRTC_TCP_MAX_CONNECTIONS";
/// Environment variable: ICE-TCP connections one source IP may hold (0 = no limit, for a TCP
/// proxy in front of the server; default 16).
pub const ENV_WEBRTC_TCP_MAX_PER_IP: &str = "BRISVOX_WEBRTC_TCP_MAX_PER_IP";
/// Environment variable: how long an ICE-TCP connection may take to name a live call before it is
/// closed (default 10s).
pub const ENV_WEBRTC_TCP_IDLE_TIMEOUT: &str = "BRISVOX_WEBRTC_TCP_IDLE_TIMEOUT";
/// Environment variable: largest ICE-TCP frame in bytes (512 to 65535; default 2048).
pub const ENV_WEBRTC_TCP_MAX_FRAME: &str = "BRISVOX_WEBRTC_TCP_MAX_FRAME";
/// Environment variable: frames queued per ICE-TCP connection towards the browser (8 to 4096;
/// default 20).
pub const ENV_WEBRTC_TCP_QUEUE: &str = "BRISVOX_WEBRTC_TCP_QUEUE";
/// Environment variable: where browsers get TURN credentials from: `coturn`, `cloudflare` or
/// `twilio`; unset or `off`: no relay offered.
pub const ENV_TURN_PROVIDER: &str = "BRISVOX_TURN_PROVIDER";
/// Environment variable (`coturn`): comma-separated `turn:` / `turns:` URLs of the coturn server.
pub const ENV_TURN_URLS: &str = "BRISVOX_TURN_URLS";
/// Environment variable (`coturn`): the server's `static-auth-secret` (never sent to browsers).
pub const ENV_TURN_SECRET: &str = "BRISVOX_TURN_SECRET";
/// Environment variable: the WebSocket fallback of the WebRTC signaling listener: the
/// path on the signaling port where a WebSocket upgrade serves the generic WebSocket audio
/// protocol, with the same credentials, `Origin` rule and limits as `POST /whip`. Default
/// `/ws`; `off` turns it off (the path is then 404). The bundled pages fall back to it when
/// WebRTC media does not connect.
pub const ENV_WEBRTC_WS_PATH: &str = "BRISVOX_WEBRTC_WS_PATH";
/// Environment variable: the server's own TURN relay: `on`, `only` or `off` (default).
/// Every call then also advertises a TURN relayed address (allocated over TCP/TLS with the
/// `BRISVOX_TURN_*` provider and credentials), so a host without UDP ingress (Railway) completes
/// calls. `only`: no direct UDP candidate at all (`BRISVOX_WEBRTC_CANDIDATE_IP` is not needed,
/// and a failed allocation refuses the offer with 503); `on`: next to the direct candidates, as
/// the fallback. Needs `BRISVOX_TURN_PROVIDER`.
pub const ENV_WEBRTC_TURN_RELAY: &str = "BRISVOX_WEBRTC_TURN_RELAY";
/// Environment variable: longest the relay setup of one call may take (credentials, DNS, TLS,
/// allocate, permissions) before the call is answered without it (default 3s).
pub const ENV_WEBRTC_TURN_RELAY_TIMEOUT: &str = "BRISVOX_WEBRTC_TURN_RELAY_TIMEOUT";
/// Environment variable: lifetime of issued TURN credentials, in seconds (30 to 86400; default
/// 600).
pub const ENV_TURN_TTL: &str = "BRISVOX_TURN_TTL";
/// Environment variable (`cloudflare`): the TURN key id.
pub const ENV_TURN_CLOUDFLARE_KEY_ID: &str = "BRISVOX_TURN_CLOUDFLARE_KEY_ID";
/// Environment variable (`cloudflare`): the TURN key API token.
pub const ENV_TURN_CLOUDFLARE_API_TOKEN: &str = "BRISVOX_TURN_CLOUDFLARE_API_TOKEN";
/// Environment variable (`twilio`): the account SID.
pub const ENV_TURN_TWILIO_ACCOUNT_SID: &str = "BRISVOX_TURN_TWILIO_ACCOUNT_SID";
/// Environment variable (`twilio`): the account auth token, or the API key secret when
/// `BRISVOX_TURN_TWILIO_API_KEY_SID` is set.
pub const ENV_TURN_TWILIO_AUTH_TOKEN: &str = "BRISVOX_TURN_TWILIO_AUTH_TOKEN";
/// Environment variable (`twilio`, optional): an API key SID to authenticate with instead of the
/// account SID.
pub const ENV_TURN_TWILIO_API_KEY_SID: &str = "BRISVOX_TURN_TWILIO_API_KEY_SID";

/// Environment variable: the Twilio listener's websocket path (default `/twilio/stream`).
pub const ENV_TWILIO_PATH: &str = "BRISVOX_TWILIO_PATH";
/// Environment variable: the Telnyx listener's websocket path (default `/telnyx/stream`).
pub const ENV_TELNYX_PATH: &str = "BRISVOX_TELNYX_PATH";
/// Environment variable: the RTVI listener's websocket path (default `/rtvi`).
pub const ENV_RTVI_PATH: &str = "BRISVOX_RTVI_PATH";

/// Environment variable: TCP accept to the end of the websocket upgrade, every listener
/// ([`ServerConfig::handshake_timeout`]).
pub const ENV_LISTENER_HANDSHAKE_TIMEOUT: &str = "BRISVOX_LISTENER_HANDSHAKE_TIMEOUT";
/// Environment variable: upgrade to the protocol's start message
/// ([`ServerConfig::start_timeout`]).
pub const ENV_LISTENER_START_TIMEOUT: &str = "BRISVOX_LISTENER_START_TIMEOUT";
/// Environment variable: interval of the server's pings ([`ServerConfig::ping_interval`]).
pub const ENV_LISTENER_PING_INTERVAL: &str = "BRISVOX_LISTENER_PING_INTERVAL";
/// Environment variable: silence from the client that ends the call
/// ([`ServerConfig::ping_timeout`]).
pub const ENV_LISTENER_PING_TIMEOUT: &str = "BRISVOX_LISTENER_PING_TIMEOUT";
/// Environment variable: longest single write to the client ([`ServerConfig::write_timeout`]).
pub const ENV_LISTENER_WRITE_TIMEOUT: &str = "BRISVOX_LISTENER_WRITE_TIMEOUT";
/// Environment variable: longest wait for the engine's queues ([`ServerConfig::send_timeout`]).
pub const ENV_LISTENER_SEND_TIMEOUT: &str = "BRISVOX_LISTENER_SEND_TIMEOUT";
/// Environment variable: the close handshake's grace ([`ServerConfig::close_grace`]).
pub const ENV_LISTENER_CLOSE_GRACE: &str = "BRISVOX_LISTENER_CLOSE_GRACE";
/// Environment variable: calls waiting to be accepted ([`ServerConfig::accept_queue`]).
pub const ENV_LISTENER_ACCEPT_QUEUE: &str = "BRISVOX_LISTENER_ACCEPT_QUEUE";
/// Environment variable: largest binary frame accepted, bytes
/// ([`ServerConfig::max_audio_bytes`]).
pub const ENV_LISTENER_MAX_AUDIO_BYTES: &str = "BRISVOX_LISTENER_MAX_AUDIO_BYTES";
/// Environment variable: largest text frame accepted, bytes
/// ([`ServerConfig::max_control_bytes`]).
pub const ENV_LISTENER_MAX_CONTROL_BYTES: &str = "BRISVOX_LISTENER_MAX_CONTROL_BYTES";

/// Largest frame size the `BRISVOX_LISTENER_MAX_*_BYTES` variables accept (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// `BRISVOX_LISTENER_*` over the default [`ServerConfig`], checked by
/// [`ServerConfig::validate`] (its `bind_addr` and `max_calls` are placeholders here).
fn listener_tunables(env: &dyn Env) -> Result<ServerConfig> {
    let mut config = ServerConfig::default();
    let durations: [(&str, &mut Duration); 7] = [
        (
            ENV_LISTENER_HANDSHAKE_TIMEOUT,
            &mut config.handshake_timeout,
        ),
        (ENV_LISTENER_START_TIMEOUT, &mut config.start_timeout),
        (ENV_LISTENER_PING_INTERVAL, &mut config.ping_interval),
        (ENV_LISTENER_PING_TIMEOUT, &mut config.ping_timeout),
        (ENV_LISTENER_WRITE_TIMEOUT, &mut config.write_timeout),
        (ENV_LISTENER_SEND_TIMEOUT, &mut config.send_timeout),
        (ENV_LISTENER_CLOSE_GRACE, &mut config.close_grace),
    ];
    for (name, slot) in durations {
        if let Some(d) = duration_var(env, name)? {
            *slot = d;
        }
    }
    if let Some(n) = count_var(
        env,
        ENV_LISTENER_ACCEPT_QUEUE,
        1,
        brisvox_transport::websocket::MAX_CALLS,
    )? {
        config.accept_queue = n;
    }
    if let Some(n) = count_var(env, ENV_LISTENER_MAX_AUDIO_BYTES, 2, MAX_FRAME_BYTES)? {
        config.max_audio_bytes = n;
    }
    if let Some(n) = count_var(env, ENV_LISTENER_MAX_CONTROL_BYTES, 512, MAX_FRAME_BYTES)? {
        config.max_control_bytes = n;
    }
    config
        .validate()
        .map_err(|e| Error::Config(format!("BRISVOX_LISTENER_*: {e}")))?;
    Ok(config)
}

/// Default generic websocket listener, used when no other listener is configured: loopback
/// only, port 8080 (the health listener is on 8081). See the [module docs](self).
pub const DEFAULT_WS_ADDR: &str = "127.0.0.1:8080";

/// Most calls the SIP server takes at once (the largest `max_calls` its configuration accepts).
pub const MAX_SIP_CALLS: usize = 100_000;

/// How long each transport gets to shut down after the drain.
pub const LISTENER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// One listener: where it binds and its protocol settings.
#[derive(Debug, Clone)]
pub struct Listener<C> {
    /// Address to bind; port 0 picks a free port ([`Listeners::addrs`]).
    pub addr: SocketAddr,
    /// The protocol's settings.
    pub config: C,
}

/// The WebRTC listener's settings: the transport's configuration, and the inference pool its
/// Opus codec work runs on (set by [`HostSettings::with_codec_pool`]; without one the codec runs
/// on the call tasks, which the transport warns about for more than a few calls).
#[cfg(feature = "webrtc")]
#[derive(Clone)]
pub struct WebRtcHost {
    /// The transport's configuration (`http_addr` and `udp_addr` are what gets bound).
    pub config: WebRtcConfig,
    /// Where Opus encode/decode jobs run (`None`: inline).
    pub codec_pool: Option<brisvox_inference::PoolHandle>,
}

#[cfg(feature = "webrtc")]
impl std::fmt::Debug for WebRtcHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcHost")
            .field("config", &self.config)
            .field("codec_pool", &self.codec_pool.is_some())
            .finish()
    }
}

/// The links from the carriers' call control to the listeners that stream the same calls. The call control is built before the listeners are bound and the listeners' hooks
/// exist only after, so the links are cells that [`Listeners::bind`] fills and the hooks read
/// when they run.
///
/// - Twilio: a REST update that ends the stream (DTMF, a warm transfer) is announced to the
///   Twilio listener ([`StreamResumer`]), which keeps the call's session for the new stream.
/// - Telnyx: the `streaming_stop` before a transfer retry is announced to the Telnyx listener
///   ([`StreamStopper`]), so the stream's close is not read as a hang-up.
///
/// Clones share the cells. An unlinked value (no listener bound) makes the announcements no-ops.
#[derive(Debug, Clone, Default)]
pub struct StreamLinks {
    twilio: Arc<OnceLock<StreamResumer>>,
    telnyx: Arc<OnceLock<StreamStopper>>,
    /// Where Twilio is told to open the new stream (`wss://`), when the server hosts the
    /// Twilio listener.
    twilio_stream_url: Option<String>,
    /// The secret of per-call Twilio stream URLs, when the listener uses them: the streams the
    /// server restarts itself (DTMF, a failed warm transfer) get a signed per-call URL.
    twilio_stream_secret: Option<Secret<String>>,
}

impl StreamLinks {
    /// The Twilio listener's stream announcer, once the listener is bound.
    pub fn twilio_resumer(&self) -> Option<&StreamResumer> {
        self.twilio.get()
    }

    /// The Telnyx listener's stream announcer, once the listener is bound.
    pub fn telnyx_stopper(&self) -> Option<&StreamStopper> {
        self.telnyx.get()
    }

    /// The `wss://` URL a resumed Twilio stream connects to (the listener's public URL), when
    /// this server hosts the Twilio listener.
    pub fn twilio_stream_url(&self) -> Option<&str> {
        self.twilio_stream_url.as_deref()
    }

    /// The secret that signs the per-call URLs of resumed Twilio streams, when the listener has
    /// one ([`brisvox_transport::twilio::TwilioConfig::stream_secret`]).
    pub fn twilio_stream_secret(&self) -> Option<&Secret<String>> {
        self.twilio_stream_secret.as_ref()
    }
}

/// The public URL of the Twilio listener as the TwiML's `<Stream url>` needs it: `wss://`.
fn stream_url(public_url: &str) -> String {
    if let Some(rest) = public_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = public_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        public_url.to_owned()
    }
}

/// An agent definition held as text: `BRISVOX_AGENT_DEFINITION` and `BRISVOX_AGENT_FORMAT`.
#[derive(Clone, PartialEq, Eq)]
pub struct InlineAgent {
    /// The definition exactly as written in the variable.
    pub text: String,
    /// Its syntax.
    pub format: Format,
}

impl std::fmt::Debug for InlineAgent {
    /// Size and syntax only: the text can be long and holds the system prompt.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineAgent")
            .field("format", &self.format)
            .field("bytes", &self.text.len())
            .finish()
    }
}

/// `BRISVOX_AGENT_DEFINITION` (with `BRISVOX_AGENT_FORMAT`), `None` when the definition is unset
/// or blank.
///
/// # Errors
/// [`Error::Config`] when the value is not UTF-8, the format is not `toml` or `yaml`, or a format
/// is given without a definition.
pub(crate) fn inline_agent(env: &dyn Env) -> Result<Option<InlineAgent>> {
    let format = match var(env, ENV_AGENT_FORMAT)? {
        None => None,
        Some(name) => Some(Format::from_name(&name).ok_or_else(|| {
            Error::Config(format!("{ENV_AGENT_FORMAT}: {name:?} is not toml or yaml"))
        })?),
    };
    // Not trimmed: the first line's indentation is part of a YAML text.
    let text = match env.var(ENV_AGENT_DEFINITION) {
        Ok(text) if !text.trim().is_empty() => Some(text),
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(Error::Config(format!(
                "{ENV_AGENT_DEFINITION} is not valid unicode"
            )));
        }
    };
    match (text, format) {
        (Some(text), format) => Ok(Some(InlineAgent {
            text,
            format: format.unwrap_or_default(),
        })),
        (None, Some(_)) => Err(Error::Config(format!(
            "{ENV_AGENT_FORMAT} is set, but {ENV_AGENT_DEFINITION} is not (the format only applies to the inline definition; a file's format follows its extension)"
        ))),
        (None, None) => Ok(None),
    }
}

/// Which transports the server hosts and what their calls run.
#[derive(Debug, Clone)]
pub struct HostSettings {
    /// The agent definition (`BRISVOX_AGENT_FILE`); one of this and [`Self::agent_inline`] is
    /// required when any listener is on.
    pub agent_file: Option<PathBuf>,
    /// The agent definition as text (`BRISVOX_AGENT_DEFINITION`); never together with
    /// [`Self::agent_file`].
    pub agent_inline: Option<InlineAgent>,
    /// The agent of the generic websocket listener's calls (`BRISVOX_WS_AGENT_FILE`), when it
    /// differs from the main one: one process then serves two agents on one inference pool,
    /// one Smart Turn executor and one admission control. The websocket agent gets no
    /// application or telephony tools (those are bound to the main agent).
    pub websocket_agent_file: Option<PathBuf>,
    /// The generic websocket listener (default [`DEFAULT_WS_ADDR`] when no other listener is
    /// on, off otherwise; see the [module docs](self)).
    pub websocket: Option<SocketAddr>,
    /// The Twilio listener (off by default).
    pub twilio: Option<Listener<TwilioConfig>>,
    /// The Telnyx listener (off by default).
    pub telnyx: Option<Listener<TelnyxConfig>>,
    /// The RTVI listener (off by default).
    pub rtvi: Option<Listener<RtviConfig>>,
    /// Session-token secrets (`BRISVOX_SESSION_SECRET`): applied by [`HostSettings::from_env`] to
    /// the generic websocket, RTVI and WebRTC listeners (set the listeners' own `sessions` when
    /// building settings by hand). Used here for the generic websocket listener.
    pub session_secrets: Option<SessionSecrets>,
    /// Static token of the generic websocket listener (`BRISVOX_WS_TOKEN`).
    pub ws_token: Option<Secret<String>>,
    /// Accept listeners without authentication on non-loopback addresses
    /// (`BRISVOX_INSECURE_LISTENERS`). Logged loudly at bind.
    pub insecure_listeners: bool,
    /// The SIP listener (feature `sip`; off by default). Its `max_calls` is set by
    /// [`Listeners::bind`].
    #[cfg(feature = "sip")]
    pub sip: Option<Listener<SipConfig>>,
    /// The WebRTC listener (feature `webrtc`; off by default). Its `max_calls` is set by
    /// [`Listeners::bind`].
    #[cfg(feature = "webrtc")]
    pub webrtc: Option<Listener<WebRtcHost>>,
    /// Timeouts and limits of every websocket-based listener (generic, Twilio, Telnyx, RTVI;
    /// `BRISVOX_LISTENER_*`). Its `bind_addr` and `max_calls` are set per listener by
    /// [`Listeners::bind`].
    pub server: ServerConfig,
    /// The cells [`Listeners::bind`] fills; read them through [`Self::stream_links`] (which also
    /// knows the Twilio stream URL). Keep the default.
    pub links: StreamLinks,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            agent_file: None,
            agent_inline: None,
            websocket_agent_file: None,
            websocket: DEFAULT_WS_ADDR.parse().ok(),
            twilio: None,
            telnyx: None,
            rtvi: None,
            session_secrets: None,
            ws_token: None,
            insecure_listeners: false,
            #[cfg(feature = "sip")]
            sip: None,
            #[cfg(feature = "webrtc")]
            webrtc: None,
            links: StreamLinks::default(),
        }
    }
}

/// `name` as an address, `None` for `off` (or unset with no `default`).
fn listen_addr(
    env: &dyn Env,
    name: &str,
    default: Option<SocketAddr>,
) -> Result<Option<SocketAddr>> {
    match addr_var(env, name)? {
        None => Ok(default),
        Some(v) if v.eq_ignore_ascii_case("off") => Ok(None),
        Some(v) => v.parse::<SocketAddr>().map(Some).map_err(|_| {
            Error::Config(format!(
                "{name}: {v:?} is not an address like 0.0.0.0:8080 (or `off`)"
            ))
        }),
    }
}

/// `BRISVOX_WEBRTC_*` as the WebRTC listener's settings; `None` when `BRISVOX_WEBRTC_ADDR` is
/// unset or `off`. The codec pool is attached later ([`HostSettings::with_codec_pool`]).
#[cfg(feature = "webrtc")]
fn webrtc_listener(
    env: &dyn Env,
    sessions: Option<&SessionSecrets>,
) -> Result<Option<Listener<WebRtcHost>>> {
    let Some(addr) = listen_addr(env, ENV_WEBRTC_ADDR, None)? else {
        return Ok(None);
    };
    let udp_addr = match addr_var(env, ENV_WEBRTC_UDP_ADDR)? {
        None => addr,
        Some(v) => v.parse::<SocketAddr>().map_err(|_| {
            Error::Config(format!(
                "{ENV_WEBRTC_UDP_ADDR}: {v:?} is not an address like 0.0.0.0:8088"
            ))
        })?,
    };
    let candidate_ip = var(env, ENV_WEBRTC_CANDIDATE_IP)?
        .map(|v| {
            v.parse::<std::net::IpAddr>().map_err(|_| {
                Error::Config(format!(
                    "{ENV_WEBRTC_CANDIDATE_IP}: {v:?} is not an IP address"
                ))
            })
        })
        .transpose()?;
    let turn = turn_config(env)?;
    let turn_relay = turn_relay_config(env, turn.is_some())?;
    let relay_only = turn_relay.as_ref().is_some_and(|relay| relay.only);
    if udp_addr.ip().is_unspecified() && candidate_ip.is_none() && !relay_only {
        return Err(Error::Config(format!(
            "{ENV_WEBRTC_CANDIDATE_IP} is required when the media socket ({udp_addr}) is bound to an unspecified address: browsers need an address to send media to"
        )));
    }
    let tcp_addr = listen_addr(env, ENV_WEBRTC_TCP_ADDR, None)?;
    if tcp_addr.is_some_and(|a| a.ip().is_unspecified()) && candidate_ip.is_none() {
        return Err(Error::Config(format!(
            "{ENV_WEBRTC_CANDIDATE_IP} is required when the ICE-TCP listener is bound to an unspecified address: browsers need an address to connect to"
        )));
    }
    let tcp_candidate_port = match var(env, ENV_WEBRTC_TCP_CANDIDATE_PORT)? {
        None => None,
        Some(v) => Some(v.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| {
            Error::Config(format!(
                "{ENV_WEBRTC_TCP_CANDIDATE_PORT}: {v:?} is not a port from 1 to 65535"
            ))
        })?),
    };
    let defaults = WebRtcConfig::default();
    let mut config = WebRtcConfig {
        http_addr: addr,
        udp_addr,
        candidate_ip,
        tcp_addr,
        tcp_candidate_port,
        turn,
        turn_relay,
        bearer_token: var(env, ENV_WEBRTC_TOKEN)?.map(Secret::new),
        sessions: sessions.cloned(),
        cors_origin: var(env, ENV_WEBRTC_CORS_ORIGIN)?,
        ws_path: match var(env, ENV_WEBRTC_WS_PATH)? {
            None => defaults.ws_path.clone(),
            Some(v) if v.eq_ignore_ascii_case("off") => None,
            Some(v) => Some(v),
        },
        trusted_proxies: trusted_proxies(env)?,
        client_ip_header: client_ip_header(env)?,
        max_http_connections_per_ip: count_var(env, ENV_WEBRTC_HTTP_MAX_PER_IP, 0, 1_000_000)?
            .unwrap_or(defaults.max_http_connections_per_ip),
        max_calls_per_ip: limit_var(env, ENV_WEBRTC_MAX_CALLS_PER_IP, 1, 1_000_000)?
            .unwrap_or(defaults.max_calls_per_ip),
        max_handshaking: count_var(env, ENV_WEBRTC_MAX_HANDSHAKING, 1, 1_000_000)?
            .or(defaults.max_handshaking),
        offers_per_minute_per_ip: limit_var(env, ENV_WEBRTC_OFFERS_PER_MINUTE, 1, 100_000)?
            .map_or(defaults.offers_per_minute_per_ip, |limit| {
                limit.and_then(|n| u32::try_from(n).ok())
            }),
        ..defaults
    };
    if let Some(n) = count_var(env, ENV_WEBRTC_TCP_MAX_CONNECTIONS, 1, 65_536)? {
        config.max_tcp_connections = n;
    }
    if let Some(n) = count_var(env, ENV_WEBRTC_TCP_MAX_PER_IP, 0, 65_536)? {
        config.max_tcp_connections_per_ip = n;
    }
    if let Some(d) = duration_var(env, ENV_WEBRTC_TCP_IDLE_TIMEOUT)? {
        config.tcp_idle_timeout = d;
    }
    if let Some(n) = count_var(env, ENV_WEBRTC_TCP_MAX_FRAME, 512, 65_535)? {
        config.tcp_max_frame = n;
    }
    if let Some(n) = count_var(env, ENV_WEBRTC_TCP_QUEUE, 8, 4_096)? {
        config.tcp_queue = n;
    }
    config
        .validate()
        .map_err(|e| Error::Config(format!("{ENV_WEBRTC_ADDR}: {e}")))?;
    Ok(Some(Listener {
        addr,
        config: WebRtcHost {
            config,
            codec_pool: None,
        },
    }))
}

/// `BRISVOX_WEBRTC_TURN_RELAY*` as the server-side TURN client settings; `None` when off.
#[cfg(feature = "webrtc")]
fn turn_relay_config(env: &dyn Env, have_provider: bool) -> Result<Option<RelayConfig>> {
    let Some(mode) = var(env, ENV_WEBRTC_TURN_RELAY)? else {
        return Ok(None);
    };
    let only = match mode.to_ascii_lowercase().as_str() {
        "off" | "false" | "0" | "no" => return Ok(None),
        "on" | "true" | "1" | "yes" => false,
        "only" => true,
        other => {
            return Err(Error::Config(format!(
                "{ENV_WEBRTC_TURN_RELAY}: {other:?} is not on, only or off"
            )));
        }
    };
    if !have_provider {
        return Err(Error::Config(format!(
            "{ENV_WEBRTC_TURN_RELAY} needs {ENV_TURN_PROVIDER} (the relay uses its credentials)"
        )));
    }
    let mut config = RelayConfig {
        only,
        ..RelayConfig::default()
    };
    if let Some(d) = duration_var(env, ENV_WEBRTC_TURN_RELAY_TIMEOUT)? {
        config.setup_timeout = d;
    }
    config
        .validate()
        .map_err(|e| Error::Config(format!("{ENV_WEBRTC_TURN_RELAY}: {e}")))?;
    Ok(Some(config))
}

/// `BRISVOX_TURN_*` as the browser TURN credential settings; `None` when `BRISVOX_TURN_PROVIDER`
/// is unset or `off`.
#[cfg(feature = "webrtc")]
fn turn_config(env: &dyn Env) -> Result<Option<TurnConfig>> {
    let Some(provider) = var(env, ENV_TURN_PROVIDER)? else {
        return Ok(None);
    };
    let need = |name: &str| {
        var(env, name)?.ok_or_else(|| {
            Error::Config(format!(
                "{name} is required when {ENV_TURN_PROVIDER} is {provider:?}"
            ))
        })
    };
    let provider = match provider.to_ascii_lowercase().as_str() {
        "off" => return Ok(None),
        "coturn" => TurnProvider::Coturn {
            urls: need(ENV_TURN_URLS)?
                .split(',')
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned)
                .collect(),
            secret: Secret::new(need(ENV_TURN_SECRET)?),
        },
        "cloudflare" => TurnProvider::Cloudflare {
            key_id: need(ENV_TURN_CLOUDFLARE_KEY_ID)?,
            api_token: Secret::new(need(ENV_TURN_CLOUDFLARE_API_TOKEN)?),
            base_url: brisvox_transport::webrtc::turn_creds::CLOUDFLARE_BASE.to_owned(),
        },
        "twilio" => {
            let account_sid = need(ENV_TURN_TWILIO_ACCOUNT_SID)?;
            TurnProvider::Twilio {
                auth_user: var(env, ENV_TURN_TWILIO_API_KEY_SID)?
                    .unwrap_or_else(|| account_sid.clone()),
                account_sid,
                auth_secret: Secret::new(need(ENV_TURN_TWILIO_AUTH_TOKEN)?),
                base_url: brisvox_transport::webrtc::turn_creds::TWILIO_BASE.to_owned(),
            }
        }
        other => {
            return Err(Error::Config(format!(
                "{ENV_TURN_PROVIDER}: {other:?} is not coturn, cloudflare or twilio (or `off`)"
            )));
        }
    };
    let mut config = TurnConfig::new(provider);
    if let Some(seconds) = count_var(env, ENV_TURN_TTL, 30, 86_400)? {
        config.ttl = Duration::from_secs(u64::try_from(seconds).unwrap_or(600));
    }
    config
        .validate()
        .map_err(|e| Error::Config(format!("{ENV_TURN_PROVIDER}: {e}")))?;
    Ok(Some(config))
}

/// `name` as a limit: `None` when unset (keep the default), `Some(None)` for `off`,
/// `Some(Some(n))` for a whole number in `min..=max`.
#[cfg(feature = "webrtc")]
fn limit_var(env: &dyn Env, name: &str, min: usize, max: usize) -> Result<Option<Option<usize>>> {
    match var(env, name)? {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("off") => Ok(Some(None)),
        Some(_) => count_var(env, name, min, max).map(Some),
    }
}

/// `BRISVOX_CLIENT_IP_HEADER`: `x-forwarded-for` (default), `forwarded` or `x-real-ip`.
#[cfg(feature = "webrtc")]
fn client_ip_header(env: &dyn Env) -> Result<brisvox_transport::webrtc::ClientIpHeader> {
    let Some(text) = var(env, ENV_CLIENT_IP_HEADER)? else {
        return Ok(brisvox_transport::webrtc::ClientIpHeader::default());
    };
    brisvox_transport::webrtc::ClientIpHeader::parse(&text).ok_or_else(|| {
        Error::Config(format!(
            "{ENV_CLIENT_IP_HEADER}: {text:?} is not x-forwarded-for, forwarded or x-real-ip"
        ))
    })
}

/// `BRISVOX_TRUSTED_PROXIES`: comma-separated CIDR blocks (empty when unset).
#[cfg(feature = "webrtc")]
fn trusted_proxies(env: &dyn Env) -> Result<Vec<Cidr>> {
    let Some(list) = var(env, ENV_TRUSTED_PROXIES)? else {
        return Ok(Vec::new());
    };
    list.split(',')
        .map(str::trim)
        .filter(|net| !net.is_empty())
        .map(|net| {
            net.parse::<Cidr>().map_err(|_| {
                Error::Config(format!(
                    "{ENV_TRUSTED_PROXIES}: {net:?} is not an IP address or CIDR block"
                ))
            })
        })
        .collect()
}

/// The session secrets of `BRISVOX_SESSION_SECRET` (and `_PREV`); `None` when unset. The error
/// names the variable and never echoes a value.
fn session_secrets(env: &dyn Env) -> Result<Option<SessionSecrets>> {
    let current = var(env, ENV_SESSION_SECRET)?;
    let previous = var(env, ENV_SESSION_SECRET_PREV)?;
    match (current, previous) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(Error::Config(format!(
            "{ENV_SESSION_SECRET_PREV} is set without {ENV_SESSION_SECRET}"
        ))),
        (Some(current), previous) => {
            SessionSecrets::new(Secret::new(current), previous.map(Secret::new))
                .map(Some)
                .map_err(|e| {
                    Error::Config(format!(
                        "{ENV_SESSION_SECRET}/{ENV_SESSION_SECRET_PREV}: {e}"
                    ))
                })
        }
    }
}

/// Largest certificate, key or CA file read, in bytes.
#[cfg(feature = "sip")]
const MAX_PEM_FILE_BYTES: u64 = 256 * 1024;

/// The PEM file at `path`, read once. The error names the variable and the path, never content.
#[cfg(feature = "sip")]
fn read_pem(name: &str, path: &str) -> Result<String> {
    use std::io::Read;

    let fail = |detail: String| Error::Config(format!("{name} {path:?}: {detail}"));
    let file = std::fs::File::open(path).map_err(|e| fail(e.to_string()))?;
    let mut text = String::new();
    file.take(MAX_PEM_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| fail(e.to_string()))?;
    if text.len() as u64 > MAX_PEM_FILE_BYTES {
        return Err(fail(format!("larger than {MAX_PEM_FILE_BYTES} bytes")));
    }
    Ok(text)
}

/// `BRISVOX_SIP_TLS_*` as the SIPS listener's settings; `None` when `BRISVOX_SIP_TLS_ADDR` is
/// unset or `off`. The certificate and key files are read here, once.
#[cfg(feature = "sip")]
fn sip_tls(env: &dyn Env) -> Result<Option<SipTlsConfig>> {
    let number = |name: &str, default: u64| -> Result<u64> {
        match var(env, name)? {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| Error::Config(format!("{name}: {v:?} is not a positive number"))),
        }
    };
    let Some(addr) = listen_addr(env, ENV_SIP_TLS_ADDR, None)? else {
        for name in [ENV_SIP_TLS_CERT, ENV_SIP_TLS_KEY, ENV_SIP_TLS_CA] {
            if var(env, name)?.is_some() {
                return Err(Error::Config(format!(
                    "{name} is set, but {ENV_SIP_TLS_ADDR} is not: the SIPS listener is off"
                )));
            }
        }
        return Ok(None);
    };
    let path = |name: &str| -> Result<String> {
        var(env, name)?
            .ok_or_else(|| Error::Config(format!("{ENV_SIP_TLS_ADDR} is set: {name} is required")))
    };
    let cert = read_pem(ENV_SIP_TLS_CERT, &path(ENV_SIP_TLS_CERT)?)?;
    let key = read_pem(ENV_SIP_TLS_KEY, &path(ENV_SIP_TLS_KEY)?)?;
    let mut tls = SipTlsConfig::new(cert, key);
    tls.bind_addr = addr;
    if let Some(ca) = var(env, ENV_SIP_TLS_CA)? {
        tls.client_ca_pem = Some(read_pem(ENV_SIP_TLS_CA, &ca)?);
    }
    tls.max_connections = usize::try_from(number(ENV_SIP_TLS_MAX_CONNECTIONS, 1024)?)
        .map_err(|_| Error::Config(format!("{ENV_SIP_TLS_MAX_CONNECTIONS}: too large")))?;
    tls.max_connections_per_ip = usize::try_from(number(ENV_SIP_TLS_MAX_PER_IP, 32)?)
        .map_err(|_| Error::Config(format!("{ENV_SIP_TLS_MAX_PER_IP}: too large")))?;
    tls.handshake_timeout = Duration::from_millis(number(ENV_SIP_TLS_HANDSHAKE_MS, 10_000)?);
    if let Some(v) = var(env, ENV_SIP_TLS_IDLE_TIMEOUT)? {
        let seconds = v.trim().parse::<u64>().map_err(|_| {
            Error::Config(format!(
                "{ENV_SIP_TLS_IDLE_TIMEOUT}: {v:?} is not a number of seconds (0 turns it off)"
            ))
        })?;
        tls.idle_timeout = Duration::from_secs(seconds);
    }
    Ok(Some(tls))
}

/// `BRISVOX_SIP_*` as the SIP listener's settings; `None` when `BRISVOX_SIP_ADDR` is unset or
/// `off`.
#[cfg(feature = "sip")]
fn sip_listener(env: &dyn Env) -> Result<Option<Listener<SipConfig>>> {
    let Some(addr) = listen_addr(env, ENV_SIP_ADDR, None)? else {
        return Ok(None);
    };
    let public_ip = var(env, ENV_SIP_PUBLIC_IP)?
        .map(|v| {
            v.parse::<std::net::IpAddr>().map_err(|_| {
                Error::Config(format!("{ENV_SIP_PUBLIC_IP}: {v:?} is not an IP address"))
            })
        })
        .transpose()?;
    let tcp = match var(env, ENV_SIP_TCP)?.map(|v| v.to_ascii_lowercase()) {
        None => false,
        Some(v) if matches!(v.as_str(), "true" | "1" | "yes" | "on") => true,
        Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "off") => false,
        Some(v) => {
            return Err(Error::Config(format!(
                "{ENV_SIP_TCP}: {v:?} is not true or false"
            )));
        }
    };
    let rtp_ports = var(env, ENV_SIP_RTP_PORTS)?
        .map(|v| {
            let bad = || {
                Error::Config(format!(
                    "{ENV_SIP_RTP_PORTS}: {v:?} is not a port range like 20000-20999"
                ))
            };
            let (low, high) = v.split_once('-').ok_or_else(bad)?;
            let low = low.trim().parse::<u16>().map_err(|_| bad())?;
            let high = high.trim().parse::<u16>().map_err(|_| bad())?;
            Ok::<_, Error>((low, high))
        })
        .transpose()?;
    let allowed_sources = var(env, ENV_SIP_ALLOWED_SOURCES)?
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|net| !net.is_empty())
                .map(|net| {
                    net.parse::<Cidr>()
                        .map_err(|e| Error::Config(format!("{ENV_SIP_ALLOWED_SOURCES}: {e}")))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let inbound_auth = match (
        var(env, ENV_SIP_AUTH_USER)?,
        var(env, ENV_SIP_AUTH_PASSWORD)?,
    ) {
        (None, None) => None,
        (Some(user), Some(password)) => Some(InboundAuth {
            realm: var(env, ENV_SIP_AUTH_REALM)?.unwrap_or_else(|| "brisvox".to_owned()),
            users: vec![(user, Secret::new(password))],
            nonce_ttl: Duration::from_secs(30),
        }),
        (Some(_), None) => {
            return Err(Error::Config(format!(
                "{ENV_SIP_AUTH_USER} is set: {ENV_SIP_AUTH_PASSWORD} is required"
            )));
        }
        (None, Some(_)) => {
            return Err(Error::Config(format!(
                "{ENV_SIP_AUTH_PASSWORD} is set: {ENV_SIP_AUTH_USER} is required"
            )));
        }
    };
    if allowed_sources.is_empty() && inbound_auth.is_none() {
        return Err(Error::Config(format!(
            "{ENV_SIP_ADDR} is set: an open SIP port is scanned within minutes; set {ENV_SIP_ALLOWED_SOURCES} (the carrier's signaling networks) and/or {ENV_SIP_AUTH_USER} with {ENV_SIP_AUTH_PASSWORD}"
        )));
    }
    let tls = sip_tls(env)?;
    let srtp = match var(env, ENV_SIP_SRTP)? {
        Some(v) => v
            .parse::<SrtpPolicy>()
            .map_err(|e| Error::Config(format!("{ENV_SIP_SRTP}: {e}")))?,
        None if tls.is_some() => SrtpPolicy::Optional,
        None => SrtpPolicy::Off,
    };
    let defaults = SipConfig::default();
    let srtp_auth_failure_limit = match var(env, ENV_SIP_SRTP_AUTH_FAIL_LIMIT)? {
        Some(v) => v.trim().parse::<u32>().map_err(|_| {
            Error::Config(format!(
                "{ENV_SIP_SRTP_AUTH_FAIL_LIMIT}: {v:?} is not a number (0 turns the limit off)"
            ))
        })?,
        None => defaults.srtp_auth_failure_limit,
    };
    let config = SipConfig {
        bind_addr: addr,
        tcp,
        public_ip,
        rtp_ports,
        allowed_sources,
        inbound_auth,
        tls,
        srtp,
        srtp_auth_failure_limit,
        ..defaults
    };
    config
        .validate()
        .map_err(|e| Error::Config(format!("{ENV_SIP_ADDR}: {e}")))?;
    Ok(Some(Listener { addr, config }))
}

impl HostSettings {
    /// No listener and no agent: a server that only serves its health endpoints.
    pub fn none() -> Self {
        Self {
            websocket: None,
            ..Self::default()
        }
    }

    /// Reads and validates the websocket listener's own agent definition
    /// ([`Self::websocket_agent_file`]); `None` when there is none. Blocking file IO.
    ///
    /// # Errors
    /// [`ConfigError`](crate::config::ConfigError) as [`config::load_file`].
    pub fn load_websocket_agent(
        &self,
        env: &dyn Env,
    ) -> Option<std::result::Result<AgentConfig, crate::config::ConfigError>> {
        self.websocket_agent_file
            .as_ref()
            .map(|path| config::load_file(path, env))
    }

    /// [`Self::load_websocket_agent`] that also returns the hash of the file's text
    /// ([`crate::reload::text_hash`]): the reload tells by it whether the file changed since.
    ///
    /// # Errors
    /// As [`Self::load_websocket_agent`].
    pub fn load_websocket_agent_hashed(
        &self,
        env: &dyn Env,
    ) -> Option<std::result::Result<(AgentConfig, u64), crate::config::ConfigError>> {
        self.websocket_agent_file.as_ref().map(|path| {
            let (hash, agent) = crate::reload::load_file_hashed(path, env);
            agent.map(|agent| (agent, hash.unwrap_or_default()))
        })
    }

    /// [`Self::load_agent`] that also returns the hash of the definition's text (of the
    /// file's, or of the inline one) for the reload.
    ///
    /// # Errors
    /// As [`Self::load_agent`].
    pub fn load_agent_hashed(
        &self,
        env: &dyn Env,
    ) -> Option<std::result::Result<(AgentConfig, u64), crate::config::ConfigError>> {
        match (&self.agent_file, &self.agent_inline) {
            (Some(path), _) => {
                let (hash, agent) = crate::reload::load_file_hashed(path, env);
                Some(agent.map(|agent| (agent, hash.unwrap_or_default())))
            }
            (None, Some(inline)) => Some(
                config::load_inline(&inline.text, inline.format, env)
                    .map(|agent| (agent, crate::reload::text_hash(&inline.text))),
            ),
            (None, None) => None,
        }
    }

    /// An agent definition is configured: a file or the inline text.
    pub fn has_agent(&self) -> bool {
        self.agent_file.is_some() || self.agent_inline.is_some()
    }

    /// Reads and validates the configured agent definition (file or inline text), with secrets
    /// from `env`; `None` when there is none. Blocking file IO: call at startup.
    ///
    /// # Errors
    /// [`ConfigError`](crate::config::ConfigError) as [`config::load_file`] and
    /// [`config::load_inline`].
    pub fn load_agent(
        &self,
        env: &dyn Env,
    ) -> Option<std::result::Result<AgentConfig, crate::config::ConfigError>> {
        match (&self.agent_file, &self.agent_inline) {
            (Some(path), _) => Some(config::load_file(path, env)),
            (None, Some(inline)) => Some(config::load_inline(&inline.text, inline.format, env)),
            (None, None) => None,
        }
    }

    /// Reads the settings from `env` (see `docs/OPERATIONS.md`); unset variables keep their
    /// defaults.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when an address does not parse, a value is not
    /// UTF-8, the Twilio listener lacks its Auth Token or public URL, a protocol's settings are
    /// invalid, or a listener is on without an agent definition.
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let agent_file = var(env, ENV_AGENT_FILE)?.map(PathBuf::from);
        let agent_inline = inline_agent(env)?;
        if agent_file.is_some() && agent_inline.is_some() {
            return Err(Error::Config(format!(
                "{ENV_AGENT_FILE} and {ENV_AGENT_DEFINITION} are both set: use one (a file path, or the definition's text)"
            )));
        }
        // The image's default path applies only when neither is set.
        let agent_file = match (agent_file, &agent_inline) {
            (None, None) => var(env, ENV_DEFAULT_AGENT_FILE)?.map(PathBuf::from),
            (file, _) => file,
        };
        let session_secrets = session_secrets(env)?;
        let insecure_listeners = flag_var(env, ENV_INSECURE_LISTENERS)?.unwrap_or(false);
        let ws_token = var(env, ENV_WS_TOKEN)?.map(Secret::new);
        let twilio = match listen_addr(env, ENV_TWILIO_ADDR, None)? {
            None => None,
            Some(addr) => {
                let token = var(env, ENV_TWILIO_AUTH_TOKEN)?.ok_or_else(|| {
                    Error::Config(format!(
                        "{ENV_TWILIO_ADDR} is set: {ENV_TWILIO_AUTH_TOKEN} is required (every stream's X-Twilio-Signature is checked)"
                    ))
                })?;
                let url = var(env, ENV_TWILIO_PUBLIC_URL)?.ok_or_else(|| {
                    Error::Config(format!(
                        "{ENV_TWILIO_ADDR} is set: {ENV_TWILIO_PUBLIC_URL} is required (the stream URL as written in the TwiML)"
                    ))
                })?;
                let defaults = TwilioConfig::default();
                let stream_secret = var(env, ENV_TWILIO_STREAM_SECRET)?.map(Secret::new);
                let allow_static_url = flag_var(env, ENV_TWILIO_ALLOW_STATIC_URL)?.unwrap_or(false);
                if stream_secret.is_none() && !allow_static_url {
                    return Err(Error::Config(format!(
                        "{ENV_TWILIO_ADDR} is set: set {ENV_TWILIO_STREAM_SECRET} (at least 32 bytes; every call's stream URL is then signed and single-use) or {ENV_TWILIO_ALLOW_STATIC_URL}=1 (legacy: the fixed URL's X-Twilio-Signature can be replayed by anyone who saw it once)"
                    )));
                }
                if allow_static_url {
                    tracing::warn!(
                        target: "brisvox::server",
                        "{ENV_TWILIO_ALLOW_STATIC_URL}=1: the fixed Twilio stream URL is accepted, so a captured X-Twilio-Signature can be replayed"
                    );
                }
                let config = TwilioConfig {
                    auth_token: Some(Secret::new(token)),
                    public_url: Some(url),
                    stream_secret,
                    allow_static_url,
                    path: var(env, ENV_TWILIO_PATH)?.unwrap_or(defaults.path.clone()),
                    ..defaults
                };
                config
                    .validate()
                    .map_err(|e| Error::Config(format!("{ENV_TWILIO_ADDR}: {e}")))?;
                Some(Listener { addr, config })
            }
        };
        let telnyx = match listen_addr(env, ENV_TELNYX_ADDR, None)? {
            None => None,
            Some(addr) => {
                let defaults = TelnyxConfig::default();
                let token = var(env, ENV_TELNYX_TOKEN)?;
                if token.is_none() && !addr.ip().is_loopback() {
                    if flag_var(env, ENV_INSECURE_LISTENERS)?.unwrap_or(false) {
                        tracing::warn!(
                            target: "brisvox::server",
                            %addr,
                            "{ENV_INSECURE_LISTENERS}=1: the Telnyx listener is reachable without {ENV_TELNYX_TOKEN}; anyone who learns the path can open calls"
                        );
                    } else {
                        return Err(Error::Config(format!(
                            "{ENV_TELNYX_ADDR} is {addr}, not a loopback address, and {ENV_TELNYX_TOKEN} is unset: Telnyx signs nothing, so the path and the stream token are the only checks; set {ENV_TELNYX_TOKEN} (and give the same value to Telnyx as stream_auth_token), or {ENV_INSECURE_LISTENERS}=1 for a network that is closed another way"
                        )));
                    }
                }
                let config = TelnyxConfig {
                    auth_token: token.map(Secret::new),
                    path: var(env, ENV_TELNYX_PATH)?.unwrap_or(defaults.path.clone()),
                    ..defaults
                };
                config
                    .validate()
                    .map_err(|e| Error::Config(format!("{ENV_TELNYX_ADDR}: {e}")))?;
                Some(Listener { addr, config })
            }
        };
        let rtvi = match listen_addr(env, ENV_RTVI_ADDR, None)? {
            None => None,
            Some(addr) => {
                let allowed_origins = var(env, ENV_RTVI_ORIGINS)?
                    .map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|o| !o.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let defaults = RtviConfig::default();
                let config = RtviConfig {
                    token: var(env, ENV_RTVI_TOKEN)?.map(Secret::new),
                    sessions: session_secrets.clone(),
                    allowed_origins,
                    path: var(env, ENV_RTVI_PATH)?.unwrap_or(defaults.path.clone()),
                    ..defaults
                };
                config
                    .validate()
                    .map_err(|e| Error::Config(format!("{ENV_RTVI_ADDR}: {e}")))?;
                Some(Listener { addr, config })
            }
        };
        #[cfg(feature = "sip")]
        let sip = sip_listener(env)?;
        #[cfg(feature = "sip")]
        let sip_on = sip.is_some();
        #[cfg(not(feature = "sip"))]
        let sip_on = {
            if listen_addr(env, ENV_SIP_ADDR, None)?.is_some() {
                return Err(Error::Config(format!(
                    "{ENV_SIP_ADDR} is set, but this binary is built without the `sip` feature"
                )));
            }
            false
        };
        #[cfg(feature = "webrtc")]
        let webrtc = webrtc_listener(env, session_secrets.as_ref())?;
        #[cfg(feature = "webrtc")]
        let webrtc_on = webrtc.is_some();
        #[cfg(not(feature = "webrtc"))]
        let webrtc_on = {
            if listen_addr(env, ENV_WEBRTC_ADDR, None)?.is_some() {
                return Err(Error::Config(format!(
                    "{ENV_WEBRTC_ADDR} is set, but this binary is built without the `webrtc` feature"
                )));
            }
            false
        };
        let other_listener =
            sip_on || webrtc_on || twilio.is_some() || telnyx.is_some() || rtvi.is_some();
        // Unauthenticated: never on by default next to another listener (module docs).
        let default_ws = if other_listener {
            None
        } else {
            Self::default().websocket
        };
        let websocket = listen_addr(env, ENV_WS_ADDR, default_ws)?;
        let websocket_agent_file = var(env, ENV_WS_AGENT_FILE)?.map(PathBuf::from);
        if websocket_agent_file.is_some() && websocket.is_none() {
            return Err(Error::Config(format!(
                "{ENV_WS_AGENT_FILE} is set but the generic websocket listener is off: set {ENV_WS_ADDR}"
            )));
        }
        let server = listener_tunables(env)?;
        let settings = Self {
            server,
            agent_file,
            agent_inline,
            websocket_agent_file,
            websocket,
            twilio,
            telnyx,
            rtvi,
            session_secrets,
            ws_token,
            insecure_listeners,
            #[cfg(feature = "sip")]
            sip,
            #[cfg(feature = "webrtc")]
            webrtc,
            links: StreamLinks::default(),
        };
        if settings.any_listener() && !settings.has_agent() {
            return Err(Error::Config(format!(
                "{ENV_AGENT_FILE} (or {ENV_AGENT_DEFINITION}) is required: the calls of the enabled listeners need an agent definition (or turn every listener off: {ENV_WS_ADDR}=off)"
            )));
        }
        settings.check_exposure()?;
        Ok(settings)
    }

    /// The listeners that would accept anyone who can reach them: bound to a non-loopback
    /// address with neither a session secret nor a static token (Telnyx: its stream token).
    /// Empty for a safe configuration. Twilio verifies its signature and SIP has its own rule,
    /// so neither is listed.
    pub fn exposure_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let exposed = |addr: SocketAddr| !addr.ip().is_loopback();
        if let Some(addr) = self.websocket
            && exposed(addr)
            && self.ws_token.is_none()
            && self.session_secrets.is_none()
        {
            problems.push(format!(
                "the generic websocket listener on {addr} has no authentication: set {ENV_SESSION_SECRET} (or {ENV_WS_TOKEN} for development), or bind {ENV_WS_ADDR} to loopback"
            ));
        }
        if let Some(l) = &self.rtvi
            && exposed(l.addr)
            && l.config.token.is_none()
            && l.config.sessions.is_none()
        {
            problems.push(format!(
                "the RTVI listener on {} has no authentication: set {ENV_SESSION_SECRET} (or {ENV_RTVI_TOKEN} for development)",
                l.addr
            ));
        }
        if let Some(l) = &self.telnyx
            && exposed(l.addr)
            && l.config.auth_token.is_none()
        {
            problems.push(format!(
                "the Telnyx listener on {} has no stream token: set {ENV_TELNYX_TOKEN} (and the same value as stream_auth_token in the Telnyx request)",
                l.addr
            ));
        }
        #[cfg(feature = "webrtc")]
        if let Some(l) = &self.webrtc
            && exposed(l.addr)
            && l.config.config.bearer_token.is_none()
            && l.config.config.sessions.is_none()
        {
            problems.push(format!(
                "the WebRTC signaling listener on {} has no authentication: set {ENV_SESSION_SECRET} (or {ENV_WEBRTC_TOKEN} for development)",
                l.addr
            ));
        }
        problems
    }

    /// Refuses a configuration with exposed listeners unless `BRISVOX_INSECURE_LISTENERS` is on.
    ///
    /// # Errors
    /// [`Error::Config`] naming every exposed listener and the way out.
    pub fn check_exposure(&self) -> Result<()> {
        let problems = self.exposure_problems();
        if problems.is_empty() || self.insecure_listeners {
            return Ok(());
        }
        Err(Error::Config(format!(
            "refusing to start: {}; or set {ENV_INSECURE_LISTENERS}=1 to accept the risk (development or a private network only)",
            problems.join("; ")
        )))
    }

    /// Logs what weakens the configuration: listeners opened by `BRISVOX_INSECURE_LISTENERS`, and
    /// static tokens (shared, long-lived; the production credential is a session token).
    fn warn_about_weak_auth(&self) {
        for problem in self.exposure_problems() {
            tracing::warn!(
                target: "brisvox::server",
                "{ENV_INSECURE_LISTENERS}=1: {problem}; anyone who can reach it spends your provider keys"
            );
        }
        let static_tokens = [
            ("generic websocket", self.ws_token.is_some()),
            (
                "RTVI",
                self.rtvi.as_ref().is_some_and(|l| l.config.token.is_some()),
            ),
            #[cfg(feature = "webrtc")]
            (
                "WebRTC",
                self.webrtc
                    .as_ref()
                    .is_some_and(|l| l.config.config.bearer_token.is_some()),
            ),
        ];
        for (name, configured) in static_tokens {
            if configured {
                tracing::warn!(
                    target: "brisvox::server",
                    "the {name} listener uses a static token: it is shared and never expires, fine for development; use {ENV_SESSION_SECRET} and short-lived session tokens in production (docs/SECURITY.md)"
                );
            }
        }
    }

    /// The links the carriers' call control uses to reach these settings' listeners (see
    /// [`StreamLinks`]): give them to [`ToolSetup::from_env_linked`](crate::ToolSetup) before
    /// binding. The cells are shared with [`Listeners::bind`], which fills them.
    pub fn stream_links(&self) -> StreamLinks {
        StreamLinks {
            twilio_stream_url: self
                .twilio
                .as_ref()
                .and_then(|l| l.config.public_url.as_deref())
                .map(stream_url),
            twilio_stream_secret: self
                .twilio
                .as_ref()
                .and_then(|l| l.config.stream_secret.clone()),
            ..self.links.clone()
        }
    }

    /// Whether any listener is on.
    pub const fn any_listener(&self) -> bool {
        #[cfg(feature = "sip")]
        let sip = self.sip.is_some();
        #[cfg(not(feature = "sip"))]
        let sip = false;
        #[cfg(feature = "webrtc")]
        let webrtc = self.webrtc.is_some();
        #[cfg(not(feature = "webrtc"))]
        let webrtc = false;
        self.websocket.is_some()
            || self.twilio.is_some()
            || self.telnyx.is_some()
            || self.rtvi.is_some()
            || sip
            || webrtc
    }

    /// Runs the WebRTC listener's Opus codec work on `pool` (the server's inference pool): the
    /// codec costs 90 to 376 us a frame, more than the 50 us budget of a tokio worker. A no-op
    /// without a WebRTC listener.
    #[cfg(feature = "webrtc")]
    #[must_use]
    pub fn with_codec_pool(mut self, pool: brisvox_inference::PoolHandle) -> Self {
        if let Some(listener) = self.webrtc.as_mut() {
            listener.config.codec_pool = Some(pool);
        }
        self
    }
}

/// Where the listeners are bound (`None`: not hosted).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListenerAddrs {
    /// The generic websocket listener.
    pub websocket: Option<SocketAddr>,
    /// The Twilio listener.
    pub twilio: Option<SocketAddr>,
    /// The Telnyx listener.
    pub telnyx: Option<SocketAddr>,
    /// The RTVI listener.
    pub rtvi: Option<SocketAddr>,
    /// The SIP listener (its UDP socket; TCP is on the same port when enabled).
    pub sip: Option<SocketAddr>,
    /// The SIPS (TLS) listener of the SIP server, when `BRISVOX_SIP_TLS_ADDR` is set.
    pub sip_tls: Option<SocketAddr>,
    /// The WebRTC listener's signaling endpoint (HTTP; the media socket is UDP, see
    /// `WebRtcServer::udp_addr`).
    pub webrtc: Option<SocketAddr>,
}

/// The gate the hosted transports admit through: a reservation of the session manager.
struct ManagerGate(SessionManager);

impl AdmissionGate for ManagerGate {
    fn try_admit(&self) -> std::result::Result<AdmissionTicket, Refusal> {
        match self.0.try_admit() {
            Ok(slot) => Ok(AdmissionTicket::new(slot)),
            Err(AdmissionError::AtCapacity { .. }) => Err(Refusal::Busy),
            Err(AdmissionError::Overloaded) => Err(Refusal::Overloaded),
            Err(AdmissionError::ProviderBudget { .. }) => Err(Refusal::ProviderBudget),
            Err(AdmissionError::ShuttingDown) => Err(Refusal::ShuttingDown),
        }
    }
}

impl AcceptedCall {
    /// The admission ticket of the call's link.
    fn take_ticket(&mut self) -> Option<AdmissionTicket> {
        match self {
            Self::Link(link) => link.take_ticket(),
            Self::Rtvi(call) => call.link.take_ticket(),
        }
    }

    /// The transport's id of the call, when it labels one (the websocket transport does).
    fn call_id(&self) -> Option<&str> {
        match self {
            Self::Link(link) => link.call_id(),
            Self::Rtvi(call) => call.link.call_id(),
        }
    }

    /// The transport that carried the call when it is not its listener's (the WebRTC listener's
    /// WebSocket fallback: `webrtc-ws`).
    fn transport(&self) -> Option<&'static str> {
        match self {
            Self::Link(link) => link.transport(),
            Self::Rtvi(call) => call.link.transport(),
        }
    }
}

/// A transport server the binary hosts.
trait Hosted: Send + 'static {
    /// The transport's name in logs.
    const KIND: &'static str;

    /// The next call; `Ok(None)` once the server has shut down.
    fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> impl Future<Output = brisvox_transport::Result<Option<AcceptedCall>>> + Send;

    /// Stops the server and waits for its tasks, at most `timeout`.
    fn stop(&self, timeout: Duration)
    -> impl Future<Output = brisvox_transport::Result<()>> + Send;
}

impl Hosted for WebSocketServer {
    const KIND: &'static str = "websocket";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        Ok(self.accept(cancel).await?.map(AcceptedCall::Link))
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.shutdown(timeout).await
    }
}

impl Hosted for TwilioServer {
    const KIND: &'static str = "twilio";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        Ok(self.accept(cancel).await?.map(AcceptedCall::Link))
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.shutdown(timeout).await
    }
}

impl Hosted for TelnyxServer {
    const KIND: &'static str = "telnyx";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        Ok(self.accept(cancel).await?.map(AcceptedCall::Link))
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.shutdown(timeout).await
    }
}

impl Hosted for RtviServer {
    const KIND: &'static str = "rtvi";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        Ok(self.accept_call(cancel).await?.map(AcceptedCall::Rtvi))
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.shutdown(timeout).await
    }
}

/// WebRTC calls reach the engine once their ICE and DTLS completed, so this listener has no
/// pre-upgrade gate: the call is admitted when it arrives ([`start_call`]) and hung up at once
/// when the manager has no room.
#[cfg(feature = "webrtc")]
impl Hosted for WebRtcServer {
    const KIND: &'static str = "webrtc";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        Ok(self.accept(cancel).await?.map(AcceptedCall::Link))
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.shutdown(timeout).await
    }
}

/// The SIP server as the accept loop hosts it: shut down gracefully (BYE to every call) when the
/// manager's cancel stage begins, see the [module docs](self).
#[cfg(feature = "sip")]
struct HostedSip {
    server: SipServer,
    /// The manager's shutdown token (its cancel stage).
    hang_up: CancellationToken,
}

#[cfg(feature = "sip")]
impl Hosted for HostedSip {
    const KIND: &'static str = "sip";

    async fn next_call(
        &mut self,
        cancel: &CancellationToken,
    ) -> brisvox_transport::Result<Option<AcceptedCall>> {
        tokio::select! {
            biased;
            () = self.hang_up.cancelled() => {
                // Hang up every call (BYE) while the signaling endpoint still runs; the INVITEs
                // waiting for their ACK end too, giving their reservations back.
                if let Err(error) = self.server.shutdown(LISTENER_STOP_TIMEOUT).await {
                    tracing::warn!(target: "brisvox::server", transport = Self::KIND, %error, "sip calls did not hang up in time");
                }
                Ok(None)
            }
            call = self.server.accept(cancel) => Ok(call?.map(AcceptedCall::Link)),
        }
    }

    async fn stop(&self, timeout: Duration) -> brisvox_transport::Result<()> {
        self.server.shutdown(timeout).await
    }
}

/// The bound listeners and their accept loops.
#[derive(Debug)]
pub struct Listeners {
    cancel: CancellationToken,
    tracker: TaskTracker,
    addrs: ListenerAddrs,
    clock: SharedClock,
}

impl Listeners {
    /// Binds every listener `settings` turns on and starts their accept loops. Each admits
    /// through `manager` (see the [module docs](self)) and runs its calls with `runner`.
    ///
    /// # Errors
    /// [`Error::Config`] when an address cannot be bound or a transport's settings are invalid;
    /// the listeners bound before it are stopped.
    pub async fn bind<R>(
        settings: &HostSettings,
        manager: &SessionManager,
        runner: &Arc<R>,
        clock: SharedClock,
    ) -> Result<Self>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        Self::bind_routed(settings, manager, runner, None, clock).await
    }

    /// [`bind`](Self::bind), with the generic websocket listener's calls run by
    /// `websocket_runner` when given (its own agent, [`HostSettings::websocket_agent_file`]);
    /// every other listener runs `runner`. Both admit through the same `manager`.
    ///
    /// # Errors
    /// As [`bind`](Self::bind).
    pub async fn bind_routed<R>(
        settings: &HostSettings,
        manager: &SessionManager,
        runner: &Arc<R>,
        websocket_runner: Option<&Arc<R>>,
        clock: SharedClock,
    ) -> Result<Self>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        settings.check_exposure()?;
        settings.warn_about_weak_auth();
        let listeners = Self {
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            addrs: ListenerAddrs::default(),
            clock,
        };
        match listeners
            .bind_all(
                settings,
                manager,
                runner,
                websocket_runner.unwrap_or(runner),
            )
            .await
        {
            Ok(addrs) => Ok(Self { addrs, ..listeners }),
            Err(e) => {
                listeners.stop(LISTENER_STOP_TIMEOUT).await;
                Err(e)
            }
        }
    }

    async fn bind_all<R>(
        &self,
        settings: &HostSettings,
        manager: &SessionManager,
        runner: &Arc<R>,
        websocket_runner: &Arc<R>,
    ) -> Result<ListenerAddrs>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        let gate: SharedGate = Arc::new(ManagerGate(manager.clone()));
        // The transports' own limit bounds sockets; the manager's refuses calls (module docs).
        let max_calls = manager.stats().max_sessions.saturating_mul(2);
        let server_config = |addr: SocketAddr| ServerConfig {
            bind_addr: addr,
            max_calls,
            ..settings.server
        };
        let parent = manager.shutdown_token();
        let clock = &self.clock;
        let mut addrs = ListenerAddrs::default();
        if let Some(addr) = settings.websocket {
            let listener = bind_tcp(WebSocketServer::KIND, addr).await?;
            let auth = Authenticator::from_config(
                Audience::WebSocket,
                settings.ws_token.as_ref(),
                settings.session_secrets.as_ref(),
                clock,
            );
            let server = WebSocketServer::serve_authenticated(
                listener,
                server_config(addr),
                clock.clone(),
                &parent,
                Some(Arc::clone(&gate)),
                auth,
            )
            .map_err(|e| transport_error(WebSocketServer::KIND, &e))?;
            addrs.websocket = Some(server.local_addr());
            self.spawn(server, manager, websocket_runner);
        }
        if let Some(l) = &settings.twilio {
            let listener = bind_tcp(TwilioServer::KIND, l.addr).await?;
            let server = TwilioServer::serve_gated(
                listener,
                server_config(l.addr),
                l.config.clone(),
                clock.clone(),
                &parent,
                Some(Arc::clone(&gate)),
            )
            .map_err(|e| transport_error(TwilioServer::KIND, &e))?;
            addrs.twilio = Some(server.local_addr());
            let _ = settings.links.twilio.set(server.stream_resumer());
            self.spawn(server, manager, runner);
        }
        if let Some(l) = &settings.telnyx {
            let listener = bind_tcp(TelnyxServer::KIND, l.addr).await?;
            let server = TelnyxServer::serve_gated(
                listener,
                server_config(l.addr),
                l.config.clone(),
                clock.clone(),
                &parent,
                Some(Arc::clone(&gate)),
            )
            .map_err(|e| transport_error(TelnyxServer::KIND, &e))?;
            addrs.telnyx = Some(server.local_addr());
            let _ = settings.links.telnyx.set(server.stream_stopper());
            self.spawn(server, manager, runner);
        }
        if let Some(l) = &settings.rtvi {
            let listener = bind_tcp(RtviServer::KIND, l.addr).await?;
            let server = RtviServer::serve_gated(
                listener,
                server_config(l.addr),
                l.config.clone(),
                clock.clone(),
                &parent,
                Some(Arc::clone(&gate)),
            )
            .map_err(|e| transport_error(RtviServer::KIND, &e))?;
            addrs.rtvi = Some(server.local_addr());
            self.spawn(server, manager, runner);
        }
        #[cfg(feature = "webrtc")]
        if let Some(l) = &settings.webrtc {
            // Offers that never connect hold a transport slot for `connect_timeout`: a quarter of
            // the manager's sessions (not of the transport's doubled limit) may be handshaking.
            let handshaking = l
                .config
                .config
                .max_handshaking
                .unwrap_or_else(|| (manager.stats().max_sessions / 4).max(1));
            let config = WebRtcConfig {
                max_calls,
                max_handshaking: Some(handshaking),
                ..l.config.config.clone()
            };
            let options = WebRtcOptions {
                codec_pool: l.config.codec_pool.clone(),
                ..WebRtcOptions::default()
            };
            let server = WebRtcServer::bind_with(config, options, clock.clone(), &parent)
                .await
                .map_err(|e| transport_error(WebRtcServer::KIND, &e))?;
            addrs.webrtc = Some(server.http_addr());
            self.spawn(server, manager, runner);
        }
        #[cfg(feature = "sip")]
        if let Some(l) = &settings.sip {
            let config = SipConfig {
                bind_addr: l.addr,
                max_calls: max_calls.min(MAX_SIP_CALLS),
                ..l.config.clone()
            };
            // Its own token, not the manager's: see the module docs.
            let server = SipServer::bind_gated(
                config,
                clock.clone(),
                &CancellationToken::new(),
                Some(Arc::clone(&gate)),
            )
            .await
            .map_err(|e| {
                Error::Config(format!(
                    "cannot bind the {} listener on {}: {e}",
                    HostedSip::KIND,
                    l.addr
                ))
            })?;
            addrs.sip = Some(server.local_addr());
            addrs.sip_tls = server.handle().tls_addr();
            self.spawn(
                HostedSip {
                    server,
                    hang_up: parent.clone(),
                },
                manager,
                runner,
            );
        }
        Ok(addrs)
    }

    /// Runs `server`'s accept loop until [`Self::stop`], then shuts the server down.
    fn spawn<H, R>(&self, server: H, manager: &SessionManager, runner: &Arc<R>)
    where
        H: Hosted,
        R: SessionRunner<Request = AcceptedCall>,
    {
        self.tracker.spawn(accept_loop(
            server,
            manager.clone(),
            Arc::clone(runner),
            self.cancel.clone(),
        ));
    }

    /// Where the listeners are bound.
    pub const fn addrs(&self) -> ListenerAddrs {
        self.addrs
    }

    /// Ends the accept loops and shuts every transport down (each bounded by `timeout`), then
    /// waits for the loops, at most `timeout` more. Call it after the drain: a transport's
    /// shutdown closes the calls still on it. `false` if a loop was still running.
    pub async fn stop(self, timeout: Duration) -> bool {
        self.cancel.cancel();
        self.tracker.close();
        let bound = timeout.saturating_mul(2);
        tokio::select! {
            biased;
            () = self.tracker.wait() => true,
            () = self.clock.sleep(bound) => false,
        }
    }
}

/// Binds `addr` for the `kind` listener.
async fn bind_tcp(kind: &str, addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| Error::Config(format!("cannot bind the {kind} listener on {addr}: {e}")))
}

fn transport_error(kind: &str, e: &brisvox_transport::Error) -> Error {
    Error::Config(format!("{kind} listener: {e}"))
}

/// Accepts `server`'s calls into sessions until `cancel` fires or the server shuts down, then
/// shuts the server down (bounded).
async fn accept_loop<H, R>(
    mut server: H,
    manager: SessionManager,
    runner: Arc<R>,
    cancel: CancellationToken,
) where
    H: Hosted,
    R: SessionRunner<Request = AcceptedCall>,
{
    loop {
        match server.next_call(&cancel).await {
            Ok(Some(call)) => start_call(&manager, &runner, call, H::KIND),
            Ok(None) => break,
            Err(error) => {
                if !cancel.is_cancelled() {
                    tracing::warn!(target: "brisvox::server", transport = H::KIND, %error, "accept failed; listener stops");
                }
                break;
            }
        }
    }
    if let Err(error) = server.stop(LISTENER_STOP_TIMEOUT).await {
        tracing::warn!(target: "brisvox::server", transport = H::KIND, %error, "transport did not stop in time");
    }
    tracing::info!(target: "brisvox::server", transport = H::KIND, "listener stopped");
}

/// Starts the session of an accepted call in the slot its connection reserved. A call without
/// one (a transport that skipped the gate, as WebRTC does) is admitted now, or hung up when the
/// manager refuses.
fn start_call<R>(manager: &SessionManager, runner: &Arc<R>, mut call: AcceptedCall, kind: &str)
where
    R: SessionRunner<Request = AcceptedCall>,
{
    let kind = call.transport().unwrap_or(kind);
    let call_id = call.call_id().unwrap_or("-").to_owned();
    let reserved = call
        .take_ticket()
        .and_then(|ticket| ticket.downcast::<SessionSlot>().ok());
    let slot = match reserved {
        Some(slot) => slot,
        None => match manager.try_admit() {
            Ok(slot) => slot,
            Err(refused) => {
                tracing::warn!(target: "brisvox::server", transport = kind, %refused, "call without a reservation refused; hanging up");
                return;
            }
        },
    };
    let handle = slot.spawn_with(runner, call);
    tracing::info!(target: "brisvox::server", transport = kind, session = %handle.id(), %call_id, "call admitted");
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_websocket_listener_is_on_by_default_and_needs_an_agent() {
        let err = HostSettings::from_env(&env(&[])).unwrap_err();
        assert!(err.to_string().contains(ENV_AGENT_FILE), "{err}");
        let s = HostSettings::from_env(&env(&[(ENV_AGENT_FILE, "agents/demo.toml")])).unwrap();
        // Unauthenticated: loopback only unless asked for.
        assert_eq!(s.websocket, Some("127.0.0.1:8080".parse().unwrap()));
        assert!(s.twilio.is_none() && s.telnyx.is_none() && s.rtvi.is_none());
        assert_eq!(s.agent_file, Some(PathBuf::from("agents/demo.toml")));
        // Every listener off: health only, no agent needed.
        let off = HostSettings::from_env(&env(&[(ENV_WS_ADDR, "off")])).unwrap();
        assert!(!off.any_listener());
        assert!(!HostSettings::none().any_listener());
    }

    #[test]
    fn the_default_agent_file_applies_only_without_a_file_or_a_definition() {
        let default = (ENV_DEFAULT_AGENT_FILE, "/etc/brisvox/agent.toml");
        let s = HostSettings::from_env(&env(&[default])).unwrap();
        assert_eq!(s.agent_file, Some(PathBuf::from("/etc/brisvox/agent.toml")));
        // An inline definition on a platform that cannot mount a file: no conflict.
        let s = HostSettings::from_env(&env(&[default, (ENV_AGENT_DEFINITION, INLINE)])).unwrap();
        assert!(s.agent_file.is_none() && s.agent_inline.is_some());
        // An explicit file wins over the default.
        let s =
            HostSettings::from_env(&env(&[default, (ENV_AGENT_FILE, "agents/x.toml")])).unwrap();
        assert_eq!(s.agent_file, Some(PathBuf::from("agents/x.toml")));
    }

    #[test]
    fn the_websocket_agent_needs_the_websocket_listener() {
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "agents/demo.toml"),
            (ENV_WS_AGENT_FILE, "agents/load.toml"),
        ]))
        .unwrap();
        assert_eq!(
            s.websocket_agent_file,
            Some(PathBuf::from("agents/load.toml"))
        );
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "agents/demo.toml"),
            (ENV_WS_AGENT_FILE, "agents/load.toml"),
            (ENV_WS_ADDR, "off"),
            (ENV_RTVI_ADDR, "127.0.0.1:0"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains(ENV_WS_AGENT_FILE), "{err}");
        assert!(HostSettings::default().websocket_agent_file.is_none());
    }

    const INLINE: &str = "[agent]\nname = \"x\"\n";

    #[test]
    fn the_agent_definition_can_come_from_a_variable() {
        let s = HostSettings::from_env(&env(&[(ENV_AGENT_DEFINITION, INLINE)])).unwrap();
        assert!(s.has_agent() && s.agent_file.is_none());
        let inline = s.agent_inline.as_ref().unwrap();
        assert_eq!(inline.text, INLINE, "the text is kept as written");
        assert_eq!(inline.format, Format::Toml);
        assert!(format!("{s:?}").contains("bytes"), "{s:?}");
        assert!(
            !format!("{s:?}").contains("name = "),
            "the text is not printed"
        );
        // The format is named, never sniffed.
        let yaml = HostSettings::from_env(&env(&[
            (ENV_AGENT_DEFINITION, "agent:\n  name: x\n"),
            (ENV_AGENT_FORMAT, " YAML "),
        ]))
        .unwrap();
        assert_eq!(yaml.agent_inline.unwrap().format, Format::Yaml);
        // A blank value is unset.
        let err = HostSettings::from_env(&env(&[(ENV_AGENT_DEFINITION, "  \n")])).unwrap_err();
        assert!(err.to_string().contains(ENV_AGENT_DEFINITION), "{err}");
    }

    #[test]
    fn a_file_and_a_definition_together_are_a_startup_error() {
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_AGENT_DEFINITION, INLINE),
        ]))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(ENV_AGENT_FILE) && err.contains(ENV_AGENT_DEFINITION),
            "{err}"
        );
        // Also with every listener off: the mistake is the same.
        assert!(
            HostSettings::from_env(&env(&[
                (ENV_WS_ADDR, "off"),
                (ENV_AGENT_FILE, "a.toml"),
                (ENV_AGENT_DEFINITION, INLINE),
            ]))
            .is_err()
        );
    }

    #[test]
    fn the_agent_format_needs_a_definition_and_a_known_name() {
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.yaml"),
            (ENV_AGENT_FORMAT, "yaml"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(ENV_AGENT_FORMAT) && err.contains(ENV_AGENT_DEFINITION),
            "{err}"
        );
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_DEFINITION, INLINE),
            (ENV_AGENT_FORMAT, "json"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains(ENV_AGENT_FORMAT), "{err}");
    }

    #[test]
    fn an_inline_definition_goes_through_the_loader_with_its_own_source_name() {
        let s = HostSettings::from_env(&env(&[(ENV_AGENT_DEFINITION, INLINE)])).unwrap();
        let err = s.load_agent(&env(&[])).unwrap().unwrap_err().to_string();
        assert!(err.contains("$BRISVOX_AGENT_DEFINITION"), "{err}");
        assert!(
            HostSettings::none().load_agent(&env(&[])).is_none(),
            "nothing configured, nothing loaded"
        );
        // The size limit of the file applies.
        let big = format!(
            "# {}\n",
            "x".repeat(usize::try_from(config::MAX_FILE_BYTES).unwrap())
        );
        let s = HostSettings::from_env(&env(&[(ENV_AGENT_DEFINITION, big.as_str())])).unwrap();
        let err = s.load_agent(&env(&[])).unwrap().unwrap_err().to_string();
        assert!(err.contains("larger than"), "{err}");
    }

    #[test]
    fn listener_addresses_take_the_port_placeholder() {
        let port = (crate::settings::ENV_PORT, "8123");
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_DEFINITION, INLINE),
            (ENV_WS_ADDR, "127.0.0.1:${PORT}"),
            port,
        ]))
        .unwrap();
        assert_eq!(s.websocket, Some("127.0.0.1:8123".parse().unwrap()));
        // Every listener's address variable, not only the generic websocket's.
        for var in [ENV_TWILIO_ADDR, ENV_TELNYX_ADDR, ENV_RTVI_ADDR] {
            let mut pairs = vec![
                (ENV_AGENT_DEFINITION, INLINE),
                (ENV_TWILIO_AUTH_TOKEN, "tok"),
                (ENV_TWILIO_PUBLIC_URL, "wss://example.com/twilio/stream"),
                (ENV_TWILIO_STREAM_SECRET, TEST_STREAM_SECRET),
                port,
            ];
            pairs.push((var, "127.0.0.1:${PORT}"));
            let s = HostSettings::from_env(&env(&pairs)).unwrap();
            let bound = match var {
                ENV_TWILIO_ADDR => s.twilio.map(|l| l.addr),
                ENV_TELNYX_ADDR => s.telnyx.map(|l| l.addr),
                _ => s.rtvi.map(|l| l.addr),
            };
            assert_eq!(bound, Some("127.0.0.1:8123".parse().unwrap()), "{var}");
        }
    }

    #[test]
    fn the_port_placeholder_without_a_usable_port_names_both_variables() {
        for port in [None, Some(""), Some("http"), Some("0"), Some("70000")] {
            let mut pairs = vec![
                (ENV_AGENT_DEFINITION, INLINE),
                (ENV_WS_ADDR, "[::1]:${PORT}"),
            ];
            if let Some(port) = port {
                pairs.push((crate::settings::ENV_PORT, port));
            }
            let err = HostSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(ENV_WS_ADDR) && err.contains("PORT"),
                "{port:?}: {err}"
            );
        }
        // The placeholder is not expanded in other variables.
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_DEFINITION, INLINE),
            (ENV_WS_TOKEN, "${PORT}"),
            (crate::settings::ENV_PORT, "8123"),
        ]))
        .unwrap();
        assert_eq!(s.ws_token.unwrap().expose(), "${PORT}");
    }

    #[test]
    fn the_unauthenticated_websocket_listener_is_off_by_default_next_to_another_listener() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let twilio = [
            agent,
            (ENV_TWILIO_ADDR, "0.0.0.0:9001"),
            (ENV_TWILIO_AUTH_TOKEN, "tok"),
            (ENV_TWILIO_PUBLIC_URL, "wss://example.com/twilio/stream"),
            (ENV_TWILIO_STREAM_SECRET, TEST_STREAM_SECRET),
        ];
        let s = HostSettings::from_env(&env(&twilio)).unwrap();
        assert!(s.twilio.is_some());
        assert_eq!(
            s.websocket, None,
            "no second, unauthenticated door by default"
        );
        for other in [
            vec![(ENV_TELNYX_ADDR, "127.0.0.1:9002")],
            vec![(ENV_RTVI_ADDR, "127.0.0.1:9003")],
        ] {
            let mut pairs = vec![agent];
            pairs.extend_from_slice(&other);
            let s = HostSettings::from_env(&env(&pairs)).unwrap();
            assert_eq!(s.websocket, None, "{other:?}");
        }
        // Asked for explicitly, it is there.
        let mut both = twilio.to_vec();
        both.push((ENV_WS_ADDR, "0.0.0.0:8080"));
        both.push((ENV_WS_TOKEN, "dev-token"));
        let s = HostSettings::from_env(&env(&both)).unwrap();
        assert_eq!(s.websocket, Some("0.0.0.0:8080".parse().unwrap()));
    }

    #[cfg(not(feature = "sip"))]
    #[test]
    fn a_sip_listener_needs_the_sip_feature() {
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_SIP_ADDR, "127.0.0.1:5060"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("`sip` feature"), "{err}");
        // `off` is fine without it.
        let s = HostSettings::from_env(&env(&[(ENV_AGENT_FILE, "a.toml"), (ENV_SIP_ADDR, "off")]))
            .unwrap();
        assert!(s.websocket.is_some());
    }

    /// Writes a fresh self-signed certificate and its key to temp files; returns their paths.
    #[cfg(feature = "sip")]
    fn tls_files(tag: &str) -> (String, String) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let dir = std::env::temp_dir();
        let base = format!("brisvox-sips-{}-{tag}", std::process::id());
        let cert = dir.join(format!("{base}.crt"));
        let key = dir.join(format!("{base}.key"));
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
        (
            cert.to_string_lossy().into_owned(),
            key.to_string_lossy().into_owned(),
        )
    }

    #[cfg(feature = "sip")]
    #[test]
    fn reads_the_sips_listener_and_the_srtp_policy() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let base = [
            agent,
            (ENV_SIP_ADDR, "127.0.0.1:5060"),
            (ENV_SIP_ALLOWED_SOURCES, "127.0.0.1"),
        ];
        let with = |extra: &[(&'static str, &str)]| {
            let mut pairs = base.to_vec();
            pairs.extend_from_slice(extra);
            HostSettings::from_env(&env(&pairs))
        };
        let (cert, key) = tls_files("env");

        // No TLS: SRTP defaults to off, and asking for it is an error (keys must not go in clear).
        let plain = with(&[]).unwrap().sip.unwrap();
        assert!(plain.config.tls.is_none());
        assert_eq!(plain.config.srtp, SrtpPolicy::Off);
        let err = with(&[(ENV_SIP_SRTP, "required")]).unwrap_err();
        assert!(err.to_string().contains("TLS"), "{err}");

        // With TLS: SRTP defaults to optional; the files are read.
        let sips = with(&[
            (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
            (ENV_SIP_TLS_CERT, &cert),
            (ENV_SIP_TLS_KEY, &key),
        ])
        .unwrap()
        .sip
        .unwrap();
        let tls = sips.config.tls.as_ref().unwrap();
        assert_eq!(tls.bind_addr, "127.0.0.1:5061".parse().unwrap());
        assert_eq!(
            (tls.max_connections, tls.max_connections_per_ip),
            (1024, 32)
        );
        assert_eq!(tls.handshake_timeout, Duration::from_secs(10));
        assert_eq!(tls.idle_timeout, Duration::from_secs(300));
        assert_eq!(sips.config.srtp_auth_failure_limit, 50);
        assert_eq!(sips.config.srtp, SrtpPolicy::Optional);
        assert!(!format!("{:?}", sips.config).contains("PRIVATE KEY"));

        let tuned = with(&[
            (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
            (ENV_SIP_TLS_CERT, &cert),
            (ENV_SIP_TLS_KEY, &key),
            (ENV_SIP_TLS_MAX_CONNECTIONS, "64"),
            (ENV_SIP_TLS_MAX_PER_IP, "4"),
            (ENV_SIP_TLS_HANDSHAKE_MS, "2500"),
            (ENV_SIP_TLS_IDLE_TIMEOUT, "45"),
            (ENV_SIP_SRTP_AUTH_FAIL_LIMIT, "0"),
            (ENV_SIP_SRTP, "Required"),
        ])
        .unwrap()
        .sip
        .unwrap();
        let tls = tuned.config.tls.as_ref().unwrap();
        assert_eq!((tls.max_connections, tls.max_connections_per_ip), (64, 4));
        assert_eq!(tls.handshake_timeout, Duration::from_millis(2500));
        assert_eq!(tls.idle_timeout, Duration::from_secs(45));
        assert_eq!(tuned.config.srtp_auth_failure_limit, 0);
        assert_eq!(tuned.config.srtp, SrtpPolicy::Required);

        // Mistakes are refused by name.
        for (extra, name) in [
            (vec![(ENV_SIP_TLS_ADDR, "127.0.0.1:5061")], ENV_SIP_TLS_CERT),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, cert.as_str()),
                ],
                ENV_SIP_TLS_KEY,
            ),
            (vec![(ENV_SIP_TLS_CERT, cert.as_str())], ENV_SIP_TLS_ADDR),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, "/no/such/file.crt"),
                    (ENV_SIP_TLS_KEY, key.as_str()),
                ],
                ENV_SIP_TLS_CERT,
            ),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, key.as_str()),
                    (ENV_SIP_TLS_KEY, key.as_str()),
                ],
                "sip tls",
            ),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, cert.as_str()),
                    (ENV_SIP_TLS_KEY, key.as_str()),
                    (ENV_SIP_SRTP, "sometimes"),
                ],
                ENV_SIP_SRTP,
            ),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, cert.as_str()),
                    (ENV_SIP_TLS_KEY, key.as_str()),
                    (ENV_SIP_TLS_MAX_PER_IP, "0"),
                ],
                ENV_SIP_TLS_MAX_PER_IP,
            ),
            (
                vec![
                    (ENV_SIP_TLS_ADDR, "127.0.0.1:5061"),
                    (ENV_SIP_TLS_CERT, cert.as_str()),
                    (ENV_SIP_TLS_KEY, key.as_str()),
                    (ENV_SIP_TLS_IDLE_TIMEOUT, "soon"),
                ],
                ENV_SIP_TLS_IDLE_TIMEOUT,
            ),
            (
                vec![(ENV_SIP_SRTP_AUTH_FAIL_LIMIT, "-1")],
                ENV_SIP_SRTP_AUTH_FAIL_LIMIT,
            ),
        ] {
            let err = with(&extra).unwrap_err().to_string();
            assert!(err.contains(name), "{extra:?}: {err}");
            assert!(!err.contains("PRIVATE KEY"), "{err}");
        }
        // `off` disables it.
        assert!(
            with(&[(ENV_SIP_TLS_ADDR, "off")])
                .unwrap()
                .sip
                .unwrap()
                .config
                .tls
                .is_none()
        );
        let _ = (std::fs::remove_file(&cert), std::fs::remove_file(&key));
    }

    #[cfg(feature = "sip")]
    #[test]
    fn reads_the_sip_listener_and_refuses_an_open_trunk() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let s = HostSettings::from_env(&env(&[
            agent,
            (ENV_SIP_ADDR, "0.0.0.0:5060"),
            (ENV_SIP_PUBLIC_IP, "203.0.113.10"),
            (ENV_SIP_TCP, "true"),
            (ENV_SIP_RTP_PORTS, "20000-20999"),
            (ENV_SIP_ALLOWED_SOURCES, " 10.0.0.0/8 , 203.0.113.7,"),
            (ENV_SIP_AUTH_USER, "carrier"),
            (ENV_SIP_AUTH_PASSWORD, "s3cr3t-pass"),
        ]))
        .unwrap();
        let sip = s.sip.unwrap();
        assert_eq!(sip.addr, "0.0.0.0:5060".parse().unwrap());
        assert_eq!(sip.config.public_ip, Some("203.0.113.10".parse().unwrap()));
        assert!(sip.config.tcp);
        assert_eq!(sip.config.rtp_ports, Some((20_000, 20_999)));
        assert_eq!(sip.config.allowed_sources.len(), 2);
        let auth = sip.config.inbound_auth.unwrap();
        assert_eq!(auth.realm, "brisvox");
        assert_eq!(auth.users[0].0, "carrier");
        assert_eq!(s.websocket, None, "SIP is another listener");

        // Sources alone, or credentials alone, are enough.
        for pairs in [
            vec![
                agent,
                (ENV_SIP_ADDR, "127.0.0.1:0"),
                (ENV_SIP_ALLOWED_SOURCES, "127.0.0.1"),
            ],
            vec![
                agent,
                (ENV_SIP_ADDR, "127.0.0.1:0"),
                (ENV_SIP_AUTH_USER, "u"),
                (ENV_SIP_AUTH_PASSWORD, "p"),
            ],
        ] {
            assert!(HostSettings::from_env(&env(&pairs)).unwrap().sip.is_some());
        }

        for (pairs, name) in [
            // Neither: an open trunk is refused.
            (
                vec![agent, (ENV_SIP_ADDR, "127.0.0.1:5060")],
                ENV_SIP_ALLOWED_SOURCES,
            ),
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "127.0.0.1:5060"),
                    (ENV_SIP_ALLOWED_SOURCES, "10.0.0.0/33"),
                ],
                ENV_SIP_ALLOWED_SOURCES,
            ),
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "127.0.0.1:5060"),
                    (ENV_SIP_AUTH_USER, "u"),
                ],
                ENV_SIP_AUTH_PASSWORD,
            ),
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "127.0.0.1:5060"),
                    (ENV_SIP_AUTH_PASSWORD, "s3cr3t-pass"),
                ],
                ENV_SIP_AUTH_USER,
            ),
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "127.0.0.1:5060"),
                    (ENV_SIP_ALLOWED_SOURCES, "127.0.0.1"),
                    (ENV_SIP_RTP_PORTS, "20000"),
                ],
                ENV_SIP_RTP_PORTS,
            ),
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "127.0.0.1:5060"),
                    (ENV_SIP_ALLOWED_SOURCES, "127.0.0.1"),
                    (ENV_SIP_TCP, "maybe"),
                ],
                ENV_SIP_TCP,
            ),
            // An unspecified address without the address to advertise: the transport's rule.
            (
                vec![
                    agent,
                    (ENV_SIP_ADDR, "0.0.0.0:5060"),
                    (ENV_SIP_ALLOWED_SOURCES, "127.0.0.1"),
                ],
                ENV_SIP_ADDR,
            ),
        ] {
            let err = HostSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{pairs:?}: {err}");
            assert!(!err.contains("s3cr3t"), "{err}");
        }
    }

    const TEST_STREAM_SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_twilio_listener_needs_per_call_urls_or_the_legacy_flag() {
        let base = [
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_TWILIO_ADDR, "127.0.0.1:9001"),
            (ENV_TWILIO_AUTH_TOKEN, "tok"),
            (ENV_TWILIO_PUBLIC_URL, "wss://example.com/twilio/stream"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut pairs = base.to_vec();
            pairs.extend_from_slice(extra);
            HostSettings::from_env(&env(&pairs))
        };
        let error = with(&[]).unwrap_err().to_string();
        assert!(error.contains(ENV_TWILIO_STREAM_SECRET), "{error}");
        assert!(error.contains(ENV_TWILIO_ALLOW_STATIC_URL), "{error}");
        let per_call = with(&[(ENV_TWILIO_STREAM_SECRET, TEST_STREAM_SECRET)]).unwrap();
        let twilio = per_call.twilio.as_ref().unwrap();
        assert!(!twilio.config.allow_static_url);
        assert!(twilio.config.stream_secret.is_some());
        // The secret reaches the call control of resumed streams, never a log line.
        assert!(per_call.stream_links().twilio_stream_secret().is_some());
        assert!(!format!("{per_call:?}").contains(TEST_STREAM_SECRET));
        let legacy = with(&[(ENV_TWILIO_ALLOW_STATIC_URL, "1")]).unwrap();
        let twilio = legacy.twilio.as_ref().unwrap();
        assert!(twilio.config.allow_static_url && twilio.config.stream_secret.is_none());
        // A short secret and a switch that is not a switch are configuration errors.
        let error = with(&[(ENV_TWILIO_STREAM_SECRET, "short")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("32 bytes"), "{error}");
        assert!(with(&[(ENV_TWILIO_ALLOW_STATIC_URL, "maybe")]).is_err());
        assert!(with(&[(ENV_TWILIO_ALLOW_STATIC_URL, "0")]).is_err());
    }

    #[test]
    fn a_network_telnyx_listener_needs_its_token_unless_insecure_listeners_is_on() {
        let with = |addr: &'static str, extra: &[(&'static str, &'static str)]| {
            let mut pairs = vec![(ENV_AGENT_FILE, "a.toml"), (ENV_TELNYX_ADDR, addr)];
            pairs.extend_from_slice(extra);
            HostSettings::from_env(&env(&pairs))
        };
        for addr in ["0.0.0.0:9002", "10.0.0.5:9002", "[::]:9002"] {
            let error = with(addr, &[]).unwrap_err().to_string();
            assert!(error.contains(ENV_TELNYX_TOKEN), "{addr}: {error}");
            assert!(error.contains(ENV_INSECURE_LISTENERS), "{addr}: {error}");
            assert!(with(addr, &[(ENV_TELNYX_TOKEN, "t")]).is_ok(), "{addr}");
            assert!(
                with(addr, &[(ENV_INSECURE_LISTENERS, "1")]).is_ok(),
                "{addr}"
            );
            assert!(with(addr, &[(ENV_INSECURE_LISTENERS, "0")]).is_err());
        }
        // Loopback stays open for development.
        for addr in ["127.0.0.1:9002", "[::1]:9002"] {
            assert!(with(addr, &[]).is_ok(), "{addr}");
        }
        assert!(with("0.0.0.0:9002", &[(ENV_INSECURE_LISTENERS, "maybe")]).is_err());
    }

    #[test]
    fn exposed_listeners_without_authentication_refuse_to_start() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let secret = "0123456789abcdef0123456789abcdef";
        // Each exposed listener alone, with the variable that would open it up.
        // (listener variables, its name in the message, the variable that opens it up)
        type Case<'a> = (Vec<(&'a str, &'a str)>, &'a str, (&'a str, &'a str));
        let exposed: Vec<Case<'_>> = vec![
            (
                vec![(ENV_WS_ADDR, "0.0.0.0:8080")],
                "generic websocket",
                (ENV_WS_TOKEN, "dev"),
            ),
            (
                vec![(ENV_RTVI_ADDR, "0.0.0.0:9003")],
                "RTVI",
                (ENV_RTVI_TOKEN, "dev"),
            ),
            (
                vec![(ENV_TELNYX_ADDR, "[::]:9002")],
                "Telnyx",
                (ENV_TELNYX_TOKEN, "dev"),
            ),
        ];
        for (base, name, opener) in exposed {
            let mut pairs = vec![agent];
            pairs.extend(base);
            let err = HostSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(name) && err.contains(ENV_INSECURE_LISTENERS),
                "{err}"
            );
            // A static token, the insecure flag or a session secret each allow it.
            for allow in [
                opener,
                (ENV_INSECURE_LISTENERS, "1"),
                (ENV_SESSION_SECRET, secret),
            ] {
                let mut with = pairs.clone();
                with.push(allow);
                // Telnyx has no session tokens (the carrier sets the stream token).
                if name == "Telnyx" && allow.0 == ENV_SESSION_SECRET {
                    assert!(HostSettings::from_env(&env(&with)).is_err());
                    continue;
                }
                let s = HostSettings::from_env(&env(&with)).unwrap_or_else(|e| {
                    panic!("{name} with {}: {e}", allow.0);
                });
                assert_eq!(
                    s.exposure_problems().is_empty(),
                    allow.0 != ENV_INSECURE_LISTENERS,
                    "{name} with {}",
                    allow.0
                );
            }
            // Loopback needs nothing.
            let loopback: Vec<_> = pairs
                .iter()
                .map(|(k, v)| {
                    (
                        *k,
                        if k.ends_with("_ADDR") {
                            "127.0.0.1:9100"
                        } else {
                            *v
                        },
                    )
                })
                .collect();
            HostSettings::from_env(&env(&loopback)).unwrap();
        }
        // Twilio always verifies its signature: exposed is fine.
        HostSettings::from_env(&env(&[
            agent,
            (ENV_TWILIO_ADDR, "0.0.0.0:9001"),
            (ENV_TWILIO_AUTH_TOKEN, "tok"),
            (ENV_TWILIO_PUBLIC_URL, "wss://example.com/twilio/stream"),
            // Per-call signed stream URLs.
            (
                ENV_TWILIO_STREAM_SECRET,
                "0123456789abcdef0123456789abcdef-stream",
            ),
        ]))
        .unwrap();
    }

    #[test]
    fn hand_built_settings_are_checked_too() {
        // `Listeners::bind` runs `check_exposure`: settings built in code, not from the
        // environment, get the same rule.
        let mut settings = HostSettings {
            websocket: Some("0.0.0.0:8080".parse().unwrap()),
            ..HostSettings::none()
        };
        let err = settings.check_exposure().unwrap_err().to_string();
        assert!(
            err.contains("generic websocket") && err.contains("0.0.0.0:8080"),
            "{err}"
        );
        settings.websocket = Some("127.0.0.1:8080".parse().unwrap());
        settings.check_exposure().unwrap();
        settings.websocket = Some("[::]:8080".parse().unwrap());
        assert!(settings.check_exposure().is_err());
        settings.ws_token = Some(Secret::new("dev".to_owned()));
        settings.check_exposure().unwrap();
        settings.ws_token = None;
        settings.insecure_listeners = true;
        settings.check_exposure().unwrap();
        assert_eq!(
            settings.exposure_problems().len(),
            1,
            "still reported, for the log"
        );
    }

    #[test]
    fn the_session_secret_is_applied_to_the_listeners_and_checked() {
        let secret = "0123456789abcdef0123456789abcdef";
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_SESSION_SECRET, secret),
            (ENV_SESSION_SECRET_PREV, "fedcba9876543210fedcba9876543210"),
            (ENV_WS_ADDR, "0.0.0.0:8080"),
            (ENV_RTVI_ADDR, "0.0.0.0:9003"),
        ]))
        .unwrap();
        assert!(s.session_secrets.is_some());
        assert!(s.exposure_problems().is_empty());
        assert!(s.rtvi.unwrap().config.sessions.is_some());
        for (pairs, needle) in [
            (vec![(ENV_SESSION_SECRET, "too-short")], ENV_SESSION_SECRET),
            (
                vec![
                    (ENV_SESSION_SECRET, secret),
                    (ENV_SESSION_SECRET_PREV, "short"),
                ],
                ENV_SESSION_SECRET_PREV,
            ),
            (
                vec![(ENV_SESSION_SECRET_PREV, secret)],
                ENV_SESSION_SECRET_PREV,
            ),
        ] {
            let mut all = vec![(ENV_AGENT_FILE, "a.toml")];
            all.extend(pairs);
            let err = HostSettings::from_env(&env(&all)).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
            assert!(!err.contains(secret) && !err.contains("too-short"), "{err}");
        }
    }

    #[cfg(feature = "webrtc")]
    #[test]
    fn the_webrtc_flood_limits_and_trusted_proxies_are_read() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let defaults = HostSettings::from_env(&env(&[agent, (ENV_WEBRTC_ADDR, "127.0.0.1:8088")]))
            .unwrap()
            .webrtc
            .unwrap()
            .config
            .config;
        assert_eq!(defaults.max_calls_per_ip, Some(8));
        assert_eq!(defaults.offers_per_minute_per_ip, Some(5));
        assert_eq!(defaults.max_handshaking, None);
        assert!(defaults.trusted_proxies.is_empty());
        assert_eq!(
            defaults.client_ip_header,
            brisvox_transport::webrtc::ClientIpHeader::XForwardedFor
        );
        assert_eq!(defaults.max_http_connections_per_ip, 8);
        let s = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
            (ENV_WEBRTC_MAX_CALLS_PER_IP, "3"),
            (ENV_WEBRTC_MAX_HANDSHAKING, "10"),
            (ENV_WEBRTC_OFFERS_PER_MINUTE, "off"),
            (ENV_TRUSTED_PROXIES, "10.0.0.0/8, 192.0.2.7 ,"),
            (ENV_CLIENT_IP_HEADER, "Forwarded"),
            (ENV_WEBRTC_HTTP_MAX_PER_IP, "0"),
        ]))
        .unwrap();
        let config = s.webrtc.unwrap().config.config;
        assert_eq!(
            config.client_ip_header,
            brisvox_transport::webrtc::ClientIpHeader::Forwarded
        );
        let real_ip = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
            (ENV_CLIENT_IP_HEADER, " X-Real-IP "),
        ]))
        .unwrap();
        assert_eq!(
            real_ip.webrtc.unwrap().config.config.client_ip_header,
            brisvox_transport::webrtc::ClientIpHeader::XRealIp
        );
        assert_eq!(config.max_http_connections_per_ip, 0);
        assert_eq!(config.max_calls_per_ip, Some(3));
        assert_eq!(config.max_handshaking, Some(10));
        assert_eq!(config.offers_per_minute_per_ip, None);
        assert_eq!(config.trusted_proxies.len(), 2);
        for (name, value) in [
            (ENV_WEBRTC_MAX_CALLS_PER_IP, "0"),
            (ENV_WEBRTC_OFFERS_PER_MINUTE, "many"),
            (ENV_TRUSTED_PROXIES, "10.0.0.0/33"),
            (ENV_TRUSTED_PROXIES, "proxy.example"),
            (ENV_CLIENT_IP_HEADER, "x-client-ip"),
            (ENV_WEBRTC_HTTP_MAX_PER_IP, "lots"),
        ] {
            let err = HostSettings::from_env(&env(&[
                agent,
                (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
                (name, value),
            ]))
            .unwrap_err();
            assert!(err.to_string().contains(name), "{name}={value}: {err}");
        }
        // CORS `*` needs authentication; an exposed signaling port too.
        let err = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
            (ENV_WEBRTC_CORS_ORIGIN, "*"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("cors_origin"), "{err}");
        HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
            (ENV_WEBRTC_CORS_ORIGIN, "*"),
            (ENV_WEBRTC_TOKEN, "dev"),
        ]))
        .unwrap();
        let err = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "0.0.0.0:8088"),
            (ENV_WEBRTC_CANDIDATE_IP, "203.0.113.10"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("WebRTC signaling"), "{err}");
    }

    #[test]
    fn reads_every_listener() {
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_WS_ADDR, "127.0.0.1:9000"),
            (ENV_TWILIO_ADDR, "127.0.0.1:9001"),
            (ENV_TWILIO_AUTH_TOKEN, "tok"),
            (ENV_TWILIO_PUBLIC_URL, "wss://example.com/twilio/stream"),
            (ENV_TWILIO_STREAM_SECRET, TEST_STREAM_SECRET),
            (ENV_TELNYX_ADDR, "127.0.0.1:9002"),
            (ENV_TELNYX_TOKEN, "t2"),
            (ENV_RTVI_ADDR, "127.0.0.1:9003"),
            (ENV_RTVI_TOKEN, "t3"),
            (ENV_RTVI_ORIGINS, " https://a.example , https://b.example,"),
        ]))
        .unwrap();
        assert_eq!(s.websocket, Some("127.0.0.1:9000".parse().unwrap()));
        let twilio = s.twilio.unwrap();
        assert_eq!(twilio.addr, "127.0.0.1:9001".parse().unwrap());
        assert_eq!(twilio.config.auth_token.unwrap().expose(), "tok");
        let telnyx = s.telnyx.unwrap();
        assert_eq!(telnyx.config.auth_token.unwrap().expose(), "t2");
        let rtvi = s.rtvi.unwrap();
        assert_eq!(rtvi.addr, "127.0.0.1:9003".parse().unwrap());
        assert_eq!(rtvi.config.token.unwrap().expose(), "t3");
        assert_eq!(
            rtvi.config.allowed_origins,
            ["https://a.example", "https://b.example"]
        );
    }

    #[test]
    fn reads_the_listener_tunables_and_paths() {
        let defaults = HostSettings::from_env(&env(&[(ENV_AGENT_FILE, "a.toml")])).unwrap();
        assert_eq!(defaults.server, ServerConfig::default());
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_TWILIO_ADDR, "127.0.0.1:9001"),
            (ENV_TWILIO_AUTH_TOKEN, "tok"),
            (ENV_TWILIO_PUBLIC_URL, "wss://example.com/calls/twilio"),
            (ENV_TWILIO_STREAM_SECRET, TEST_STREAM_SECRET),
            (ENV_TWILIO_PATH, "/calls/twilio"),
            (ENV_TELNYX_ADDR, "127.0.0.1:9002"),
            (ENV_TELNYX_PATH, "/calls/telnyx"),
            (ENV_RTVI_ADDR, "127.0.0.1:9003"),
            (ENV_RTVI_PATH, "/bot"),
            (ENV_LISTENER_HANDSHAKE_TIMEOUT, "3s"),
            (ENV_LISTENER_START_TIMEOUT, "4s"),
            (ENV_LISTENER_PING_INTERVAL, "2s"),
            (ENV_LISTENER_PING_TIMEOUT, "9s"),
            (ENV_LISTENER_WRITE_TIMEOUT, "1500ms"),
            (ENV_LISTENER_SEND_TIMEOUT, "700ms"),
            (ENV_LISTENER_CLOSE_GRACE, "1s"),
            (ENV_LISTENER_ACCEPT_QUEUE, "64"),
            (ENV_LISTENER_MAX_AUDIO_BYTES, "32000"),
            (ENV_LISTENER_MAX_CONTROL_BYTES, "4096"),
        ]))
        .unwrap();
        assert_eq!(s.twilio.unwrap().config.path, "/calls/twilio");
        assert_eq!(s.telnyx.unwrap().config.path, "/calls/telnyx");
        assert_eq!(s.rtvi.unwrap().config.path, "/bot");
        let c = s.server;
        assert_eq!(c.handshake_timeout, Duration::from_secs(3));
        assert_eq!(c.start_timeout, Duration::from_secs(4));
        assert_eq!(c.ping_interval, Duration::from_secs(2));
        assert_eq!(c.ping_timeout, Duration::from_secs(9));
        assert_eq!(c.write_timeout, Duration::from_millis(1500));
        assert_eq!(c.send_timeout, Duration::from_millis(700));
        assert_eq!(c.close_grace, Duration::from_secs(1));
        assert_eq!(c.accept_queue, 64);
        assert_eq!(c.max_audio_bytes, 32_000);
        assert_eq!(c.max_control_bytes, 4096);

        // Each bad value names its variable; the server's own rules hold across variables.
        let agent = (ENV_AGENT_FILE, "a.toml");
        for (pair, name) in [
            (
                (ENV_LISTENER_SEND_TIMEOUT, "soon"),
                ENV_LISTENER_SEND_TIMEOUT,
            ),
            ((ENV_LISTENER_CLOSE_GRACE, "0s"), "close_grace"),
            ((ENV_LISTENER_PING_TIMEOUT, "1s"), "ping_timeout"),
            ((ENV_LISTENER_ACCEPT_QUEUE, "0"), ENV_LISTENER_ACCEPT_QUEUE),
            (
                (ENV_LISTENER_MAX_CONTROL_BYTES, "100"),
                ENV_LISTENER_MAX_CONTROL_BYTES,
            ),
        ] {
            let e = HostSettings::from_env(&env(&[agent, pair])).unwrap_err();
            assert!(e.to_string().contains(name), "{name}: {e}");
        }
    }

    #[test]
    fn bad_values_name_the_variable_and_never_echo_a_secret() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        for (pairs, name) in [
            (vec![agent, (ENV_WS_ADDR, "localhost")], ENV_WS_ADDR),
            (vec![agent, (ENV_RTVI_ADDR, "nope")], ENV_RTVI_ADDR),
            // Twilio without its token or its public URL: refused, not served unsigned.
            (
                vec![agent, (ENV_TWILIO_ADDR, "127.0.0.1:1")],
                ENV_TWILIO_AUTH_TOKEN,
            ),
            (
                vec![
                    agent,
                    (ENV_TWILIO_ADDR, "127.0.0.1:1"),
                    (ENV_TWILIO_AUTH_TOKEN, "s3cr3t-token"),
                ],
                ENV_TWILIO_PUBLIC_URL,
            ),
            (
                vec![
                    agent,
                    (ENV_TWILIO_ADDR, "127.0.0.1:1"),
                    (ENV_TWILIO_AUTH_TOKEN, "s3cr3t-token"),
                    (ENV_TWILIO_PUBLIC_URL, "ftp://x"),
                    (ENV_TWILIO_ALLOW_STATIC_URL, "1"),
                ],
                ENV_TWILIO_ADDR,
            ),
        ] {
            let err = HostSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{pairs:?}: {err}");
            assert!(!err.contains("s3cr3t"), "{err}");
        }
    }

    #[cfg(not(feature = "webrtc"))]
    #[test]
    fn a_webrtc_listener_needs_the_webrtc_feature() {
        let err = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("`webrtc` feature"), "{err}");
        // `off` is fine without it.
        let s = HostSettings::from_env(&env(&[
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_WEBRTC_ADDR, "off"),
        ]))
        .unwrap();
        assert!(s.websocket.is_some());
    }

    #[cfg(feature = "webrtc")]
    #[test]
    fn reads_the_webrtc_listener() {
        let agent = (ENV_AGENT_FILE, "a.toml");
        let s = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
            (ENV_WEBRTC_TOKEN, "s3cr3t-token"),
            (ENV_WEBRTC_CORS_ORIGIN, "http://localhost:8000"),
        ]))
        .unwrap();
        let l = s.webrtc.as_ref().unwrap();
        assert_eq!(l.addr, "127.0.0.1:8088".parse().unwrap());
        let c = &l.config.config;
        assert_eq!(c.http_addr, l.addr);
        // The media socket defaults to the signaling address's IP and port number.
        assert_eq!(c.udp_addr, "127.0.0.1:8088".parse().unwrap());
        assert_eq!(c.cors_origin.as_deref(), Some("http://localhost:8000"));
        assert!(c.bearer_token.is_some());
        assert!(l.config.codec_pool.is_none());
        assert_eq!(s.websocket, None, "WebRTC is another listener");
        assert!(s.any_listener());
        assert!(
            !format!("{s:?}").contains("s3cr3t"),
            "the token stays secret"
        );

        let s = HostSettings::from_env(&env(&[
            agent,
            (ENV_WEBRTC_ADDR, "0.0.0.0:8088"),
            (ENV_WEBRTC_UDP_ADDR, "0.0.0.0:50000"),
            (ENV_WEBRTC_CANDIDATE_IP, "203.0.113.10"),
            (ENV_SESSION_SECRET, "0123456789abcdef0123456789abcdef"),
        ]))
        .unwrap();
        let c = &s.webrtc.unwrap().config.config;
        assert_eq!(c.udp_addr.port(), 50_000);
        assert_eq!(c.candidate_ip, Some("203.0.113.10".parse().unwrap()));
        assert!(c.bearer_token.is_none());
        assert!(c.sessions.is_some());

        for (pairs, name) in [
            // Browsers need an address to send media to.
            (
                vec![agent, (ENV_WEBRTC_ADDR, "0.0.0.0:8088")],
                ENV_WEBRTC_CANDIDATE_IP,
            ),
            (
                vec![
                    agent,
                    (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
                    (ENV_WEBRTC_UDP_ADDR, "nope"),
                ],
                ENV_WEBRTC_UDP_ADDR,
            ),
            (
                vec![
                    agent,
                    (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
                    (ENV_WEBRTC_CANDIDATE_IP, "not-an-ip"),
                ],
                ENV_WEBRTC_CANDIDATE_IP,
            ),
            // The transport's own checks apply (header values hold no control characters).
            (
                vec![
                    agent,
                    (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
                    (ENV_WEBRTC_CORS_ORIGIN, "http://a\r\nX: y"),
                ],
                ENV_WEBRTC_ADDR,
            ),
            // A listener needs an agent.
            (vec![(ENV_WEBRTC_ADDR, "127.0.0.1:8088")], ENV_AGENT_FILE),
        ] {
            let err = HostSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{pairs:?}: {err}");
        }
    }

    #[cfg(feature = "webrtc")]
    #[test]
    fn reads_the_websocket_fallback_path() {
        let base = [
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_WEBRTC_ADDR, "127.0.0.1:8088"),
        ];
        let path = |extra: &[(&str, &str)]| {
            let mut pairs = base.to_vec();
            pairs.extend_from_slice(extra);
            HostSettings::from_env(&env(&pairs)).map(|s| s.webrtc.unwrap().config.config.ws_path)
        };
        // On by default, at /ws.
        assert_eq!(path(&[]).unwrap().as_deref(), Some("/ws"));
        assert_eq!(
            path(&[(ENV_WEBRTC_WS_PATH, "/voice/socket")])
                .unwrap()
                .as_deref(),
            Some("/voice/socket")
        );
        assert_eq!(path(&[(ENV_WEBRTC_WS_PATH, "off")]).unwrap(), None);
        assert_eq!(path(&[(ENV_WEBRTC_WS_PATH, "OFF")]).unwrap(), None);
        // The transport's own checks apply.
        for bad in ["ws", "/ws/", "/whip", "/whip/x", "/a?b", "/"] {
            let err = path(&[(ENV_WEBRTC_WS_PATH, bad)]).unwrap_err().to_string();
            assert!(
                err.contains(ENV_WEBRTC_ADDR) && err.contains("ws_path"),
                "{bad}: {err}"
            );
        }
    }

    #[cfg(feature = "webrtc")]
    #[test]
    fn reads_the_ice_tcp_and_turn_settings() {
        let base = [
            (ENV_AGENT_FILE, "a.toml"),
            (ENV_WEBRTC_ADDR, "0.0.0.0:8088"),
            (ENV_WEBRTC_UDP_ADDR, "0.0.0.0:50000"),
            (ENV_WEBRTC_CANDIDATE_IP, "203.0.113.10"),
            // A non-loopback signaling address needs a credential.
            (ENV_WEBRTC_TOKEN, "webrtc-token"),
        ];
        let with = |extra: &[(&str, &str)]| {
            let mut pairs = base.to_vec();
            pairs.extend_from_slice(extra);
            HostSettings::from_env(&env(&pairs))
        };
        let c = with(&[]).unwrap().webrtc.unwrap().config.config;
        assert!(c.tcp_addr.is_none() && c.turn.is_none());

        let c = with(&[
            (ENV_WEBRTC_TCP_ADDR, "0.0.0.0:8443"),
            (ENV_WEBRTC_TCP_CANDIDATE_PORT, "15140"),
        ])
        .unwrap()
        .webrtc
        .unwrap()
        .config
        .config;
        assert_eq!(c.tcp_addr, Some("0.0.0.0:8443".parse().unwrap()));
        assert_eq!(c.tcp_candidate_port, Some(15_140));
        // The ICE-TCP limits default to the library's and are overridable.
        assert_eq!(
            (
                c.max_tcp_connections,
                c.max_tcp_connections_per_ip,
                c.tcp_queue
            ),
            (256, 16, 20)
        );
        let c = with(&[
            (ENV_WEBRTC_TCP_ADDR, "0.0.0.0:8443"),
            (ENV_WEBRTC_TCP_MAX_CONNECTIONS, "1000"),
            (ENV_WEBRTC_TCP_MAX_PER_IP, "0"),
            (ENV_WEBRTC_TCP_IDLE_TIMEOUT, "3s"),
            (ENV_WEBRTC_TCP_MAX_FRAME, "4096"),
            (ENV_WEBRTC_TCP_QUEUE, "128"),
        ])
        .unwrap()
        .webrtc
        .unwrap()
        .config
        .config;
        assert_eq!(c.max_tcp_connections, 1000);
        assert_eq!(c.max_tcp_connections_per_ip, 0);
        assert_eq!(c.tcp_idle_timeout, Duration::from_secs(3));
        assert_eq!((c.tcp_max_frame, c.tcp_queue), (4096, 128));
        for (name, value) in [
            (ENV_WEBRTC_TCP_MAX_CONNECTIONS, "0"),
            (ENV_WEBRTC_TCP_MAX_CONNECTIONS, "70000"),
            (ENV_WEBRTC_TCP_MAX_FRAME, "100"),
            (ENV_WEBRTC_TCP_QUEUE, "2"),
            (ENV_WEBRTC_TCP_IDLE_TIMEOUT, "soon"),
        ] {
            assert!(with(&[(name, value)]).is_err(), "{name}={value}");
        }
        assert!(
            with(&[(ENV_WEBRTC_TCP_ADDR, "off")])
                .unwrap()
                .webrtc
                .unwrap()
                .config
                .config
                .tcp_addr
                .is_none()
        );

        let c = with(&[
            (ENV_TURN_PROVIDER, "coturn"),
            (
                ENV_TURN_URLS,
                "turns:t.example.com:443?transport=tcp, turn:t.example.com:3478",
            ),
            (ENV_TURN_SECRET, "s3cr3t-turn-secret"),
            (ENV_TURN_TTL, "900"),
        ])
        .unwrap();
        assert!(
            !format!("{c:?}").contains("s3cr3t"),
            "the TURN secret stays secret"
        );
        let turn = c.webrtc.unwrap().config.config.turn.unwrap();
        assert_eq!(turn.ttl, Duration::from_secs(900));
        match turn.provider {
            TurnProvider::Coturn { urls, .. } => assert_eq!(urls.len(), 2),
            other => panic!("{other:?}"),
        }
        let turn = with(&[
            (ENV_TURN_PROVIDER, "Cloudflare"),
            (ENV_TURN_CLOUDFLARE_KEY_ID, "kid"),
            (ENV_TURN_CLOUDFLARE_API_TOKEN, "tok"),
        ])
        .unwrap()
        .webrtc
        .unwrap()
        .config
        .config
        .turn
        .unwrap();
        assert_eq!(turn.ttl, Duration::from_secs(600), "the default lifetime");
        let turn = with(&[
            (ENV_TURN_PROVIDER, "twilio"),
            (ENV_TURN_TWILIO_ACCOUNT_SID, "AC1"),
            (ENV_TURN_TWILIO_API_KEY_SID, "SK1"),
            (ENV_TURN_TWILIO_AUTH_TOKEN, "secret"),
        ])
        .unwrap()
        .webrtc
        .unwrap()
        .config
        .config
        .turn
        .unwrap();
        match turn.provider {
            TurnProvider::Twilio {
                account_sid,
                auth_user,
                ..
            } => assert_eq!((account_sid.as_str(), auth_user.as_str()), ("AC1", "SK1")),
            other => panic!("{other:?}"),
        }
        assert!(
            with(&[(ENV_TURN_PROVIDER, "off")])
                .unwrap()
                .webrtc
                .unwrap()
                .config
                .config
                .turn
                .is_none()
        );

        let provider = [
            (ENV_TURN_PROVIDER, "coturn"),
            (ENV_TURN_URLS, "turns:t.example.com:443?transport=tcp"),
            (ENV_TURN_SECRET, "s"),
        ];
        assert!(
            with(&provider)
                .unwrap()
                .webrtc
                .unwrap()
                .config
                .config
                .turn_relay
                .is_none(),
            "the relay is opt-in"
        );
        for (mode, only) in [("on", false), ("TRUE", false), ("only", true)] {
            let mut pairs = provider.to_vec();
            pairs.push((ENV_WEBRTC_TURN_RELAY, mode));
            pairs.push((ENV_WEBRTC_TURN_RELAY_TIMEOUT, "2s"));
            let relay = with(&pairs)
                .unwrap()
                .webrtc
                .unwrap()
                .config
                .config
                .turn_relay
                .unwrap();
            assert_eq!(relay.only, only, "{mode}");
            assert_eq!(relay.setup_timeout, Duration::from_secs(2));
        }

        for (extra, name) in [
            (vec![(ENV_WEBRTC_TURN_RELAY, "on")], ENV_TURN_PROVIDER),
            (
                [&provider[..], &[(ENV_WEBRTC_TURN_RELAY, "maybe")]].concat(),
                ENV_WEBRTC_TURN_RELAY,
            ),
            (vec![(ENV_WEBRTC_TCP_ADDR, "nope")], ENV_WEBRTC_TCP_ADDR),
            (
                vec![
                    (ENV_WEBRTC_TCP_ADDR, "0.0.0.0:1"),
                    (ENV_WEBRTC_TCP_CANDIDATE_PORT, "0"),
                ],
                ENV_WEBRTC_TCP_CANDIDATE_PORT,
            ),
            (vec![(ENV_TURN_PROVIDER, "bogus")], ENV_TURN_PROVIDER),
            (vec![(ENV_TURN_PROVIDER, "coturn")], ENV_TURN_URLS),
            (
                vec![
                    (ENV_TURN_PROVIDER, "coturn"),
                    (ENV_TURN_URLS, "turn:t.example.com"),
                ],
                ENV_TURN_SECRET,
            ),
            (
                vec![
                    (ENV_TURN_PROVIDER, "cloudflare"),
                    (ENV_TURN_CLOUDFLARE_KEY_ID, "k"),
                ],
                ENV_TURN_CLOUDFLARE_API_TOKEN,
            ),
            (
                vec![
                    (ENV_TURN_PROVIDER, "twilio"),
                    (ENV_TURN_TWILIO_ACCOUNT_SID, "AC1"),
                ],
                ENV_TURN_TWILIO_AUTH_TOKEN,
            ),
            (
                vec![
                    (ENV_TURN_PROVIDER, "coturn"),
                    (ENV_TURN_URLS, "turn:t.example.com"),
                    (ENV_TURN_SECRET, "s"),
                    (ENV_TURN_TTL, "5"),
                ],
                ENV_TURN_TTL,
            ),
            // The transport's own validation (a URL that is not turn:/turns:).
            (
                vec![
                    (ENV_TURN_PROVIDER, "coturn"),
                    (ENV_TURN_URLS, "http://t.example.com"),
                    (ENV_TURN_SECRET, "s"),
                ],
                ENV_TURN_PROVIDER,
            ),
        ] {
            let err = with(&extra).unwrap_err().to_string();
            assert!(err.contains(name), "{extra:?}: {err}");
        }
    }
}
