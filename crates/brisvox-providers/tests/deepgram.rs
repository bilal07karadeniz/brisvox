//! Deepgram STT adapter against a local mock of the Deepgram live protocol: wire format, result
//! mapping, finalize, keepalive, errors, reconnect with replay, the provider contract
//! suite, and one live test (`BRISVOX_LIVE_TESTS=1`).
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
use brisvox_providers::deepgram::{DeepgramConfig, DeepgramStt, Encoding};
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

const KEY: &str = "dg-test-key-0123456789";

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

impl Conn {
    /// Completes the WebSocket handshake.
    async fn accept(self) -> Option<Server> {
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
                .header("dg-error", "mock refusal")
                .header("dg-request-id", "req-1")
                .body(Some(
                    r#"{"err_code":"MOCK","err_msg":"refused"}"#.to_owned(),
                ))
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

/// A Deepgram `Results` message.
fn results(
    text: &str,
    is_final: bool,
    speech_final: bool,
    from_finalize: bool,
    start: f64,
    duration: f64,
) -> String {
    json!({
        "type": "Results",
        "channel_index": [0, 1],
        "duration": duration,
        "start": start,
        "is_final": is_final,
        "speech_final": speech_final,
        "from_finalize": from_finalize,
        "channel": {"alternatives": [{
            "transcript": text,
            "confidence": 0.99,
            "words": [{"word": text, "start": start, "end": start + duration, "confidence": 0.99}],
        }]},
        "metadata": {"request_id": "req-1", "model_info": {"name": "nova-3"}},
    })
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
fn config(server: &MockServer) -> DeepgramConfig {
    let mut c = DeepgramConfig::new(Secret::new(KEY.to_owned()));
    c.base_url = Some(server.base_url());
    c.io_timeout = Duration::from_secs(2);
    // Longer than any test waits, but below the 10 s the config accepts. (Longer than
    // io_timeout, too: tests that wait for KeepAlive set their own values.)
    c.keepalive_interval = Duration::from_secs(9);
    c.finalize_timeout = Duration::from_millis(250);
    c.close_timeout = Duration::from_millis(300);
    c.reconnect_backoff = Duration::from_millis(10);
    c.replay_speed = 1000.0;
    c
}

fn adapter(server: &MockServer, tweak: impl FnOnce(&mut DeepgramConfig)) -> DeepgramStt {
    let mut c = config(server);
    tweak(&mut c);
    DeepgramStt::new(c, clock())
}

async fn connect(stt: &DeepgramStt) -> SttSession {
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

// ---------------------------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_handshake_carries_token_auth_and_the_documented_query() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
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
        "/v1/listen?model=nova-3&language=tr&encoding=linear16&sample_rate=16000\
         &interim_results=true&endpointing=300&smart_format=true"
    );
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Token {KEY}").as_str())
    );
    assert!(
        !requests[0].uri.contains(KEY),
        "the key must not be in the URL"
    );
}

#[tokio::test]
async fn mulaw_telephony_audio_is_announced_with_its_encoding_and_rate() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.encoding = Encoding::Mulaw);
    let _session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: Some("en".into()),
        })
        .await
        .unwrap();
    let uri = server.requests()[0].uri.clone();
    assert!(uri.contains("encoding=mulaw&sample_rate=8000"), "{uri}");
}

#[tokio::test]
async fn audio_goes_out_as_binary_frames_in_order_and_unchanged() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let session = connect(&stt).await;
    for fill in 1..=3 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    server
        .log
        .wait("3 audio frames", |l| l.audio(0).len() == 3)
        .await;
    let audio = server.log.audio(0);
    for (i, frame) in audio.iter().enumerate() {
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
        for msg in [
            results("mer", false, false, false, 0.0, 0.3),
            results("merhaba", false, false, false, 0.0, 0.6),
            json!({"type": "SpeechStarted", "channel": [0, 1], "timestamp": 0.1}).to_string(),
            json!({"type": "Metadata", "request_id": "r", "duration": 0.0, "channels": 0})
                .to_string(),
            json!({"type": "FutureThing", "x": 1}).to_string(),
            results("", false, false, false, 0.6, 0.1),
            results("merhaba nasılsın", true, false, false, 0.0, 1.0),
            results("iyiyim", false, false, false, 1.0, 0.5),
            results("iyiyim", true, true, false, 1.0, 0.7),
            results("", true, true, false, 1.7, 0.4),
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
    for _ in 0..5 {
        got.push(flat(event(&mut session).await));
    }
    assert_eq!(
        got,
        [
            t("mer", false, false, false),
            t("merhaba", false, false, false),
            t("merhaba nasılsın", true, false, false),
            t("iyiyim", false, false, false),
            t("iyiyim", true, true, false),
        ]
    );
    // The trailing blank speech_final closes nothing (the utterance already ended): no event.
    let extra = tokio::time::timeout(Duration::from_millis(200), session.events.recv()).await;
    assert!(extra.is_err(), "unexpected event {extra:?}");
}

/// A final `Results` message of `language=multi`: every word carries its language.
fn multi_results(text: &str, word_languages: &[&str], speech_final: bool) -> String {
    let words: Vec<_> = text
        .split_whitespace()
        .zip(word_languages)
        .map(|(word, language)| json!({"word": word, "language": language, "confidence": 0.9}))
        .collect();
    json!({
        "type": "Results",
        "is_final": true,
        "speech_final": speech_final,
        "from_finalize": false,
        "start": 0.0,
        "duration": 1.0,
        "channel": {"alternatives": [{
            "transcript": text,
            "confidence": 0.95,
            "languages": ["tr", "en"],
            "words": words,
        }]},
    })
    .to_string()
}

#[tokio::test]
async fn multi_language_asks_for_hints_and_reports_the_language_of_final_segments() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // A Turkish sentence with an English word (code-switching), then an English one.
        s.send(&multi_results(
            "bana order durumunu göster",
            &["tr", "en", "tr", "tr"],
            false,
        ))
        .await;
        s.send(&multi_results(
            "where is my order",
            &["en", "en", "en", "en"],
            true,
        ))
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.language = Some("multi".to_owned());
        c.language_hints = vec!["en".to_owned(), "tr".to_owned()];
    });
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: None,
        })
        .await
        .unwrap();
    session.input.send(chunk(1)).await.unwrap();
    let uri = server.requests()[0].uri.clone();
    assert!(uri.contains("&language=multi&"), "{uri}");
    assert!(uri.ends_with("&language_hint=en&language_hint=tr"), "{uri}");
    let mut got = Vec::new();
    for _ in 0..2 {
        match event(&mut session).await {
            SttEvent::Transcript { text, language, .. } => {
                got.push((text.to_string(), language.map(|l| l.code.to_string())));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        got,
        [
            (
                "bana order durumunu göster".to_owned(),
                Some("tr".to_owned())
            ),
            ("where is my order".to_owned(), Some("en".to_owned())),
        ]
    );
}

#[tokio::test]
async fn a_monolingual_session_reports_no_language() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(&results("merhaba", true, true, false, 0.0, 1.0))
            .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    match event(&mut session).await {
        SttEvent::Transcript { language, .. } => assert!(language.is_none()),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn results_without_a_type_field_parse_like_the_documented_example() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(
            r#"{"channel_index":[0,1],"duration":1.039875,"start":0,"is_final":false,
            "channel":{"alternatives":[{"transcript":"another big","confidence":0.96,"words":[]}]}}"#,
        )
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("another big", false, false, false)
    );
}

#[tokio::test]
async fn utterance_end_closes_an_utterance_endpointing_did_not() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(results("hello there", false, false, false, 0.0, 1.0))
            .await;
        // -1: already finalized, ignored. Then the real one.
        s.send(json!({"type": "UtteranceEnd", "channel": [0, 1], "last_word_end": -1}).to_string())
            .await;
        s.send(
            json!({"type": "UtteranceEnd", "channel": [0, 1], "last_word_end": 1.0}).to_string(),
        )
        .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.utterance_end_ms = Some(1000));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello there", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("hello there", true, true, false)
    );
    assert!(server.requests()[0].uri.contains("utterance_end_ms=1000"));
}

#[tokio::test]
async fn speech_final_on_a_non_final_result_is_handled_in_both_readings() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Reading 1: finals came first; the flag closes the utterance.
        s.send(results("first part", true, false, false, 0.0, 1.0))
            .await;
        s.send(results("", false, true, false, 1.0, 0.3)).await;
        // Reading 2: the flag precedes the final, which inherits it.
        s.send(results("", false, true, false, 1.3, 0.2)).await;
        s.send(results("second", true, false, false, 1.3, 0.5))
            .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("first part", true, false, false)
    );
    assert_eq!(flat(event(&mut session).await), t("", true, true, false));
    assert_eq!(
        flat(event(&mut session).await),
        t("second", true, true, false)
    );
}

#[tokio::test]
async fn interims_are_dropped_when_the_consumer_lags_but_finals_are_not() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        for i in 0..400 {
            s.send(results(&format!("word {i}"), false, false, false, 0.0, 0.5))
                .await;
        }
        s.send(results("the end", true, true, false, 0.0, 1.0))
            .await;
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
            assert_eq!((text.as_str(), speech_final), ("the end", true));
            break;
        }
        interims += 1;
    }
    assert!(
        (1..400).contains(&interims),
        "some interims survive, the flood does not: {interims}"
    );
}

// ---------------------------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_sends_the_documented_message_and_the_answer_ends_the_utterance() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(results("tamam", false, false, false, 0.0, 0.5))
            .await;
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == r#"{"type":"Finalize"}"# => {
                    // The documentation does not say whether speech_final is set: it is not.
                    s.send(results("tamam evet", true, false, true, 0.0, 0.9))
                        .await;
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
        t("tamam evet", true, true, true)
    );
    assert_eq!(server.log.texts(0), [r#"{"type":"Finalize"}"#]);
}

#[tokio::test]
async fn an_unanswered_finalize_ends_with_a_synthetic_final_after_the_timeout() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.send(results("partial text", false, false, false, 0.0, 0.5))
                .await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    // With an interim heard: it becomes the final.
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
    // Nothing heard at all: empty text, still ends the utterance.
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
}

// ---------------------------------------------------------------------------------------------
// Keepalive and silence
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn keepalive_is_sent_when_no_audio_flows_and_not_while_it_does() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(150)
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
    assert!(
        server
            .log
            .texts(0)
            .iter()
            .all(|t| t == r#"{"type":"KeepAlive"}"#)
    );
    // The first KeepAlive came a keepalive interval after the last audio.
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
}

#[tokio::test]
async fn a_connection_that_never_hears_audio_gets_20ms_of_silence_first() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100)
    });
    let _linear = connect(&stt).await;
    server
        .log
        .wait("silence and a KeepAlive", |l| {
            !l.audio(0).is_empty() && !l.texts(0).is_empty()
        })
        .await;
    let silence = server.log.audio(0);
    assert_eq!(silence.len(), 1, "silence once, then KeepAlive");
    assert_eq!(silence[0].as_ref(), vec![0_u8; 640].as_slice());
    // The server saw the audio before the first KeepAlive.
    assert!(matches!(server.log.all()[0].what, What::Audio(_)));

    // mu-law: 20 ms at 8 kHz is 160 bytes of 0xFF.
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.encoding = Encoding::Mulaw;
    });
    let _mulaw = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: None,
        })
        .await
        .unwrap();
    server
        .log
        .wait("mulaw silence", |l| !l.audio(1).is_empty())
        .await;
    assert_eq!(
        server.log.audio(1)[0].as_ref(),
        vec![0xFF_u8; 160].as_slice()
    );
}

#[tokio::test]
async fn the_response_watchdog_is_off_during_a_keepalive_pause() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    // The server never says anything. Audio, then a pause longer than io_timeout: because the
    // adapter has switched to KeepAlive, that silence is expected and no error follows.
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.io_timeout = Duration::from_millis(400);
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let idle = tokio::time::timeout(Duration::from_millis(900), session.events.recv()).await;
    assert!(idle.is_err(), "no event expected during a pause: {idle:?}");
    // And audio can follow.
    session.input.send(chunk(2)).await.unwrap();
    server
        .log
        .wait("the second audio frame", |l| l.audio(0).len() >= 2)
        .await;
}

// ---------------------------------------------------------------------------------------------
// Hang-up
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hang_up_sends_close_stream_closes_events_at_once_and_drops_late_results() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == r#"{"type":"CloseStream"}"# => {
                    // Per the docs: flush the remaining audio as final results, then Metadata.
                    s.send(results("late words", true, true, false, 0.0, 1.0))
                        .await;
                    s.send(json!({"type": "Metadata", "request_id": "r"}).to_string())
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
        .wait("CloseStream", |l| {
            l.texts(0) == [r#"{"type":"CloseStream"}"#.to_owned()]
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// Errors
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
            matches!(err, Error::Rejected { status, provider } if status == code && provider == "deepgram"),
            "{code}: {err:?}"
        );
        assert!(!format!("{err} {err:?}").contains(KEY));
    }
}

#[tokio::test]
async fn close_1008_means_undecodable_audio_and_is_not_retried() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.close_with(1008, "DATA-0000").await;
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 3);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 1008, .. }
    ));
    expect_closed(&mut session).await;
    assert_eq!(server.connections(), 1, "a retry would fail the same way");
}

#[tokio::test]
async fn a_schema_error_or_garbage_is_a_protocol_error_that_ends_the_session() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            s.send(
                json!({"type": "Error", "variant": "SchemaError", "description": "bad message",
                       "message": "the client text"})
                .to_string(),
            )
            .await;
        } else {
            s.send("this is not json").await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    match expect_error(&mut session).await {
        Error::Protocol { detail, .. } => {
            assert!(detail.contains("SchemaError") && detail.contains("bad message"));
            assert!(!detail.contains("the client text"));
        }
        other => panic!("{other:?}"),
    }
    expect_closed(&mut session).await;

    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    match expect_error(&mut session).await {
        Error::Protocol { detail, .. } => assert!(!detail.contains("this is not json")),
        other => panic!("{other:?}"),
    }
    expect_closed(&mut session).await;
}

#[tokio::test]
async fn other_server_error_messages_are_non_fatal_as_documented() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        // Deepgram: "Error (non-fatal; fatal failures close the socket)". A variant the adapter
        // does not know must not kill a healthy call.
        s.send(
            json!({"type": "Error", "variant": "SomethingNew", "description": "careful",
                   "code": "X"})
            .to_string(),
        )
        .await;
        s.send(results("still here", false, false, false, 0.0, 0.1))
            .await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("still here", false, false, false)
    );
    // A rejected Configure is the adapter's own fault, like a SchemaError: fatal.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(
            json!({"type": "Error", "variant": "InvalidConfigureMessage", "description": "no"})
                .to_string(),
        )
        .await;
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
}

#[tokio::test]
async fn invalid_settings_fail_before_any_connection_is_made() {
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let bad_model = adapter(&server, |c| c.model = "nova-3&x=1".to_owned());
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
    // KeepAlive slower than the server's 10 s no-audio close can never keep a connection alive.
    let slow_keepalive = adapter(&server, |c| c.keepalive_interval = Duration::from_secs(10));
    assert!(matches!(
        slow_keepalive.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(server.connections(), 0);
}

#[test]
fn the_provider_debug_output_never_contains_the_key() {
    let c = DeepgramConfig::new(Secret::new(KEY.to_owned()));
    let stt = DeepgramStt::new(c, clock());
    assert!(!format!("{stt:?}").contains(KEY));
    assert_eq!(stt.name(), "deepgram");
}

// ---------------------------------------------------------------------------------------------
// Reconnect with replay
// ---------------------------------------------------------------------------------------------

fn fills(frames: &[Bytes]) -> Vec<u8> {
    frames.iter().map(|f| f[0]).collect()
}

#[tokio::test]
async fn a_dropped_connection_is_replaced_and_only_unfinalized_audio_is_replayed() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        match s.index {
            0 => {
                s.until_audio(2).await;
                // Chunk 1 (0.0-0.1 s) is final.
                s.send(results("one", true, false, false, 0.0, 0.1)).await;
                s.until_audio(4).await;
                // Drop without a closing handshake.
            }
            1 => {
                // Replay of chunks 2, 3, 4.
                s.until_audio(3).await;
                // Timestamps restart at 0 on a new connection: 0.0-0.2 s is chunks 2 and 3.
                s.send(results("two", true, false, false, 0.0, 0.2)).await;
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
        t("one", true, false, false)
    );
    session.input.send(chunk(3)).await.unwrap();
    session.input.send(chunk(4)).await.unwrap();
    // Reconnect happens silently: the next thing is the final of the replayed audio.
    assert_eq!(
        flat(event(&mut session).await),
        t("two", true, false, false)
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
    // All three connections used the same URL and credentials.
    let requests = server.requests();
    assert!(requests.iter().all(|r| r.uri == requests[0].uri));
    assert!(
        requests
            .iter()
            .all(|r| r.authorization.as_deref() == Some(format!("Token {KEY}").as_str()))
    );
}

#[tokio::test]
async fn timestamps_are_offset_by_the_silence_the_adapter_inserted() {
    // The first connection hears 20 ms of generated silence (640 bytes), then chunk 7 (3200
    // bytes). The server finalizes 0.07 s of its stream: 2240 bytes, of which 640 are the
    // silence, so 1600 bytes of the caller's audio are final and 1600 must be replayed.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            s.until_audio(2).await;
            s.send(results("half", true, false, false, 0.0, 0.07)).await;
            // Give the client time to process the final, then drop.
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.max_reconnects = 1;
    });
    let mut session = connect(&stt).await;
    server.log.wait("silence", |l| l.audio(0).len() == 1).await;
    session.input.send(chunk(7)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("half", true, false, false)
    );
    server.log.wait("replay", |l| !l.audio(1).is_empty()).await;
    let replay = server.log.audio(1);
    assert_eq!(replay.iter().map(Bytes::len).sum::<usize>(), 1600);
    assert!(replay.iter().all(|f| f.iter().all(|b| *b == 7)));
}

#[tokio::test]
async fn a_pending_finalize_is_sent_again_after_the_replay() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        if s.index == 0 {
            // Takes the audio and the Finalize, answers nothing, and dies.
            loop {
                if let Some(What::Text(_)) = s.next().await {
                    return;
                }
            }
        }
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == r#"{"type":"Finalize"}"# => {
                    s.send(results("answered", true, false, true, 0.0, 0.2))
                        .await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("answered", true, true, true)
    );
    // Connection 1 heard the replayed audio first and then the Finalize.
    let seq: Vec<String> = server
        .log
        .all()
        .iter()
        .filter(|s| s.conn == 1)
        .map(|s| match &s.what {
            What::Audio(b) => format!("audio{}", b[0]),
            What::Text(t) => t.clone(),
        })
        .collect();
    assert_eq!(seq, ["audio1", "audio2", r#"{"type":"Finalize"}"#]);
}

#[tokio::test]
async fn reconnecting_gives_up_after_the_configured_attempts_with_disconnected() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
            s.close_with(1011, "NET-0001").await;
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
            s.close_with(1011, "NET-0001").await;
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
    // Attempt n waits `base * 2^n` after the previous connection (timers never fire early; the
    // small allowance is for the clock granularity of the platform).
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
        s.close_with(1011, "NET-0000").await;
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
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            s.until_audio(1).await;
        } else {
            conn.reject(401).await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 5);
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 401, .. }
    ));
    assert_eq!(server.connections(), 2, "a bad key is not retried");
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
        c.reconnect_backoff = Duration::from_millis(300);
    });
    let session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    // Wait until the session is in the backoff after the refused attempt: from here on only the
    // reconnect loop can be reading the input.
    server
        .log
        .wait("first audio", |l| l.audio(0).len() == 1)
        .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.connections() < 2 {
        assert!(Instant::now() < deadline, "no reconnect attempt");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    // More than the input channel holds (64): a session that does not read its input while it
    // reconnects would block the caller here for the rest of the backoff (600 ms).
    let burst = async {
        for fill in 2..=71_u8 {
            session.input.send(chunk(fill)).await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_millis(300), burst)
        .await
        .expect("the caller was blocked while the session reconnected");
    server
        .log
        .wait("replay on connection 2", |l| l.audio(2).len() == 71)
        .await;
    assert_eq!(fills(&server.log.audio(2)), (1..=71).collect::<Vec<u8>>());
    // The attempt after the refusal waited its (doubled) backoff.
    let at = server.accepted_at();
    assert!(at[2] - at[1] >= Duration::from_millis(540));
}

/// Mock for the ring-trim tests: connection 0 takes one chunk and a Finalize, answers it with a
/// `from_finalize` result that reports far less audio than was sent (`answer`), or says nothing,
/// and closes when the test says so. Later connections just listen.
fn finalize_then_drop_server(
    answer: bool,
    drop_now: Arc<tokio::sync::Notify>,
) -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    handler(move |conn| {
        let drop_now = Arc::clone(&drop_now);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            if s.index != 0 {
                s.drain().await;
                return;
            }
            loop {
                tokio::select! {
                    () = drop_now.notified() => {
                        s.close_with(1011, "NET-0001").await;
                        return;
                    }
                    seen = s.next() => match seen {
                        Some(What::Text(t)) if answer && t == r#"{"type":"Finalize"}"# => {
                            // Reports 10 ms of audio only: the ordinary trim at the end of a
                            // final would leave almost the whole chunk in the ring.
                            s.send(results("hello", true, true, true, 0.0, 0.01)).await;
                        }
                        Some(_) => {}
                        None => return,
                    },
                }
            }
        }
    })
}

/// After the Finalize is over, a dropped connection must not replay the audio it flushed.
async fn finalized_audio_is_not_replayed(answer: bool) {
    let drop_now = Arc::new(tokio::sync::Notify::new());
    let server = MockServer::start(finalize_then_drop_server(answer, Arc::clone(&drop_now))).await;
    // Plenty of time for the real answer; the synthetic one comes after 150 ms.
    let stt = adapter(&server, |c| {
        c.finalize_timeout = Duration::from_millis(150);
        c.finalize_settle_gap = Duration::ZERO;
    });
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    let expected = if answer {
        t("hello", true, true, true)
    } else {
        t("", true, true, true)
    };
    assert_eq!(flat(event(&mut session).await), expected);
    server
        .log
        .wait("the Finalize", |l| {
            l.texts(0).iter().any(|t| t.contains("Finalize"))
        })
        .await;
    drop_now.notify_one();
    session.input.send(chunk(2)).await.unwrap();
    server
        .log
        .wait("the replay", |l| !l.audio(1).is_empty())
        .await;
    // Give a (wrongly) replayed chunk 1 time to show up.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replay = server.log.audio(1);
    assert!(
        replay.iter().all(|f| f.iter().all(|b| *b == 2)),
        "audio finalized on the first connection was replayed: {:?}",
        fills(&replay)
    );
    assert_eq!(replay.iter().map(Bytes::len).sum::<usize>(), 3200);
    assert!(
        !server.log.texts(1).iter().any(|t| t.contains("Finalize")),
        "a finished Finalize was sent again"
    );
}

#[tokio::test]
async fn an_answered_finalize_trims_the_ring_to_where_it_was_sent() {
    finalized_audio_is_not_replayed(true).await;
}

#[tokio::test]
async fn a_timed_out_finalize_trims_the_ring_to_where_it_was_sent() {
    finalized_audio_is_not_replayed(false).await;
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
    // Nine gaps of 100 ms of audio each: 900 ms / speed. Sending is never faster than the
    // speed (Deepgram ingests at most 1.25x real time), so the lower bound is tight; the upper
    // bound only catches a replay that crawls.
    for (speed, expected_ms) in [(4.0_f64, 225_u64), (1.25, 720)] {
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
            s.until_audio(10).await;
        } else {
            s.drain().await;
        }
    }))
    .await;
    // 250 ms = 8000 bytes of ring; the caller's chunks are 3201 bytes (a frame split inside a
    // 16-bit sample). Eviction cuts whole chunks, which would leave the replay on an odd byte.
    let stt = adapter(&server, |c| c.replay_window = Duration::from_millis(250));
    let session = connect(&stt).await;
    for fill in 1..=10_u8 {
        session
            .input
            .send(SttInput::Audio(Bytes::from(vec![fill; 3201])))
            .await
            .unwrap();
    }
    server.log.wait("replay", |l| l.audio(1).len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replayed: usize = server.log.audio(1).iter().map(Bytes::len).sum();
    // Ten chunks are 32010 bytes, an even total: an even replay length means an even start.
    assert_eq!(
        replayed % 2,
        0,
        "the replay began inside a sample ({replayed} bytes)"
    );
    assert!(replayed >= 4000, "{replayed}");
}

// ---------------------------------------------------------------------------------------------
// Silent servers, anti-flap
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_goes_silent_while_audio_was_sent_is_replaced_not_fatal() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        if s.index == 0 {
            // A half-open connection: nothing ever comes back, no close frame either.
            s.drain().await;
        } else {
            s.send(results("back again", false, false, false, 0.0, 0.1))
                .await;
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
    let silent = || {
        handler(|conn| async move {
            if let Some(mut s) = conn.accept().await {
                s.drain().await;
            }
        })
    };
    // One reconnect: the replacement is silent as well.
    let server = MockServer::start(silent()).await;
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
    let server = MockServer::start(silent()).await;
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
async fn a_flapping_server_that_only_sends_metadata_is_not_reconnected_forever() {
    // Every connection: one message that is not a transcript, then a 1011 close.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(json!({"type": "Metadata", "request_id": "r"}).to_string())
            .await;
        s.close_with(1011, "NET-0000").await;
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
async fn a_transcript_message_proves_the_connection_and_restores_the_reconnect_budget() {
    // Every connection answers once and then drops: with a budget of one the session survives
    // several drops, because each answer resets the count.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(results(
            &format!("c{}", s.index),
            false,
            false,
            false,
            0.0,
            0.1,
        ))
        .await;
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

// ---------------------------------------------------------------------------------------------
// Finalize: bounds and races
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_finalize_bound_holds_while_a_reconnect_is_in_the_way() {
    let server = MockServer::start(handler(|conn| async move {
        if conn.index == 0 {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            // Takes the audio and the Finalize and dies without answering.
            loop {
                if let Some(What::Text(_)) = s.next().await {
                    return;
                }
            }
        }
        // The replacement connection takes far longer than the finalize bound to come up.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    let started = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_millis(1200),
        "the synthetic final waited for the reconnect: {waited:?}"
    );
}

#[tokio::test]
async fn a_late_answer_after_the_synthetic_final_is_deduplicated_or_delivered() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(1).await;
        s.send(results("merhaba", false, false, false, 0.0, 0.1))
            .await;
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == r#"{"type":"Finalize"}"# => {
                    // Slower than finalize_timeout.
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let text = if s.index == 0 {
                        "Merhaba."
                    } else {
                        "merhaba dünya"
                    };
                    s.send(results(text, true, false, true, 0.0, 0.1)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |_| {});
    // Same words: the server's copy of what the synthetic final already said.
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", true, true, true)
    );
    let extra = tokio::time::timeout(Duration::from_millis(800), session.events.recv()).await;
    assert!(extra.is_err(), "a duplicate utterance end: {extra:?}");
    // Different words: real text the synthetic final missed, delivered as a late end.
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", true, true, true)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dünya", true, true, true)
    );
}

#[tokio::test]
async fn an_endpointed_final_during_a_finalize_keeps_its_own_label() {
    // "hello" is endpointed just as the Finalize goes out; the Finalize's answer for the audio
    // after it ("world") arrives afterwards. Neither may be mistaken for the other.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(5).await;
        loop {
            match s.next().await {
                Some(What::Text(t)) if t == r#"{"type":"Finalize"}"# => {
                    s.send(results("hello", true, true, false, 0.0, 0.1)).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    s.send(results("world", true, false, true, 0.1, 0.4)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.finalize_timeout = Duration::from_secs(3);
        c.finalize_settle_gap = Duration::from_millis(50);
    });
    let mut session = connect(&stt).await;
    for fill in 1..=5 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", true, true, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("world", true, true, true)
    );
}

#[tokio::test]
async fn a_final_that_reaches_the_finalize_position_answers_it_without_waiting() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(2).await;
        loop {
            match s.next().await {
                // The endpointed final of all the audio sent so far, no from_finalize.
                Some(What::Text(t)) if t == r#"{"type":"Finalize"}"# => {
                    s.send(results("done", true, true, false, 0.0, 0.2)).await;
                }
                Some(_) => {}
                None => return,
            }
        }
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("done", true, true, true));
}

#[tokio::test]
async fn a_finalize_with_nothing_left_to_flush_is_answered_at_once() {
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(2).await;
        // Endpointing fired for everything heard so far.
        s.send(results("hello", true, true, false, 0.0, 0.2)).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(3));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", true, true, false)
    );
    let started = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "waited for the finalize timeout: {:?}",
        started.elapsed()
    );
    // Nothing to flush, so nothing was sent.
    assert!(server.log.texts(0).is_empty(), "{:?}", server.log.texts(0));

    // The shortcut is off when the unreported audio is longer than the gap.
    let server = MockServer::start(handler(|conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        s.until_audio(2).await;
        s.send(results("hello", true, true, false, 0.0, 0.2)).await;
        s.until_audio(6).await;
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    session.input.send(chunk(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", true, true, false)
    );
    // 600 ms more audio the server has not reported on: more than the 500 ms default gap.
    for fill in 3..=8 {
        session.input.send(chunk(fill)).await.unwrap();
    }
    let started = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(server.log.texts(0), [r#"{"type":"Finalize"}"#]);
}

// ---------------------------------------------------------------------------------------------
// Explicit timeouts
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
async fn a_peer_that_never_answers_the_close_is_given_up_on_after_close_timeout() {
    let ended: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&ended);
    let server = MockServer::start(handler(move |conn| {
        let seen = Arc::clone(&seen);
        async move {
            let Some(mut s) = conn.accept().await else {
                return;
            };
            // Reads everything, including CloseStream, and never answers or closes.
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
        .wait("CloseStream", |l| !l.texts(0).is_empty())
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
            s.send(results(
                &format!("segment {i}"),
                true,
                false,
                false,
                0.0,
                0.1,
            ))
            .await;
        }
        s.drain().await;
    }))
    .await;
    let stt = adapter(&server, |c| c.io_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(chunk(1)).await.unwrap();
    // The caller does not read. The adapter waits for io_timeout, then treats it as gone: it
    // stops, tells the server (CloseStream) and closes the channel after what it had queued.
    server
        .log
        .wait("CloseStream", |l| !l.texts(0).is_empty())
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
    let server = MockServer::start(handler(|conn| async move {
        if let Some(mut s) = conn.accept().await {
            s.drain().await;
        }
    }))
    .await;
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
    provider: Arc<DeepgramStt>,
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
        }));
        Self {
            server,
            script,
            provider,
        }
    }
}

impl SttHarness for Harness {
    type P = DeepgramStt;

    fn provider(&self) -> Arc<DeepgramStt> {
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

fn contract_results(text: &str, is_final: bool, from_finalize: bool) -> String {
    // Deliberately without speech_final: the adapter must set it on a from_finalize result.
    results(text, is_final, false, from_finalize, 0.0, 0.04)
}

/// The scripted Deepgram of the contract suite: interims on the first audio, finals on
/// `Finalize`, plus the injected faults.
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
                Some(What::Audio(_)) => {
                    audio_seen = true;
                    if !started {
                        started = true;
                        if let Some(u) = &utterance {
                            for interim in &u.interims {
                                queue.push_back(contract_results(interim, false, false));
                            }
                        }
                    }
                }
                Some(What::Text(text)) if text.contains("Finalize") => {
                    if started {
                        queue.clear();
                        started = false;
                        if let Some(u) = &utterance {
                            for fin in &u.finals {
                                queue.push_back(contract_results(fin, true, true));
                            }
                        }
                    }
                }
                Some(What::Text(text)) if text.contains("CloseStream") => {
                    s.send(json!({"type": "Metadata", "request_id": "r"}).to_string()).await;
                    s.close_with(1000, "").await;
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
// Live (BRISVOX_LIVE_TESTS=1 and DEEPGRAM_API_KEY)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn live_session_streams_audio_and_finalizes_without_errors() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and DEEPGRAM_API_KEY to run");
        return;
    }
    let Ok(key) = std::env::var("DEEPGRAM_API_KEY") else {
        eprintln!("skipped: DEEPGRAM_API_KEY is not set");
        return;
    };
    let mut cfg = DeepgramConfig::new(Secret::new(key));
    cfg.language = Some("en".to_owned());
    // The adapter synthesizes a `speech_final && from_finalize` answer of its own when Deepgram
    // stays silent (after `finalize_timeout`) or when the recognizer looks idle
    // (`finalize_settle_gap`). Both are disabled / pushed out here, so only a real
    // `from_finalize` answer from Deepgram arrives within the asserted time.
    cfg.finalize_timeout = Duration::from_secs(8);
    cfg.finalize_settle_gap = Duration::ZERO;
    let stt = DeepgramStt::new(cfg, clock());
    stt.warm().await.expect("warm");
    let mut session = stt.connect(stt_cfg()).await.expect("connect");
    // Two seconds of a quiet 440 Hz tone at real-time pace in 20 ms chunks (not speech: the
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
    let mut saw_speech_final = false;
    let mut answered_after = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), session.events.recv()).await {
            Ok(Some(SttEvent::Transcript {
                speech_final,
                from_finalize,
                ..
            })) => {
                if speech_final && from_finalize {
                    saw_speech_final = true;
                    answered_after = finalize_sent.elapsed();
                    break;
                }
            }
            Ok(Some(SttEvent::Error(e))) => panic!("live session failed: {e}"),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_speech_final, "Finalize must always end the utterance");
    eprintln!("live: Deepgram answered Finalize after {answered_after:?}");
    // finalize_timeout is 8 s, so an answer within 3 s can only have come from Deepgram
    // (docs/providers/deepgram.md, UNVERIFIED items 2 and 3).
    assert!(
        answered_after < Duration::from_secs(3),
        "Deepgram did not answer Finalize with a from_finalize result in time ({answered_after:?});          the adapter's own fallback would have fired only after 8 s"
    );
}
