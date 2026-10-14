//! WHIP-style signaling: the HTTP side of the WebRTC transport.
//!
//! See the module docs of [`super`] for the request/response contract. This file has the accept
//! loop, the routing, the offer checks and the creation of a call.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ring::rand::SecureRandom;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::net::TcpType;
use str0m::{Candidate, Rtc};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::http::{ReadError, Request, Response, close_politely, read_request, write_response};
use super::limits;
use super::media::MediaPath;
use super::peer::{Peer, PeerParts, SessionGuard};
use super::tcp::IpCounts;
use super::{Counters, DEMO_HTML, DEMO_MODULE_JS, Shared};
use crate::auth::{self, AuthError, Grant};
use crate::channel;

/// Pause after a failed `accept` (descriptor exhaustion), so the loop does not spin.
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Seconds a refused client is told to wait at the call limit.
const RETRY_AFTER_SECONDS: &str = "1";

/// Accepts signaling connections until cancelled.
pub(crate) async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let slots = Arc::new(Semaphore::new(shared.config.max_http_connections));
    let per_ip = Arc::new(IpCounts::default());
    loop {
        let accepted = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => {
                // Over the limit: close without an answer (no work is spent on the stranger).
                if let Ok(slot) = Arc::clone(&slots).try_acquire_owned() {
                    // One address cannot hold every slot with idle connections (an IPv6 address
                    // counts as its /64). A trusted proxy's connections are many visitors':
                    // exempt, the proxy's own limits apply.
                    let ip_slot = if limits::is_trusted_proxy(
                        &shared.config.trusted_proxies,
                        peer.ip(),
                    ) {
                        None
                    } else {
                        let Some(ip_slot) = per_ip.acquire(
                            limits::rate_key(peer.ip()),
                            shared.config.max_http_connections_per_ip,
                        ) else {
                            tracing::debug!(%peer, "webrtc signaling connection refused: address at its limit");
                            continue;
                        };
                        Some(ip_slot)
                    };
                    let shared_for_task = Arc::clone(&shared);
                    shared.tracker.spawn(async move {
                        let slots = HttpSlots {
                            _slot: slot,
                            _ip_slot: ip_slot,
                        };
                        tokio::select! {
                            biased;
                            () = shared_for_task.cancel.cancelled() => {}
                            () = serve_connection(stream, peer, &shared_for_task, slots) => {}
                        }
                    });
                }
            }
            Err(error) => {
                tracing::warn!(%error, "webrtc signaling accept failed");
                tokio::select! {
                    biased;
                    () = shared.cancel.cancelled() => return,
                    () = shared.clock.sleep(ACCEPT_ERROR_BACKOFF) => {}
                }
            }
        }
    }
}

/// What a signaling connection holds from the HTTP connection pools: dropped with it, or at a
/// WebSocket upgrade (the connection is a call from then on, bounded by the call limits).
struct HttpSlots {
    _slot: tokio::sync::OwnedSemaphorePermit,
    _ip_slot: Option<super::tcp::IpSlot>,
}

/// Reads one request, answers it, closes. A WebSocket upgrade on [`Fallback::path`] is answered
/// with `101` and the connection becomes a call (`ws.rs`).
async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    slots: HttpSlots,
) {
    let config = &shared.config;
    let request = read_request(
        &mut stream,
        &*shared.clock,
        config.request_timeout,
        config.max_sdp_bytes,
    )
    .await;
    let response = match request {
        Ok(request) => {
            let client = limits::client_ip(
                peer.ip(),
                &config.trusted_proxies,
                config.client_ip_header,
                request
                    .header_joined(config.client_ip_header.header_name())
                    .as_deref(),
            );
            if let Some(fallback) = &shared.ws
                && request.path.trim_end_matches('/') == fallback.path
            {
                match super::ws::prepare(shared, &request, client) {
                    Ok(accepted) => {
                        // The HTTP connection pools' slots are given back: from here the
                        // connection is a call.
                        drop(slots);
                        super::ws::upgrade(shared, fallback, stream, peer, accepted).await;
                        return;
                    }
                    Err(response) => response,
                }
            } else {
                handle(shared, &request, client).await
            }
        }
        Err(ReadError::Gone) => return,
        Err(ReadError::Slow) => {
            Counters::bump(&shared.stats.bad_requests);
            Response::text(408, "request timeout")
        }
        Err(ReadError::Refuse(response)) => {
            Counters::bump(&shared.stats.bad_requests);
            response
        }
    };
    tracing::debug!(%peer, status = response.status, "webrtc signaling request");
    // Responses carry session URLs and credentials' verdicts: nothing may cache them.
    let response = with_cors(
        shared,
        response
            .header("Cache-Control", "no-store")
            // Nothing this server sends is ever to be reinterpreted as another type.
            .header("X-Content-Type-Options", "nosniff"),
    );
    write_response(&mut stream, &*shared.clock, config.write_timeout, &response).await;
    close_politely(&mut stream, &*shared.clock).await;
}

/// Adds the CORS headers when an origin is configured.
fn with_cors(shared: &Shared, response: Response) -> Response {
    match &shared.config.cors_origin {
        Some(origin) => response
            .header("Access-Control-Allow-Origin", origin.clone())
            .header("Access-Control-Expose-Headers", "Location, ETag, Link"),
        None => response,
    }
}

/// Routes a request.
async fn handle(shared: &Arc<Shared>, request: &Request, client: IpAddr) -> Response {
    let config = &shared.config;
    let path = request.path.trim_end_matches('/');
    let session = path
        .strip_prefix(config.whip_path.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|id| !id.is_empty() && !id.contains('/'));
    if path == config.whip_path {
        return match request.method.as_str() {
            "POST" => whip_post(shared, request, client).await,
            "OPTIONS" => options(shared, "POST, OPTIONS"),
            _ => method_not_allowed("POST, OPTIONS"),
        };
    }
    if path == ice_path(&config.whip_path) {
        return match request.method.as_str() {
            "GET" => ice_get(shared, request, client).await,
            "OPTIONS" => options(shared, "GET, OPTIONS"),
            _ => method_not_allowed("GET, OPTIONS"),
        };
    }
    if let Some(id) = session {
        return match request.method.as_str() {
            "DELETE" => whip_delete(shared, request, id),
            "OPTIONS" => options(shared, "DELETE, OPTIONS"),
            // PATCH (trickle ICE, ICE restart) is not supported: signaling is non-trickle.
            _ => method_not_allowed("DELETE, OPTIONS"),
        };
    }
    if let Some(demo) = &config.demo_path
        && path == demo.trim_end_matches('/')
    {
        let module = config.demo_module_path().unwrap_or_default();
        return match request.method.as_str() {
            "GET" => {
                // The page's one inline script is allowed by a fresh nonce; nothing else inline
                // may run, and nothing loads from another origin.
                let Some(nonce) = new_session_id(shared) else {
                    return Response::text(500, "no randomness");
                };
                Response::new(200)
                    .header("Content-Security-Policy", demo_page_csp(&nonce))
                    .header("Referrer-Policy", "no-referrer")
                    .body(
                        "text/html; charset=utf-8",
                        DEMO_HTML
                            .replace("__WHIP_PATH__", &config.whip_path)
                            .replace("__WS_PATH__", config.ws_path.as_deref().unwrap_or_default())
                            .replace("__MODULE_PATH__", &module)
                            .replace("__NONCE__", &nonce)
                            .into_bytes(),
                    )
            }
            _ => method_not_allowed("GET"),
        };
    }
    // The demo page's script: the shared browser module (`sdk/web/brisvox-call.js`).
    if config
        .demo_module_path()
        .is_some_and(|module| path == module)
    {
        return match request.method.as_str() {
            "GET" => Response::new(200)
                .header("Content-Security-Policy", DEMO_MODULE_CSP)
                .body(
                    "text/javascript; charset=utf-8",
                    DEMO_MODULE_JS.as_bytes().to_vec(),
                ),
            _ => method_not_allowed("GET"),
        };
    }
    Response::text(404, "not found")
}

/// The demo page's Content-Security-Policy. Scripts: the module from the same origin, the one
/// inline script by `nonce`, and `blob:` because the module loads its AudioWorklet from a Blob URL
/// (a worklet is governed by `script-src`; `worker-src` is there for browsers that apply it).
/// Network: the same origin only (signaling and the WebSocket fallback). Styles: the page's inline
/// `<style>`. Nothing may frame the page, change its base URL or submit a form.
pub(crate) fn demo_page_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; script-src 'self' blob: 'nonce-{nonce}'; worker-src blob:; \
         style-src 'unsafe-inline'; connect-src 'self'; media-src 'self' blob: mediastream:; \
         img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

/// The Content-Security-Policy of the served JavaScript module: it is a script, never a document.
pub(crate) const DEMO_MODULE_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// Where the browser reads its ICE servers: `<whip_path>/ice`. (Session ids are 32 hex digits,
/// so `ice` never names a session.)
fn ice_path(whip_path: &str) -> String {
    format!("{whip_path}/ice")
}

/// `GET <whip_path>/ice`: the TURN servers for a page to pass to `RTCPeerConnection`, in the
/// browser's `RTCIceServer` shape: `{"iceServers":[...]}`. An empty list when no relay is
/// configured or the provider is down (the page then connects directly). The credentials are
/// short-lived and per request, so the answer is `no-store`. Same bearer check as `POST`.
async fn ice_get(shared: &Shared, request: &Request, client: IpAddr) -> Response {
    // Checked like a `POST` but never used up: the page fetches the ICE servers with the same
    // single-use session token it then sends with its offer.
    let grant = match authenticate(shared, request) {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    // A per-client token bucket of its own (this endpoint mints relay credentials; the offer
    // limiter of `POST` counts offers, which a page makes once per call after this request).
    if let Some(turn) = &shared.turn
        && !turn.allow_ice_request(client)
    {
        return Response::text(429, "too many requests").header("Retry-After", "2");
    }
    let servers = match (&shared.turn, turn_label(shared, &grant)) {
        (Some(turn), Some(label)) => turn.servers(&label).await,
        _ => Vec::new(),
    };
    Response::new(200).header("Cache-Control", "no-store").body(
        "application/json",
        super::turn_creds::ice_servers_json(&servers),
    )
}

/// What a coturn username carries after the expiry: the label of the request's session token
/// (traceable to the application's own session id), else a random id.
fn turn_label(shared: &Shared, grant: &Grant) -> Option<String> {
    match grant.claims() {
        Some(claims) => Some(super::turn_creds::session_label(&claims.sid)),
        None => new_session_id(shared),
    }
}

fn method_not_allowed(allow: &'static str) -> Response {
    Response::text(405, "method not allowed").header("Allow", allow)
}

/// `OPTIONS`: what the endpoint accepts (RFC 9725 section 4.2) and the CORS preflight answer.
fn options(shared: &Shared, allow: &'static str) -> Response {
    let mut response = Response::new(200).header("Allow", allow);
    if allow.contains("POST") {
        response = response.header("Accept-Post", "application/sdp");
    }
    if shared.config.cors_origin.is_some() {
        response = response
            .header("Access-Control-Allow-Methods", allow)
            .header(
                "Access-Control-Allow-Headers",
                "Content-Type, Authorization",
            )
            .header("Access-Control-Max-Age", "600");
    }
    response
}

/// The credential in the `Authorization: Bearer` header, if any.
fn presented(request: &Request) -> Option<&str> {
    request.header("authorization").and_then(auth::bearer)
}

/// Checks the credential of a `POST` without using a session token up (it is used up once the
/// offer is accepted for processing, see [`whip_post`]).
fn authenticate(shared: &Shared, request: &Request) -> Result<Grant, Response> {
    shared.auth.peek(presented(request)).map_err(|error| {
        tracing::debug!(%error, "webrtc offer not authorized");
        refusal_for(error)
    })
}

/// The response for a refused credential: `401` with `WWW-Authenticate` (`413` for an oversize
/// token, `503` when the single-use set is full). The reason is not told to the client, except
/// that a token is wanted.
fn refusal_for(error: AuthError) -> Response {
    match error {
        AuthError::TooLarge => Response::text(413, "token too large"),
        AuthError::Busy => {
            Response::text(503, "server busy").header("Retry-After", RETRY_AFTER_SECONDS)
        }
        AuthError::Missing => unauthorized(),
        _ => Response::text(401, "unauthorized")
            .header("WWW-Authenticate", "Bearer error=\"invalid_token\""),
    }
}

fn unauthorized() -> Response {
    Response::text(401, "unauthorized").header("WWW-Authenticate", "Bearer")
}

/// `DELETE` is authorized by the session URL when session tokens are in use (the id is 128
/// random bits from the system RNG, known only to the client the `201` went to, and the token
/// that opened the call is single use and may have expired by hang-up time). With only a static
/// token (or none) it keeps the rule of RFC 9725: the same bearer token as `POST`.
fn whip_delete(shared: &Shared, request: &Request, id: &str) -> Response {
    if !shared.auth.accepts_sessions()
        && let Err(response) = authenticate(shared, request)
    {
        Counters::bump(&shared.stats.offers_unauthorized);
        return response;
    }
    let hangup = shared
        .registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .hangup(id);
    match hangup {
        Some(hangup) => {
            // A session that never connected opened no call: its token is given back here, before
            // the `200`, so the page's WebSocket fallback right after it cannot lose a race with
            // the call task winding down. (Once per session id: see `SessionAuth::refund`. A
            // session that connected keeps its token spent.)
            if let Some(claims) = &hangup.unconnected_claims {
                shared.auth.refund(&Grant::Session(claims.clone()));
            }
            hangup.token.cancel();
            Response::new(200)
        }
        None => Response::text(404, "no such session"),
    }
}

/// Why an offer was refused.
#[derive(Debug)]
pub(crate) enum OfferError {
    /// The SDP is unusable: `400` with this text.
    Bad(String),
    /// Our side failed: `500`.
    Internal(String),
}

/// Counts one offer (a `POST` or a WebSocket upgrade) against `client`'s rate; `429` with
/// `Retry-After` when the address has used its allowance.
pub(super) fn check_offer_rate(shared: &Shared, client: IpAddr) -> Result<(), Response> {
    let Some(rate) = &shared.offer_rate else {
        return Ok(());
    };
    let (allowed, retry, full) = {
        let mut rate = rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = rate.table_full;
        let allowed = rate.allow(client, shared.clock.now());
        (allowed, rate.retry_after_secs(), rate.table_full != before)
    };
    if full {
        Counters::bump(&shared.stats.rate_table_full);
    }
    if allowed {
        Ok(())
    } else {
        Counters::bump(&shared.stats.offers_rate_limited);
        Err(Response::text(429, "too many offers from this address")
            .header("Retry-After", retry.to_string()))
    }
}

async fn whip_post(shared: &Arc<Shared>, request: &Request, client: IpAddr) -> Response {
    let refused = |response: Response| {
        Counters::bump(&shared.stats.offers_refused);
        response
    };
    let grant = match authenticate(shared, request) {
        Ok(grant) => grant,
        Err(response) => {
            Counters::bump(&shared.stats.offers_unauthorized);
            return refused(response);
        }
    };
    let content_type = request
        .header("content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/sdp")) {
        return refused(
            Response::text(415, "Content-Type must be application/sdp")
                .header("Accept-Post", "application/sdp"),
        );
    }
    let Ok(offer) = std::str::from_utf8(&request.body) else {
        return refused(Response::text(400, "the offer is not UTF-8"));
    };
    if let Err(response) = check_offer_rate(shared, client) {
        return refused(response);
    }
    let Ok(permit) = Arc::clone(&shared.permits).try_acquire_owned() else {
        return refused(
            Response::text(503, "server busy").header("Retry-After", RETRY_AFTER_SECONDS),
        );
    };
    // Offers that never connect hold a call slot until `connect_timeout`: a smaller pool for
    // calls still in their handshake keeps them from taking every slot.
    let Ok(handshake) = Arc::clone(&shared.handshake_permits).try_acquire_owned() else {
        Counters::bump(&shared.stats.offers_handshake_limited);
        return refused(
            Response::text(503, "too many calls connecting")
                .header("Retry-After", RETRY_AFTER_SECONDS),
        );
    };
    let Some(id) = new_session_id(shared) else {
        return refused(Response::text(500, "no randomness"));
    };
    // The guard frees the slot, the per-address count and the registry entry on every exit.
    let Some(guard) = SessionGuard::reserve(shared, id.clone(), permit, client) else {
        Counters::bump(&shared.stats.offers_ip_limited);
        return refused(
            Response::text(429, "too many calls from this address")
                .header("Retry-After", RETRY_AFTER_SECONDS),
        );
    };
    // The credential is used up only now that the call has a slot: a refusal for capacity above
    // does not burn the client's token (nor does a relay-only 503 below, which refunds it). A
    // second request with the same session id loses here.
    if let Err(error) = shared.auth.consume(&grant) {
        tracing::debug!(%error, "webrtc session token not accepted");
        Counters::bump(&shared.stats.offers_unauthorized);
        return refused(refusal_for(error));
    }
    let claims = grant.claims().cloned();
    let offer = offer.to_owned();
    let (queue_tx, queue_rx) = channel::bounded(shared.config.inbound_queue);
    // The server's own TURN allocation, with permissions for the offer's peers, before
    // the answer exists (the browser sends the first connectivity check).
    let relay = match super::relay::open_for_offer(shared, &id, &offer, &queue_tx).await {
        Ok(relay) => relay,
        Err(super::relay::RelayUnavailable) => {
            // The server could not serve the call (a capacity-class refusal like the ones above):
            // the client may retry with the same single-use token.
            shared.auth.refund(&grant);
            return refused(
                Response::text(503, "media relay unavailable")
                    .header("Retry-After", RETRY_AFTER_SECONDS),
            );
        }
    };
    let relayed = relay.as_ref().map(super::relay::Relay::relayed);
    let now = shared.clock.now().into_std();
    // SDP parsing, ICE credentials, the codecs and the resamplers cost a few hundred
    // microseconds together: not on a tokio worker.
    let build_shared = Arc::clone(shared);
    // The TURN credentials (a cache hit, or a bounded fetch) are fetched while the peer is built.
    // A session token names the credentials' label; otherwise the call's own id does.
    let turn_label = grant.claims().map_or_else(
        || id.clone(),
        |claims| super::turn_creds::session_label(&claims.sid),
    );
    let turn = async {
        match &shared.turn {
            // Bounded: a provider outage must not stall the offer (the page has its servers
            // from `GET .../ice` already).
            Some(turn) => {
                turn.servers_bounded(&turn_label, super::turn_creds::SIGNALING_BUDGET)
                    .await
            }
            None => Vec::new(),
        }
    };
    let (built, ice_servers) = tokio::join!(
        tokio::task::spawn_blocking(move || build_peer_with(&build_shared, &offer, now, relayed)),
        turn
    );
    let setup = match built {
        Ok(Ok(setup)) => setup,
        Ok(Err(OfferError::Bad(message))) => {
            tracing::debug!(%message, "webrtc offer refused");
            return refused(Response::text(400, &message));
        }
        Ok(Err(OfferError::Internal(message))) => {
            tracing::warn!(%message, "webrtc session setup failed");
            return refused(Response::text(500, "session setup failed"));
        }
        Err(error) => {
            tracing::error!(%error, "webrtc session setup panicked");
            return refused(Response::text(500, "session setup failed"));
        }
    };

    let hangup = CancellationToken::new();
    let connected = Arc::new(AtomicBool::new(false));
    {
        let mut registry = shared
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.insert(
            id.clone(),
            setup.ufrag,
            hangup.clone(),
            Arc::clone(&connected),
            claims.clone(),
            queue_tx,
        );
    }
    Counters::bump(&shared.stats.sessions_created);
    let peer = Peer::new(PeerParts {
        shared: Arc::clone(shared),
        rtc: setup.rtc,
        mid: setup.mid,
        pt: setup.pt,
        can_send: setup.can_send,
        path: setup.path,
        inbound: queue_rx,
        hangup,
        connected,
        cancel: shared.cancel.child_token(),
        created: shared.clock.now(),
        handshake: Some(handshake),
        claims,
        relay,
    });
    shared.tracker.spawn(peer.run_guarded(guard));

    // RFC 9725 section 4.6: STUN/TURN servers as `Link: <url>; rel="ice-server"` on the 201.
    let mut response =
        Response::new(201).header("Location", format!("{}/{id}", shared.config.whip_path));
    for link in super::turn_creds::link_headers(&ice_servers) {
        response = response.header("Link", link);
    }
    response.body("application/sdp", setup.answer.into_bytes())
}

/// 128 random bits as hex: the session URL is the capability to hang up.
pub(super) fn new_session_id(shared: &Shared) -> Option<String> {
    let mut bytes = [0u8; 16];
    shared.random.fill(&mut bytes).ok()?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push(char::from_digit(u32::from(byte >> 4), 16)?);
        id.push(char::from_digit(u32::from(byte & 0xf), 16)?);
    }
    Some(id)
}

/// Everything `POST` needs from a built session.
pub(crate) struct Setup {
    pub(crate) rtc: Rtc,
    pub(crate) mid: Mid,
    pub(crate) pt: Pt,
    pub(crate) can_send: bool,
    pub(crate) path: MediaPath,
    pub(crate) answer: String,
    pub(crate) ufrag: String,
}

/// Removes the candidate lines an `str0m` ICE agent cannot use: browsers hide their host
/// addresses behind mDNS names (`<uuid>.local`) that the server cannot resolve. They are not
/// needed: the server learns the browser's real address from its STUN requests.
pub(crate) fn strip_unusable_candidates(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let unusable = trimmed.strip_prefix("a=candidate:").is_some_and(|rest| {
            // foundation component transport priority address port typ ...
            rest.split(' ')
                .nth(4)
                .is_some_and(|address| address.parse::<std::net::IpAddr>().is_err())
        });
        if !unusable {
            out.push_str(line);
        }
    }
    out
}

/// The `a=mid` of the first `m=audio` section of `sdp` that is not rejected (port 0).
fn audio_mid(sdp: &str) -> Option<Mid> {
    let mut in_audio = false;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(media) = line.strip_prefix("m=") {
            let mut parts = media.split(' ');
            in_audio = parts.next() == Some("audio") && parts.next() != Some("0");
        } else if in_audio && let Some(mid) = line.strip_prefix("a=mid:") {
            return Some(Mid::from(mid.trim()));
        }
    }
    None
}

/// The offer constraints of RFC 9725 section 4.4 that the SDP parser does not enforce: WHIP
/// answers are never partially successful, so an offer is refused whole unless it has exactly
/// one media section, and that one is live audio with `rtcp-mux`. (BUNDLE is moot with one
/// section.) The checks are on the text, before the more expensive parse.
pub(crate) fn check_offer_shape(offer: &str) -> Result<(), OfferError> {
    let bad = |message: String| Err(OfferError::Bad(message));
    let sections: Vec<&str> = offer
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("m="))
        .collect();
    let Some(section) = sections.first() else {
        return bad("the offer has no media section".to_owned());
    };
    if sections.len() != 1 {
        return bad(format!(
            "the offer has {} media sections, exactly one audio section is accepted              (RFC 9725 4.4: no partially successful answers)",
            sections.len()
        ));
    }
    let mut parts = section.trim_start_matches("m=").split(' ');
    if parts.next() != Some("audio") || parts.next() == Some("0") {
        return bad("the offer must be exactly one live audio section".to_owned());
    }
    if !offer.lines().any(|line| line.trim() == "a=rtcp-mux") {
        return bad("the offer must use rtcp-mux (RFC 9725 4.4)".to_owned());
    }
    Ok(())
}

/// The local ICE username fragment of an SDP answer.
fn answer_ufrag(answer: &str) -> Option<String> {
    answer.lines().find_map(|line| {
        line.trim()
            .strip_prefix("a=ice-ufrag:")
            .map(|ufrag| ufrag.trim().to_owned())
    })
}

/// Builds the `Rtc`, applies the offer and prepares the audio path. Runs on a blocking thread.
#[cfg(test)]
pub(crate) fn build_peer(
    shared: &Shared,
    offer: &str,
    now: std::time::Instant,
) -> Result<Setup, OfferError> {
    build_peer_with(shared, offer, now, None)
}

/// [`build_peer`] with the call's TURN relayed address, advertised as a host candidate.
pub(crate) fn build_peer_with(
    shared: &Shared,
    offer: &str,
    now: std::time::Instant,
    relayed: Option<SocketAddr>,
) -> Result<Setup, OfferError> {
    let config = &shared.config;
    check_offer_shape(offer)?;
    let offer_text = strip_unusable_candidates(offer);
    let offer = SdpOffer::from_sdp_string(&offer_text)
        .map_err(|e| OfferError::Bad(format!("the offer does not parse: {e}")))?;

    let mut rtc = Rtc::builder()
        .set_crypto_provider(Arc::clone(&shared.crypto))
        .set_dtls_cert(shared.cert.clone())
        .set_ice_lite(config.ice_lite)
        .clear_codecs()
        .enable_opus(true, false)
        .set_reordering_size_audio(config.reorder_packets)
        .set_reordering_timeout_audio(Some(config.reorder_timeout))
        .build(now);
    // Relay-only mode has no public UDP address to advertise.
    if !shared
        .relay
        .as_ref()
        .is_some_and(super::relay::RelayService::only)
    {
        let candidate = Candidate::host(shared.candidate, "udp")
            .map_err(|e| OfferError::Internal(format!("host candidate: {e}")))?;
        rtc.add_local_candidate(candidate);
    }
    if let Some(relayed) = relayed {
        // ICE-lite refuses relayed local candidates, so the TURN relayed address goes in as a
        // host candidate. Added after the direct one: `str0m` gives a later candidate of the same
        // kind a lower local preference, so a working direct path wins.
        let candidate = Candidate::host(relayed, "udp")
            .map_err(|e| OfferError::Internal(format!("relayed candidate: {e}")))?;
        rtc.add_local_candidate(candidate);
    }
    if let Some(tcp) = shared.tcp_candidate {
        // Passive TCP host candidate (RFC 6544): its priority is below the UDP one's, so a
        // browser that can use UDP does.
        let candidate = Candidate::builder()
            .tcp()
            .host(tcp)
            .tcptype(TcpType::Passive)
            .build()
            .map_err(|e| OfferError::Internal(format!("tcp host candidate: {e}")))?;
        rtc.add_local_candidate(candidate);
    }
    let answer: SdpAnswer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| OfferError::Bad(format!("the offer was refused: {e}")))?;
    let answer = answer.to_sdp_string();
    let ufrag = answer_ufrag(&answer)
        .ok_or_else(|| OfferError::Internal("the answer has no ice-ufrag".to_owned()))?;

    // `Event::MediaAdded` only comes with the first poll after the connection is up, so the
    // audio section is found in the offer text.
    let mid = audio_mid(&offer_text)
        .ok_or_else(|| OfferError::Bad("the offer has no audio media section".to_owned()))?;
    let direction = rtc
        .media(mid)
        .filter(|media| media.kind() == MediaKind::Audio)
        .map(|media| media.direction())
        .ok_or_else(|| OfferError::Bad("the offer has no usable audio media section".to_owned()))?;
    if !direction.is_receiving() {
        return Err(OfferError::Bad(
            "the offer must send audio (sendrecv or sendonly)".to_owned(),
        ));
    }
    let pt = rtc
        .writer(mid)
        .and_then(|writer| {
            writer
                .payload_params()
                .find(|params| params.spec().codec == Codec::Opus)
                .map(|params| params.pt())
        })
        .ok_or_else(|| OfferError::Bad("the offer does not include Opus audio".to_owned()))?;
    let path = MediaPath::new(
        &*shared.options.opus,
        &config.opus,
        config.input_rate,
        config.output_rate,
    )
    .map_err(|e| OfferError::Internal(e.to_string()))?;
    Ok(Setup {
        rtc,
        mid,
        pt,
        can_send: matches!(direction, Direction::SendRecv | Direction::SendOnly),
        path,
        answer,
        ufrag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_candidates_are_removed_and_address_candidates_stay() {
        let sdp = "v=0\r\n\
a=candidate:1 1 udp 2113937151 0a1b2c3d-4e5f-6789-abcd-ef0123456789.local 54321 typ host generation 0\r\n\
a=candidate:2 1 udp 2113937151 192.168.1.5 54322 typ host generation 0\r\n\
a=candidate:3 1 udp 1677729535 2001:db8::1 54323 typ srflx raddr 0.0.0.0 rport 0\r\n\
a=ice-ufrag:abcd\r\n";
        let cleaned = strip_unusable_candidates(sdp);
        assert!(!cleaned.contains(".local"));
        assert!(cleaned.contains("192.168.1.5"));
        assert!(cleaned.contains("2001:db8::1"));
        assert!(cleaned.contains("a=ice-ufrag:abcd"));
        assert_eq!(cleaned.lines().count(), 4);
    }

    #[test]
    fn the_audio_mid_is_the_first_live_audio_section() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:v\r\n\
m=audio 0 UDP/TLS/RTP/SAVPF 111\r\na=mid:dead\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:a1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:a2\r\n";
        assert_eq!(audio_mid(sdp), Some(Mid::from("a1")));
        assert_eq!(audio_mid("v=0\r\nm=video 9 X 96\r\na=mid:v\r\n"), None);
        assert_eq!(audio_mid("v=0\r\nm=audio 9 X 111\r\n"), None);
    }

    #[test]
    fn tokens_compare_exactly() {
        assert!(auth::constant_time_eq(b"secret", b"secret"));
        assert!(!auth::constant_time_eq(b"secret", b"secreT"));
        assert!(!auth::constant_time_eq(b"secret", b"secre"));
        assert!(!auth::constant_time_eq(b"", b"x"));
        assert!(auth::constant_time_eq(b"", b""));
    }

    #[test]
    fn the_ufrag_is_read_from_the_answer() {
        assert_eq!(
            answer_ufrag("v=0\r\na=ice-pwd:x\r\na=ice-ufrag:AbC1\r\n"),
            Some("AbC1".to_owned())
        );
        assert_eq!(answer_ufrag("v=0\r\n"), None);
    }
}
