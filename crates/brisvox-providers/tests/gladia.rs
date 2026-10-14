//! Gladia STT adapter against a local mock of the Gladia live protocol: the HTTP init call, the
//! WebSocket, result mapping, finalize, keepalive, errors, reconnect with replay, the
//! provider contract suite, and one live test (`BRISVOX_LIVE_TESTS=1`).
//!
//! The mock is one `TcpListener` that answers `POST /v2/live` (and `HEAD`) as a minimal HTTP/1.1
//! server and upgrades `GET` requests with `tokio_tungstenite::accept_hdr_async`. Real sockets do
//! not work with paused tokio time, so everything runs on the real `SystemClock` with short
//! deadlines.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::result_large_err
)]

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::gladia::{Encoding, GladiaConfig, GladiaStt, Region};
use brisvox_providers::{Error, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::contract::{Fault, SttHarness, Utterance, stt_contract};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

const KEY: &str = "gla-test-key-0123456789";
const STOP: &str = r#"{"type":"stop_recording"}"#;

// ---------------------------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the server saw on one WebSocket connection.
#[derive(Debug, Clone)]
enum What {
    Audio(Bytes),
    Text(String),
    /// A WebSocket Ping from the client (the library answers it with a Pong).
    Ping,
}

#[derive(Debug, Clone)]
struct Seen {
    conn: usize,
    at: Instant,
    what: What,
}

/// Everything the server saw on its WebSockets, shared with the test.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Seen>>>);

impl Log {
    fn push(&self, conn: usize, what: What) {
        lock(&self.0).push(Seen {
            conn,
            at: Instant::now(),
            what,
        });
    }

    fn all(&self) -> Vec<Seen> {
        lock(&self.0).clone()
    }

    fn audio(&self, conn: usize) -> Vec<Bytes> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn)
            .filter_map(|s| match s.what {
                What::Audio(b) => Some(b),
                What::Text(_) | What::Ping => None,
            })
            .collect()
    }

    fn texts(&self, conn: usize) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn)
            .filter_map(|s| match s.what {
                What::Text(t) => Some(t),
                What::Audio(_) | What::Ping => None,
            })
            .collect()
    }

    /// Pings the client sent on connection `conn`.
    fn pings(&self, conn: usize) -> usize {
        self.all()
            .iter()
            .filter(|s| s.conn == conn && matches!(s.what, What::Ping))
            .count()
    }

    /// Waits (real time) until `cond` holds for the log, failing the test after 5 s.
    async fn wait(&self, what: &str, cond: impl Fn(&Log) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The WebSocket upgrade request the client made.
#[derive(Debug, Clone, Default)]
struct Req {
    uri: String,
    headers: Vec<(String, String)>,
}

/// The HTTP request of an init call.
#[derive(Debug, Clone)]
struct InitReq {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl InitReq {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// How the mock answers one init call.
#[derive(Debug, Clone)]
enum Init {
    /// `201` with a session URL that points back at the mock.
    Ok,
    /// An error status with Gladia's error body (which echoes the key, to prove it is not
    /// repeated).
    Status(u16),
    /// `201` with this raw body.
    Body(String),
    /// `201` with this `url`.
    Url(String),
    /// Waits, then answers like `Ok`.
    Stall(Duration),
    /// Accepts and never answers.
    Hang,
    /// `307` with this `Location` (a second listener the key must never reach).
    Redirect(String),
}

type InitHook = Arc<dyn Fn(usize) -> Init + Send + Sync>;

#[derive(Default)]
struct Shared {
    /// WebSocket connections accepted.
    connections: AtomicUsize,
    heads: AtomicUsize,
    /// The HEAD requests of `warm()`.
    head_reqs: Mutex<Vec<InitReq>>,
    /// The `DELETE /v2/live/{id}` clean-up calls.
    deletes: Mutex<Vec<InitReq>>,
    requests: Mutex<Vec<Req>>,
    inits: Mutex<Vec<InitReq>>,
    init_at: Mutex<Vec<Instant>>,
    init_script: Mutex<VecDeque<Init>>,
    init_hook: Mutex<Option<InitHook>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// A WebSocket connection the server has accepted at TCP level but not yet answered.
struct Conn {
    stream: TcpStream,
    index: usize,
    shared: Arc<Shared>,
    log: Log,
}

type Ws = WebSocketStream<TcpStream>;

impl Conn {
    /// Completes the WebSocket handshake.
    async fn accept(self) -> Option<Server> {
        let shared = Arc::clone(&self.shared);
        let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            lock(&shared.requests).push(Req {
                uri: req.uri().to_string(),
                headers: req
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
                    .collect(),
            });
            Ok(resp)
        };
        let ws = accept_hdr_async(self.stream, callback).await.ok()?;
        Some(Server {
            ws,
            index: self.index,
            log: self.log,
            audio_frames: 0,
        })
    }

    /// Answers the upgrade request with an HTTP error.
    async fn reject(self, status: u16) {
        let callback = move |_req: &Request, _resp: Response| -> Result<Response, ErrorResponse> {
            Err(http::Response::builder()
                .status(status)
                .body(Some(r#"{"message":"refused"}"#.to_owned()))
                .unwrap())
        };
        let _ = accept_hdr_async(self.stream, callback).await;
    }
}

/// The server side of an accepted WebSocket.
struct Server {
    ws: Ws,
    index: usize,
    log: Log,
    audio_frames: usize,
}

impl Server {
    /// Next client message, logged. `None` when the connection is over.
    async fn next(&mut self) -> Option<What> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Binary(b)) => {
                    self.audio_frames += 1;
                    self.log.push(self.index, What::Audio(b.clone()));
                    return Some(What::Audio(b));
                }
                Ok(Message::Text(t)) => {
                    self.log.push(self.index, What::Text(t.to_string()));
                    return Some(What::Text(t.to_string()));
                }
                Ok(Message::Ping(_)) => self.log.push(self.index, What::Ping),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// Reads until `n` audio frames arrived on this connection.
    async fn until_audio(&mut self, n: usize) -> bool {
        while self.audio_frames < n {
            if self.next().await.is_none() {
                return false;
            }
        }
        true
    }

    /// Reads until the client goes away.
    async fn drain(&mut self) {
        while self.next().await.is_some() {}
    }

    async fn send(&mut self, text: impl Into<String>) {
        let _ = self.ws.send(Message::text(text.into())).await;
    }

    async fn close_with(&mut self, code: u16, reason: &str) {
        let _ = self
            .ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: reason.to_owned().into(),
            })))
            .await;
        // Let the client read the close frame before the socket goes.
        let _ = tokio::time::timeout(Duration::from_millis(300), self.drain()).await;
    }
}

type Handler = Arc<dyn Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync>;

struct MockServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    log: Log,
    accept_task: AbortHandle,
}

impl MockServer {
    async fn start<F>(ws_handler: F) -> Self
    where
        F: Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        let handler: Handler = Arc::new(ws_handler);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let log = Log::default();
        let accept_shared = Arc::clone(&shared);
        let accept_log = log.clone();
        let accept = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    // A transient error (a connection reset before it was accepted, under
                    // load) must not end the loop: later connects would be refused.
                    Err(_) => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                };
                let task = tokio::spawn(serve_tcp(
                    stream,
                    addr,
                    Arc::clone(&accept_shared),
                    accept_log.clone(),
                    Arc::clone(&handler),
                ));
                lock(&accept_shared.tasks).push(task.abort_handle());
            }
        });
        Self {
            addr,
            shared,
            log,
            accept_task: accept.abort_handle(),
        }
    }

    /// The init endpoint's base URL.
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    fn heads(&self) -> usize {
        self.shared.heads.load(Ordering::SeqCst)
    }

    fn head_requests(&self) -> Vec<InitReq> {
        lock(&self.shared.head_reqs).clone()
    }

    fn deletes(&self) -> Vec<InitReq> {
        lock(&self.shared.deletes).clone()
    }

    /// Every HTTP request of any kind that reached this server.
    fn http_requests(&self) -> usize {
        self.inits().len() + self.heads() + self.deletes().len()
    }

    fn requests(&self) -> Vec<Req> {
        lock(&self.shared.requests).clone()
    }

    fn inits(&self) -> Vec<InitReq> {
        lock(&self.shared.inits).clone()
    }

    /// When each init call arrived.
    fn init_at(&self) -> Vec<Instant> {
        lock(&self.shared.init_at).clone()
    }

    /// The answers to the next init calls, in order; afterwards `Init::Ok`.
    fn script_init(&self, answers: impl IntoIterator<Item = Init>) {
        lock(&self.shared.init_script).extend(answers);
    }

    /// Decides every init answer by the 0-based number of the call (overrides the script).
    fn init_hook(&self, hook: impl Fn(usize) -> Init + Send + Sync + 'static) {
        *lock(&self.shared.init_hook) = Some(Arc::new(hook));
    }

    /// Aborts every connection task (stalled ones included).
    fn abort_handlers(&self) {
        for handle in lock(&self.shared.tasks).drain(..) {
            handle.abort();
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.abort_handlers();
    }
}

fn handler<F, Fut>(f: F) -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static
where
    F: Fn(Conn) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    move |conn| Box::pin(f(conn))
}

/// Sorts a TCP connection into HTTP (init, warm-up) or WebSocket by its first bytes.
async fn serve_tcp(
    stream: TcpStream,
    addr: SocketAddr,
    shared: Arc<Shared>,
    log: Log,
    ws_handler: Handler,
) {
    let mut first = [0_u8; 8];
    loop {
        match stream.peek(&mut first).await {
            Ok(n) if n >= 4 => break,
            Ok(0) | Err(_) => return,
            Ok(_) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
    if first.starts_with(b"GET ") {
        let index = shared.connections.fetch_add(1, Ordering::SeqCst);
        ws_handler(Conn {
            stream,
            index,
            shared,
            log,
        })
        .await;
    } else {
        serve_http(stream, addr, shared).await;
    }
}

/// Reads one HTTP/1.1 request: the head and a `Content-Length` body.
async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let start = end + 4;
            while data.len() < start + length {
                let n = stream.read(&mut buf).await.ok()?;
                if n == 0 {
                    return None;
                }
                data.extend_from_slice(&buf[..n]);
            }
            return Some((head, data[start..start + length].to_vec()));
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    respond_with(stream, status, "", body).await;
}

/// `extra` is zero or more complete header lines, each ending in CRLF.
async fn respond_with(stream: &mut TcpStream, status: u16, extra: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         {extra}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn serve_http(mut stream: TcpStream, addr: SocketAddr, shared: Arc<Shared>) {
    let Some((head, body)) = read_request(&mut stream).await else {
        return;
    };
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let record = |method: String, target: String, headers: Vec<(String, String)>| InitReq {
        method,
        target,
        headers,
        body: String::new(),
    };
    if method == "HEAD" {
        lock(&shared.head_reqs).push(record(method, target, headers));
        shared.heads.fetch_add(1, Ordering::SeqCst);
        respond(&mut stream, 405, "").await;
        return;
    }
    if method == "DELETE" {
        lock(&shared.deletes).push(record(method, target, headers));
        respond(&mut stream, 202, "").await;
        return;
    }
    let n = {
        let mut inits = lock(&shared.inits);
        inits.push(InitReq {
            method,
            target,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        inits.len() - 1
    };
    lock(&shared.init_at).push(Instant::now());
    let hook = lock(&shared.init_hook).clone();
    let answer = match hook {
        Some(hook) => hook(n),
        None => lock(&shared.init_script).pop_front().unwrap_or(Init::Ok),
    };
    let ok_url = format!("ws://{addr}/v2/live?token=tok-{n}");
    let ok_body = |url: &str| {
        json!({"id": format!("sess-{n}"), "created_at": "2026-10-08T00:00:00.000Z", "url": url})
            .to_string()
    };
    match answer {
        Init::Ok => respond(&mut stream, 201, &ok_body(&ok_url)).await,
        Init::Url(url) => respond(&mut stream, 201, &ok_body(&url)).await,
        Init::Body(raw) => respond(&mut stream, 201, &raw).await,
        Init::Status(code) => {
            let body = json!({
                "timestamp": "2026-10-08T00:00:00.000Z", "path": "/v2/live",
                "request_id": "G-req-1", "statusCode": code,
                "message": format!("mock refusal for key {KEY}"),
            })
            .to_string();
            respond(&mut stream, code, &body).await;
        }
        Init::Stall(duration) => {
            tokio::time::sleep(duration).await;
            respond(&mut stream, 201, &ok_body(&ok_url)).await;
        }
        Init::Hang => tokio::time::sleep(Duration::from_secs(60)).await,
        Init::Redirect(location) => {
            respond_with(&mut stream, 307, &format!("Location: {location}\r\n"), "").await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Gladia messages
// ---------------------------------------------------------------------------------------------

fn transcript(id: &str, text: &str, is_final: bool, end: f64) -> String {
    json!({
        "session_id": "sess", "created_at": "2026-10-08T00:00:00Z", "type": "transcript",
        "data": {"id": id, "is_final": is_final, "utterance": {
            "start": 0.0, "end": end, "confidence": 0.9, "channel": 0,
            "words": [{"word": text, "start": 0.0, "end": end, "confidence": 0.9}],
            "text": text, "language": "tr"}}
    })
    .to_string()
}

fn speech(kind: &str, time: f64) -> String {
    json!({"session_id": "sess", "created_at": "2026-10-08T00:00:00Z", "type": kind,
        "data": {"time": time, "channel": 0}})
    .to_string()
}

fn audio_ack(acknowledged: bool) -> String {
    json!({"session_id": "sess", "created_at": "2026-10-08T00:00:00Z", "type": "audio_chunk",
        "acknowledged": acknowledged,
        "error": if acknowledged { serde_json::Value::Null } else {
            json!({"status_code": 400, "exception": "InvalidArgument",
                   "message": "Unsupported sample rate"}) },
        "data": {"byte_range": [0, 3199], "time_range": [0.0, 0.1]}})
    .to_string()
}

fn lifecycle(kind: &str) -> String {
    json!({"session_id": "sess", "created_at": "2026-10-08T00:00:00Z", "type": kind}).to_string()
}

// ---------------------------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------------------------

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn stt_cfg() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

/// Adapter settings for tests: short waits, fast replay.
fn config(server: &MockServer) -> GladiaConfig {
    let mut c = GladiaConfig::new(Secret::new(KEY.to_owned()));
    c.base_url = Some(server.base_url());
    c.io_timeout = Duration::from_secs(2);
    // Longer than any test waits, but below the 30 s the config accepts. Tests that wait for the
    // silence set their own values.
    c.keepalive_interval = Duration::from_secs(9);
    c.finalize_timeout = Duration::from_millis(250);
    c.close_timeout = Duration::from_millis(300);
    c.reconnect_backoff = Duration::from_millis(10);
    c.replay_speed = 1000.0;
    c
}

fn adapter(server: &MockServer, tweak: impl FnOnce(&mut GladiaConfig)) -> GladiaStt {
    let mut c = config(server);
    tweak(&mut c);
    GladiaStt::new(c, clock()).expect("adapter")
}

async fn connect(stt: &GladiaStt) -> SttSession {
    stt.connect(stt_cfg()).await.expect("connect")
}

/// 100 ms of 16 kHz PCM16, filled with `fill`.
fn chunk(fill: u8) -> SttInput {
    SttInput::Audio(Bytes::from(vec![fill; 3200]))
}

async fn event(session: &mut SttSession) -> SttEvent {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("no event within 5 s")
        .expect("the events closed")
}

/// (text, is_final, speech_final, from_finalize) of a transcript event.
fn flat(event: SttEvent) -> (String, bool, bool, bool) {
    match event {
        SttEvent::Transcript {
            text,
            is_final,
            speech_final,
            from_finalize,
        } => (text.to_string(), is_final, speech_final, from_finalize),
        SttEvent::Error(e) => panic!("unexpected error event: {e:?}"),
    }
}

fn t(text: &str, f: bool, s: bool, ff: bool) -> (String, bool, bool, bool) {
    (text.to_owned(), f, s, ff)
}

async fn expect_closed(session: &mut SttSession) {
    let next = tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("the events did not close within 5 s");
    assert!(next.is_none(), "expected the events to close, got {next:?}");
}

async fn expect_error(session: &mut SttSession) -> Error {
    match event(session).await {
        SttEvent::Error(e) => e,
        other => panic!("expected an error event, got {other:?}"),
    }
}

/// The fill bytes of 3200-byte test frames.
fn fills(frames: &[Bytes]) -> Vec<u8> {
    frames.iter().map(|f| f[0]).collect()
}

/// A server that accepts and reads until the client leaves.
fn draining() -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Init call and handshake
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_init_call_carries_the_key_and_the_documented_body_and_the_socket_only_a_token() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |_| {});
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr-TR".into()),
        })
        .await
        .unwrap();
    let inits = server.inits();
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].method, "POST");
    assert_eq!(inits[0].target, "/v2/live");
    assert_eq!(inits[0].header("x-gladia-key"), Some(KEY));
    assert!(
        inits[0]
            .header("content-type")
            .is_some_and(|v| v.starts_with("application/json"))
    );
    assert_eq!(
        inits[0].json(),
        json!({
            "encoding": "wav/pcm", "bit_depth": 16, "sample_rate": 16000, "channels": 1,
            "model": "solaria-1", "endpointing": 0.1,
            "maximum_duration_without_endpointing": 15.0,
            "language_config": {"languages": ["tr"], "code_switching": false},
            "messages_config": {
                "receive_partial_transcripts": true, "receive_final_transcripts": true,
                "receive_speech_events": true, "receive_pre_processing_events": false,
                "receive_realtime_processing_events": false,
                "receive_post_processing_events": false, "receive_acknowledgments": false,
                "receive_errors": true, "receive_lifecycle_events": false
            }
        })
    );
    // The socket is opened on the URL the init call returned and carries no key.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].uri, "/v2/live?token=tok-0");
    for (name, value) in &requests[0].headers {
        assert!(
            !name.eq_ignore_ascii_case("x-gladia-key")
                && !name.eq_ignore_ascii_case("authorization")
                && !value.contains(KEY),
            "the key must not travel on the socket: {name}"
        );
    }
}

#[tokio::test]
async fn region_vocabulary_and_telephony_settings_reach_the_init_call() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| {
        c.region = Some(Region::EuWest);
        c.encoding = Encoding::Ulaw;
        c.acknowledgments = true;
        c.endpointing = 0.25;
        c.vocabulary = vec!["Brisvox".to_owned()];
        c.language = Some("en".to_owned());
    });
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        })
        .await
        .unwrap();
    let init = &server.inits()[0];
    assert_eq!(init.target, "/v2/live?region=eu-west");
    let body = init.json();
    assert_eq!(body["encoding"], "wav/ulaw");
    assert_eq!(body["bit_depth"], 8);
    assert_eq!(body["sample_rate"], 8000);
    assert_eq!(body["endpointing"], 0.25);
    assert_eq!(body["language_config"]["languages"], json!(["en"]));
    assert_eq!(body["messages_config"]["receive_acknowledgments"], true);
    assert_eq!(
        body["realtime_processing"]["custom_vocabulary_config"]["vocabulary"],
        json!(["Brisvox"])
    );
}

#[tokio::test]
async fn audio_goes_out_as_binary_frames_in_order_and_unchanged() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    for fill in 1..=3 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("3 audio frames", |l| l.audio(0).len() == 3)
        .await;
    for (i, frame) in server.log.audio(0).iter().enumerate() {
        assert_eq!(frame.as_ref(), vec![i as u8 + 1; 3200].as_slice());
    }
    assert!(
        server.log.texts(0).is_empty(),
        "no control message while audio flows"
    );
}

// ---------------------------------------------------------------------------------------------
// Result mapping
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_server_message_type_is_handled() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(lifecycle("start_session")).await;
        s.send(lifecycle("start_recording")).await;
        s.send(speech("speech_start", 0.1)).await;
        s.send(transcript("00-00000001", "merhaba", false, 0.4))
            .await;
        s.send(audio_ack(true)).await;
        s.send(json!({"type": "stop_recording", "acknowledged": false,
            "error": {"status_code": 409, "exception": "ConflictError",
                      "message": "Recording already stopped"}})
        .to_string())
            .await;
        s.send(transcript("00-00000001", "merhaba dünya", false, 0.8))
            .await;
        s.send(speech("speech_end", 0.9)).await;
        s.send(transcript("00-00000001", "Merhaba dünya.", true, 0.9))
            .await;
        s.send(
            json!({"type": "post_transcript", "data": {"full_transcript": "x", "languages": ["tr"]}})
                .to_string(),
        )
        .await;
        s.send(
            json!({"type": "post_final_transcript", "data": {"metadata": {"audio_duration": 1.0}}})
                .to_string(),
        )
        .await;
        s.send(json!({"type": "translation", "data": {}}).to_string())
            .await;
        s.send(json!({"type": "something_new", "extra": [1, 2]}).to_string())
            .await;
        s.send(transcript("00-00000002", "iyiyim", false, 1.4)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(flat(event(&mut session).await));
    }
    assert_eq!(
        got,
        [
            t("merhaba", false, false, false),
            t("merhaba dünya", false, false, false),
            t("Merhaba dünya.", true, true, false),
            t("iyiyim", false, false, false),
        ]
    );
}

#[tokio::test]
async fn a_transcript_without_is_final_is_a_partial_and_blank_ones_are_dropped() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // The shape of the documented quickstart example: no is_final, no words.
        s.send(
            json!({"type": "transcript", "session_id": "s", "created_at": "t",
                "data": {"id": "00_00000000", "utterance": {"text": "Merhaba, ben Sami.",
                    "start": 0.188, "end": 2.852, "language": "tr", "channel": 0}}})
            .to_string(),
        )
        .await;
        s.send(transcript("00_00000000", "", false, 1.0)).await;
        s.send(transcript("00_00000000", "   ", true, 1.0)).await;
        s.send(transcript("00_00000001", "tamam", true, 3.0)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba, ben Sami.", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("tamam", true, true, false)
    );
}

#[tokio::test]
async fn partials_are_dropped_when_the_consumer_lags_but_finals_are_not() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        for i in 0..400 {
            s.send(transcript("u1", &format!("word {i}"), false, 0.5))
                .await;
        }
        s.send(transcript("u1", "the end", true, 1.0)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    // The consumer does not read while the server floods.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut partials = 0;
    loop {
        let (text, is_final, speech_final, _) = flat(event(&mut session).await);
        if is_final {
            assert_eq!((text.as_str(), speech_final), ("the end", true));
            break;
        }
        partials += 1;
    }
    assert!(
        (1..400).contains(&partials),
        "some partials survive, the flood does not: {partials}"
    );
}

// ---------------------------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_sends_nothing_and_gladias_own_final_answers_it() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(speech("speech_start", 0.1)).await;
        s.send(transcript("u1", "tamam", false, 0.5)).await;
        // Endpointing closes the utterance a little later.
        tokio::time::sleep(Duration::from_millis(150)).await;
        s.send(transcript("u1", "Tamam, evet.", true, 0.9)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(2));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("tamam", false, false, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Tamam, evet.", true, true, true)
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "the real final must answer, not the timeout"
    );
    assert!(
        server.log.texts(0).is_empty(),
        "Gladia has no finalize message: nothing may be sent for it"
    );
}

#[tokio::test]
async fn an_unanswered_finalize_ends_with_the_last_partial_and_the_late_real_final_is_dropped() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(transcript("u1", "partial text", false, 0.5)).await;
        // Far later than the finalize timeout.
        tokio::time::sleep(Duration::from_millis(700)).await;
        s.send(transcript("u1", "Partial text, corrected.", false, 0.6))
            .await;
        s.send(transcript("u1", "Partial text, corrected.", true, 0.7))
            .await;
        s.send(transcript("u2", "sonraki", false, 1.5)).await;
        s.send(transcript("u2", "Sonraki cümle.", true, 1.9)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("partial text", false, false, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("partial text", true, true, true)
    );
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_millis(650),
        "{waited:?}"
    );
    // The corrected text of the ended utterance never arrives; the next utterance does.
    assert_eq!(
        flat(event(&mut session).await),
        t("sonraki", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("Sonraki cümle.", true, true, false)
    );
}

#[tokio::test]
async fn a_finalize_on_a_session_that_heard_nothing_waits_and_then_ends_empty() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(start.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn a_finalize_with_nothing_open_is_answered_at_once_with_an_empty_final() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(transcript("u1", "bitti", false, 0.5)).await;
        s.send(transcript("u1", "Bitti.", true, 0.8)).await;
        s.drain().await;
    }))
    .await;
    // A long timeout: only the immediate answer can pass within the asserted time.
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(5));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bitti", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("Bitti.", true, true, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn speech_without_a_partial_yet_makes_a_finalize_wait_for_its_final() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(transcript("u0", "önce", true, 0.4)).await;
        s.send(speech("speech_start", 1.0)).await;
        // Long enough that the client's Finalize (300 ms after the first final) still comes before the
        // final even when the machine is busy.
        tokio::time::sleep(Duration::from_millis(900)).await;
        s.send(transcript("u1", "Sonra.", true, 1.4)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("önce", true, true, false)
    );
    // speech_start has been seen (it arrives right after the final), no partial yet. The adapter
    // exposes no sign that it has processed it, so the test gives it time: a Finalize that got
    // there first would be answered at once as "nothing open" (an empty final), which is what a
    // busy machine produced with 40 ms.
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Sonra.", true, true, true)
    );
}

// ---------------------------------------------------------------------------------------------
// Keepalive and the response watchdog
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn silence_is_sent_when_no_audio_flows_and_not_while_it_does() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(120)
    });
    let started = Instant::now();
    let session = connect(&stt).await;
    server
        .log
        .wait("two silence frames", |l| l.audio(0).len() >= 2)
        .await;
    let first = lock(&server.log.0)[0].at;
    assert!(first - started >= Duration::from_millis(100));
    for frame in server.log.audio(0) {
        assert_eq!(
            frame.as_ref(),
            vec![0_u8; 640].as_slice(),
            "20 ms of PCM16 silence"
        );
    }
    // While audio flows more often than the interval, no silence is added.
    let before = server.log.audio(0).len();
    for fill in 1..=12 {
        session.input.send(chunk(fill)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let frames = server.log.audio(0);
    let silent = frames[before..].iter().filter(|f| f.len() == 640).count();
    assert_eq!(silent, 0, "silence was added while audio was flowing");
    assert_eq!(frames.len() - before, 12);
    assert!(server.log.texts(0).is_empty());
}

#[tokio::test]
async fn telephony_silence_uses_the_encodings_own_silence_byte() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| {
        c.encoding = Encoding::Ulaw;
        c.keepalive_interval = Duration::from_millis(100);
    });
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        })
        .await
        .unwrap();
    server
        .log
        .wait("a silence frame", |l| !l.audio(0).is_empty())
        .await;
    assert_eq!(
        server.log.audio(0)[0].as_ref(),
        vec![0xFF_u8; 160].as_slice()
    );
}

/// Acknowledges every audio frame, like Gladia with `receive_acknowledgments`.
fn acking() -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        while let Some(message) = s.next().await {
            if matches!(message, What::Audio(_)) {
                s.send(audio_ack(true)).await;
            }
        }
    })
}

#[tokio::test]
async fn with_acknowledgments_a_silent_server_is_replaced_and_then_reported_as_a_timeout() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| {
        c.acknowledgments = true;
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 1;
    });
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(chunk(1)).await.unwrap();
    let error = expect_error(&mut session).await;
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error:?}"
    );
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(750) && waited < Duration::from_secs(3),
        "two silent sessions of about 400 ms each: {waited:?}"
    );
    expect_closed(&mut session).await;
    assert_eq!(
        server.connections(),
        2,
        "the silent session was replaced once"
    );
    assert_eq!(server.inits().len(), 2, "a replacement is a new init call");
    // The replaced session and the failed one were both told to stop.
    server
        .log
        .wait("stop_recording on both sessions", |l| {
            l.texts(0) == [STOP.to_owned()] && l.texts(1) == [STOP.to_owned()]
        })
        .await;
}

#[tokio::test]
async fn acknowledged_silence_keeps_the_watchdog_quiet_while_the_caller_is_idle() {
    let server = MockServer::start(acking()).await;
    let stt = adapter(&server, |c| {
        c.acknowledgments = true;
        c.io_timeout = Duration::from_millis(400);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(1500), session.events.recv()).await;
    assert!(quiet.is_err(), "the idle line must stay up: {quiet:?}");
    assert_eq!(server.connections(), 1);
    // io_timeout / 2 = 200 ms between the 20 ms silence frames.
    assert!(
        server.log.audio(0).len() >= 4,
        "{} frames",
        server.log.audio(0).len()
    );
}

#[tokio::test]
async fn an_acknowledgment_that_refuses_audio_is_a_protocol_error() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(audio_ack(false)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let error = expect_error(&mut session).await;
    assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("InvalidArgument") && !text.contains("Unsupported sample rate"));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 1, "a retry would fail the same way");
}

// ---------------------------------------------------------------------------------------------
// Hang-up
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hang_up_sends_stop_recording_closes_events_at_once_and_drops_late_results() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == STOP => {
                    // Whatever follows a stop is the caller's past.
                    s.send(transcript("u1", "late words", true, 1.0)).await;
                    s.send(json!({"type": "post_final_transcript", "data": {}}).to_string())
                        .await;
                    s.close_with(1000, "").await;
                    return;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server.log.wait("audio", |l| l.audio(0).len() == 1).await;
    let SttSession { input, mut events } = session;
    drop(input);
    let closed = tokio::time::timeout(Duration::from_millis(250), events.recv()).await;
    assert!(
        matches!(closed, Ok(None)),
        "events must close at once and deliver nothing: {closed:?}"
    );
    server
        .log
        .wait("stop_recording", |l| l.texts(0) == [STOP.to_owned()])
        .await;
}

#[tokio::test]
async fn a_peer_that_never_answers_the_stop_is_given_up_on_after_close_timeout() {
    let ended: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&ended);
    let server = MockServer::start(handler(move |conn| {
        let seen = Arc::clone(&seen);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            // Reads everything, including stop_recording, and never answers or closes.
            s.drain().await;
            *lock(&seen) = Some(Instant::now());
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.close_timeout = Duration::from_millis(300));
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server.log.wait("audio", |l| l.audio(0).len() == 1).await;
    let hung_up = Instant::now();
    drop(session);
    server
        .log
        .wait("stop_recording", |l| !l.texts(0).is_empty())
        .await;
    server
        .log
        .wait("the session to let go of the socket", |_| {
            lock(&ended).is_some()
        })
        .await;
    let held = lock(&ended).unwrap() - hung_up;
    assert!(
        held >= Duration::from_millis(250) && held < Duration::from_secs(2),
        "the socket was held for {held:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_refusals_map_to_rejected_with_the_http_status_and_never_leak_the_key() {
    let server = MockServer::start(draining()).await;
    let codes = [400_u16, 401, 403, 422, 429, 500, 503];
    server.script_init(codes.map(Init::Status));
    let stt = adapter(&server, |_| {});
    for code in codes {
        let err = stt.connect(stt_cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Rejected { status, provider } if status == code && provider == "gladia"),
            "{code}: {err:?}"
        );
        // The mock's error body echoes the key; the error must not repeat anything of it.
        assert!(!format!("{err} {err:?}").contains(KEY));
    }
    assert_eq!(server.connections(), 0, "no socket after a refused init");
}

#[tokio::test]
async fn an_unusable_init_response_is_a_protocol_error_that_never_repeats_the_url() {
    let server = MockServer::start(draining()).await;
    server.script_init([
        Init::Body("<html>gateway</html>".to_owned()),
        Init::Body(r#"{"id":"x"}"#.to_owned()),
        Init::Url("wss://".to_owned()),
        // The token would travel in the clear to a remote host.
        Init::Url("ws://gladia.example.com/v2/live?token=SECRET-TOKEN".to_owned()),
        Init::Url("https://gladia.example.com/v2/live?token=SECRET-TOKEN".to_owned()),
    ]);
    let stt = adapter(&server, |_| {});
    for n in 0..5 {
        let err = stt.connect(stt_cfg()).await.unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{n}: {err:?}");
        let text = format!("{err} {err:?}");
        assert!(
            !text.contains("SECRET-TOKEN") && !text.contains(KEY),
            "{text}"
        );
    }
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn a_refused_websocket_upgrade_maps_to_rejected_without_leaking_the_token() {
    let server = MockServer::start(handler(|conn| async move { conn.reject(401).await })).await;
    let stt = adapter(&server, |_| {});
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    assert!(
        matches!(err, Error::Rejected { status: 401, .. }),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains("tok-0"));
}

#[tokio::test]
async fn garbage_on_the_socket_is_a_protocol_error_that_ends_the_session() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(transcript("u1", "fine", false, 0.2)).await;
        s.send("<<not json>>").await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("fine", false, false, false)
    );
    let error = expect_error(&mut session).await;
    assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    assert!(!error.to_string().contains("not json"));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 1, "not retried");
}

#[tokio::test]
async fn error_messages_below_500_are_fatal_and_above_are_reconnected() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        let status = if s.index == 0 { 503 } else { 400 };
        s.send(
            json!({"type": "error", "error": {"status_code": status,
                "exception": "Boom", "message": "free text with secrets"}})
            .to_string(),
        )
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let error = expect_error(&mut session).await;
    assert!(
        matches!(&error, Error::Protocol { detail, .. } if detail.contains("Boom (400)") && !detail.contains("secrets")),
        "{error:?}"
    );
    expect_closed(&mut session).await;
    assert_eq!(
        server.connections(),
        2,
        "the 503 was replaced, the 400 was not"
    );
}

#[tokio::test]
async fn the_server_ending_the_session_opens_a_new_one() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.send(lifecycle("end_session")).await;
            s.drain().await;
        } else {
            s.send(transcript("u1", "yeni oturum", true, 0.5)).await;
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("yeni oturum", true, true, false)
    );
    assert_eq!(server.inits().len(), 2);
}

#[tokio::test]
async fn invalid_settings_fail_before_any_connection_is_made() {
    let server = MockServer::start(draining()).await;
    let mut bad = config(&server);
    bad.model = "solaria 1".to_owned();
    assert!(matches!(
        GladiaStt::new(bad, clock()),
        Err(Error::Protocol { .. })
    ));
    let mut bad = config(&server);
    bad.base_url = Some("http://gladia.example.com".to_owned());
    assert!(
        GladiaStt::new(bad, clock()).is_err(),
        "cleartext to a remote host"
    );
    let mut bad = config(&server);
    bad.api_key = Secret::new("line\nbreak".to_owned());
    let err = GladiaStt::new(bad, clock()).unwrap_err();
    assert!(!format!("{err}").contains("line"));
    // An unsupported sample rate is caught per session.
    let stt = adapter(&server, |_| {});
    let result = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz24000,
            language: None,
        })
        .await;
    assert!(matches!(result, Err(Error::Protocol { .. })));
    assert!(server.inits().is_empty(), "nothing may reach the network");
}

#[tokio::test]
async fn the_provider_debug_output_never_contains_the_key() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |_| {});
    assert!(!format!("{stt:?} {:?}", stt.config()).contains(KEY));
    assert_eq!(stt.name(), "gladia");
}

// ---------------------------------------------------------------------------------------------
// Reconnect and replay
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_session_is_replaced_by_a_new_init_and_only_unfinalized_audio_is_replayed() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(2).await;
                // Chunk 1 (0.0-0.1 s) is final.
                s.send(transcript("u1", "one", true, 0.1)).await;
                s.until_audio(4).await;
                // Drop without a closing handshake.
            }
            1 => {
                // Replay of chunks 2, 3, 4. Utterance times restart at 0 in a new session:
                // 0.0-0.2 s is chunks 2 and 3.
                s.until_audio(3).await;
                s.send(transcript("u1", "two", true, 0.2)).await;
                s.until_audio(4).await;
            }
            _ => s.drain().await,
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("one", true, true, false));
    session.input.send(chunk(3)).await.unwrap();
    session.input.send(chunk(4)).await.unwrap();
    // The reconnect happens silently: the next thing is the final of the replayed audio.
    assert_eq!(flat(event(&mut session).await), t("two", true, true, false));
    session.input.send(chunk(5)).await.unwrap();
    server
        .log
        .wait("the second reconnect to hear 4 and 5", |l| {
            l.audio(2).len() >= 2
        })
        .await;
    assert_eq!(fills(&server.log.audio(0)), [1, 2, 3, 4]);
    assert_eq!(fills(&server.log.audio(1)), [2, 3, 4, 5]);
    assert_eq!(fills(&server.log.audio(2)), [4, 5]);
    // Three sessions, three init calls, three different tokens, the key on every init.
    let inits = server.inits();
    assert_eq!(inits.len(), 3);
    assert!(inits.iter().all(|i| i.header("x-gladia-key") == Some(KEY)));
    let uris: Vec<String> = server.requests().into_iter().map(|r| r.uri).collect();
    assert_eq!(
        uris,
        [
            "/v2/live?token=tok-0",
            "/v2/live?token=tok-1",
            "/v2/live?token=tok-2"
        ]
    );
}

#[tokio::test]
async fn timestamps_are_offset_by_the_silence_the_adapter_inserted() {
    // Session 0 hears chunk 1 (3200 bytes), 20 ms of generated silence (640 bytes) and chunk 7
    // (3200 bytes). The server finalizes 0.17 s of its stream: 5440 bytes, of which 640 are the
    // silence, so 4800 bytes of the caller's audio are final and 1600 bytes of chunk 7 must be
    // replayed.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(3).await;
            s.send(transcript("u1", "bir", true, 0.17)).await;
            // Wait for the final to be consumed, then drop.
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100)
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("the silence frame", |l| l.audio(0).len() == 2)
        .await;
    assert_eq!(server.log.audio(0)[1].len(), 640);
    session.input.send(chunk(7)).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("bir", true, true, false));
    server
        .log
        .wait("the replay", |l| !l.audio(1).is_empty())
        .await;
    let first = server.log.audio(1)[0].clone();
    assert_eq!(first.len(), 1600);
    assert!(first.iter().all(|b| *b == 7));
}

#[tokio::test]
async fn reconnecting_gives_up_after_the_configured_attempts_and_reports_the_last_refusal() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "boom").await;
    }))
    .await;
    // The first init works, every later one is refused.
    server.init_hook(|n| if n == 0 { Init::Ok } else { Init::Status(503) });
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 503, .. }
    ));
    expect_closed(&mut session).await;
    assert_eq!(server.inits().len(), 3, "the original and two attempts");
}

#[tokio::test]
async fn a_lost_session_without_a_reason_to_blame_is_reported_as_disconnected() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "boom").await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 0);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    expect_closed(&mut session).await;
    assert_eq!(
        server.inits().len(),
        1,
        "with reconnecting off the first drop is final"
    );
}

#[tokio::test]
async fn reconnect_attempts_back_off_exponentially() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "boom").await;
    }))
    .await;
    server.init_hook(|n| if n == 0 { Init::Ok } else { Init::Status(503) });
    let base = Duration::from_millis(80);
    let stt = adapter(&server, |c| {
        c.max_reconnects = 3;
        c.reconnect_backoff = base;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let _ = expect_error(&mut session).await;
    assert_eq!(server.inits().len(), 4, "the original and three attempts");
    // The attempt that follows init number n waits `base * 2^n` after the previous init (timers
    // never fire early; the allowance is for the clock granularity of the platform). The gap
    // after the very first init also holds the time the session lived, so it is skipped.
    let at = server.init_at();
    for (n, pair) in at.windows(2).enumerate().skip(1) {
        let gap = pair[1] - pair[0];
        let expected = base * (1 << n);
        assert!(
            gap >= expected.mul_f64(0.9),
            "attempt {n} came after {gap:?}, expected at least {expected:?}"
        );
    }
}

#[tokio::test]
async fn a_credential_failure_while_reconnecting_is_surfaced_at_once() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "boom").await;
    }))
    .await;
    server.init_hook(|n| if n == 0 { Init::Ok } else { Init::Status(401) });
    let stt = adapter(&server, |c| c.max_reconnects = 5);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 401, .. }
    ));
    assert_eq!(server.inits().len(), 2, "a retry cannot fix credentials");
}

#[tokio::test]
async fn audio_sent_while_reconnecting_is_kept_and_replayed() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // The replacement session takes 300 ms to be created.
    server.init_hook(|n| {
        if n == 0 {
            Init::Ok
        } else {
            Init::Stall(Duration::from_millis(300))
        }
    });
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("the second init", |_| server.inits().len() == 2)
        .await;
    for fill in 2..=4 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay", |l| l.audio(1).len() >= 4)
        .await;
    assert_eq!(fills(&server.log.audio(1)), [1, 2, 3, 4]);
}

async fn finalize_while_the_session_drops(answer_after_reconnect: bool) {
    let server = MockServer::start(handler(move |conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
            s.send(transcript("u1", "merhaba", false, 0.3)).await;
            // Gone before the final. In the timeout variant the drop comes late and the
            // replacement session is slow to be created, so that a deadline that restarted at
            // the reconnect would be far off the request-anchored one (see below).
            let drop_after = if answer_after_reconnect { 100 } else { 400 };
            tokio::time::sleep(Duration::from_millis(drop_after)).await;
        } else {
            s.until_audio(1).await;
            if answer_after_reconnect {
                s.send(transcript("u1", "Merhaba.", true, 0.3)).await;
            }
            s.drain().await;
        }
    }))
    .await;
    if !answer_after_reconnect {
        // The new session is created 300 ms after the drop: the reconnect completes at about
        // 400 + 10 + 300 = 710 ms after the request, inside the 900 ms bound.
        server.init_hook(|n| {
            if n == 0 {
                Init::Ok
            } else {
                Init::Stall(Duration::from_millis(300))
            }
        });
    }
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(900));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    let got = flat(event(&mut session).await);
    let waited = start.elapsed();
    if answer_after_reconnect {
        assert_eq!(got, t("Merhaba.", true, true, true));
        assert!(waited < Duration::from_millis(800), "{waited:?}");
    } else {
        // Nothing arrives from the new session: the bound runs from the request and the text
        // heard before the drop is not lost. A deadline that restarted when the reconnect
        // completed (about 710 ms after the request) would fire near 1600 ms, well outside the
        // window.
        assert_eq!(got, t("merhaba", true, true, true));
        assert!(
            waited >= Duration::from_millis(850) && waited < Duration::from_millis(1250),
            "{waited:?}"
        );
    }
    assert_eq!(server.inits().len(), 2);
}

#[tokio::test]
async fn a_pending_finalize_is_answered_by_the_replacement_sessions_final() {
    finalize_while_the_session_drops(true).await;
}

#[tokio::test]
async fn the_finalize_bound_holds_across_a_reconnect_and_keeps_the_text_heard_before() {
    finalize_while_the_session_drops(false).await;
}

#[tokio::test]
async fn the_replay_ring_is_bounded_by_the_replay_window() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(10).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // 250 ms of audio = 8000 bytes = at most 3 chunks of 3200 bytes (whole chunks are dropped).
    let stt = adapter(&server, |c| c.replay_window = Duration::from_millis(250));
    let session = connect(&stt).await;
    for fill in 1..=10 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server.log.wait("replay", |l| l.audio(1).len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replay = fills(&server.log.audio(1));
    assert!(
        (1..=3).contains(&replay.len()) && replay.last() == Some(&10),
        "{replay:?}"
    );
}

/// The wall time between the first and the last of ten 100 ms chunks replayed after a drop.
async fn replay_spread(speed: f64) -> Duration {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(10).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.replay_speed = speed);
    let session = connect(&stt).await;
    for fill in 1..=10 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server.log.wait("replay", |l| l.audio(1).len() == 10).await;
    let times: Vec<Instant> = server
        .log
        .all()
        .iter()
        .filter(|s| s.conn == 1)
        .map(|s| s.at)
        .collect();
    *times.last().unwrap() - times[0]
}

#[tokio::test]
async fn replayed_audio_is_paced_at_the_configured_speed() {
    // Nine gaps of 100 ms of audio each: 900 ms / speed. Sending is never faster than the speed,
    // so the lower bound is tight; the upper bound only catches a replay that crawls.
    for (speed, expected_ms) in [(4.0_f64, 225_u64), (1.5, 600)] {
        let spread = replay_spread(speed).await;
        let expected = Duration::from_millis(expected_ms);
        assert!(
            spread >= expected.mul_f64(0.9),
            "speed {speed}: replay too fast ({spread:?}, expected about {expected:?})"
        );
        assert!(
            spread <= expected + Duration::from_millis(600),
            "speed {speed}: replay too slow ({spread:?}, expected about {expected:?})"
        );
    }
}

#[tokio::test]
async fn a_replay_never_starts_in_the_middle_of_a_sample() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(6).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // Odd-sized chunks and a window that cuts through them.
    let stt = adapter(&server, |c| c.replay_window = Duration::from_millis(10));
    let session = connect(&stt).await;
    for fill in 1..=6 {
        session
            .input
            .send(SttInput::Audio(Bytes::from(vec![fill; 641])))
            .await
            .unwrap();
    }
    server.log.wait("replay", |l| !l.audio(1).is_empty()).await;
    let total: usize = server.log.audio(1).iter().map(Bytes::len).sum();
    assert_eq!(
        total % 2,
        0,
        "a replay of 16-bit audio starts on a sample boundary"
    );
}

#[tokio::test]
async fn a_flapping_server_that_never_says_anything_is_not_reconnected_forever() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1000, "").await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    assert_eq!(server.connections(), 3, "the original and two attempts");
}

#[tokio::test]
async fn a_transcript_proves_the_session_and_restores_the_reconnect_budget() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index < 3 {
            // Every session works for a moment, then drops.
            s.send(transcript("u1", &format!("session {}", s.index), true, 0.1))
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // One reconnect per drop: three drops in a row only work because each session proved itself.
    let stt = adapter(&server, |c| c.max_reconnects = 1);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    for n in 0..3_u8 {
        assert_eq!(
            flat(event(&mut session).await),
            t(&format!("session {n}"), true, true, false)
        );
        // The next session has audio to hear: this chunk is replayed into it.
        session.input.send(chunk(n + 2)).await.unwrap();
    }
    // The third drop is replaced as well.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.connections() < 4 {
        assert!(Instant::now() < deadline, "the third drop was not replaced");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_init_call_that_never_answers_times_out() {
    let server = MockServer::start(draining()).await;
    server.script_init([Init::Hang]);
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let started = Instant::now();
    let result = stt.connect(stt_cfg()).await;
    assert!(
        matches!(
            result,
            Err(Error::Core(brisvox_core::Error::Timeout { .. }))
        ),
        "{:?}",
        result.err()
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(3),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_websocket_handshake_that_never_completes_times_out_within_the_same_deadline() {
    let server = MockServer::start(handler(|conn| async move {
        // TCP is accepted, the upgrade is never answered.
        let _held = conn;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }))
    .await;
    // The init call takes 200 ms of the 500 ms budget: the deadline covers both steps.
    server.script_init([Init::Stall(Duration::from_millis(200))]);
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(500));
    let started = Instant::now();
    let result = stt.connect(stt_cfg()).await;
    assert!(
        matches!(
            result,
            Err(Error::Core(brisvox_core::Error::Timeout { .. }))
        ),
        "{:?}",
        result.err()
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(450) && waited < Duration::from_millis(900),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_write_that_cannot_complete_times_out_and_the_session_is_replaced() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            // Accepts and never reads: the socket buffers fill up and the client's write blocks.
            std::future::pending::<()>().await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let session = connect(&stt).await;
    let input = session.input.clone();
    let feeder = tokio::spawn(async move {
        for _ in 0..400 {
            let big = SttInput::Audio(Bytes::from(vec![0_u8; 1 << 20]));
            if input.send(big).await.is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.connections() < 2 {
        assert!(
            Instant::now() < deadline,
            "a blocked write was never given up on"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    feeder.abort();
    drop(session);
}

#[tokio::test]
async fn a_consumer_that_stops_reading_finals_ends_the_session_after_io_timeout() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Far more finals than the events channel holds.
        for i in 0..100 {
            s.send(transcript(
                &format!("u{i}"),
                &format!("segment {i}"),
                true,
                0.1,
            ))
            .await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    let started = Instant::now();
    session.input.send(chunk(1)).await.unwrap();
    // The caller does not read. The adapter waits for io_timeout, then treats it as gone: it
    // stops, tells the server (stop_recording) and closes the channel after what it had queued.
    server
        .log
        .wait("stop_recording", |l| !l.texts(0).is_empty())
        .await;
    let stopped_at = server
        .log
        .all()
        .iter()
        .find(|s| matches!(&s.what, What::Text(t) if t == STOP))
        .map(|s| s.at)
        .unwrap();
    let lasted = stopped_at - started;
    assert!(
        lasted >= Duration::from_millis(250) && lasted < Duration::from_secs(3),
        "the session must wait about io_timeout for a stalled consumer, not give up at once \
         nor hang: {lasted:?}"
    );
    let mut received = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("the events channel never closed")
    {
        assert!(matches!(event, SttEvent::Transcript { .. }));
        received += 1;
    }
    assert!((1..100).contains(&received), "received {received}");
}

// ---------------------------------------------------------------------------------------------
// Review fixes: liveness, retiring sessions, replay trimming, finalize in flight, hang-up
// ---------------------------------------------------------------------------------------------

/// Accepts and then goes silent for good: never reads, never answers a ping. What a half-open
/// link (NAT or route loss without FIN/RST) looks like from the client.
fn half_open() -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(|conn| async move {
        let Some(_s) = conn.accept().await else {
            return;
        };
        std::future::pending::<()>().await;
    })
}

#[tokio::test]
async fn a_half_open_link_is_detected_by_the_liveness_ping_even_without_acknowledgments() {
    let server = MockServer::start(half_open()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 1;
    });
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(chunk(1)).await.unwrap();
    let error = expect_error(&mut session).await;
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error:?}"
    );
    // Two sessions, each: 100 ms until the ping, 400 ms until it counts as unanswered.
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(4),
        "{waited:?}"
    );
    expect_closed(&mut session).await;
    assert_eq!(
        server.connections(),
        2,
        "the dead session was replaced once"
    );
    assert_eq!(server.inits().len(), 2);
}

#[tokio::test]
async fn with_the_liveness_ping_off_a_silent_peer_is_not_noticed() {
    let server = MockServer::start(half_open()).await;
    let stt = adapter(&server, |c| {
        c.liveness_ping = false;
        c.keepalive_interval = Duration::from_millis(100);
        c.io_timeout = Duration::from_millis(300);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(1200), session.events.recv()).await;
    assert!(quiet.is_err(), "nothing may happen: {quiet:?}");
    assert_eq!(server.connections(), 1);
    assert_eq!(server.log.pings(0), 0, "no ping may be sent");
}

#[tokio::test]
async fn a_quiet_but_responsive_line_is_normal_and_keeps_being_pinged() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(80);
        c.io_timeout = Duration::from_millis(300);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(1200), session.events.recv()).await;
    assert!(
        quiet.is_err(),
        "a healthy idle line must stay up: {quiet:?}"
    );
    assert_eq!(server.connections(), 1);
    // The pongs came back, so more than one ping was sent over the second.
    assert!(
        server.log.pings(0) >= 3,
        "{} pings seen",
        server.log.pings(0)
    );
}

#[tokio::test]
async fn a_session_replaced_for_silence_is_told_to_stop_before_the_new_init_call() {
    // The server enforces a concurrency limit of one live session, like the free plan: the
    // init call is refused with 429 while the previous session has not been stopped.
    let live = Arc::new(AtomicUsize::new(0));
    let handler_live = Arc::clone(&live);
    let server = MockServer::start(handler(move |conn| {
        let live = Arc::clone(&handler_live);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            live.fetch_add(1, Ordering::SeqCst);
            if s.index == 0 {
                // Never acknowledges; lets go of its slot only when told to stop.
                loop {
                    match s.next().await {
                        Some(What::Text(t)) if t == STOP => break,
                        Some(_) => {}
                        None => break,
                    }
                }
                live.fetch_sub(1, Ordering::SeqCst);
                s.drain().await;
            } else {
                s.until_audio(1).await;
                s.send(transcript("u1", "yeni oturum", true, 0.1)).await;
                s.drain().await;
                live.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }))
    .await;
    let init_live = Arc::clone(&live);
    server.init_hook(move |_| {
        if init_live.load(Ordering::SeqCst) >= 1 {
            Init::Status(429)
        } else {
            Init::Ok
        }
    });
    let stt = adapter(&server, |c| {
        c.acknowledgments = true;
        c.io_timeout = Duration::from_millis(300);
        c.reconnect_backoff = Duration::from_millis(60);
        c.max_reconnects = 1;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("yeni oturum", true, true, false),
        "without stop_recording the re-init would have been answered 429"
    );
    assert_eq!(server.log.texts(0), [STOP]);
}

#[tokio::test]
async fn a_failed_session_is_told_to_stop_after_the_error_was_delivered() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(audio_ack(false)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let error = expect_error(&mut session).await;
    assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    expect_closed(&mut session).await;
    server
        .log
        .wait("stop_recording on the failed session", |l| {
            l.texts(0) == [STOP.to_owned()]
        })
        .await;
}

#[tokio::test]
async fn a_session_replaced_after_a_server_error_is_told_to_stop_but_a_dead_socket_is_not_written_to()
 {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.send(
                json!({"type": "error", "error": {"status_code": 503, "exception": "Boom"}})
                    .to_string(),
            )
            .await;
            s.drain().await;
        } else {
            // The peer closes: nothing is left to stop.
            s.close_with(1011, "gone").await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 1);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let _ = expect_error(&mut session).await;
    expect_closed(&mut session).await;
    server
        .log
        .wait("stop_recording on the replaced session", |l| {
            l.texts(0) == [STOP.to_owned()]
        })
        .await;
    assert!(server.log.texts(1).is_empty(), "{:?}", server.log.texts(1));
}

#[tokio::test]
async fn a_gap_in_the_replay_does_not_make_a_later_drop_replay_audio_that_was_final() {
    // Window of three chunks, replay at real time. Session 0 hears chunks 0-2 and drops. While
    // the replacement replays (chunk 0 goes out at once, the next is 100 ms away) chunks 3-6
    // arrive and the ring evicts everything up to chunk 4: chunks 1-3 are never sent. Session 1
    // therefore hears chunk 0, then 4, 5, 6 and its timeline lacks the gap.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(3).await;
            }
            1 => {
                // Heard so far: chunks 0, 4, 5 = 0.3 s of the session's timeline.
                s.until_audio(3).await;
                s.send(transcript("u1", "bir", true, 0.3)).await;
                s.until_audio(4).await;
            }
            _ => s.drain().await,
        }
    }))
    .await;
    server.init_hook(|n| {
        if n == 1 {
            Init::Stall(Duration::from_millis(300))
        } else {
            Init::Ok
        }
    });
    let stt = adapter(&server, |c| {
        c.replay_window = Duration::from_millis(300);
        c.replay_speed = 1.0;
        c.max_reconnects = 3;
    });
    let mut session = connect(&stt).await;
    for fill in 0..3 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the first replayed chunk", |l| l.audio(1).len() == 1)
        .await;
    for fill in 3..7 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    assert_eq!(flat(event(&mut session).await), t("bir", true, true, false));
    server
        .log
        .wait("the third session to be replayed to", |l| {
            !l.audio(2).is_empty()
        })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fills(&server.log.audio(1)), [0, 4, 5, 6]);
    // The final covered chunks 0, 4 and 5: only chunk 6 may be heard again, and not 4 or 5.
    assert_eq!(fills(&server.log.audio(2)), [6]);
}

#[tokio::test]
async fn a_finalize_during_a_reconnect_waits_for_the_audio_that_is_being_replayed() {
    // The old session said "one" for chunk 1 and dropped with chunk 2 unfinalized. The Finalize
    // comes while the replacement is being opened: chunk 2's text must answer it, not an empty
    // answer that leaves the real final to arrive late.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(2).await;
            s.send(transcript("u1", "one", true, 0.1)).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            s.until_audio(1).await;
            s.send(transcript("u1", "two", true, 0.1)).await;
            s.drain().await;
        }
    }))
    .await;
    server.init_hook(|n| {
        if n == 1 {
            Init::Stall(Duration::from_millis(400))
        } else {
            Init::Ok
        }
    });
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("one", true, true, false));
    server
        .log
        .wait("the second init", |_| server.inits().len() == 2)
        .await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("two", true, true, true),
        "the replayed audio's own final answers the Finalize"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_finalize_right_after_a_reconnect_on_an_idle_call_is_answered_at_once() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
            s.send(transcript("u1", "bitti", true, 0.1)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // A long timeout: only the immediate answer can pass within the asserted time.
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(5));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bitti", true, true, false)
    );
    server
        .log
        .wait("the replacement session", |_| server.connections() == 2)
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "nothing was in flight: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_finalize_between_a_final_and_its_speech_end_is_answered_at_once() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(speech("speech_start", 0.0)).await;
        s.send(transcript("u1", "tamam", true, 0.1)).await;
        // speech_end only after the caller's Finalize.
        tokio::time::sleep(Duration::from_millis(500)).await;
        s.send(speech("speech_end", 0.1)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(5));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("tamam", true, true, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(start.elapsed() < Duration::from_millis(400));
    // After the speech_end as well.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(start.elapsed() < Duration::from_millis(400));
}

#[tokio::test]
async fn a_missed_speech_end_costs_one_finalize_timeout_not_every_finalize() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Speech starts and nothing else ever comes.
        s.send(speech("speech_start", 0.0)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(start.elapsed() >= Duration::from_millis(250));
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(
        start.elapsed() < Duration::from_millis(150),
        "the second Finalize waited {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn odd_transcript_shapes_do_not_end_the_call() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        for odd in [
            r#"{"type":"transcript","data":{"id":"u1","is_final":false,"utterance":{"text":null,"end":0.2}}}"#,
            r#"{"type":"transcript","data":{"id":123,"is_final":false,"utterance":{"text":"sayı","end":0.2}}}"#,
            r#"{"type":null,"data":{}}"#,
            r#"{"type":7}"#,
            r#"{"type":"transcript","data":{"id":123,"is_final":true,"utterance":{"text":"Sayı.","end":0.3}}}"#,
        ] {
            s.send(odd).await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 0);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("sayı", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("Sayı.", true, true, false)
    );
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn dropping_only_the_events_receiver_ends_the_session_and_stops_the_server_side() {
    let closed = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&closed);
    let server = MockServer::start(handler(move |conn| {
        let seen = Arc::clone(&seen);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            loop {
                match s.next().await {
                    Some(What::Text(t)) if t == STOP => {
                        s.close_with(1000, "").await;
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            seen.fetch_add(1, Ordering::SeqCst);
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100)
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server.log.wait("audio", |l| !l.audio(0).is_empty()).await;
    let SttSession { input, events } = session;
    drop(events);
    // The input stays open: the session must notice the missing consumer by itself.
    server
        .log
        .wait("stop_recording", |l| l.texts(0) == [STOP.to_owned()])
        .await;
    tokio::time::timeout(Duration::from_secs(2), input.closed())
        .await
        .expect("the session task kept running after the consumer left");
    server
        .log
        .wait("the connection to close", |_| {
            closed.load(Ordering::SeqCst) == 1
        })
        .await;
    // No keepalive silence goes out any more.
    let frames = server.log.audio(0).len();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(server.log.audio(0).len(), frames);
}

#[tokio::test]
async fn hanging_up_during_the_reconnect_backoff_makes_no_further_init_call() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Gone: the client enters its backoff.
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.reconnect_backoff = Duration::from_millis(700);
        c.max_reconnects = 3;
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server.log.wait("audio", |l| !l.audio(0).is_empty()).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(server.inits().len(), 1, "still backing off");
    let SttSession { input, events } = session;
    drop(events);
    tokio::time::timeout(Duration::from_millis(400), input.closed())
        .await
        .expect("the hang-up was not noticed during the backoff");
    // Well past the end of the backoff.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(server.inits().len(), 1, "no init call after the hang-up");
}

#[tokio::test]
async fn hanging_up_during_a_stalled_reinit_cuts_the_stall_short() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
    }))
    .await;
    server.init_hook(|n| {
        if n == 0 {
            Init::Ok
        } else {
            Init::Stall(Duration::from_secs(30))
        }
    });
    let stt = adapter(&server, |c| {
        c.io_timeout = Duration::from_secs(20);
        c.max_reconnects = 3;
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("the stalled re-init", |_| server.inits().len() == 2)
        .await;
    let SttSession { input, events } = session;
    drop(events);
    // Neither the stall (30 s) nor the connect deadline (20 s) is waited out.
    tokio::time::timeout(Duration::from_secs(1), input.closed())
        .await
        .expect("the hang-up was not noticed while the re-init stalled");
    assert_eq!(server.inits().len(), 2);
    assert_eq!(server.connections(), 1);
}

// ---------------------------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------------------------

/// No further event within `wait`.
async fn expect_quiet(session: &mut SttSession, wait: Duration) {
    if let Ok(next) = tokio::time::timeout(wait, session.events.recv()).await {
        panic!("expected no further event, got {next:?}");
    }
}

#[tokio::test]
async fn every_finalize_gets_its_own_answer_when_a_second_one_arrives_while_one_waits() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(speech("speech_start", 0.1)).await;
        s.send(transcript("u1", "tamam", false, 0.5)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.send(transcript("u1", "Tamam, evet.", true, 0.9)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(2));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("tamam", false, false, false)
    );
    // Two VAD stops before the provider's final: the engine's ledger holds two entries and pops
    // one per from_finalize final, so both must be answered.
    session.input.send(SttInput::Finalize).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Tamam, evet.", true, true, true)
    );
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    // Nothing is open any more: a third one is answered at once, and only once.
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    expect_quiet(&mut session, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn finalizes_that_time_out_together_are_each_answered() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    for _ in 0..3 {
        session.input.send(SttInput::Finalize).await.unwrap();
    }
    for _ in 0..3 {
        assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    }
    expect_quiet(&mut session, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn a_finalize_sent_while_the_session_reconnects_is_answered_too() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
            s.send(transcript("u1", "merhaba", false, 0.3)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            s.until_audio(1).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            s.send(transcript("u1", "Merhaba.", true, 0.3)).await;
            s.drain().await;
        }
    }))
    .await;
    server.init_hook(|n| {
        if n == 0 {
            Init::Ok
        } else {
            Init::Stall(Duration::from_millis(300))
        }
    });
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    server
        .log
        .wait("the second init", |_| server.inits().len() == 2)
        .await;
    // The session is being replaced right now.
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba.", true, true, true)
    );
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    expect_quiet(&mut session, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_sample_rate_override_that_differs_from_the_audio_is_refused_before_any_call() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |c| c.sample_rate_hz = Some(48_000));
    // The caller sends 16 kHz audio: declaring 48 kHz would make Gladia mis-read every byte.
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    assert!(matches!(err, Error::Protocol { .. }), "{err:?}");
    assert!(server.inits().is_empty());
    // The same value as the audio's is fine and is what is declared.
    let stt = adapter(&server, |c| c.sample_rate_hz = Some(16_000));
    let _session = stt.connect(stt_cfg()).await.unwrap();
    assert_eq!(server.inits()[0].json()["sample_rate"], 16_000);
}

/// Waits for a clean-up call to reach the server.
async fn delete_arrives(server: &MockServer) {
    server
        .log
        .wait("the clean-up DELETE", |_| !server.deletes().is_empty())
        .await;
}

fn assert_cleanup_of_session_zero(server: &MockServer) {
    let deletes = server.deletes();
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].method, "DELETE");
    assert_eq!(deletes[0].target, "/v2/live/sess-0");
    assert_eq!(deletes[0].header("x-gladia-key"), Some(KEY));
}

#[tokio::test]
async fn a_session_whose_upgrade_was_refused_is_deleted_again() {
    let server = MockServer::start(handler(|conn| async move { conn.reject(503).await })).await;
    let stt = adapter(&server, |_| {});
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    assert!(
        matches!(err, Error::Rejected { status: 503, .. }),
        "{err:?}"
    );
    delete_arrives(&server).await;
    assert_cleanup_of_session_zero(&server);
}

#[tokio::test]
async fn a_session_whose_upgrade_stalls_past_the_deadline_is_deleted_again() {
    let server = MockServer::start(handler(|conn| async move {
        let _held = conn;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    assert!(
        matches!(err, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{err:?}"
    );
    delete_arrives(&server).await;
    assert_cleanup_of_session_zero(&server);
}

#[tokio::test]
async fn a_connect_that_is_cancelled_after_init_deletes_the_session_it_created() {
    let server = MockServer::start(handler(|conn| async move {
        let _held = conn;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }))
    .await;
    // The deadline is far away: the caller gives up first and drops the connect future.
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_secs(5));
    let connecting = stt.connect(stt_cfg());
    let outcome = tokio::time::timeout(Duration::from_millis(300), connecting).await;
    assert!(outcome.is_err(), "the upgrade was supposed to stall");
    assert_eq!(server.inits().len(), 1);
    delete_arrives(&server).await;
    assert_cleanup_of_session_zero(&server);
}

#[tokio::test]
async fn a_replacement_session_that_never_got_a_socket_is_deleted_too() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            s.close_with(1011, "boom").await;
        } else {
            // The replacement is created by init but its upgrade is refused.
            conn.reject(503).await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 1);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 503, .. }
    ));
    delete_arrives(&server).await;
    let deletes = server.deletes();
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    // The second session, not the first (which was used).
    assert_eq!(deletes[0].target, "/v2/live/sess-1");
}

#[tokio::test]
async fn working_sessions_and_refused_inits_are_never_deleted() {
    let server = MockServer::start(draining()).await;
    server.script_init([Init::Status(401), Init::Ok]);
    let stt = adapter(&server, |_| {});
    // No session was created by a refused init, so there is nothing to delete.
    assert!(stt.connect(stt_cfg()).await.is_err());
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    drop(session);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(server.deletes().is_empty(), "{:?}", server.deletes());
}

async fn reconnect_retries_through(code: u16) {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.close_with(1011, "boom").await;
        } else {
            s.send(transcript("u1", "Merhaba.", true, 0.1)).await;
            s.drain().await;
        }
    }))
    .await;
    // The replacement is refused once (the free plan's concurrency slot is still busy) and then
    // created.
    server.init_hook(move |n| if n == 1 { Init::Status(code) } else { Init::Ok });
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba.", true, true, false),
        "{code} must be retried, not surfaced"
    );
    assert_eq!(server.inits().len(), 3, "{code}");
}

#[tokio::test]
async fn a_429_while_reconnecting_is_retried() {
    reconnect_retries_through(429).await;
}

#[tokio::test]
async fn a_408_while_reconnecting_is_retried() {
    reconnect_retries_through(408).await;
}

#[tokio::test]
async fn a_redirecting_init_endpoint_is_not_followed_so_the_key_goes_nowhere_else() {
    let elsewhere = MockServer::start(draining()).await;
    let server = MockServer::start(draining()).await;
    server.script_init([Init::Redirect(format!("{}/v2/live", elsewhere.base_url()))]);
    let stt = adapter(&server, |_| {});
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    assert!(
        matches!(err, Error::Rejected { status: 307, .. }),
        "{err:?}"
    );
    assert_eq!(server.inits().len(), 1);
    assert_eq!(
        elsewhere.http_requests(),
        0,
        "the key header followed the redirect"
    );
    assert_eq!(elsewhere.connections(), 0);
}

// ---------------------------------------------------------------------------------------------
// Warm
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn warm_touches_the_init_host_without_credentials_and_creates_no_session() {
    let server = MockServer::start(draining()).await;
    let stt = adapter(&server, |_| {});
    stt.warm().await.unwrap();
    stt.warm().await.unwrap();
    assert_eq!(server.heads(), 2);
    let heads = server.head_requests();
    assert_eq!(heads.len(), 2);
    for head in &heads {
        assert_eq!(head.method, "HEAD");
        assert_eq!(head.target, "/v2/live");
        assert!(
            head.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("host")),
            "the mock did record the request headers: {head:?}"
        );
        assert!(!head.target.contains(KEY));
        for (name, value) in &head.headers {
            let lower = name.to_ascii_lowercase();
            assert!(
                lower != "x-gladia-key"
                    && lower != "authorization"
                    && lower != "cookie"
                    && !value.contains(KEY),
                "warm must send no credentials: {name}"
            );
        }
    }
    assert!(
        server.inits().is_empty(),
        "a session holds a concurrency slot"
    );
    assert_eq!(server.connections(), 0);
    // An unreachable host is an error, not a hang or a panic.
    let down = adapter(&server, |c| {
        c.base_url = Some("http://127.0.0.1:9".to_owned());
        c.io_timeout = Duration::from_millis(500);
    });
    assert!(down.warm().await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Contract suite
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Script {
    utterance: Option<Utterance>,
    faults: Vec<Fault>,
    /// After a scripted drop the "service" stays down until the next `reset`.
    outage: bool,
}

impl Script {
    /// The answer to the next init call; consumes the faults that act at connect time.
    fn init(&mut self) -> Init {
        if self.outage {
            return Init::Status(503);
        }
        let mut answer = Init::Ok;
        self.faults.retain(|fault| match fault {
            Fault::Reject { status } => {
                answer = Init::Status(*status);
                false
            }
            Fault::ConnectStall { duration } => {
                answer = Init::Stall(*duration);
                false
            }
            _ => true,
        });
        answer
    }
}

struct Harness {
    server: MockServer,
    script: Arc<Mutex<Script>>,
    provider: Arc<GladiaStt>,
}

/// The deadline the contract checks hold the adapter to. Shorter than the suite's default so the
/// timeout checks stay quick on a real clock.
const CONTRACT_DEADLINE: Duration = Duration::from_millis(600);

/// How long after the last audio the mock's endpointing closes the utterance.
const ENDPOINTING: Duration = Duration::from_millis(80);

impl Harness {
    async fn start() -> Self {
        let script = Arc::new(Mutex::new(Script::default()));
        let s = Arc::clone(&script);
        let server = MockServer::start(handler(move |conn| scripted(conn, Arc::clone(&s)))).await;
        let s = Arc::clone(&script);
        server.init_hook(move |_| lock(&s).init());
        let provider = Arc::new(adapter(&server, |c| {
            c.io_timeout = CONTRACT_DEADLINE;
            c.finalize_timeout = Duration::from_millis(400);
            c.close_timeout = Duration::from_millis(300);
            // The mock acknowledges every chunk: that is the liveness signal the timeout checks
            // exercise (a server that stops talking is reported as Core(Timeout)).
            c.acknowledgments = true;
            // The suite's timeout checks expect a silent provider to be reported as
            // Core(Timeout) at the deadline. With a reconnect left the adapter replaces the
            // silent session first (tested separately), so the suite runs without one.
            c.max_reconnects = 0;
        }));
        Self {
            server,
            script,
            provider,
        }
    }
}

impl SttHarness for Harness {
    type P = GladiaStt;

    fn provider(&self) -> Arc<GladiaStt> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        clock()
    }

    fn io_timeout(&self) -> Duration {
        CONTRACT_DEADLINE
    }

    fn reset(&self) {
        *lock(&self.script) = Script::default();
        self.server.abort_handlers();
    }

    fn script(&self, utterance: Utterance) {
        lock(&self.script).utterance = Some(utterance);
    }

    fn inject(&self, fault: Fault) {
        lock(&self.script).faults.push(fault);
    }
}

/// The scripted Gladia of the contract suite: an acknowledgment per chunk, partials on the first
/// audio, the final once endpointing closes the utterance, plus the injected faults. `Stall`
/// holds back the outgoing queue (the acknowledgments behind it too), like a stalled server.
async fn scripted(conn: Conn, script: Arc<Mutex<Script>>) {
    let (utterance, faults) = {
        let mut s = lock(&script);
        (s.utterance.take(), std::mem::take(&mut s.faults))
    };
    let mut disconnect_after = None;
    let mut protocol_after = None;
    let mut stalls: Vec<(usize, Duration)> = Vec::new();
    for fault in faults {
        match fault {
            Fault::Disconnect { after_events } => disconnect_after = Some(after_events),
            Fault::Protocol { after_events } => protocol_after = Some(after_events),
            Fault::Stall {
                before_event,
                duration,
            } => stalls.push((before_event, duration)),
            Fault::Reject { .. } | Fault::ConnectStall { .. } => {}
        }
    }
    let Some(mut s) = conn.accept().await else {
        return;
    };
    // (is a transcript event, text); acknowledgments are not events.
    let mut queue: VecDeque<(bool, String)> = VecDeque::new();
    let mut delivered = 0_usize;
    let mut audio_seen = false;
    // 0: not started, 1: partials sent and the final due, 2: the final was queued.
    let mut phase = 0_u8;
    let mut final_due: Option<Instant> = None;
    let mut stall_until: Option<Instant> = None;
    let mut stall_applied: HashSet<usize> = HashSet::new();
    loop {
        if audio_seen {
            if disconnect_after == Some(delivered) {
                lock(&script).outage = true;
                return;
            }
            if protocol_after == Some(delivered) {
                protocol_after = None;
                s.send("<<not json>>").await;
            }
        }
        if final_due.is_some_and(|t| t <= Instant::now()) {
            final_due = None;
            phase = 2;
            if let Some(u) = &utterance {
                for fin in &u.finals {
                    queue.push_back((true, transcript("u1", fin, true, 0.1)));
                }
            }
        }
        if let Some((is_event, _)) = queue.front() {
            if !*is_event {
                if let Some((_, text)) = queue.pop_front() {
                    s.send(text).await;
                }
                continue;
            }
            if stall_until.is_none() && stall_applied.insert(delivered) {
                stall_until = stalls
                    .iter()
                    .find(|(n, _)| *n == delivered)
                    .map(|(_, d)| Instant::now() + *d);
            }
            if stall_until.is_none_or(|t| t <= Instant::now()) {
                stall_until = None;
                if let Some((_, text)) = queue.pop_front() {
                    s.send(text).await;
                    delivered += 1;
                }
                continue;
            }
        }
        let wake = [stall_until, final_due].into_iter().flatten().min();
        let wait = async {
            match wake {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            message = s.next() => match message {
                None => return,
                Some(What::Audio(_)) => {
                    audio_seen = true;
                    queue.push_back((false, audio_ack(true)));
                    if phase == 0 {
                        phase = 1;
                        if let Some(u) = &utterance {
                            for interim in &u.interims {
                                queue.push_back((true, transcript("u1", interim, false, 0.05)));
                            }
                        }
                    }
                    if phase == 1 && utterance.is_some() {
                        final_due = Some(Instant::now() + ENDPOINTING);
                    }
                }
                Some(What::Text(text)) if text == STOP => {
                    s.send(lifecycle("end_recording")).await;
                    s.close_with(1000, "").await;
                    return;
                }
                Some(What::Text(_) | What::Ping) => {}
            },
            () = wait => {}
        }
    }
}

#[tokio::test]
async fn the_provider_contract_suite_passes() {
    let harness = Harness::start().await;
    let report = stt_contract(&harness).await;
    let failures: Vec<_> = report.failures().collect();
    assert!(failures.is_empty(), "contract failures: {failures:#?}");
    assert_eq!(report.checks.len(), 7);
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1 and GLADIA_API_KEY)
// ---------------------------------------------------------------------------------------------

/// The first segment of the Turkish fixture (16 kHz mono PCM16, 44-byte WAV header).
fn turkish_speech() -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/tr_two_utterances.wav");
    let wav = std::fs::read(path).expect("fixtures/tr_two_utterances.wav");
    // Segment 0 is samples 8000..100800 (fixtures/tr_two_utterances.json).
    wav[44 + 8000 * 2..44 + 100_800 * 2].to_vec()
}

#[tokio::test]
async fn live_session_transcribes_turkish_and_finalizes_without_errors() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and GLADIA_API_KEY to run");
        return;
    }
    let Ok(key) = std::env::var("GLADIA_API_KEY") else {
        eprintln!("skipped: GLADIA_API_KEY is not set");
        return;
    };
    let mut cfg = GladiaConfig::new(Secret::new(key));
    cfg.language = Some("tr".to_owned());
    let stt = GladiaStt::new(cfg, clock()).expect("adapter");
    stt.warm().await.expect("warm");
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr".into()),
        })
        .await
        .expect("connect");
    // Real-time pace in 20 ms chunks: the speech, then a second of silence so that Gladia's
    // endpointing closes the utterance by itself.
    let mut pcm = turkish_speech();
    pcm.extend(vec![0_u8; 32_000]);
    let input = session.input.clone();
    let feeder = tokio::spawn(async move {
        for frame in pcm.chunks(640) {
            if input
                .send(SttInput::Audio(Bytes::copy_from_slice(frame)))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = input.send(SttInput::Finalize).await;
    });
    let mut partials = 0;
    let mut finals = Vec::new();
    let mut finalize_answer = None;
    let deadline = Instant::now() + Duration::from_secs(25);
    while finalize_answer.is_none() && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), session.events.recv()).await {
            Ok(Some(SttEvent::Transcript {
                text,
                is_final,
                speech_final,
                from_finalize,
            })) => {
                if !is_final {
                    partials += 1;
                } else if from_finalize {
                    assert!(speech_final, "the answer to a Finalize ends the utterance");
                    finalize_answer = Some(text);
                } else {
                    finals.push(text);
                }
            }
            Ok(Some(SttEvent::Error(e))) => panic!("live session failed: {e}"),
            Ok(None) | Err(_) => break,
        }
    }
    feeder.abort();
    assert!(
        finalize_answer.is_some(),
        "the Finalize must always be answered"
    );
    let heard: String = finals.join(" ");
    eprintln!(
        "live: {partials} partials, {} finals, transcript: {heard}",
        finals.len()
    );
    assert!(
        !heard.trim().is_empty(),
        "Gladia returned no final text for the Turkish speech"
    );
}
