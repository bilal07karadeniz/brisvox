//! The WebSocket fallback on the signaling port.
//!
//! A browser on a network that passes only HTTP(S) (a strict corporate network, a host with a
//! single proxied HTTP port such as Railway) cannot get WebRTC media through. The same listener
//! therefore also answers a WebSocket upgrade (RFC 6455) on [`WebRtcConfig::ws_path`] and serves
//! the generic WebSocket audio protocol (`docs/WEBSOCKET_PROTOCOL.md`) over the upgraded stream:
//! the connection and call code is [`crate::websocket`]'s, not a copy. Only the HTTP side differs
//! from the generic listener, because the request was already read by this module's HTTP parser:
//! the handshake is answered here and the raw stream is handed over.
//!
//! # What is checked, in this order
//! 1. `GET` with the upgrade headers (`Upgrade: websocket`, `Connection: Upgrade`,
//!    `Sec-WebSocket-Version: 13`, a 16-byte base64 `Sec-WebSocket-Key`): `405`, `426`, `400`.
//! 2. The `Origin` header, when the browser sent one (see [`origin_allowed`]): `403`.
//! 3. The credential, with the signaling listener's own [`Authenticator`](crate::auth) (the same
//!    static token and session tokens, audience `brisvox-webrtc`, as `POST <whip_path>`): in
//!    `Authorization: Bearer`, the subprotocol `brisvox-token.<token>` or `?token=`. A missing
//!    one is `401`, an invalid, expired, reused or wrong-audience one `403`, an oversize one
//!    `413`. A session token is only peeked here.
//! 4. The offer rate of the client address ([`WebRtcConfig::offers_per_minute_per_ip`], shared
//!    with `POST`): `429`.
//! 5. A call slot (`503`), a slot in the handshake pool (`503`), the per-address call cap
//!    ([`WebRtcConfig::max_calls_per_ip`]; `429`), then the session token is used up.
//!
//! Every refusal is answered before the upgrade, as plain HTTP. After `101` the connection leaves
//! the signaling pool of HTTP connections (it is a call now, bounded by `max_calls`), and the
//! generic protocol runs: `start` within five seconds, ping/pong keepalive, bounded frames,
//! bounded queues, `max_s` of a session token enforced.
//!
//! A session token that opened a WebRTC session which then never connected is given back when the
//! page hangs the session up with `DELETE` (before the `200`; once per session id, see
//! `SessionAuth::refund`), so a page that falls back from WebRTC to this endpoint can present the
//! same token again. A timeout, a close or a connected session keeps the token spent.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::http::{Request, Response, write_switching_protocols};
use super::peer::SessionGuard;
use super::{Counters, Shared, signaling};
use crate::auth::{self, AuthError, Grant};
use crate::websocket::upgrade_auth::query_param;
use crate::websocket::{Admitted, ServerConfig, Shared as WsShared, connection};

/// Seconds a refused client is told to wait at the call limit.
const RETRY_AFTER_SECONDS: &str = "1";

/// The WebSocket side of a [`Shared`]: where it is served and the websocket transport's state.
pub(crate) struct Fallback {
    /// The path of the upgrade, from [`WebRtcConfig::ws_path`].
    pub(crate) path: String,
    pub(crate) ws: Arc<WsShared>,
}

impl Fallback {
    /// The websocket transport's settings for a call on the signaling port: its timeouts follow
    /// the signaling listener's, the rest are the generic listener's defaults.
    pub(crate) fn server_config(config: &super::WebRtcConfig) -> ServerConfig {
        ServerConfig {
            bind_addr: config.http_addr,
            max_calls: config.max_calls,
            handshake_timeout: config.request_timeout,
            write_timeout: config.write_timeout,
            send_timeout: config.send_timeout,
            accept_queue: config.accept_queue,
            ..ServerConfig::default()
        }
    }
}

/// An upgrade that passed every check: answer `101` and run the call.
pub(crate) struct Accepted {
    accept_key: String,
    subprotocol: Option<String>,
    grant: Grant,
    guard: SessionGuard,
    handshake: tokio::sync::OwnedSemaphorePermit,
    client: IpAddr,
}

/// Whether `value` (a header that lists tokens) names `token`, ignoring case.
fn lists(value: Option<String>, token: &str) -> bool {
    value.is_some_and(|value| {
        value
            .split(',')
            .any(|entry| entry.trim().eq_ignore_ascii_case(token))
    })
}

/// The `Origin` rule. A WebSocket is not subject to CORS, so the browser's `Origin` is checked
/// here, to the same effect as the CORS headers of `POST`: a page may open the call only when the
/// signaling endpoint would let it read the answer.
/// - `cors_origin` `*`: any origin;
/// - `cors_origin` an origin: that origin (ignoring case), or the page served from this very host;
/// - no `cors_origin`: only a page served from this very host (the `Origin`'s authority equals
///   the request's `Host`; the scheme is not compared, a TLS proxy hides it).
///
/// A request without `Origin` (not a browser) is judged by its credential alone.
fn origin_allowed(config: &super::WebRtcConfig, request: &Request) -> bool {
    let Some(origin) = request.header("origin") else {
        return true;
    };
    if config.cors_origin.as_deref() == Some("*") {
        return true;
    }
    if config
        .cors_origin
        .as_deref()
        .is_some_and(|allowed| allowed.eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    request
        .header("host")
        .is_some_and(|host| host.eq_ignore_ascii_case(authority))
}

/// The credential of the upgrade request, wherever it travels (the three places of the generic
/// listener, see `crate::websocket::upgrade_auth`).
fn credential(request: &Request) -> Option<String> {
    request
        .header("authorization")
        .and_then(auth::bearer)
        .map(str::to_owned)
        .or_else(|| {
            request
                .header_joined("sec-websocket-protocol")
                .and_then(|list| auth::subprotocol_token(&list).map(str::to_owned))
        })
        .or_else(|| {
            request
                .query
                .as_deref()
                .and_then(|query| query_param(query, "token"))
        })
}

fn refuse(shared: &Shared, response: Response) -> Response {
    Counters::bump(&shared.stats.ws_refused);
    response
}

/// Checks an upgrade request and reserves everything the call needs. `Err` is the HTTP answer.
pub(crate) fn prepare(
    shared: &Arc<Shared>,
    request: &Request,
    client: IpAddr,
) -> Result<Accepted, Response> {
    let config = &shared.config;
    if request.method != "GET" {
        return Err(refuse(
            shared,
            Response::text(405, "method not allowed").header("Allow", "GET"),
        ));
    }
    let upgrade = lists(request.header_joined("upgrade"), "websocket")
        && lists(request.header_joined("connection"), "upgrade");
    if !upgrade {
        return Err(refuse(
            shared,
            Response::text(426, "websocket upgrade required").header("Upgrade", "websocket"),
        ));
    }
    if request.header("sec-websocket-version") != Some("13") {
        return Err(refuse(
            shared,
            Response::text(426, "websocket version 13 required")
                .header("Sec-WebSocket-Version", "13"),
        ));
    }
    let key = request.header("sec-websocket-key").unwrap_or_default();
    if STANDARD.decode(key).map_or(true, |bytes| bytes.len() != 16) {
        return Err(refuse(shared, Response::text(400, "bad Sec-WebSocket-Key")));
    }
    if !origin_allowed(config, request) {
        tracing::debug!("webrtc websocket upgrade refused: origin not allowed");
        Counters::bump(&shared.stats.offers_unauthorized);
        return Err(refuse(shared, Response::text(403, "origin not allowed")));
    }
    let grant = shared
        .auth
        .peek(credential(request).as_deref())
        .map_err(|error| {
            tracing::debug!(%error, "webrtc websocket upgrade not authorized");
            Counters::bump(&shared.stats.offers_unauthorized);
            refuse(shared, refusal_for(error))
        })?;
    signaling::check_offer_rate(shared, client).map_err(|response| refuse(shared, response))?;
    let Ok(permit) = Arc::clone(&shared.permits).try_acquire_owned() else {
        return Err(refuse(
            shared,
            Response::text(503, "server busy").header("Retry-After", RETRY_AFTER_SECONDS),
        ));
    };
    let Ok(handshake) = Arc::clone(&shared.handshake_permits).try_acquire_owned() else {
        Counters::bump(&shared.stats.offers_handshake_limited);
        return Err(refuse(
            shared,
            Response::text(503, "too many calls connecting")
                .header("Retry-After", RETRY_AFTER_SECONDS),
        ));
    };
    let Some(id) = signaling::new_session_id(shared) else {
        return Err(refuse(shared, Response::text(500, "no randomness")));
    };
    let Some(guard) = SessionGuard::reserve(shared, id, permit, client) else {
        Counters::bump(&shared.stats.offers_ip_limited);
        return Err(refuse(
            shared,
            Response::text(429, "too many calls from this address")
                .header("Retry-After", RETRY_AFTER_SECONDS),
        ));
    };
    // Used up only now that the call has a slot: a refusal for capacity above does not burn it.
    if let Err(error) = shared.auth.consume(&grant) {
        tracing::debug!(%error, "webrtc websocket session token not accepted");
        Counters::bump(&shared.stats.offers_unauthorized);
        return Err(refuse(shared, refusal_for(error)));
    }
    let subprotocol = request
        .header_joined("sec-websocket-protocol")
        .and_then(|list| auth::selected_subprotocol(&list))
        .filter(|selected| selected.bytes().all(|b| b.is_ascii_graphic()));
    Ok(Accepted {
        accept_key: derive_accept_key(key.as_bytes()),
        subprotocol,
        grant,
        guard,
        handshake,
        client,
    })
}

/// The answer for a refused credential, as the generic listener gives it.
fn refusal_for(error: AuthError) -> Response {
    match error {
        AuthError::Missing => {
            Response::text(401, "token required").header("WWW-Authenticate", "Bearer")
        }
        AuthError::TooLarge => Response::text(413, "token too large"),
        AuthError::Busy => {
            Response::text(503, "server busy").header("Retry-After", RETRY_AFTER_SECONDS)
        }
        _ => Response::text(403, "invalid token"),
    }
}

/// Answers `101` and runs the call to its end on its own task (the connection leaves the
/// signaling accept task, so a server shutdown says goodbye to it through the websocket
/// transport instead of dropping it). Returns `false` when the answer could not be written.
pub(crate) async fn upgrade(
    shared: &Arc<Shared>,
    fallback: &Fallback,
    mut stream: TcpStream,
    peer: SocketAddr,
    accepted: Accepted,
) -> bool {
    let Accepted {
        accept_key,
        subprotocol,
        grant,
        guard,
        handshake,
        client,
    } = accepted;
    let written = write_switching_protocols(
        &mut stream,
        &*shared.clock,
        shared.config.write_timeout,
        &accept_key,
        subprotocol.as_deref(),
    )
    .await;
    if !written {
        return false;
    }
    Counters::bump(&shared.stats.ws_upgrades);
    let _ = stream.set_nodelay(true);
    let max_duration: Option<Duration> = grant.claims().and_then(auth::SessionClaims::max_duration);
    let ws_shared = Arc::clone(&fallback.ws);
    // The call's address in its metadata and logs is the client's, not the proxy's.
    let client_addr = SocketAddr::new(client, peer.port());
    shared.tracker.spawn(async move {
        let config = connection::ws_config(&ws_shared.config);
        let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
        let admitted = Admitted::hosted(Some(handshake));
        connection::run_upgraded(
            ws,
            client_addr,
            admitted,
            ws_shared,
            Some(grant),
            max_duration,
            "webrtc-ws",
        )
        .await;
        drop(guard);
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin_request(origin: Option<&str>, host: Option<&str>) -> Request {
        Request::for_test(
            "GET",
            "/ws",
            &[("Origin", origin), ("Host", host)]
                .into_iter()
                .filter_map(|(name, value)| value.map(|value| (name, value)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_origin_rule() {
        let none = super::super::WebRtcConfig::default();
        let allowed = |config: &super::super::WebRtcConfig, origin, host| {
            origin_allowed(config, &origin_request(origin, host))
        };
        // No Origin: not a browser, the credential decides.
        assert!(allowed(&none, None, Some("voice.example.com")));
        // Same host: allowed, whatever the scheme.
        assert!(allowed(
            &none,
            Some("https://voice.example.com"),
            Some("voice.example.com")
        ));
        assert!(allowed(
            &none,
            Some("http://localhost:8088"),
            Some("localhost:8088")
        ));
        assert!(allowed(
            &none,
            Some("https://VOICE.example.com"),
            Some("voice.example.com")
        ));
        // Another site: refused without a CORS origin.
        assert!(!allowed(
            &none,
            Some("https://evil.example"),
            Some("voice.example.com")
        ));
        assert!(!allowed(&none, Some("https://evil.example"), None));
        assert!(!allowed(
            &none,
            Some("https://voice.example.com.evil.example"),
            Some("voice.example.com")
        ));
        let pinned = super::super::WebRtcConfig {
            cors_origin: Some("https://app.example".to_owned()),
            ..super::super::WebRtcConfig::default()
        };
        assert!(allowed(&pinned, Some("https://app.example"), Some("api")));
        assert!(allowed(&pinned, Some("https://APP.example"), Some("api")));
        assert!(!allowed(&pinned, Some("https://evil.example"), Some("api")));
        let star = super::super::WebRtcConfig {
            cors_origin: Some("*".to_owned()),
            bearer_token: Some(brisvox_core::Secret::new("t".to_owned())),
            ..super::super::WebRtcConfig::default()
        };
        assert!(allowed(
            &star,
            Some("https://anything.example"),
            Some("api")
        ));
    }
}
