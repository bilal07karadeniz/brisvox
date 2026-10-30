//! AssemblyAI STT adapter against a local mock of the AssemblyAI v3 streaming protocol: wire
//! format, turn mapping, finalize (`ForceEndpoint`), keepalive and the silence watchdog, errors
//! and close codes, reconnect with replay, the provider contract suite, and one live test
//! (`BRISVOX_LIVE_TESTS=1`).
//!
//! The mock is a plain `TcpListener` plus `tokio_tungstenite::accept_hdr_async`. Real sockets do
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
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::assemblyai::{AssemblyAiConfig, AssemblyAiStt, Encoding};
use brisvox_providers::{Error, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::contract::{Fault, SttHarness, Utterance, stt_contract};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

const KEY: &str = "aai-test-key-0123456789";
const FORCE_ENDPOINT: &str = r#"{"type":"ForceEndpoint"}"#;
const TERMINATE: &str = r#"{"type":"Terminate"}"#;
const KEEP_ALIVE: &str = r#"{"type":"KeepAlive"}"#;

/// The smallest and largest chunk (bytes of 16 kHz PCM16) the real server accepts: 50 ms and
/// 1000 ms.
const SERVER_MIN: usize = 1600;
const SERVER_MAX: usize = 32_000;

// ---------------------------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the server saw on one connection.
#[derive(Debug, Clone)]
enum What {
    Audio(Bytes),
    Text(String),
}

#[derive(Debug, Clone)]
struct Seen {
    conn: usize,
    at: Instant,
    what: What,
}

/// Everything the server saw, shared with the test.
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
                What::Text(_) => None,
            })
            .collect()
    }

    fn audio_times(&self, conn: usize) -> Vec<Instant> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn && matches!(s.what, What::Audio(_)))
            .map(|s| s.at)
            .collect()
    }

    fn texts(&self, conn: usize) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn)
            .filter_map(|s| match s.what {
                What::Text(t) => Some(t),
                What::Audio(_) => None,
            })
            .collect()
    }

    /// Sizes of audio frames the real server would have refused (outside 50..1000 ms).
    fn chunk_violations(&self) -> Vec<usize> {
        self.all()
            .into_iter()
            .filter_map(|s| match s.what {
                What::Audio(b) if !(SERVER_MIN..=SERVER_MAX).contains(&b.len()) => Some(b.len()),
                _ => None,
            })
            .collect()
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

/// The request the client made.
#[derive(Debug, Clone, Default)]
struct Req {
    uri: String,
    authorization: Option<String>,
}

#[derive(Default)]
struct Shared {
    connections: AtomicUsize,
    /// When each TCP connection was accepted, by connection index.
    accepted_at: Mutex<Vec<Instant>>,
    requests: Mutex<Vec<Req>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// A connection the server has accepted at TCP level but not yet answered.
struct Conn {
    stream: TcpStream,
    index: usize,
    shared: Arc<Shared>,
    log: Log,
}

type Ws = WebSocketStream<TcpStream>;

/// The `Begin` message of the reference.
fn begin() -> String {
    json!({
        "type": "Begin",
        "id": "3207b601-2054-48df-ba77-8784dfcf9fb8",
        "expires_at": 1_772_570_132_u64,
        "configuration": {
            "model": "universal-3-6-pro", "mode": "balanced", "api_version": "2025-05-12",
            "speaker_labels": false, "redact_pii": false, "filter_profanity": false,
            "domain": null, "voice_focus": null
        }
    })
    .to_string()
}

impl Conn {
    /// Completes the WebSocket handshake without sending `Begin`.
    async fn accept_raw(self) -> Option<Server> {
        let shared = Arc::clone(&self.shared);
        let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            lock(&shared.requests).push(Req {
                uri: req.uri().to_string(),
                authorization: req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
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

    /// Completes the handshake and sends `Begin`, like the real service.
    async fn accept(self) -> Option<Server> {
        let mut server = self.accept_raw().await?;
        server.send(begin()).await;
        Some(server)
    }

    /// Answers the upgrade request with an HTTP error.
    async fn reject(self, status: u16) {
        let shared = Arc::clone(&self.shared);
        let callback = move |req: &Request, _resp: Response| -> Result<Response, ErrorResponse> {
            lock(&shared.requests).push(Req {
                uri: req.uri().to_string(),
                authorization: None,
            });
            Err(http::Response::builder()
                .status(status)
                .body(Some(r#"{"error":"refused"}"#.to_owned()))
                .unwrap())
        };
        let _ = accept_hdr_async(self.stream, callback).await;
    }
}

/// The server side of an accepted connection.
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
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(Message::Ping(_)) => self.log.push(self.index, What::Text("<ping>".to_owned())),
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

    /// Answers `Terminate` like the service: `Termination`, then a normal close.
    async fn terminate(&mut self) {
        self.send(
            json!({"type": "Termination", "audio_duration_seconds": 1, "session_duration_seconds": 2})
                .to_string(),
        )
        .await;
        self.close_with(1000, "").await;
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
    async fn start<F>(handler: F) -> Self
    where
        F: Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        let handler: Handler = Arc::new(handler);
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
                let index = accept_shared.connections.fetch_add(1, Ordering::SeqCst);
                lock(&accept_shared.accepted_at).push(Instant::now());
                let conn = Conn {
                    stream,
                    index,
                    shared: Arc::clone(&accept_shared),
                    log: accept_log.clone(),
                };
                let task = tokio::spawn(handler(conn));
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

    fn base_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Req> {
        lock(&self.shared.requests).clone()
    }

    /// When each connection was accepted, by connection index.
    fn accepted_at(&self) -> Vec<Instant> {
        lock(&self.shared.accepted_at).clone()
    }

    /// Aborts every connection task (stalled handshakes included).
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

/// A server that accepts, says `Begin` and reads until the client leaves.
fn idle_server() -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    })
}

/// A `Turn` message. `end_ms`: end of the last word in ms of the connection's audio.
fn turn(order: u64, text: &str, end_of_turn: bool, end_ms: u32) -> String {
    json!({
        "type": "Turn",
        "turn_order": order,
        "turn_is_formatted": end_of_turn,
        "end_of_turn": end_of_turn,
        "transcript": text,
        "end_of_turn_confidence": if end_of_turn { 1.0 } else { 0.0 },
        "words": [{"text": text, "start": 0, "end": end_ms, "confidence": 0.99,
                   "word_is_final": end_of_turn}],
    })
    .to_string()
}

fn heartbeat() -> String {
    json!({"type": "Heartbeat", "total_audio_received_ms": 1000, "total_duration_ms": 1000,
           "realtime_factor": 1.0, "max_speech_probability": 0.5})
    .to_string()
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
fn config(server: &MockServer) -> AssemblyAiConfig {
    let mut c = AssemblyAiConfig::new(Secret::new(KEY.to_owned()));
    c.base_url = Some(server.base_url());
    c.io_timeout = Duration::from_secs(2);
    c.keepalive_interval = Duration::from_secs(9);
    c.finalize_timeout = Duration::from_millis(250);
    c.finalize_idle_timeout = Duration::from_millis(100);
    c.close_timeout = Duration::from_millis(300);
    c.reconnect_backoff = Duration::from_millis(10);
    c.replay_speed = 1000.0;
    c
}

fn adapter(server: &MockServer, tweak: impl FnOnce(&mut AssemblyAiConfig)) -> AssemblyAiStt {
    let mut c = config(server);
    tweak(&mut c);
    AssemblyAiStt::new(c, clock())
}

async fn connect(stt: &AssemblyAiStt) -> SttSession {
    stt.connect(stt_cfg()).await.expect("connect")
}

/// 100 ms of 16 kHz PCM16, filled with `fill`.
fn chunk(fill: u8) -> SttInput {
    SttInput::Audio(Bytes::from(vec![fill; 3200]))
}

/// 20 ms of 16 kHz PCM16, filled with `fill` (the engine's frame size).
fn frame(fill: u8) -> SttInput {
    SttInput::Audio(Bytes::from(vec![fill; 640]))
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
            ..
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

async fn expect_no_event(session: &mut SttSession, wait: Duration) {
    let extra = tokio::time::timeout(wait, session.events.recv()).await;
    assert!(extra.is_err(), "unexpected event {extra:?}");
}

fn fills(frames: &[Bytes]) -> Vec<u8> {
    frames.iter().map(|f| f[0]).collect()
}

// ---------------------------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_handshake_carries_the_bare_key_and_the_documented_query() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |_| {});
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr".into()),
        })
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].uri,
        "/v3/ws?speech_model=universal-3-6-pro&encoding=pcm_s16le&sample_rate=16000\
         &language_codes=%5B%22tr%22%5D&session_heartbeat=true"
    );
    // The key is the whole header value: no `Bearer`, no `Token`.
    assert_eq!(requests[0].authorization.as_deref(), Some(KEY));
    assert!(
        !requests[0].uri.contains(KEY),
        "the key must not be in the URL"
    );
}

#[tokio::test]
async fn language_detection_asks_for_the_codes_and_reports_the_language_of_each_final_turn() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Recorded shape: the language fields appear on the finished turn only.
        s.send(&turn(0, "siparişim", false, 400)).await;
        s.send(
            &json!({"type": "Turn", "turn_order": 0, "turn_is_formatted": true,
                    "end_of_turn": true, "transcript": "siparişim nerede",
                    "language_code": "tr", "language_confidence": 0.97,
                    "words": [{"text": "siparişim", "start": 0, "end": 900, "confidence": 0.9}]})
            .to_string(),
        )
        .await;
        s.send(
            &json!({"type": "Turn", "turn_order": 1, "turn_is_formatted": true,
                    "end_of_turn": true, "transcript": "where is it",
                    "language_code": "en", "language_confidence": 0.41,
                    "words": [{"text": "where", "start": 1000, "end": 1900, "confidence": 0.9}]})
            .to_string(),
        )
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.language_codes = vec!["en".to_owned(), "tr".to_owned()];
        c.language_detection = true;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let uri = server.requests()[0].uri.clone();
    assert!(
        uri.contains("&language_codes=%5B%22en%22%2C%22tr%22%5D&"),
        "{uri}"
    );
    assert!(uri.contains("&language_detection=true"), "{uri}");
    let mut finals = Vec::new();
    while finals.len() < 2 {
        if let SttEvent::Transcript {
            text,
            is_final: true,
            language,
            ..
        } = event(&mut session).await
        {
            finals.push((
                text.to_string(),
                language.map(|l| (l.code.to_string(), l.confidence)),
            ));
        }
    }
    assert_eq!(
        finals,
        [
            (
                "siparişim nerede".to_owned(),
                Some(("tr".to_owned(), Some(0.97)))
            ),
            (
                "where is it".to_owned(),
                Some(("en".to_owned(), Some(0.41)))
            ),
        ]
    );
}

#[tokio::test]
async fn mulaw_telephony_audio_is_announced_with_its_encoding_and_rate() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| c.encoding = Encoding::Mulaw);
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        })
        .await
        .unwrap();
    let uri = server.requests()[0].uri.clone();
    assert!(uri.contains("encoding=pcm_mulaw&sample_rate=8000"), "{uri}");
    assert!(!uri.contains("language_codes"), "{uri}");
}

#[tokio::test]
async fn long_enough_chunks_go_out_as_binary_frames_in_order_and_unchanged() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    for fill in 1..=3 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("3 audio frames", |l| l.audio(0).len() == 3)
        .await;
    for (i, f) in server.log.audio(0).iter().enumerate() {
        assert_eq!(f.as_ref(), vec![i as u8 + 1; 3200].as_slice());
    }
    assert!(
        server.log.texts(0).is_empty(),
        "no control message while audio flows"
    );
}

#[tokio::test]
async fn short_frames_are_batched_to_fifty_milliseconds_and_the_remainder_is_padded() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| c.batch_flush = Some(Duration::from_millis(60)));
    let session = connect(&stt).await;
    // Five 20 ms frames: the first three make 60 ms and go out together, the other two (40 ms)
    // are held until the caller has been quiet for batch_flush, then padded with silence.
    for fill in 1..=5 {
        session.input.send(frame(fill)).await.unwrap();
    }
    server
        .log
        .wait("two audio frames", |l| l.audio(0).len() == 2)
        .await;
    let audio = server.log.audio(0);
    assert_eq!(audio[0].len(), 1920);
    assert_eq!(&audio[0][..640], vec![1; 640].as_slice());
    assert_eq!(&audio[0][1280..], vec![3; 640].as_slice());
    assert_eq!(audio[1].len(), 1600, "padded to 50 ms");
    assert_eq!(&audio[1][..640], vec![4; 640].as_slice());
    assert_eq!(&audio[1][640..1280], vec![5; 640].as_slice());
    assert_eq!(&audio[1][1280..], vec![0; 320].as_slice(), "silence");
    assert!(server.log.chunk_violations().is_empty());
    // Telephony mu-law is padded with its own silence byte.
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.encoding = Encoding::Mulaw;
        c.batch_flush = Some(Duration::from_millis(50));
    });
    let session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        })
        .await
        .unwrap();
    session
        .input
        .send(SttInput::Audio(Bytes::from(vec![0x12; 80])))
        .await
        .unwrap();
    server
        .log
        .wait("the padded frame", |l| !l.audio(0).is_empty())
        .await;
    let audio = server.log.audio(0);
    assert_eq!(audio[0].len(), 400);
    assert_eq!(audio[0][79], 0x12);
    assert_eq!(audio[0][80], 0xFF);
}

#[tokio::test]
async fn oversized_chunks_are_split_into_valid_ones_without_losing_a_byte() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    // 2.5 s in one go (the server accepts at most 1 s per chunk).
    let data: Vec<u8> = (0..80_000_u32).map(|i| (i % 251) as u8).collect();
    session
        .input
        .send(SttInput::Audio(Bytes::from(data.clone())))
        .await
        .unwrap();
    server
        .log
        .wait("all the audio", |l| {
            l.audio(0).iter().map(Bytes::len).sum::<usize>() == 80_000
        })
        .await;
    assert!(server.log.chunk_violations().is_empty());
    assert!(server.log.audio(0).len() >= 5);
    let joined: Vec<u8> = server
        .log
        .audio(0)
        .iter()
        .flat_map(|b| b.to_vec())
        .collect();
    assert_eq!(joined, data);
}

// ---------------------------------------------------------------------------------------------
// Turn mapping
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_server_message_type_is_handled() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        for msg in [
            json!({"type": "Begin", "id": "again"}).to_string(),
            json!({"type": "SpeechStarted", "timestamp": 1216, "confidence": 0.98}).to_string(),
            turn(0, "mer", false, 300),
            heartbeat(),
            json!({"type": "FutureThing", "x": 1}).to_string(),
            json!({"type": "Error", "error": "something odd"}).to_string(),
            turn(0, "", false, 0),
            json!({"type": "SpeakerRevision", "turn_order": 0}).to_string(),
            turn(0, "merhaba", false, 600),
            turn(0, "merhaba", false, 600),
            turn(0, "Merhaba dünya.", true, 1000),
            // Late partial and a repeated final of the ended turn.
            turn(0, "merhaba dünya", false, 1000),
            turn(0, "Merhaba dünya.", true, 1000),
            // The closing turn after a Terminate / an empty endpoint.
            turn(1, "", true, 0),
            turn(1, "iyiyim", false, 1500),
        ] {
            s.send(msg).await;
        }
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
            t("mer", false, false, false),
            t("merhaba", false, false, false),
            t("Merhaba dünya.", true, true, false),
            t("iyiyim", false, false, false),
        ]
    );
    expect_no_event(&mut session, Duration::from_millis(200)).await;
}

#[tokio::test]
async fn interims_are_dropped_when_the_consumer_lags_but_finals_are_not() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        for i in 0..400 {
            s.send(turn(0, &format!("word {i}"), false, 500)).await;
        }
        s.send(turn(0, "The end.", true, 1000)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    // The consumer does not read while the server floods.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut interims = 0;
    loop {
        let (text, is_final, speech_final, _) = flat(event(&mut session).await);
        if is_final {
            assert_eq!((text.as_str(), speech_final), ("The end.", true));
            break;
        }
        interims += 1;
    }
    assert!(
        (1..400).contains(&interims),
        "some interims survive, the flood does not: {interims}"
    );
}

#[tokio::test]
async fn format_turns_waits_for_the_formatted_twin_on_universal_streaming() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(turn(0, "hello world", false, 500)).await;
        // Unformatted final, then the formatted one (message-sequence page).
        s.send(
            json!({"type": "Turn", "turn_order": 0, "turn_is_formatted": false,
                   "end_of_turn": true, "transcript": "hello world", "words": []})
            .to_string(),
        )
        .await;
        s.send(
            json!({"type": "Turn", "turn_order": 0, "turn_is_formatted": true,
                   "end_of_turn": true, "transcript": "Hello, world.", "words": []})
            .to_string(),
        )
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.speech_model = "universal-streaming-english".to_owned();
        c.format_turns = true;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello world", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("Hello, world.", true, true, false)
    );
    expect_no_event(&mut session, Duration::from_millis(150)).await;
    let uri = server.requests()[0].uri.clone();
    assert!(uri.contains("format_turns=true"), "{uri}");
}

// ---------------------------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_sends_force_endpoint_and_the_final_turn_answers_it() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(turn(0, "tamam", false, 500)).await;
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                    s.send(turn(0, "Tamam evet.", true, 900)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("tamam", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Tamam evet.", true, true, true)
    );
    assert_eq!(server.log.texts(0), [FORCE_ENDPOINT]);
    // A second Finalize while none is open is a new request; one while one is open adds nothing.
    session.input.send(SttInput::Finalize).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.log.texts(0).len(), 2);
}

#[tokio::test]
async fn a_held_audio_remainder_goes_out_before_the_force_endpoint() {
    let server = MockServer::start(idle_server()).await;
    // batch_flush is off (the default): only the Finalize can push the 20 ms frame out.
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    session.input.send(frame(7)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    server
        .log
        .wait("audio and ForceEndpoint", |l| {
            !l.audio(0).is_empty() && !l.texts(0).is_empty()
        })
        .await;
    let all = server.log.all();
    assert!(matches!(&all[0].what, What::Audio(b) if b.len() == 1600 && b[0] == 7 && b[1599] == 0));
    assert!(matches!(&all[1].what, What::Text(t) if t == FORCE_ENDPOINT));
    assert!(server.log.chunk_violations().is_empty());
}

#[tokio::test]
async fn an_unanswered_finalize_ends_with_a_synthetic_final_after_the_timeout() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.send(turn(0, "partial text", false, 500)).await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.finalize_timeout = Duration::from_millis(300);
        c.finalize_idle_timeout = Duration::from_millis(60);
    });
    // A turn in progress: the interim becomes the final, after the full wait.
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
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
    // No audio at all since the connect: the (shorter) idle wait, empty text, still ends the
    // utterance.
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(40) && waited < Duration::from_millis(250),
        "{waited:?}"
    );
    // Audio was given but the server has produced no partial yet: speech may be on its way, so
    // the full wait applies.
    let mut session = connect(&stt).await;
    session.input.send(chunk(2)).await.unwrap();
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_late_answer_after_the_synthetic_final_is_deduplicated_or_delivered() {
    // The server answers the ForceEndpoint 450 ms late (the adapter gave up after 200 ms). Case 0
    // answers with the words the synthetic final already had, case 1 with more.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(turn(0, "bir iki", false, 500)).await;
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    if s.index == 0 {
                        s.send(turn(0, "Bir iki.", true, 900)).await;
                    } else {
                        s.send(turn(0, "Bir iki üç.", true, 1100)).await;
                    }
                    // A new turn afterwards proves nothing else was swallowed.
                    s.send(turn(1, "dört", false, 1500)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(200));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", true, true, true)
    );
    // The duplicate is swallowed; the next event is the next turn's interim.
    assert_eq!(
        flat(event(&mut session).await),
        t("dört", false, false, false)
    );
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", true, true, true)
    );
    // Real speech the synthetic final missed is delivered as another utterance end.
    assert_eq!(
        flat(event(&mut session).await),
        t("Bir iki üç.", true, true, true)
    );
}

#[tokio::test]
async fn an_empty_final_turn_answers_a_finalize_with_nothing_to_flush() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                    s.send(turn(0, "", true, 0)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.finalize_idle_timeout = Duration::from_secs(5)
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "answered, not timed out"
    );
}

// ---------------------------------------------------------------------------------------------
// Keepalive and silence
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn keepalive_is_sent_with_an_inactivity_timeout_when_no_audio_flows() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.inactivity_timeout = Some(Duration::from_secs(30));
        c.keepalive_interval = Duration::from_millis(150);
    });
    let session = connect(&stt).await;
    // Audio every 50 ms for 600 ms: no KeepAlive.
    for fill in 0..12 {
        session.input.send(chunk(fill)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        server.log.texts(0).is_empty(),
        "KeepAlive while audio flowed: {:?}",
        server.log.texts(0)
    );
    // Then silence: KeepAlive repeats.
    server
        .log
        .wait("two KeepAlives", |l| l.texts(0).len() >= 2)
        .await;
    assert!(server.log.texts(0).iter().all(|t| t == KEEP_ALIVE));
    let all = server.log.all();
    let last_audio = all
        .iter()
        .filter(|s| matches!(s.what, What::Audio(_)))
        .map(|s| s.at)
        .max()
        .unwrap();
    let first_keepalive = all
        .iter()
        .find(|s| matches!(s.what, What::Text(_)))
        .unwrap()
        .at;
    assert!(first_keepalive - last_audio >= Duration::from_millis(120));
    // Audio resumes: KeepAlive stops.
    let before = server.log.texts(0).len();
    session.input.send(chunk(99)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.log.texts(0).len(), before);
    let uri = server.requests()[0].uri.clone();
    assert!(uri.contains("inactivity_timeout=30"), "{uri}");
}

#[tokio::test]
async fn without_an_inactivity_timeout_no_keepalive_is_sent() {
    // The docs: KeepAlive is only needed with `inactivity_timeout`.
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(50)
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(server.log.texts(0).is_empty());
    assert!(!server.requests()[0].uri.contains("inactivity_timeout"));
}

#[tokio::test]
async fn a_server_that_goes_silent_while_audio_was_given_is_replaced_not_fatal() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            // A half-open connection: nothing ever comes back, no close frame either.
            s.drain().await;
        } else {
            s.send(turn(0, "back again", false, 100)).await;
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    let started = Instant::now();
    session.input.send(chunk(5)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("back again", false, false, false)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the silence was cut short: {:?}",
        started.elapsed()
    );
    assert_eq!(server.connections(), 2);
    assert_eq!(fills(&server.log.audio(1)), [5], "the audio was replayed");
}

#[tokio::test]
async fn a_server_that_stays_silent_ends_the_session_with_a_timeout_when_no_reconnect_is_left() {
    // One reconnect: the replacement is silent as well.
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Core(brisvox_core::Error::Timeout { .. })
    ));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 2);
    // No reconnect at all: the timeout is reported at the deadline.
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.io_timeout = Duration::from_millis(300);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Core(brisvox_core::Error::Timeout { .. })
    ));
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn heartbeats_keep_a_quiet_but_healthy_connection_alive() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        // The caller is silent (digital silence): the only messages are heartbeats.
        loop {
            tokio::select! {
                message = s.next() => if message.is_none() { return },
                () = tokio::time::sleep(Duration::from_millis(100)) => s.send(heartbeat()).await,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(0)).await.unwrap();
    expect_no_event(&mut session, Duration::from_millis(900)).await;
    assert_eq!(server.connections(), 1, "the connection was never replaced");
}

#[tokio::test]
async fn a_line_that_never_carried_audio_is_never_silent_heartbeats_or_not() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.heartbeat = false;
        c.io_timeout = Duration::from_millis(200);
    });
    let mut session = connect(&stt).await;
    expect_no_event(&mut session, Duration::from_millis(700)).await;
    assert_eq!(server.connections(), 1);
    let uri = server.requests()[0].uri.clone();
    assert!(!uri.contains("session_heartbeat"), "{uri}");
    assert!(server.log.texts(0).is_empty(), "no probe without audio");
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(200));
    let mut session = connect(&stt).await;
    expect_no_event(&mut session, Duration::from_millis(700)).await;
}

// ---------------------------------------------------------------------------------------------
// Hang-up
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hang_up_sends_terminate_closes_events_at_once_and_drops_late_turns() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == TERMINATE => {
                    // Per the docs: the final turn for the audio already sent, then Termination.
                    s.send(turn(0, "Late words.", true, 1000)).await;
                    s.terminate().await;
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
        .wait("Terminate", |l| l.texts(0) == [TERMINATE.to_owned()])
        .await;
}

#[tokio::test]
async fn terminate_is_sent_even_when_no_audio_was_ever_given() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    drop(session);
    server
        .log
        .wait("Terminate", |l| l.texts(0) == [TERMINATE.to_owned()])
        .await;
}

#[tokio::test]
async fn a_peer_that_never_answers_terminate_is_given_up_on_after_close_timeout() {
    let ended: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&ended);
    let server = MockServer::start(handler(move |conn| {
        let seen = Arc::clone(&seen);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            // Reads everything, including Terminate, and never answers or closes.
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
        .wait("Terminate", |l| !l.texts(0).is_empty())
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
// Errors and close codes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn handshake_refusals_map_to_rejected_with_the_http_status_and_never_leak_the_key() {
    let status = Arc::new(AtomicU16::new(0));
    let s = Arc::clone(&status);
    let server = MockServer::start(handler(move |conn| {
        let s = Arc::clone(&s);
        async move { conn.reject(s.load(Ordering::SeqCst)).await }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    for code in [400_u16, 401, 402, 403, 429, 500, 503] {
        status.store(code, Ordering::SeqCst);
        let err = stt.connect(stt_cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Rejected { status, provider } if status == code && provider == "assemblyai"),
            "{code}: {err:?}"
        );
        assert!(!format!("{err} {err:?}").contains(KEY));
    }
}

#[tokio::test]
async fn a_close_instead_of_begin_maps_by_close_code() {
    let reply = Arc::new(Mutex::new((1008_u16, String::new())));
    let r = Arc::clone(&reply);
    let server = MockServer::start(handler(move |conn| {
        let r = Arc::clone(&r);
        async move {
            let Some(mut s) = conn.accept_raw().await else {
                return;
            };
            // An Error frame precedes the close, as documented.
            s.send(json!({"type": "Error", "error": "nope"}).to_string())
                .await;
            let (code, reason) = lock(&r).clone();
            s.close_with(code, &reason).await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let cases = [
        (
            1008,
            "Unauthorized Connection: Missing Authorization header",
            1008,
        ),
        (
            1008,
            "Unauthorized Connection: Too many concurrent sessions",
            429,
        ),
        (
            3009,
            "Unauthorized Connection: Too many concurrent sessions",
            429,
        ),
        (1011, "Internal error", 1011),
        (3005, "Session Cancelled: An error occurred", 3005),
    ];
    for (code, reason, expected) in cases {
        *lock(&reply) = (code, reason.to_owned());
        let err = stt.connect(stt_cfg()).await.unwrap_err();
        assert!(
            matches!(err, Error::Rejected { status, .. } if status == expected),
            "{code} {reason}: {err:?}"
        );
        assert!(!format!("{err} {err:?}").contains(KEY));
    }
    *lock(&reply) = (3007, String::new());
    assert!(matches!(
        stt.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
}

#[tokio::test]
async fn a_first_message_other_than_begin_is_a_protocol_error() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept_raw().await {
            s.send(turn(0, "too early", false, 100)).await;
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let err = stt.connect(stt_cfg()).await.unwrap_err();
    match err {
        Error::Protocol { detail, .. } => assert!(!detail.contains("too early")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_begin_for_another_model_is_tolerated() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept_raw().await {
            s.send(
                json!({"type": "Begin", "id": "x",
                       "configuration": {"model": "universal-streaming-english"}})
                .to_string(),
            )
            .await;
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    assert!(stt.connect(stt_cfg()).await.is_ok());
}

#[tokio::test]
async fn close_codes_during_a_session_are_mapped_as_documented() {
    let code = Arc::new(Mutex::new((1008_u16, String::new())));
    let c = Arc::clone(&code);
    let server = MockServer::start(handler(move |conn| {
        let c = Arc::clone(&c);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            let (code, reason) = lock(&c).clone();
            s.close_with(code, &reason).await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    // (close code, reason, Some(status) for Rejected or None for Protocol)
    for (code_, reason, status) in [
        (
            1008_u16,
            "Unauthorized Connection: out of credit",
            Some(1008_u16),
        ),
        (
            1008,
            "Unauthorized Connection: Too many concurrent sessions",
            Some(429),
        ),
        (3009, "Too many concurrent sessions", Some(429)),
        (3006, "Invalid JSON: {}", None),
        (3007, "Input duration violation: 20 ms", None),
    ] {
        *lock(&code) = (code_, reason.to_owned());
        let before = server.connections();
        let mut session = connect(&stt).await;
        session.input.send(chunk(1)).await.unwrap();
        match (expect_error(&mut session).await, status) {
            (Error::Rejected { status: got, .. }, Some(want)) => assert_eq!(got, want),
            (Error::Protocol { detail, .. }, None) => {
                assert!(detail.contains(&code_.to_string()), "{detail}");
            }
            (other, want) => panic!("{code_}: {other:?}, wanted {want:?}"),
        }
        expect_closed(&mut session).await;
        assert_eq!(
            server.connections(),
            before + 1,
            "{code_} must not be retried"
        );
    }
}

#[tokio::test]
async fn transient_closes_and_an_unrequested_termination_reconnect_and_the_call_goes_on() {
    let code = Arc::new(Mutex::new((3005_u16, String::new())));
    let c = Arc::clone(&code);
    let server = MockServer::start(handler(move |conn| {
        let c = Arc::clone(&c);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            if s.index % 2 == 0 {
                let (code, reason) = lock(&c).clone();
                if code == 0 {
                    // An unrequested Termination: the server ends the session on its own.
                    s.terminate().await;
                } else {
                    s.close_with(code, &reason).await;
                }
            } else {
                s.send(turn(0, "still here", false, 100)).await;
                s.drain().await;
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let cases = [
        (3005_u16, "Session Cancelled: An error occurred"),
        (3008, "Session Expired: Maximum session duration exceeded"),
        (1011, "Internal error"),
        (1000, ""),
        (0, ""),
    ];
    for (n, (code_, reason)) in cases.into_iter().enumerate() {
        *lock(&code) = (code_, reason.to_owned());
        // Even connection indexes die, odd ones live: every case starts on an even index.
        while server.connections() % 2 == 1 {
            let _keep = connect(&stt).await;
        }
        let before = server.connections();
        let mut session = connect(&stt).await;
        session.input.send(chunk(1)).await.unwrap();
        assert_eq!(
            flat(event(&mut session).await),
            t("still here", false, false, false),
            "case {n} ({code_})"
        );
        assert_eq!(server.connections(), before + 2, "case {n} ({code_})");
    }
}

#[tokio::test]
async fn garbage_and_malformed_turns_are_protocol_errors_without_content() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        match s.index {
            0 => s.send("this is not json").await,
            1 => {
                s.send(
                    json!({"type": "Turn", "turn_order": 0, "transcript": "secret words"})
                        .to_string(),
                )
                .await;
            }
            _ => s.send("[]").await,
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    for _ in 0..3 {
        let mut session = connect(&stt).await;
        session.input.send(chunk(1)).await.unwrap();
        match expect_error(&mut session).await {
            Error::Protocol { detail, .. } => {
                assert!(!detail.contains("this is not json") && !detail.contains("secret words"));
            }
            other => panic!("{other:?}"),
        }
        expect_closed(&mut session).await;
    }
    assert_eq!(server.connections(), 3, "a protocol error is not retried");
}

#[tokio::test]
async fn invalid_settings_fail_before_any_connection_is_made() {
    let server = MockServer::start(idle_server()).await;
    let bad_model = adapter(&server, |c| c.speech_model = "x&y=1".to_owned());
    assert!(matches!(
        bad_model.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    let cleartext = adapter(&server, |c| {
        c.base_url = Some("ws://example.com".to_owned());
    });
    assert!(matches!(
        cleartext.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    let slow_keepalive = adapter(&server, |c| {
        c.inactivity_timeout = Some(Duration::from_secs(10));
        c.keepalive_interval = Duration::from_secs(10);
    });
    assert!(matches!(
        slow_keepalive.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    let too_many = adapter(&server, |c| c.keyterms = vec!["k".to_owned(); 101]);
    assert!(matches!(
        too_many.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(server.connections(), 0);
}

#[test]
fn the_provider_debug_output_never_contains_the_key() {
    let c = AssemblyAiConfig::new(Secret::new(KEY.to_owned()));
    let stt = AssemblyAiStt::new(c, clock());
    assert!(!format!("{stt:?}").contains(KEY));
    assert_eq!(stt.name(), "assemblyai");
}

// ---------------------------------------------------------------------------------------------
// Reconnect with replay
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_connection_is_replaced_and_only_unfinished_audio_is_replayed() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(2).await;
                // The first turn ends 100 ms into the stream: chunk 1 is done.
                s.send(turn(0, "One.", true, 100)).await;
                s.until_audio(4).await;
                // Drop without a closing handshake.
            }
            1 => {
                // Replay of chunks 2, 3, 4. Times restart at 0: 0-200 ms are chunks 2 and 3.
                s.until_audio(3).await;
                s.send(turn(0, "Two.", true, 200)).await;
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
    assert_eq!(
        flat(event(&mut session).await),
        t("One.", true, true, false)
    );
    session.input.send(chunk(3)).await.unwrap();
    session.input.send(chunk(4)).await.unwrap();
    // Reconnecting happens silently: the next thing is the final of the replayed audio.
    assert_eq!(
        flat(event(&mut session).await),
        t("Two.", true, true, false)
    );
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
    assert_eq!(server.connections(), 3);
    // Every connection used the same URL and credentials; every replayed chunk is a valid one.
    let requests = server.requests();
    assert!(requests.iter().all(|r| r.uri == requests[0].uri));
    assert!(
        requests
            .iter()
            .all(|r| r.authorization.as_deref() == Some(KEY))
    );
    assert!(server.log.chunk_violations().is_empty());
}

#[tokio::test]
async fn an_answered_finalize_trims_the_replay_to_where_the_force_endpoint_was_sent() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(2).await;
                loop {
                    match s.next().await {
                        // The answer carries no word times at all.
                        Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                            s.send(
                                json!({"type": "Turn", "turn_order": 0, "end_of_turn": true,
                                       "turn_is_formatted": true, "transcript": "Tamam."})
                                .to_string(),
                            )
                            .await;
                            break;
                        }
                        Some(_) => {}
                        None => return,
                    }
                }
                s.until_audio(3).await;
                // Dropped after the third chunk.
            }
            _ => s.drain().await,
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Tamam.", true, true, true)
    );
    session.input.send(chunk(3)).await.unwrap();
    server
        .log
        .wait("the replay", |l| !l.audio(1).is_empty())
        .await;
    // Chunks 1 and 2 are before the ForceEndpoint: not replayed.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fills(&server.log.audio(1)), [3]);
}

#[tokio::test]
async fn a_pending_force_endpoint_is_sent_again_after_the_replay() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            // Takes the audio and the ForceEndpoint, then drops without answering.
            while s.log.texts(0).is_empty() {
                if s.next().await.is_none() {
                    return;
                }
            }
        } else {
            loop {
                match s.next().await {
                    Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                        s.send(turn(0, "Evet.", true, 100)).await;
                    }
                    Some(_) => {}
                    None => return,
                }
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(5));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Evet.", true, true, true)
    );
    // The new connection heard the audio first and the ForceEndpoint after it.
    let second: Vec<_> = server
        .log
        .all()
        .into_iter()
        .filter(|s| s.conn == 1)
        .map(|s| matches!(s.what, What::Audio(_)))
        .collect();
    assert_eq!(second, [true, false]);
}

#[tokio::test]
async fn reconnecting_gives_up_after_the_configured_attempts_with_disconnected() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            s.close_with(1011, "Internal error").await;
        } else {
            conn.reject(503).await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 3, "the original and two attempts");
}

#[tokio::test]
async fn reconnect_attempts_back_off_exponentially() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            s.close_with(3005, "Session Cancelled").await;
        } else {
            conn.reject(503).await;
        }
    }))
    .await;
    let base = Duration::from_millis(80);
    let stt = adapter(&server, |c| {
        c.max_reconnects = 3;
        c.reconnect_backoff = base;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    assert_eq!(server.connections(), 4, "the original and three attempts");
    // Attempt n waits `base * 2^n` after the previous connection.
    let at = server.accepted_at();
    for (n, pair) in at.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        let expected = base * (1 << n);
        assert!(
            gap >= expected.mul_f64(0.9),
            "attempt {} came after {gap:?}, expected at least {expected:?}",
            n + 1
        );
    }
}

#[tokio::test]
async fn with_reconnecting_off_the_first_drop_is_final() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "Internal error").await;
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
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn a_credential_failure_while_reconnecting_is_surfaced_as_such() {
    // Refused at the upgrade (401) or by a 1008 close instead of Begin: neither is retried.
    for use_close in [false, true] {
        let server = MockServer::start(handler(move |conn| async move {
            if conn.index == 0 {
                let Some(mut s) = conn.accept().await else {
                    return;
                };
                s.until_audio(1).await;
            } else if use_close {
                if let Some(mut s) = conn.accept_raw().await {
                    s.close_with(
                        1008,
                        "Unauthorized Connection: Missing Authorization header",
                    )
                    .await;
                }
            } else {
                conn.reject(401).await;
            }
        }))
        .await;
        let stt = adapter(&server, |c| c.max_reconnects = 5);
        let mut session = connect(&stt).await;
        session.input.send(chunk(1)).await.unwrap();
        let want = if use_close { 1008 } else { 401 };
        assert!(
            matches!(expect_error(&mut session).await, Error::Rejected { status, .. } if status == want)
        );
        assert_eq!(server.connections(), 2, "a bad key is not retried");
    }
}

#[tokio::test]
async fn audio_sent_while_reconnecting_is_kept_and_replayed() {
    let server = MockServer::start(handler(|conn| async move {
        match conn.index {
            0 => {
                let Some(mut s) = conn.accept().await else {
                    return;
                };
                s.until_audio(1).await;
            }
            // The first attempt is refused; the second one works.
            1 => conn.reject(503).await,
            _ => {
                if let Some(mut s) = conn.accept().await {
                    s.drain().await;
                }
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.max_reconnects = 3;
        c.reconnect_backoff = Duration::from_millis(100);
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("the first reconnect attempt", |_| server.connections() >= 2)
        .await;
    session.input.send(chunk(2)).await.unwrap();
    session.input.send(chunk(3)).await.unwrap();
    server
        .log
        .wait("the replay", |l| l.audio(2).len() >= 3)
        .await;
    assert_eq!(fills(&server.log.audio(2)), [1, 2, 3]);
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
    // 300 ms of window = 9600 bytes: the newest three 100 ms chunks.
    let stt = adapter(&server, |c| c.replay_window = Duration::from_millis(300));
    let session = connect(&stt).await;
    for fill in 1..=10 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay", |l| l.audio(1).len() >= 3)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fills(&server.log.audio(1)), [8, 9, 10]);
}

/// Replays six 100 ms chunks at `speed` and returns the time between the first and the last.
async fn replay_spread(speed: f64) -> Duration {
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
    let stt = adapter(&server, |c| c.replay_speed = speed);
    let session = connect(&stt).await;
    for fill in 1..=6 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay", |l| l.audio(1).len() >= 6)
        .await;
    let times = server.log.audio_times(1);
    *times.last().unwrap() - times[0]
}

#[tokio::test]
async fn replayed_audio_is_paced_at_the_configured_speed() {
    // Five gaps of 100 ms / speed between six chunks. The bounds are tight enough to tell the
    // configured speed from real time (500 ms) and from a fast replay (a few ms).
    let at_2x = replay_spread(2.0).await;
    assert!(
        at_2x >= Duration::from_millis(220) && at_2x < Duration::from_millis(380),
        "2x: expected about 250 ms, got {at_2x:?}"
    );
    let at_4x = replay_spread(4.0).await;
    assert!(
        at_4x >= Duration::from_millis(100) && at_4x < Duration::from_millis(220),
        "4x: expected about 125 ms, got {at_4x:?}"
    );
}

#[tokio::test]
async fn a_reconnect_does_not_replay_old_silence() {
    // Forty 100 ms chunks, no turn ever reported: the ring holds 4 s of nothing. Replaying all
    // of it at real time would leave the new connection 4 s behind the caller for good.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(40).await;
            s.close_with(1011, "Internal error").await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    for fill in 1..=40 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay", |l| l.audio(1).len() >= 10)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Only the last second (the pre-roll), in whole chunks.
    assert_eq!(fills(&server.log.audio(1)), (31..=40).collect::<Vec<u8>>());
    assert!(server.log.chunk_violations().is_empty());
}

#[tokio::test]
async fn a_reconnect_replays_the_turn_in_progress_from_before_its_first_partial() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(30).await;
            s.send(turn(0, "Merhaba", false, 500)).await;
            s.until_audio(40).await;
            s.close_with(1011, "Internal error").await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    for fill in 1..=30 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    // The partial arrives when 3 s of audio were sent.
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba", false, false, false)
    );
    for fill in 31..=40 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay", |l| l.audio(1).len() >= 30)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // From 2 s before the first partial (chunk 11) to the end: nothing of the turn is lost.
    assert_eq!(fills(&server.log.audio(1)), (11..=40).collect::<Vec<u8>>());
}

#[tokio::test]
async fn word_times_map_correctly_after_the_ring_evicted_audio_during_a_paced_replay() {
    // The replay is paced at real time (the default) while the caller's audio arrives faster, so
    // the ring (500 ms window) evicts chunks the new connection was never given. The server's
    // clock does not run over that gap: its word times must be mapped past it, or the next
    // reconnect replays a turn that was already delivered.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(5).await;
                s.close_with(1011, "Internal error").await;
            }
            1 => {
                // Heard chunk 1 (replay) and chunk 11 (the first one left in the ring): 200 ms.
                s.until_audio(2).await;
                s.send(turn(0, "Tamam.", true, 200)).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                s.close_with(1011, "Internal error").await;
            }
            _ => s.drain().await,
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.max_reconnects = 2;
        c.replay_window = Duration::from_millis(500);
        c.replay_speed = 1.0;
    });
    let mut session = connect(&stt).await;
    for fill in 1..=5 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay to start", |l| !l.audio(1).is_empty())
        .await;
    for fill in 6..=15 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    assert_eq!(
        flat(event(&mut session).await),
        t("Tamam.", true, true, false)
    );
    server
        .log
        .wait("the second reconnect", |l| l.audio(2).len() >= 4)
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fills(&server.log.audio(1))[..2], [1, 11]);
    // Chunk 11 ended the delivered turn: it is not replayed again.
    assert_eq!(fills(&server.log.audio(2)), [12, 13, 14, 15]);
}

#[tokio::test]
async fn live_audio_during_a_paced_replay_queues_behind_it_in_order_and_in_rate() {
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start(rate_checked_server(
        Duration::from_millis(300),
        8,
        Arc::clone(&tripped),
    ))
    .await;
    // The library default for the replay pace, not the fast test one.
    let stt = adapter(&server, |c| {
        c.replay_speed = AssemblyAiConfig::new(Secret::new(KEY.to_owned())).replay_speed;
        // The mock never speaks: keep the silence watch out of the way.
        c.io_timeout = Duration::from_secs(10);
    });
    let mut session = connect(&stt).await;
    for fill in 1..=8 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    // The caller keeps talking at real time while the replay drains: 1.8 s of audio in all.
    for fill in 9..=18 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the replay and the live audio", |l| l.audio(1).len() == 18)
        .await;
    assert!(
        !tripped.load(Ordering::SeqCst),
        "replay plus live audio ran ahead of real time (the server would close with 3007)"
    );
    assert_eq!(fills(&server.log.audio(1)), (1..=18).collect::<Vec<u8>>());
    expect_no_event(&mut session, Duration::from_millis(200)).await;
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
async fn a_finalize_after_a_natural_final_does_not_wait_for_an_answer_that_may_not_come() {
    // The model ended "Evet." by itself; the engine's VAD stop (a Finalize) arrives later, with
    // silence in between that the engine forwards as audio. Nothing is open on the model, and a
    // ForceEndpoint with no open turn may never be answered: the short wait applies.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Audio(b)) if b[0] == 1 => s.send(turn(0, "Evet.", true, 100)).await,
                Some(What::Audio(b)) if b[0] == 9 => s.send(turn(1, "Hayir", false, 100)).await,
                // ForceEndpoint and everything else: no answer.
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.finalize_timeout = Duration::from_millis(700);
        c.finalize_idle_timeout = Duration::from_millis(100);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Evet.", true, true, false)
    );
    // Silence after the final, then the late VAD stop.
    session.input.send(chunk(0)).await.unwrap();
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(80) && waited < Duration::from_millis(450),
        "the idle wait was expected, got {waited:?}"
    );
    // New speech opens a turn again: the full wait applies once more.
    session.input.send(chunk(9)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Hayir", false, false, false)
    );
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Hayir", true, true, true)
    );
    assert!(
        start.elapsed() >= Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_half_open_server_is_replaced_while_audio_flows_with_the_production_ordering() {
    // keepalive_interval (150 ms) below io_timeout (600 ms), like the defaults (4 s / 10 s);
    // the other watchdog tests have the opposite order. Audio flows without a gap, so the line
    // is never quiet and the watch must fire at io_timeout.
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(150);
        c.io_timeout = Duration::from_millis(600);
    });
    let session = connect(&stt).await;
    let start = Instant::now();
    while server.connections() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "the silent server was never replaced"
        );
        let _ = session.input.send(frame(7)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(500),
        "replaced early: {waited:?}"
    );
}

#[tokio::test]
async fn a_quiet_line_pauses_the_silence_watch_until_audio_resumes() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.io_timeout = Duration::from_millis(400);
    });
    let mut session = connect(&stt).await;
    // One burst, then a pause far longer than io_timeout: the server owes nothing.
    session.input.send(chunk(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(server.connections(), 1, "a quiet line is not a dead one");
    expect_no_event(&mut session, Duration::from_millis(50)).await;
    // Audio resumes: the watch is armed again and a server that stays silent is replaced.
    let start = Instant::now();
    while server.connections() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "the silent server was never replaced after the audio resumed"
        );
        let _ = session.input.send(frame(2)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_flapping_server_that_only_says_begin_is_not_reconnected_forever() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1011, "Internal error").await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 3, "the original and two attempts");
}

#[tokio::test]
async fn a_turn_proves_the_connection_and_restores_the_reconnect_budget() {
    // Every connection answers once and then drops: with a budget of one the session survives
    // several drops, because each turn resets the count.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(turn(0, &format!("c{}", s.index), false, 100)).await;
        // Dropped without a closing handshake.
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 1);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    for n in 0..3 {
        assert_eq!(
            flat(event(&mut session).await),
            t(&format!("c{n}"), false, false, false)
        );
    }
}

#[tokio::test]
async fn a_heartbeat_also_restores_the_reconnect_budget() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(heartbeat()).await;
        if s.index >= 3 {
            s.send(turn(0, "survived", false, 100)).await;
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 1);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("survived", false, false, false)
    );
}

#[tokio::test]
async fn the_finalize_bound_holds_while_a_reconnect_is_in_the_way() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
        } else {
            conn.reject(503).await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.max_reconnects = 10;
        c.reconnect_backoff = Duration::from_millis(200);
        c.finalize_timeout = Duration::from_millis(300);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("the first reconnect attempt", |_| server.connections() >= 2)
        .await;
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    // The caller's wait is bounded from its request, whatever the reconnect is doing.
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(
        start.elapsed() < Duration::from_millis(900),
        "{:?}",
        start.elapsed()
    );
}

// ---------------------------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_connect_that_never_completes_times_out() {
    let server = MockServer::start(handler(|conn| async move {
        // TCP is accepted, the WebSocket upgrade is never answered.
        let _held = conn;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }))
    .await;
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
async fn a_server_that_upgrades_but_never_says_begin_times_out_the_connect() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept_raw().await {
            s.drain().await;
        }
    }))
    .await;
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
    assert!(started.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn a_write_that_cannot_complete_times_out_and_the_connection_is_replaced() {
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
            s.send(turn(i, &format!("segment {i}"), true, 100)).await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    // The caller does not read. The adapter waits for io_timeout, then treats it as gone: it
    // stops, tells the server (Terminate) and closes the channel after what it had queued.
    server
        .log
        .wait("Terminate", |l| !l.texts(0).is_empty())
        .await;
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
// Warm
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn warm_resolves_and_caches_tls_without_opening_a_connection() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |_| {});
    stt.warm().await.unwrap();
    stt.warm().await.unwrap();
    assert_eq!(server.connections(), 0);
    // A wss target builds the shared TLS config and resolves its name (localhost, so no network
    // is needed); still no connection is opened, and it does not matter that nothing listens.
    let tls = adapter(&server, |c| {
        c.base_url = Some("wss://localhost:1".to_owned())
    });
    tls.warm().await.unwrap();
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------------------------
// Review fixes: real-time pacing, chunk minimum at odd rates, finalize wait, liveness
// ---------------------------------------------------------------------------------------------

/// A server that, like the real one, closes the session with 3007 when it has received more audio
/// than wall-clock time (plus `allowance`) has passed since the first chunk of the connection.
/// Connection 0 takes `first_audio` chunks and then drops with 1011; the rate check applies to the
/// connections after it (the live burst of connection 0 is the test's, not the adapter's).
fn rate_checked_server(
    allowance: Duration,
    first_audio: usize,
    tripped: Arc<std::sync::atomic::AtomicBool>,
) -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(move |conn| {
        let tripped = Arc::clone(&tripped);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            if s.index == 0 {
                s.until_audio(first_audio).await;
                s.close_with(1011, "Internal error").await;
                return;
            }
            let mut first: Option<Instant> = None;
            let mut audio_ms = 0.0_f64;
            while let Some(message) = s.next().await {
                let What::Audio(bytes) = message else {
                    continue;
                };
                let start = *first.get_or_insert_with(Instant::now);
                audio_ms += bytes.len() as f64 / 32.0; // 16 kHz PCM16
                if audio_ms
                    > start.elapsed().as_secs_f64() * 1000.0 + allowance.as_secs_f64() * 1000.0
                {
                    tripped.store(true, Ordering::SeqCst);
                    s.close_with(
                        3007,
                        "Audio Transmission Rate Exceeded: Received 0.8 sec. audio in 0.4 sec",
                    )
                    .await;
                    return;
                }
            }
        }
    })
}

#[tokio::test]
async fn a_replay_with_the_default_config_never_runs_ahead_of_real_time() {
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start(rate_checked_server(
        Duration::from_millis(150),
        8,
        Arc::clone(&tripped),
    ))
    .await;
    // The library default for the replay pace, not the fast test one.
    let stt = adapter(&server, |c| {
        c.replay_speed = AssemblyAiConfig::new(Secret::new(KEY.to_owned())).replay_speed;
    });
    let mut session = connect(&stt).await;
    for fill in 1..=8 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("the whole replay", |l| l.audio(1).len() == 8)
        .await;
    assert!(
        !tripped.load(Ordering::SeqCst),
        "the server would have closed with 3007"
    );
    assert_eq!(fills(&server.log.audio(1)), [1, 2, 3, 4, 5, 6, 7, 8]);
    expect_no_event(&mut session, Duration::from_millis(200)).await;
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
async fn the_mock_rate_check_catches_a_replay_faster_than_the_allowance() {
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start(rate_checked_server(
        Duration::from_millis(150),
        8,
        Arc::clone(&tripped),
    ))
    .await;
    // Twice real time: 800 ms of audio in 350 ms. (The old default of 1.25x ran only ~90 ms
    // ahead of the 150 ms allowance at the end, a margin that seven paced sleeps overshooting by
    // a Windows timer tick each can eat on a loaded machine; then the check never fired.)
    let stt = adapter(&server, |c| c.replay_speed = 2.0);
    let mut session = connect(&stt).await;
    for fill in 1..=8 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    // The 3007 for the rate is transient: another reconnect is tried and, with the budget of one
    // spent, the call ends with Disconnected rather than a protocol error.
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    assert!(tripped.load(Ordering::SeqCst), "the rate check never fired");
}

/// Connection 0 closes after the first audio with `code` and `reason`; connection 1 is healthy.
/// The session must reconnect, replay the audio and carry on without an error.
async fn a_close_that_reconnects(code: u16, reason: &'static str) {
    let server = MockServer::start(handler(move |conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
            s.close_with(code, reason).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(9)).await.unwrap();
    server
        .log
        .wait("the replay", |l| l.audio(1).len() == 1)
        .await;
    assert_eq!(fills(&server.log.audio(1)), [9]);
    session.input.send(chunk(10)).await.unwrap();
    server
        .log
        .wait("live audio", |l| l.audio(1).len() == 2)
        .await;
    expect_no_event(&mut session, Duration::from_millis(200)).await;
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
async fn a_3006_close_for_inactivity_reconnects_instead_of_ending_the_call() {
    a_close_that_reconnects(
        3006,
        "Session terminated due to inactivity: No messages received for 10 seconds",
    )
    .await;
}

#[tokio::test]
async fn a_3007_close_for_the_transmission_rate_reconnects_instead_of_ending_the_call() {
    a_close_that_reconnects(
        3007,
        "Audio Transmission Rate Exceeded: Received 3.0 sec. audio in 1.0 sec",
    )
    .await;
}

#[tokio::test]
async fn the_minimum_chunk_is_never_under_fifty_milliseconds_at_22050_hz() {
    // 50 ms at 22.05 kHz PCM16 is 2205 bytes, an odd number: 2204 bytes (49.98 ms) is refused.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        while let Some(message) = s.next().await {
            if let What::Audio(b) = message
                && !(2205..=44_100).contains(&b.len())
            {
                s.close_with(3007, "Input duration violation").await;
                return;
            }
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz22050,
            language: None,
        })
        .await
        .unwrap();
    // Two 25 ms frames (1102 bytes each) are 49.98 ms: they must wait for more.
    let frame = |fill: u8| SttInput::Audio(Bytes::from(vec![fill; 1102]));
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(server.log.audio(0).is_empty(), "49.98 ms must be held");
    // A Finalize pads to the smallest valid chunk, 2206 bytes (whole samples, over 50 ms).
    session.input.send(SttInput::Finalize).await.unwrap();
    server
        .log
        .wait("the padded chunk", |l| !l.audio(0).is_empty())
        .await;
    let audio = server.log.audio(0);
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].len(), 2206);
    assert!(matches!(
        event(&mut session).await,
        SttEvent::Transcript { is_final: true, .. }
    ));
}

#[tokio::test]
async fn a_pause_in_the_callers_audio_inserts_no_silence_into_the_stream() {
    let server = MockServer::start(idle_server()).await;
    let session = connect(&adapter(&server, |_| {})).await;
    session.input.send(frame(1)).await.unwrap();
    // Network jitter well above the old 40 ms flush: nothing may be sent in the gap.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(server.log.audio(0).is_empty());
    session.input.send(frame(2)).await.unwrap();
    session.input.send(frame(3)).await.unwrap();
    server
        .log
        .wait("the chunk", |l| !l.audio(0).is_empty())
        .await;
    let audio = server.log.audio(0);
    assert_eq!(audio.len(), 1);
    let expected: Vec<u8> = [1_u8, 2, 3].iter().flat_map(|f| vec![*f; 640]).collect();
    assert_eq!(audio[0].as_ref(), expected.as_slice(), "no fake silence");
    // Only the Finalize pads (it needs the audio out).
    session.input.send(frame(4)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    server
        .log
        .wait("the padded chunk", |l| l.audio(0).len() == 2)
        .await;
    let padded = &server.log.audio(0)[1];
    assert_eq!(padded.len(), 1600);
    assert!(padded[..640].iter().all(|b| *b == 4) && padded[640..].iter().all(|b| *b == 0));
}

#[tokio::test]
async fn a_short_utterance_without_a_partial_waits_for_the_force_endpoint_answer() {
    // "evet": the engine's VAD ends the speech and sends Finalize before the model has produced
    // any partial. The real final comes 400 ms after the ForceEndpoint.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == FORCE_ENDPOINT => {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    s.send(turn(0, "Evet.", true, 100)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.finalize_timeout = Duration::from_millis(1500);
        c.finalize_idle_timeout = Duration::from_millis(100);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let start = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    // One final, the real one, flagged as the answer: no empty synthetic final first.
    assert_eq!(
        flat(event(&mut session).await),
        t("Evet.", true, true, true)
    );
    assert!(start.elapsed() >= Duration::from_millis(350));
    expect_no_event(&mut session, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn a_final_without_words_trims_the_replay_so_a_reconnect_delivers_nothing_twice() {
    let wordless = |text: &str| {
        json!({"type": "Turn", "turn_order": 0, "turn_is_formatted": true, "end_of_turn": true,
               "transcript": text, "end_of_turn_confidence": 0.9})
        .to_string()
    };
    let server = MockServer::start(handler(move |conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(1).await;
            s.send(wordless("Merhaba.")).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            s.close_with(1011, "Internal error").await;
        } else {
            // A replayed utterance would be transcribed again.
            while let Some(message) = s.next().await {
                if matches!(message, What::Audio(_)) {
                    s.send(wordless("Merhaba.")).await;
                }
            }
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba.", true, true, false)
    );
    server
        .log
        .wait("the second connection", |_| server.connections() == 2)
        .await;
    expect_no_event(&mut session, Duration::from_millis(400)).await;
    assert!(
        server.log.audio(1).is_empty(),
        "the delivered turn was replayed"
    );
}

#[tokio::test]
async fn without_heartbeats_a_ping_probes_a_silent_connection_and_an_answer_keeps_it() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| {
        c.heartbeat = false;
        c.io_timeout = Duration::from_millis(200);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(0)).await.unwrap();
    expect_no_event(&mut session, Duration::from_millis(900)).await;
    assert_eq!(server.connections(), 1, "the pongs kept it alive");
    assert!(
        server.log.texts(0).iter().any(|t| t == "<ping>"),
        "no ping was sent"
    );
}

#[tokio::test]
async fn without_heartbeats_a_connection_that_answers_nothing_is_declared_lost() {
    // Accepts, then never reads again: no pong, no heartbeat, nothing.
    let server = MockServer::start(handler(|conn| async move {
        let Some(_s) = conn.accept().await else {
            return;
        };
        std::future::pending::<()>().await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.heartbeat = false;
        c.io_timeout = Duration::from_millis(250);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Core(brisvox_core::Error::Timeout { .. })
    ));
    // A probe after io_timeout, then another io_timeout for the answer.
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_fatal_error_ends_the_session_on_the_server_with_terminate() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send("<<not json>>").await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Protocol { .. }
    ));
    server
        .log
        .wait("Terminate", |l| l.texts(0).contains(&TERMINATE.to_owned()))
        .await;
}

#[tokio::test]
async fn a_connection_replaced_for_silence_is_terminated_first() {
    let server = MockServer::start(idle_server()).await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(250));
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    server
        .log
        .wait("Terminate on the replaced connection", |l| {
            l.texts(0).contains(&TERMINATE.to_owned())
        })
        .await;
    server
        .log
        .wait("the replacement", |l| l.audio(1).len() == 1)
        .await;
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

struct Harness {
    server: MockServer,
    script: Arc<Mutex<Script>>,
    provider: Arc<AssemblyAiStt>,
}

/// The deadline the contract checks hold the adapter to. Shorter than the suite's default so the
/// timeout checks stay quick on a real clock.
const CONTRACT_DEADLINE: Duration = Duration::from_millis(600);

impl Harness {
    async fn start() -> Self {
        let script = Arc::new(Mutex::new(Script::default()));
        let s = Arc::clone(&script);
        let server = MockServer::start(handler(move |conn| scripted(conn, Arc::clone(&s)))).await;
        let provider = Arc::new(adapter(&server, |c| {
            c.io_timeout = CONTRACT_DEADLINE;
            c.finalize_timeout = Duration::from_millis(300);
            c.close_timeout = Duration::from_millis(300);
            c.keepalive_interval = Duration::from_secs(4);
            // The suite's timeout checks expect a silent provider to be reported as
            // Core(Timeout) at the deadline. With a reconnect left the adapter replaces the
            // silent connection first (tested separately), so the suite runs without one.
            c.max_reconnects = 0;
            // The suite sends one 20 ms frame and expects an interim: a source that goes quiet
            // for good is what the optional flush is for.
            c.batch_flush = Some(Duration::from_millis(60));
        }));
        Self {
            server,
            script,
            provider,
        }
    }
}

impl SttHarness for Harness {
    type P = AssemblyAiStt;

    fn provider(&self) -> Arc<AssemblyAiStt> {
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

/// The scripted AssemblyAI of the contract suite: partial turns on the first audio, final turns
/// on `ForceEndpoint`, plus the injected faults. Like the real service it closes the session
/// with 3007 on a chunk outside 50..1000 ms, so the suite also checks the adapter's batching.
async fn scripted(conn: Conn, script: Arc<Mutex<Script>>) {
    let (utterance, faults, outage) = {
        let mut s = lock(&script);
        if s.outage {
            (None, Vec::new(), true)
        } else {
            (s.utterance.take(), std::mem::take(&mut s.faults), false)
        }
    };
    if outage {
        conn.reject(503).await;
        return;
    }
    let mut disconnect_after = None;
    let mut protocol_after = None;
    let mut stalls: Vec<(usize, Duration)> = Vec::new();
    for fault in faults {
        match fault {
            Fault::Reject { status } => {
                conn.reject(status).await;
                return;
            }
            Fault::ConnectStall { duration } => tokio::time::sleep(duration).await,
            Fault::Disconnect { after_events } => disconnect_after = Some(after_events),
            Fault::Protocol { after_events } => protocol_after = Some(after_events),
            Fault::Stall {
                before_event,
                duration,
            } => stalls.push((before_event, duration)),
        }
    }
    let Some(mut s) = conn.accept().await else {
        return;
    };
    let mut delivered = 0_usize;
    let mut audio_seen = false;
    let mut started = false;
    let mut order = 0_u64;
    let mut queue: VecDeque<String> = VecDeque::new();
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
        if !queue.is_empty() {
            if stall_until.is_none() && stall_applied.insert(delivered) {
                stall_until = stalls
                    .iter()
                    .find(|(n, _)| *n == delivered)
                    .map(|(_, d)| Instant::now() + *d);
            }
            if stall_until.is_none_or(|t| t <= Instant::now()) {
                stall_until = None;
                if let Some(text) = queue.pop_front() {
                    s.send(text).await;
                    delivered += 1;
                }
                continue;
            }
        }
        let wait = async {
            match stall_until {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            message = s.next() => match message {
                None => return,
                Some(What::Audio(audio)) => {
                    audio_seen = true;
                    if !(SERVER_MIN..=SERVER_MAX).contains(&audio.len()) {
                        s.close_with(3007, "Input duration violation").await;
                        return;
                    }
                    if !started {
                        started = true;
                        if let Some(u) = &utterance {
                            for interim in &u.interims {
                                queue.push_back(turn(order, interim, false, 100));
                            }
                        }
                    }
                }
                Some(What::Text(text)) if text == FORCE_ENDPOINT => {
                    if started {
                        queue.clear();
                        started = false;
                        if let Some(u) = &utterance {
                            for fin in &u.finals {
                                queue.push_back(turn(order, fin, true, 100));
                                order += 1;
                            }
                        }
                    }
                }
                Some(What::Text(text)) if text == TERMINATE => {
                    s.terminate().await;
                    return;
                }
                Some(What::Text(_)) => {}
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
// Live (BRISVOX_LIVE_TESTS=1 and ASSEMBLYAI_API_KEY)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn live_session_streams_audio_and_finalizes_without_errors() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and ASSEMBLYAI_API_KEY to run");
        return;
    }
    let Ok(key) = std::env::var("ASSEMBLYAI_API_KEY") else {
        eprintln!("skipped: ASSEMBLYAI_API_KEY is not set");
        return;
    };
    let mut cfg = AssemblyAiConfig::new(Secret::new(key));
    cfg.language = Some("en".to_owned());
    // Both waits long: an answer within 3 s can only have come from AssemblyAI itself, and the
    // adapter's own fallback (which ends the utterance anyway) would fire only after 8 s.
    cfg.finalize_timeout = Duration::from_secs(8);
    cfg.finalize_idle_timeout = Duration::from_secs(8);
    let stt = AssemblyAiStt::new(cfg, clock());
    stt.warm().await.expect("warm");
    let mut session = stt.connect(stt_cfg()).await.expect("connect");
    // Two seconds of a quiet 440 Hz tone at real-time pace in 20 ms frames (not speech: the
    // point is the protocol round trip, not recognition).
    let mut phase = 0.0_f32;
    for _ in 0..100 {
        let mut pcm = Vec::with_capacity(640);
        for _ in 0..320 {
            let sample = (phase.sin() * 800.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            phase += 2.0 * std::f32::consts::PI * 440.0 / 16_000.0;
        }
        session
            .input
            .send(SttInput::Audio(Bytes::from(pcm)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.input.send(SttInput::Finalize).await.unwrap();
    let finalize_sent = Instant::now();
    let mut answered_after = None;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), session.events.recv()).await {
            Ok(Some(SttEvent::Transcript {
                speech_final,
                from_finalize,
                ..
            })) => {
                if speech_final && from_finalize {
                    answered_after = Some(finalize_sent.elapsed());
                    break;
                }
            }
            Ok(Some(SttEvent::Error(e))) => panic!("live session failed: {e}"),
            Ok(None) | Err(_) => break,
        }
    }
    let answered_after = answered_after.expect("Finalize must always end the utterance");
    eprintln!(
        "live: Finalize ended the utterance after {answered_after:?} \
         (an answer under 3 s is AssemblyAI's own; docs/providers/assemblyai.md, UNVERIFIED 4)"
    );
}
