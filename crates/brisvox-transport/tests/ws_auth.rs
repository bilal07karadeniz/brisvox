//! Credentials at the generic websocket listener: static token, session tokens, the
//! three places a token may travel, and the session claims in the call's metadata.
#![cfg(feature = "websocket")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)] // test helpers (see clippy.toml)

use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_transport::auth::{Audience, Authenticator, SessionSecrets, TokenSpec, mint_token};
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerConfig, Start, WebSocketServer};
use brisvox_transport::{SessionConfig, Transport, TransportEvent, TransportSession};
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);
const SECRET: &str = "a-generic-ws-session-secret-of-32-bytes";

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn unix_now() -> u64 {
    SystemClock::shared()
        .wall()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn spec(sid: &str, audience: Audience) -> TokenSpec {
    TokenSpec {
        audience,
        sid: sid.to_owned(),
        not_before: unix_now() - 5,
        expires: unix_now() + 120,
        agent: Some("web".to_owned()),
        max_s: Some(300),
        meta: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
    }
}

fn mint(spec: &TokenSpec) -> String {
    mint_token(&Secret::new(SECRET.to_owned()), spec).unwrap()
}

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: WebSocketServer,
}

impl Rig {
    async fn start(static_token: Option<&str>, sessions: bool) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let secrets =
            sessions.then(|| SessionSecrets::new(Secret::new(SECRET.to_owned()), None).unwrap());
        let static_token = static_token.map(|t| Secret::new(t.to_owned()));
        let auth = Authenticator::from_config(
            Audience::WebSocket,
            static_token.as_ref(),
            secrets.as_ref(),
            &clock,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = WebSocketServer::serve_authenticated(
            listener,
            ServerConfig::default(),
            clock.clone(),
            &parent,
            None,
            auth,
        )
        .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    async fn connect(
        &self,
        path_and_query: &str,
        headers: &[(&'static str, &str)],
    ) -> Result<Client, WsError> {
        let url = format!("ws://{}{path_and_query}", self.server.local_addr());
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

    /// Plays the client's `start` (with a forged `session_id` of its own) and the engine's
    /// `Started` event; returns its call metadata.
    async fn open(&mut self, mut ws: Client) -> BTreeMap<String, String> {
        let mut start = Start::new(16_000, Encoding::Pcm16);
        start
            .metadata
            .insert("session_id".to_owned(), "forged".to_owned());
        start
            .metadata
            .insert("session.agent".to_owned(), "forged".to_owned());
        start.metadata.insert("app".to_owned(), "kept".to_owned());
        ws.send(Message::text(ClientMessage::Start(start).to_json()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let link = tokio::time::timeout(WAIT, self.server.accept(&cancel))
            .await
            .expect("no call")
            .unwrap()
            .expect("server closed");
        let mut session = TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap();
        let TransportEvent::Started(info) = session.recv().await.unwrap() else {
            panic!("Started is the first event");
        };
        // The client sees `started`.
        let reply = tokio::time::timeout(WAIT, ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Text(text) = reply else {
            panic!("{reply:?}");
        };
        assert!(text.contains("started"), "{text}");
        info.metadata
    }
}

fn status(result: &Result<Client, WsError>) -> u16 {
    match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_open_listener_needs_no_credential() {
    let rig = Rig::start(None, false).await;
    assert_eq!(status(&rig.connect("/call", &[]).await), 101);
}

#[tokio::test]
async fn the_static_token_works_in_the_header_the_subprotocol_and_the_query() {
    let rig = Rig::start(Some("dev-ws-token"), false).await;
    assert_eq!(status(&rig.connect("/call", &[]).await), 401);
    assert_eq!(status(&rig.connect("/call?token=nope", &[]).await), 403);
    assert_eq!(
        status(&rig.connect("/call?token=dev-ws-token", &[]).await),
        101
    );
    assert_eq!(
        status(
            &rig.connect("/call", &[("Authorization", "Bearer dev-ws-token")])
                .await
        ),
        101
    );
    assert_eq!(
        status(
            &rig.connect(
                "/call",
                &[(
                    "Sec-WebSocket-Protocol",
                    "brisvox, brisvox-token.dev-ws-token"
                )]
            )
            .await
        ),
        101
    );
    let Err(WsError::Http(response)) = rig.connect("/call", &[]).await else {
        panic!("expected a refusal");
    };
    assert_eq!(response.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn a_session_token_opens_one_call_with_its_claims_and_the_clients_cannot_forge_them() {
    let mut rig = Rig::start(None, true).await;
    let token = mint(&spec("ws-visitor", Audience::WebSocket));
    let ws = rig
        .connect("/call", &[("Authorization", &format!("Bearer {token}"))])
        .await
        .expect("a valid token connects");
    let metadata = rig.open(ws).await;
    assert_eq!(metadata["session_id"], "ws-visitor");
    assert_eq!(metadata["session.agent"], "web");
    assert_eq!(metadata["session.max_s"], "300");
    assert_eq!(metadata["session.meta.tenant"], "acme");
    assert_eq!(metadata["app"], "kept", "ordinary client metadata survives");

    // Single use; and the other audiences' tokens do not open this listener.
    assert_eq!(
        status(
            &rig.connect("/call", &[("Authorization", &format!("Bearer {token}"))])
                .await
        ),
        403
    );
    let rtvi = mint(&spec("other", Audience::Rtvi));
    assert_eq!(
        status(&rig.connect(&format!("/call?token={rtvi}"), &[]).await),
        403
    );
    assert_eq!(status(&rig.connect("/call", &[]).await), 401);
    let huge = "a".repeat(5_000);
    assert_eq!(
        status(
            &rig.connect("/call", &[("Authorization", &format!("Bearer {huge}"))])
                .await
        ),
        413
    );
}

#[tokio::test]
async fn a_client_cannot_claim_session_metadata_on_an_open_listener() {
    let mut rig = Rig::start(None, false).await;
    let ws = rig.connect("/call", &[]).await.unwrap();
    let metadata = rig.open(ws).await;
    assert!(!metadata.contains_key("session_id"), "{metadata:?}");
    assert!(!metadata.contains_key("session.agent"), "{metadata:?}");
    assert_eq!(metadata["app"], "kept");
}
