//! TURN credential issuing tests: `Link: rel="ice-server"` on the WHIP `201` and
//! `GET /whip/ice`, for the coturn shared secret and for Cloudflare and Twilio against a local
//! mock of their REST APIs (plain `http://` on loopback is allowed for exactly this).
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)] // test helpers (see clippy.toml)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use brisvox_core::{Secret, SystemClock};
use brisvox_transport::auth::{Audience, SessionSecrets, TokenSpec, mint_token};
use brisvox_transport::webrtc::turn_creds::session_label;
use brisvox_transport::webrtc::{TurnConfig, TurnProvider, WebRtcConfig, WebRtcServer};
use str0m::media::{Direction, MediaKind};
use str0m::{Candidate, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(8);

struct Reply {
    status: u16,
    /// Every header, in order, names lowercased (`Link` repeats).
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn one(&self, name: &str) -> Option<&str> {
        self.all(name).first().copied()
    }
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(text.as_bytes()).await.unwrap();
    let _ = stream.write_all(body).await;
    let mut raw = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut raw))
        .await
        .expect("response timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

fn offer() -> String {
    let mut rtc = Rtc::builder()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .clear_codecs()
        .enable_opus(true, false)
        .build(SystemClock::shared().now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    offer.to_sdp_string()
}

async fn post_whip(addr: SocketAddr, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![("Content-Type", "application/sdp")];
    headers.extend_from_slice(extra);
    request(addr, "POST", "/whip", &headers, offer().as_bytes()).await
}

fn config(turn: Option<TurnConfig>) -> WebRtcConfig {
    WebRtcConfig {
        http_addr: ([127, 0, 0, 1], 0).into(),
        udp_addr: ([127, 0, 0, 1], 0).into(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        turn,
        ..WebRtcConfig::default()
    }
}

async fn start(config: WebRtcConfig) -> (WebRtcServer, CancellationToken) {
    let parent = CancellationToken::new();
    let server = WebRtcServer::bind(config, SystemClock::shared(), &parent)
        .await
        .unwrap();
    (server, parent)
}

fn coturn(ttl: Duration) -> TurnConfig {
    let mut config = TurnConfig::new(TurnProvider::Coturn {
        urls: vec![
            "turns:turn.example.com:443?transport=tcp".to_owned(),
            "turn:turn.example.com:3478?transport=udp".to_owned(),
        ],
        secret: Secret::new("shared-secret".to_owned()),
    });
    config.ttl = ttl;
    config
}

fn unix_now() -> u64 {
    SystemClock::shared()
        .wall()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn hmac_sha1_base64(secret: &str, text: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, text.as_bytes()))
}

/// Parses `<url>; rel="ice-server"; username="u"; credential="c"`.
fn parse_link(link: &str) -> (String, String, String) {
    let (url, rest) = link.split_once(">; ").unwrap();
    assert!(rest.starts_with("rel=\"ice-server\""), "{link}");
    let param = |name: &str| {
        let start = rest.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
        rest[start..].split('"').next().unwrap().to_owned()
    };
    (
        url.trim_start_matches('<').to_owned(),
        param("username"),
        param("credential"),
    )
}

// ---------------------------------------------------------------------------------------------
// coturn shared secret

#[tokio::test]
async fn the_whip_201_carries_coturn_credentials_as_ice_server_links() {
    let (server, _parent) = start(config(Some(coturn(Duration::from_secs(600))))).await;
    let reply = post_whip(server.http_addr(), &[]).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let session_id = reply
        .one("location")
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let links = reply.all("link");
    assert_eq!(links.len(), 2, "one Link per URL: {links:?}");
    let (url, username, credential) = parse_link(links[0]);
    assert_eq!(url, "turns:turn.example.com:443?transport=tcp");
    let (expiry, sid) = username.split_once(':').unwrap();
    assert_eq!(sid, session_id);
    let expiry: u64 = expiry.parse().unwrap();
    let now = unix_now();
    assert!(
        (now + 590..=now + 610).contains(&expiry),
        "expiry {expiry} vs now {now}"
    );
    assert_eq!(credential, hmac_sha1_base64("shared-secret", &username));
    let (url2, username2, credential2) = parse_link(links[1]);
    assert_eq!(url2, "turn:turn.example.com:3478?transport=udp");
    assert_eq!((username2, credential2), (username, credential));
    // The secret itself never travels.
    assert!(!reply.body.contains("shared-secret"));
    assert!(
        reply
            .headers
            .iter()
            .all(|(_, v)| !v.contains("shared-secret"))
    );
}

#[tokio::test]
async fn without_turn_the_201_has_no_link_and_ice_is_an_empty_list() {
    let (server, _parent) = start(config(None)).await;
    let reply = post_whip(server.http_addr(), &[]).await;
    assert_eq!(reply.status, 201);
    assert!(reply.all("link").is_empty());
    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert_eq!(ice.one("cache-control"), Some("no-store"));
    assert!(
        ice.one("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    let json: serde_json::Value = serde_json::from_str(&ice.body).unwrap();
    assert_eq!(json, serde_json::json!({ "iceServers": [] }));
}

#[tokio::test]
async fn get_ice_returns_browser_shaped_servers_with_fresh_credentials_per_request() {
    let (server, _parent) = start(config(Some(coturn(Duration::from_secs(600))))).await;
    let first = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    let second = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.one("cache-control"), Some("no-store"));
    let a: serde_json::Value = serde_json::from_str(&first.body).unwrap();
    let b: serde_json::Value = serde_json::from_str(&second.body).unwrap();
    let entry = &a["iceServers"][0];
    assert_eq!(entry["urls"][0], "turns:turn.example.com:443?transport=tcp");
    assert_eq!(entry["urls"][1], "turn:turn.example.com:3478?transport=udp");
    let username = entry["username"].as_str().unwrap();
    assert_eq!(
        entry["credential"].as_str().unwrap(),
        hmac_sha1_base64("shared-secret", username)
    );
    // The username embeds a session id: each request gets its own.
    assert_ne!(
        a["iceServers"][0]["username"],
        b["iceServers"][0]["username"]
    );
}

#[tokio::test]
async fn coturn_usernames_carry_the_label_of_the_session_token_sid() {
    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    let secrets = SessionSecrets::new(Secret::new(SECRET.to_owned()), None).unwrap();
    let (server, _parent) = start(WebRtcConfig {
        sessions: Some(secrets),
        ..config(Some(coturn(Duration::from_secs(600))))
    })
    .await;
    let addr = server.http_addr();
    let token = |sid: &str| {
        let now = unix_now();
        mint_token(
            &Secret::new(SECRET.to_owned()),
            &TokenSpec {
                audience: Audience::Webrtc,
                sid: sid.to_owned(),
                not_before: now.saturating_sub(5),
                expires: now + 120,
                agent: None,
                max_s: None,
                meta: std::collections::BTreeMap::new(),
            },
        )
        .unwrap()
    };
    let username = |reply: &Reply| {
        let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        value["iceServers"][0]["username"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // A sid with characters that would break a header still yields a plain label.
    let odd_sid = "visitor \"42\", x=y";
    let bearer = format!("Bearer {}", token(odd_sid));
    let first = request(addr, "GET", "/whip/ice", &[("Authorization", &bearer)], &[]).await;
    assert_eq!(first.status, 200, "{}", first.body);
    let (expiry, label) = username(&first)
        .split_once(':')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap();
    assert!(expiry.parse::<u64>().is_ok());
    assert_eq!(label, session_label(odd_sid));
    assert_eq!(label.len(), 16);
    assert!(label.bytes().all(|b| b.is_ascii_hexdigit()));
    // The token is not used up by the ICE request, and the same sid gives the same label.
    let second = request(addr, "GET", "/whip/ice", &[("Authorization", &bearer)], &[]).await;
    assert!(username(&second).ends_with(&format!(":{label}")));
    // The offer with that token (the call it opens) uses the label too.
    let offer = request(
        addr,
        "POST",
        "/whip",
        &[
            ("Content-Type", "application/sdp"),
            ("Authorization", &bearer),
        ],
        offer().as_bytes(),
    )
    .await;
    assert_eq!(offer.status, 201);
    let links = offer.all("link");
    assert!(
        links.iter().any(|l| l.contains(&format!(":{label}\""))),
        "{links:?}"
    );
    // Another visitor, another label.
    let other = format!("Bearer {}", token("visitor-43"));
    let third = request(addr, "GET", "/whip/ice", &[("Authorization", &other)], &[]).await;
    assert_ne!(username(&third).split_once(':').unwrap().1, label);
}

#[test]
fn the_session_label_is_a_stable_hash_of_the_sid() {
    // SHA-256("visitor-42") starts with these 8 bytes; applications can compute the same.
    assert_eq!(session_label("visitor-42").len(), 16);
    assert_eq!(session_label("visitor-42"), session_label("visitor-42"));
    assert_ne!(session_label("visitor-42"), session_label("visitor-43"));
    // SHA-256("") = e3b0c44298fc1c14...
    assert_eq!(session_label(""), "e3b0c44298fc1c14");
}

#[tokio::test]
async fn get_ice_needs_the_bearer_token_when_one_is_set_and_follows_the_cors_rules() {
    let (server, _parent) = start(WebRtcConfig {
        bearer_token: Some(Secret::new("tok-123".to_owned())),
        cors_origin: Some("https://app.example.com".to_owned()),
        ..config(Some(coturn(Duration::from_secs(600))))
    })
    .await;
    let denied = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(denied.status, 401);
    assert!(!denied.body.contains("turn.example.com"));
    let wrong = request(
        server.http_addr(),
        "GET",
        "/whip/ice",
        &[("Authorization", "Bearer nope")],
        &[],
    )
    .await;
    assert_eq!(wrong.status, 401);
    let ok = request(
        server.http_addr(),
        "GET",
        "/whip/ice",
        &[
            ("Authorization", "Bearer tok-123"),
            ("Origin", "https://app.example.com"),
        ],
        &[],
    )
    .await;
    assert_eq!(ok.status, 200);
    assert_eq!(
        ok.one("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    // The preflight allows GET with an Authorization header.
    let preflight = request(server.http_addr(), "OPTIONS", "/whip/ice", &[], &[]).await;
    assert_eq!(preflight.status, 200);
    assert!(
        preflight
            .one("access-control-allow-methods")
            .unwrap()
            .contains("GET")
    );
    assert!(
        preflight
            .one("access-control-allow-headers")
            .unwrap()
            .contains("Authorization")
    );
    // POST /whip lets the page read the Link headers cross-origin.
    let created = post_whip(server.http_addr(), &[("Authorization", "Bearer tok-123")]).await;
    assert_eq!(created.status, 201);
    assert!(
        created
            .one("access-control-expose-headers")
            .unwrap()
            .contains("Link")
    );
    // Other methods are refused.
    let post = request(server.http_addr(), "POST", "/whip/ice", &[], &[]).await;
    assert_eq!(post.status, 405);
}

// ---------------------------------------------------------------------------------------------
// A mock of the Cloudflare and Twilio REST APIs

#[derive(Clone, Debug)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Clone)]
enum Behaviour {
    Answer(u16, String),
    /// Reads the request and never answers.
    Hang,
}

struct Mock {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let behaviour = Arc::new(Mutex::new(behaviour));
        let (seen_task, behaviour_task) = (Arc::clone(&seen), Arc::clone(&behaviour));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (seen, behaviour) = (Arc::clone(&seen_task), Arc::clone(&behaviour_task));
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (head, mut body) = loop {
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            break (head.to_owned(), body.to_owned());
                        }
                    };
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap().to_owned();
                    let headers: Vec<(String, String)> = lines
                        .filter_map(|l| l.split_once(": "))
                        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
                        .collect();
                    let length: usize = headers
                        .iter()
                        .find(|(n, _)| n == "content-length")
                        .map_or(0, |(_, v)| v.parse().unwrap());
                    while body.len() < length {
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        body.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                    seen.lock().unwrap().push(Seen {
                        request_line,
                        headers,
                        body,
                    });
                    let behaviour = behaviour.lock().unwrap().clone();
                    match behaviour {
                        Behaviour::Answer(status, json) => {
                            let response = format!(
                                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                                json.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.shutdown().await;
                        }
                        Behaviour::Hang => {
                            SystemClock::shared().sleep(Duration::from_secs(30)).await;
                        }
                    }
                });
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

const CLOUDFLARE_ANSWER: &str = r#"{"iceServers":[
  {"urls":["stun:stun.cloudflare.com:3478","stun:stun.cloudflare.com:53"]},
  {"urls":["turn:turn.cloudflare.com:3478?transport=udp","turn:turn.cloudflare.com:53?transport=udp",
           "turns:turn.cloudflare.com:443?transport=tcp"],
   "username":"cf-user","credential":"cf-cred"}]}"#;

fn cloudflare(mock: &Mock) -> TurnConfig {
    TurnConfig::new(TurnProvider::Cloudflare {
        key_id: "key123".to_owned(),
        api_token: Secret::new("cf-api-token".to_owned()),
        base_url: mock.url(),
    })
}

fn header<'a>(seen: &'a Seen, name: &str) -> Option<&'a str> {
    seen.headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn cloudflare_credentials_are_fetched_once_cached_and_served_as_links_and_json() {
    let mock = Mock::start(Behaviour::Answer(201, CLOUDFLARE_ANSWER.to_owned())).await;
    let (server, _parent) = start(config(Some(cloudflare(&mock)))).await;

    let created = post_whip(server.http_addr(), &[]).await;
    assert_eq!(created.status, 201);
    let links = created.all("link");
    // stun (port 53 dropped) + two TURN URLs (port 53 dropped) = 1 + 2.
    assert_eq!(links.len(), 3, "{links:?}");
    assert!(
        links
            .iter()
            .any(|l| l.starts_with("<stun:stun.cloudflare.com:3478>; rel=\"ice-server\""))
    );
    let turns = parse_link(links.iter().find(|l| l.starts_with("<turns:")).unwrap());
    assert_eq!(turns.0, "turns:turn.cloudflare.com:443?transport=tcp");
    assert_eq!((turns.1.as_str(), turns.2.as_str()), ("cf-user", "cf-cred"));

    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    let json: serde_json::Value = serde_json::from_str(&ice.body).unwrap();
    assert_eq!(json["iceServers"][1]["credential"], "cf-cred");
    assert_eq!(json["iceServers"][1]["urls"].as_array().unwrap().len(), 2);

    // One upstream request served both (60 s cache), and it was the documented one.
    let seen = mock.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        seen[0].request_line,
        "POST /v1/turn/keys/key123/credentials/generate-ice-servers HTTP/1.1"
    );
    assert_eq!(
        header(&seen[0], "authorization"),
        Some("Bearer cf-api-token")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
        serde_json::json!({ "ttl": 600 })
    );
    // The API token never reaches the browser.
    assert!(
        !created
            .headers
            .iter()
            .any(|(_, v)| v.contains("cf-api-token"))
    );
    assert!(!ice.body.contains("cf-api-token"));
}

#[tokio::test]
async fn twilio_credentials_use_basic_auth_and_a_form_ttl() {
    let answer = r#"{"username":"u","password":"p","ttl":"600","account_sid":"AC1","ice_servers":[
      {"url":"stun:global.stun.twilio.com:3478","urls":"stun:global.stun.twilio.com:3478"},
      {"url":"turn:global.turn.twilio.com:443?transport=tcp","urls":"turn:global.turn.twilio.com:443?transport=tcp",
       "username":"tw-user","credential":"tw-cred"}]}"#;
    let mock = Mock::start(Behaviour::Answer(201, answer.to_owned())).await;
    let turn = TurnConfig::new(TurnProvider::Twilio {
        account_sid: "AC123".to_owned(),
        auth_user: "SK456".to_owned(),
        auth_secret: Secret::new("tw-secret".to_owned()),
        base_url: mock.url(),
    });
    let (server, _parent) = start(config(Some(turn))).await;
    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    let json: serde_json::Value = serde_json::from_str(&ice.body).unwrap();
    assert_eq!(json["iceServers"][1]["username"], "tw-user");
    assert_eq!(
        json["iceServers"][1]["urls"][0],
        "turn:global.turn.twilio.com:443?transport=tcp"
    );
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].request_line,
        "POST /2010-04-01/Accounts/AC123/Tokens.json HTTP/1.1"
    );
    let basic = base64::engine::general_purpose::STANDARD.encode("SK456:tw-secret");
    assert_eq!(
        header(&seen[0], "authorization"),
        Some(format!("Basic {basic}").as_str())
    );
    assert_eq!(seen[0].body, "Ttl=600");
    assert!(!ice.body.contains("tw-secret"));
}

#[tokio::test]
async fn a_failing_provider_degrades_to_no_relay_without_failing_the_call_and_is_not_hammered() {
    let mock = Mock::start(Behaviour::Answer(500, "{}".to_owned())).await;
    let (server, _parent) = start(config(Some(cloudflare(&mock)))).await;
    let created = post_whip(server.http_addr(), &[]).await;
    assert_eq!(
        created.status, 201,
        "the call must not fail: {}",
        created.body
    );
    assert!(created.all("link").is_empty());
    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ice.body).unwrap(),
        serde_json::json!({ "iceServers": [] })
    );
    // The failure is remembered for a while: one upstream request in total.
    assert_eq!(mock.requests().len(), 1);

    // Garbage that is not JSON degrades the same way.
    let garbage = Mock::start(Behaviour::Answer(201, "not json".to_owned())).await;
    let (server2, _parent2) = start(config(Some(cloudflare(&garbage)))).await;
    let ice = request(server2.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert!(ice.body.contains("[]"));
    assert_eq!(garbage.requests().len(), 1);

    // JSON with no usable server degrades the same way.
    let empty = Mock::start(Behaviour::Answer(201, r#"{"iceServers":[]}"#.to_owned())).await;
    let (server3, _parent3) = start(config(Some(cloudflare(&empty)))).await;
    let ice = request(server3.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert!(ice.body.contains("[]"));
    assert_eq!(empty.requests().len(), 1);
}

#[tokio::test]
async fn a_hanging_provider_is_cut_off_at_the_fetch_deadline() {
    let mock = Mock::start(Behaviour::Hang).await;
    let mut turn = cloudflare(&mock);
    turn.fetch_timeout = Duration::from_millis(300);
    let (server, _parent) = start(config(Some(turn))).await;
    let clock = SystemClock::shared();
    let started = clock.now();
    let created = post_whip(server.http_addr(), &[]).await;
    let took = clock.now().saturating_duration_since(started);
    assert_eq!(created.status, 201);
    assert!(created.all("link").is_empty());
    assert!(took < Duration::from_secs(3), "{took:?}");
    // A second request right away does not wait again (the failure is remembered).
    let started = clock.now();
    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert!(
        clock.now().saturating_duration_since(started) < Duration::from_millis(250),
        "{:?}",
        clock.now().saturating_duration_since(started)
    );
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn concurrent_requests_share_one_upstream_fetch() {
    let mock = Mock::start(Behaviour::Answer(201, CLOUDFLARE_ANSWER.to_owned())).await;
    let (server, _parent) = start(config(Some(cloudflare(&mock)))).await;
    let addr = server.http_addr();
    let mut tasks = Vec::new();
    // At most the ICE burst (5) and the per-address connection cap (8): all from 127.0.0.1.
    for _ in 0..5 {
        tasks.push(tokio::spawn(async move {
            request(addr, "GET", "/whip/ice", &[], &[]).await
        }));
    }
    for task in tasks {
        let reply = task.await.unwrap();
        assert!(reply.body.contains("cf-cred"));
    }
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn a_remote_plain_http_provider_url_is_refused_at_startup() {
    let turn = TurnConfig::new(TurnProvider::Cloudflare {
        key_id: "k".to_owned(),
        api_token: Secret::new("t".to_owned()),
        base_url: "http://turn.example.com".to_owned(),
    });
    let parent = CancellationToken::new();
    let error = WebRtcServer::bind(config(Some(turn)), SystemClock::shared(), &parent)
        .await
        .err()
        .expect("must be refused");
    assert!(error.to_string().contains("turn"), "{error}");
}

#[tokio::test]
async fn a_provider_outage_does_not_stall_the_offer_for_the_fetch_deadline() {
    // The default fetch deadline is 5 s; `POST /whip` waits at most about 1.5 s for credentials.
    let mock = Mock::start(Behaviour::Hang).await;
    let turn = cloudflare(&mock);
    assert_eq!(turn.fetch_timeout, Duration::from_secs(5));
    let (server, _parent) = start(config(Some(turn))).await;
    let clock = SystemClock::shared();
    let started = clock.now();
    let created = post_whip(server.http_addr(), &[]).await;
    let took = clock.now().saturating_duration_since(started);
    assert_eq!(created.status, 201, "{}", created.body);
    assert!(created.all("link").is_empty());
    assert!(
        took < Duration::from_millis(3_500),
        "the offer waited {took:?} for the TURN provider"
    );
    // The failure is remembered: the next offer and the ICE endpoint do not wait at all.
    let started = clock.now();
    let created = post_whip(server.http_addr(), &[]).await;
    let ice = request(server.http_addr(), "GET", "/whip/ice", &[], &[]).await;
    assert_eq!((created.status, ice.status), (201, 200));
    assert!(
        clock.now().saturating_duration_since(started) < Duration::from_secs(1),
        "{:?}",
        clock.now().saturating_duration_since(started)
    );
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn get_ice_is_rate_limited_per_client_and_options_does_not_advertise_post() {
    let (server, _parent) = start(config(Some(coturn(Duration::from_secs(600))))).await;
    let addr = server.http_addr();
    // A burst is served; then the client is told to slow down.
    let mut limited = None;
    for n in 0..60 {
        let reply = request(addr, "GET", "/whip/ice", &[], &[]).await;
        if reply.status == 429 {
            limited = Some((n, reply));
            break;
        }
        assert_eq!(reply.status, 200);
    }
    let (n, reply) = limited.expect("the limiter never engaged");
    assert!(n >= 5, "limited after only {n} requests");
    assert!(
        n < 20,
        "the burst is small: limited only after {n} requests"
    );
    assert!(reply.one("retry-after").is_some());
    assert!(!reply.body.contains("turn.example.com"));
    // Signaling itself is not affected by the ICE endpoint's limiter.
    assert_eq!(post_whip(addr, &[]).await.status, 201);

    // OPTIONS: the ICE endpoint takes GET only; the offer endpoint takes SDP POSTs.
    let ice = request(addr, "OPTIONS", "/whip/ice", &[], &[]).await;
    assert_eq!(ice.status, 200);
    assert_eq!(ice.one("allow"), Some("GET, OPTIONS"));
    assert!(ice.one("accept-post").is_none());
    let whip = request(addr, "OPTIONS", "/whip", &[], &[]).await;
    assert_eq!(whip.one("accept-post"), Some("application/sdp"));
}
