//! The WebSocket fallback on the WebRTC signaling port: a WebSocket upgrade on
//! `ws_path` serves the generic WebSocket audio protocol with the signaling listener's
//! authentication, `Origin` rule and limits. Real sockets on 127.0.0.1, the real clock; the
//! engine is played by a `TransportSession` on every accepted call.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)] // test helpers (see clippy.toml)

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_transport::auth::{Audience, SessionSecrets, TokenSpec, mint_token};
use brisvox_transport::webrtc::{WebRtcConfig, WebRtcServer};
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerMessage, Start};
use brisvox_transport::{
    CallInfo, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use str0m::media::{Direction, MediaKind};
use str0m::{Candidate, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(8);
const SESSION_SECRET: &str = "a-session-secret-of-at-least-32-bytes!";
const STATIC_TOKEN: &str = "dev-signaling-token";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn unix_now() -> u64 {
    SystemClock::shared()
        .wall()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn spec(sid: &str, audience: Audience) -> TokenSpec {
    let now = unix_now();
    TokenSpec {
        audience,
        sid: sid.to_owned(),
        not_before: now.saturating_sub(5),
        expires: now + 120,
        agent: Some("web".to_owned()),
        max_s: None,
        meta: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
    }
}

fn mint(spec: &TokenSpec) -> String {
    mint_token(&Secret::new(SESSION_SECRET.to_owned()), spec).unwrap()
}

fn session_token(sid: &str) -> String {
    mint(&spec(sid, Audience::Webrtc))
}

fn config() -> WebRtcConfig {
    WebRtcConfig {
        http_addr: ([127, 0, 0, 1], 0).into(),
        udp_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 8,
        offers_per_minute_per_ip: None,
        connect_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        sessions: Some(SessionSecrets::new(Secret::new(SESSION_SECRET.to_owned()), None).unwrap()),
        ..WebRtcConfig::default()
    }
}

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: WebRtcServer,
}

impl Rig {
    async fn start(config: WebRtcConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = WebRtcServer::bind(config, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.http_addr()
    }

    /// A websocket client for `path_and_query` with extra request headers.
    async fn connect(
        &self,
        path_and_query: &str,
        headers: &[(&'static str, &str)],
    ) -> Result<Client, WsError> {
        let url = format!("ws://{}{path_and_query}", self.addr());
        let mut request = url.into_client_request().unwrap();
        for (name, value) in headers {
            request
                .headers_mut()
                .insert(*name, HeaderValue::from_str(value).unwrap());
        }
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request))
            .await
            .expect("connect timed out")?;
        Ok(ws)
    }

    /// Connects with a session token in the subprotocol (what a browser does).
    async fn connect_token(&self, token: &str) -> Result<Client, WsError> {
        let protocols = format!("brisvox, brisvox-token.{token}");
        self.connect("/ws", &[("Sec-WebSocket-Protocol", &protocols)])
            .await
    }

    /// Plays the client's `start`, takes the call on the engine side and returns the engine's
    /// session with the call's info; the client has read `started`.
    async fn open(&mut self, ws: &mut Client) -> (TransportSession, CallInfo) {
        let start = Start::new(16_000, Encoding::Pcm16);
        ws.send(Message::text(ClientMessage::Start(start).to_json()))
            .await
            .unwrap();
        let link = tokio::time::timeout(WAIT, self.server.accept(&self.parent))
            .await
            .expect("no call arrived")
            .unwrap()
            .expect("the server stopped");
        let mut session = TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap();
        let TransportEvent::Started(info) = next_event(&mut session).await else {
            panic!("Started is the first event");
        };
        let ServerMessage::Started { sample_rate, .. } = next_message(ws).await else {
            panic!("started expected");
        };
        assert_eq!(sample_rate, 16_000);
        (session, info)
    }

    async fn wait_for_active(&self, count: usize) {
        let wait = async {
            while self.server.active_calls() != count {
                self.clock.sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("active calls never became {count}"));
    }
}

async fn next_event(session: &mut TransportSession) -> TransportEvent {
    tokio::time::timeout(WAIT, session.recv())
        .await
        .expect("no event")
        .expect("session ended")
}

/// The next caller audio frame the engine sees.
async fn next_audio(session: &mut TransportSession) -> AudioFrame {
    loop {
        if let TransportEvent::Audio(frame) = next_event(session).await {
            return frame;
        }
    }
}

/// The next text frame of the client as a server message.
async fn next_message(ws: &mut Client) -> ServerMessage {
    loop {
        let frame = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no message")
            .expect("closed")
            .expect("ws error");
        match frame {
            Message::Text(text) => return ServerMessage::parse(&text).unwrap(),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn status(result: &Result<Client, WsError>) -> u16 {
    match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("{other:?}"),
    }
}

/// 20 ms of 16 kHz PCM16 whose bytes are all `fill`.
fn pcm(fill: u8) -> Vec<u8> {
    vec![fill; 640]
}

// ---------------------------------------------------------------------------------------------
// Raw HTTP

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
}

/// Sends `raw` bytes and reads the answer until the server closes.
async fn raw_http(addr: SocketAddr, raw: &[u8]) -> Option<HttpResponse> {
    let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .unwrap();
    let _ = stream.write_all(raw).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(WAIT, stream.read_to_end(&mut out))
        .await
        .expect("no answer");
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, _) = text.split_once("\r\n\r\n")?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    Some(HttpResponse { status, headers })
}

async fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Option<HttpResponse> {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: test\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    raw_http(addr, request.as_bytes()).await
}

const UPGRADE: &[(&str, &str)] = &[
    ("Upgrade", "websocket"),
    ("Connection", "Upgrade"),
    ("Sec-WebSocket-Version", "13"),
    ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

/// A valid WebRTC offer for a raw `POST` that never connects.
fn offer_sdp(clock: &SharedClock) -> String {
    let crypto = std::sync::Arc::new(str0m::crypto::from_feature_flags());
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto)
        .build(clock.now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    offer.to_sdp_string()
}

async fn post_offer(addr: SocketAddr, sdp: &str, bearer: &str) -> HttpResponse {
    let request = format!(
        "POST /whip HTTP/1.1\r\nHost: test\r\nContent-Type: application/sdp\r\nAuthorization: Bearer {bearer}\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    raw_http(addr, request.as_bytes()).await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// The protocol over the signaling port

#[tokio::test]
async fn a_session_token_opens_a_call_with_an_audio_round_trip() {
    let mut rig = Rig::start(config()).await;
    let token = session_token("visitor-1");
    let mut ws = rig.connect_token(&token).await.expect("upgrade");
    let (mut session, info) = rig.open(&mut ws).await;
    assert_eq!(info.metadata["transport"], "webrtc-ws");
    assert_eq!(info.metadata["session_id"], "visitor-1");
    assert_eq!(info.metadata["session.agent"], "web");
    assert_eq!(info.metadata["session.meta.tenant"], "acme");
    assert!(info.call_id.starts_with("ws-"), "{}", info.call_id);

    // Caller to engine.
    for fill in 1..=5u8 {
        ws.send(Message::binary(pcm(fill))).await.unwrap();
    }
    for fill in 1..=5u8 {
        let frame = next_audio(&mut session).await;
        assert_eq!(frame.sample_rate(), SampleRate::Hz16000);
        assert_eq!(frame.data().as_ref(), pcm(fill).as_slice());
    }
    // Engine to caller.
    for fill in 101..=105u8 {
        let epoch = TurnEpoch::new(0);
        let frame = AudioFrame::new(
            Bytes::from(pcm(fill)),
            SampleRate::Hz16000,
            rig.clock.now(),
            u64::from(fill),
            epoch,
        )
        .unwrap();
        session
            .send(TransportCommand::Audio {
                epoch,
                utterance: UtteranceId::new(1),
                frame,
            })
            .await
            .unwrap();
    }
    let mut heard = Vec::new();
    while heard.len() < 5 {
        let frame = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no audio")
            .unwrap()
            .unwrap();
        if let Message::Binary(data) = frame {
            heard.push(data[0]);
        }
    }
    assert_eq!(heard, vec![101, 102, 103, 104, 105]);

    let stats = rig.server.stats();
    assert_eq!(stats.ws_upgrades, 1);
    assert_eq!(rig.server.active_calls(), 1);
    // The client hangs up.
    ws.send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
    loop {
        if let TransportEvent::Stopped { reason } = next_event(&mut session).await {
            assert_eq!(reason, StopReason::PeerHangup);
            break;
        }
    }
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn the_static_token_works_in_the_header_the_subprotocol_and_the_query() {
    let rig = Rig::start(WebRtcConfig {
        bearer_token: Some(Secret::new(STATIC_TOKEN.to_owned())),
        sessions: None,
        ..config()
    })
    .await;
    assert_eq!(status(&rig.connect("/ws", &[]).await), 401);
    assert_eq!(status(&rig.connect("/ws?token=nope", &[]).await), 403);
    assert_eq!(
        status(&rig.connect(&format!("/ws?token={STATIC_TOKEN}"), &[]).await),
        101
    );
    assert_eq!(
        status(
            &rig.connect(
                "/ws",
                &[("Authorization", &format!("Bearer {STATIC_TOKEN}"))]
            )
            .await
        ),
        101
    );
    assert_eq!(status(&rig.connect_token(STATIC_TOKEN).await), 101);
    let Err(WsError::Http(response)) = rig.connect("/ws", &[]).await else {
        panic!("a refusal expected");
    };
    assert_eq!(response.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn missing_invalid_reused_and_oversize_tokens_are_refused_before_the_upgrade() {
    let rig = Rig::start(config()).await;
    // No token at all, junk, another listener's token, an expired one, a foreign signature.
    assert_eq!(status(&rig.connect("/ws", &[]).await), 401);
    assert_eq!(status(&rig.connect_token("junk").await), 403);
    let rtvi = mint(&spec("aud", Audience::Rtvi));
    assert_eq!(status(&rig.connect_token(&rtvi).await), 403);
    let ws_audience = mint(&spec("aud2", Audience::WebSocket));
    assert_eq!(status(&rig.connect_token(&ws_audience).await), 403);
    let mut old = spec("old", Audience::Webrtc);
    old.not_before = unix_now() - 1_000;
    old.expires = unix_now() - 500;
    assert_eq!(status(&rig.connect_token(&mint(&old)).await), 403);
    let foreign = mint_token(
        &Secret::new("another-secret-another-secret-32by".to_owned()),
        &spec("foreign", Audience::Webrtc),
    )
    .unwrap();
    assert_eq!(status(&rig.connect_token(&foreign).await), 403);
    let huge = "a".repeat(5_000);
    assert_eq!(
        status(
            &rig.connect("/ws", &[("Authorization", &format!("Bearer {huge}"))])
                .await
        ),
        413
    );
    assert_eq!(rig.server.active_calls(), 0, "no refusal holds a slot");
    // A good token is single use.
    let token = session_token("once");
    let first = rig.connect_token(&token).await.expect("first use");
    assert_eq!(status(&rig.connect_token(&token).await), 403);
    drop(first);
    assert!(rig.server.stats().offers_unauthorized >= 7);
}

#[tokio::test]
async fn a_refusal_for_capacity_does_not_burn_the_token() {
    let rig = Rig::start(WebRtcConfig {
        max_calls: 1,
        ..config()
    })
    .await;
    let first = rig
        .connect_token(&session_token("first"))
        .await
        .expect("first call");
    let second = session_token("second");
    let busy = rig.connect_token(&second).await;
    assert_eq!(status(&busy), 503);
    let Err(WsError::Http(response)) = busy else {
        panic!("a refusal expected");
    };
    assert_eq!(response.headers()["retry-after"], "1");
    drop(first);
    rig.wait_for_active(0).await;
    assert_eq!(
        status(&rig.connect_token(&second).await),
        101,
        "the token survived the 503"
    );
}

#[tokio::test]
async fn the_per_address_call_cap_and_the_offer_rate_apply() {
    let rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(1),
        ..config()
    })
    .await;
    let first = rig
        .connect_token(&session_token("a"))
        .await
        .expect("first call");
    let second = rig.connect_token(&session_token("b")).await;
    assert_eq!(status(&second), 429);
    let Err(WsError::Http(response)) = second else {
        panic!("a refusal expected");
    };
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(rig.server.stats().offers_ip_limited, 1);
    drop(first);
    rig.wait_for_active(0).await;
    assert_eq!(status(&rig.connect_token(&session_token("c")).await), 101);

    // The offer rate is the one of POST /whip: two upgrades a minute, then 429.
    let limited = Rig::start(WebRtcConfig {
        offers_per_minute_per_ip: Some(2),
        ..config()
    })
    .await;
    for sid in ["r1", "r2"] {
        let ws = limited.connect_token(&session_token(sid)).await;
        assert_eq!(status(&ws), 101);
        drop(ws);
    }
    let over = limited.connect_token(&session_token("r3")).await;
    assert_eq!(status(&over), 429);
    let Err(WsError::Http(response)) = over else {
        panic!("a refusal expected");
    };
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(limited.server.stats().offers_rate_limited, 1);
}

#[tokio::test]
async fn the_handshake_pool_applies() {
    let rig = Rig::start(WebRtcConfig {
        max_handshaking: Some(1),
        ..config()
    })
    .await;
    // The first connection never sends `start`: it holds the handshake slot.
    let waiting = rig
        .connect_token(&session_token("h1"))
        .await
        .expect("first");
    let busy = rig.connect_token(&session_token("h2")).await;
    assert_eq!(status(&busy), 503);
    assert_eq!(rig.server.stats().offers_handshake_limited, 1);
    drop(waiting);
}

#[tokio::test]
async fn a_websocket_call_leaves_the_http_connection_pool() {
    // One HTTP connection at a time: a live call must not occupy it.
    let rig = Rig::start(WebRtcConfig {
        max_http_connections: 1,
        max_http_connections_per_ip: 1,
        ..config()
    })
    .await;
    let mut ws = rig
        .connect_token(&session_token("pool"))
        .await
        .expect("upgrade");
    ws.send(Message::text(
        ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)).to_json(),
    ))
    .await
    .unwrap();
    // The call is open, and the demo page is still served.
    let page = get(rig.addr(), "/demo", &[]).await.unwrap();
    assert_eq!(page.status, 200);
}

#[tokio::test]
async fn a_disabled_path_is_404_and_so_is_everything_else_under_it() {
    let rig = Rig::start(WebRtcConfig {
        ws_path: None,
        ..config()
    })
    .await;
    let mut headers = UPGRADE.to_vec();
    headers.push(("Authorization", "Bearer x"));
    assert_eq!(get(rig.addr(), "/ws", &headers).await.unwrap().status, 404);
    assert_eq!(status(&rig.connect("/ws", &[]).await), 404);
    assert_eq!(rig.server.stats().ws_upgrades, 0);

    // Enabled: only the exact path (a trailing slash is the same path).
    let rig = Rig::start(config()).await;
    assert_eq!(
        get(rig.addr(), "/ws/other", &headers).await.unwrap().status,
        404
    );
    assert_eq!(get(rig.addr(), "/wss", &headers).await.unwrap().status, 404);
}

#[tokio::test]
async fn a_custom_path_is_served_and_the_default_is_not() {
    let rig = Rig::start(WebRtcConfig {
        ws_path: Some("/voice/socket".to_owned()),
        ..config()
    })
    .await;
    let token = session_token("custom");
    let protocols = format!("brisvox, brisvox-token.{token}");
    let ok = rig
        .connect("/voice/socket", &[("Sec-WebSocket-Protocol", &protocols)])
        .await;
    assert_eq!(status(&ok), 101);
    assert_eq!(status(&rig.connect("/ws", &[]).await), 404);
}

#[tokio::test]
async fn bad_upgrade_requests_are_refused_with_plain_http() {
    let rig = Rig::start(config()).await;
    let addr = rig.addr();
    let with_token: Vec<(&str, &str)> = UPGRADE
        .iter()
        .copied()
        .chain([("Authorization", "Bearer x")])
        .collect();
    // Not an upgrade at all.
    let plain = get(addr, "/ws", &[]).await.unwrap();
    assert_eq!(plain.status, 426);
    assert_eq!(plain.headers["upgrade"], "websocket");
    // Wrong version.
    let mut old: Vec<(&str, &str)> = with_token.clone();
    old.retain(|(name, _)| *name != "Sec-WebSocket-Version");
    old.push(("Sec-WebSocket-Version", "8"));
    let wrong = get(addr, "/ws", &old).await.unwrap();
    assert_eq!(wrong.status, 426);
    assert_eq!(wrong.headers["sec-websocket-version"], "13");
    // A key that is not 16 base64 bytes, and none.
    let mut bad_key: Vec<(&str, &str)> = with_token.clone();
    bad_key.retain(|(name, _)| *name != "Sec-WebSocket-Key");
    bad_key.push(("Sec-WebSocket-Key", "short"));
    assert_eq!(get(addr, "/ws", &bad_key).await.unwrap().status, 400);
    let mut no_key: Vec<(&str, &str)> = with_token.clone();
    no_key.retain(|(name, _)| *name != "Sec-WebSocket-Key");
    assert_eq!(get(addr, "/ws", &no_key).await.unwrap().status, 400);
    // Not GET.
    let post = raw_http(
        addr,
        b"POST /ws HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n",
    )
    .await
    .unwrap();
    assert_eq!(post.status, 405);
    assert_eq!(post.headers["allow"], "GET");
    // An oversize head, a body, garbage: refused or dropped, never a call.
    let big = "x".repeat(9_000);
    let huge_head = get(addr, "/ws", &[("X-Big", &big)]).await.unwrap();
    assert_eq!(huge_head.status, 431);
    assert!(
        raw_http(addr, b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03")
            .await
            .is_none_or(|r| r.status >= 400)
    );
    assert!(
        raw_http(addr, b"GET /ws HTTP/1.1\r\nHost: t\r\nBroken\r\n\r\n")
            .await
            .is_none_or(|r| r.status == 400)
    );
    assert_eq!(rig.server.active_calls(), 0);
    // The listener is fine.
    assert_eq!(
        status(&rig.connect_token(&session_token("after")).await),
        101
    );
}

#[tokio::test]
async fn an_oversize_token_in_the_query_is_refused() {
    let rig = Rig::start(config()).await;
    let huge = "a".repeat(5_000);
    assert_eq!(
        status(&rig.connect(&format!("/ws?token={huge}"), &[]).await),
        413
    );
}

#[tokio::test]
async fn an_oversize_frame_ends_only_that_call() {
    let mut rig = Rig::start(config()).await;
    let mut bad = rig.connect_token(&session_token("big")).await.unwrap();
    let (mut session, _) = rig.open(&mut bad).await;
    bad.send(Message::binary(vec![0u8; 200 * 1024]))
        .await
        .unwrap();
    // The call ends (a fatal error or a reset), and the engine sees it go.
    loop {
        match next_event(&mut session).await {
            TransportEvent::Stopped { .. } => break,
            _ => continue,
        }
    }
    rig.wait_for_active(0).await;
    // Another call is unaffected.
    let mut good = rig.connect_token(&session_token("fine")).await.unwrap();
    let (_session, _) = rig.open(&mut good).await;
}

#[tokio::test]
async fn the_origin_rule_follows_the_cors_setting() {
    // No CORS origin: only a page served from the same host may open the call.
    let rig = Rig::start(config()).await;
    let protocols = |token: &str| format!("brisvox, brisvox-token.{token}");
    let host = rig.addr().to_string();
    let foreign = rig
        .connect(
            "/ws",
            &[
                ("Origin", "https://evil.example"),
                ("Sec-WebSocket-Protocol", &protocols(&session_token("o1"))),
            ],
        )
        .await;
    assert_eq!(status(&foreign), 403);
    let same = rig
        .connect(
            "/ws",
            &[
                ("Origin", &format!("http://{host}")),
                ("Sec-WebSocket-Protocol", &protocols(&session_token("o2"))),
            ],
        )
        .await;
    assert_eq!(status(&same), 101);
    // The refused page's token was not used up.
    let again = rig.connect_token(&session_token("o1")).await;
    assert_eq!(status(&again), 101);

    // A configured origin is let in.
    let pinned = Rig::start(WebRtcConfig {
        cors_origin: Some("https://app.example".to_owned()),
        ..config()
    })
    .await;
    let app = pinned
        .connect(
            "/ws",
            &[
                ("Origin", "https://app.example"),
                ("Sec-WebSocket-Protocol", &protocols(&session_token("p1"))),
            ],
        )
        .await;
    assert_eq!(status(&app), 101);
    let other = pinned
        .connect(
            "/ws",
            &[
                ("Origin", "https://evil.example"),
                ("Sec-WebSocket-Protocol", &protocols(&session_token("p2"))),
            ],
        )
        .await;
    assert_eq!(status(&other), 403);
}

#[tokio::test]
async fn a_token_of_a_webrtc_session_that_never_connected_is_given_back() {
    let rig = Rig::start(config()).await;
    let addr = rig.addr();
    let sdp = offer_sdp(&rig.clock);
    // WebRTC first: the offer is answered, media never connects, the page hangs up and falls
    // back to the WebSocket. The DELETE's 200 already means the token is usable (no waiting for
    // the call task to wind down), so the immediate upgrade must work every time.
    for round in 0..20 {
        let token = session_token(&format!("fallback-{round}"));
        let offer = post_offer(addr, &sdp, &token).await;
        assert_eq!(offer.status, 201);
        // While the session is pending the token is spent.
        assert_eq!(status(&rig.connect_token(&token).await), 403);
        let location = offer.headers["location"].clone();
        let request =
            format!("DELETE {location} HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(
            raw_http(addr, request.as_bytes()).await.unwrap().status,
            200
        );
        assert_eq!(
            status(&rig.connect_token(&token).await),
            101,
            "round {round}: the same token opens the fallback right after the DELETE"
        );
        // And that was its one call.
        assert_eq!(status(&rig.connect_token(&token).await), 403);
    }
}

#[tokio::test]
async fn a_token_is_refunded_at_most_once_across_webrtc_and_the_websocket() {
    let rig = Rig::start(config()).await;
    let addr = rig.addr();
    let sdp = offer_sdp(&rig.clock);
    let token = session_token("once");
    let delete = |location: String| async move {
        let request =
            format!("DELETE {location} HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n");
        raw_http(addr, request.as_bytes()).await.unwrap().status
    };
    let first = post_offer(addr, &sdp, &token).await;
    assert_eq!(first.status, 201);
    assert_eq!(delete(first.headers["location"].clone()).await, 200);
    // The refund is spent: WebRTC again, hung up again, no second refund.
    let second = post_offer(addr, &sdp, &token).await;
    assert_eq!(second.status, 201);
    assert_eq!(delete(second.headers["location"].clone()).await, 200);
    rig.wait_for_active(0).await;
    assert_eq!(status(&rig.connect_token(&token).await), 403);
    assert_eq!(post_offer(addr, &sdp, &token).await.status, 401);
}

#[tokio::test]
async fn a_session_tokens_max_s_ends_the_call() {
    let mut rig = Rig::start(config()).await;
    let mut limited = spec("short", Audience::Webrtc);
    limited.max_s = Some(1);
    let mut ws = rig.connect_token(&mint(&limited)).await.unwrap();
    let (mut session, info) = rig.open(&mut ws).await;
    assert_eq!(info.metadata["session.max_s"], "1");
    loop {
        if let TransportEvent::Stopped { .. } = next_event(&mut session).await {
            break;
        }
    }
    // The client was told.
    let mut stopped = false;
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(WAIT, ws.next()).await {
        if let Message::Text(text) = frame
            && matches!(ServerMessage::parse(&text), Ok(ServerMessage::Stop { .. }))
        {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "the client got stop");
}

#[tokio::test]
async fn a_server_shutdown_says_goodbye_to_websocket_calls() {
    let mut rig = Rig::start(config()).await;
    let mut ws = rig.connect_token(&session_token("bye")).await.unwrap();
    let (_session, _) = rig.open(&mut ws).await;
    rig.server.shutdown(WAIT).await.unwrap();
    let mut reason = None;
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(WAIT, ws.next()).await {
        if let Message::Text(text) = frame
            && let Ok(ServerMessage::Stop { reason: r }) = ServerMessage::parse(&text)
        {
            reason = Some(r);
            break;
        }
    }
    assert_eq!(reason.as_deref(), Some("shutdown"));
}
