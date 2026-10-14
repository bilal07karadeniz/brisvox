//! A local mock of the Deepgram Aura TTS WebSocket (`/v1/speak`), speaking the wire protocol of
//! docs/providers/deepgram-aura.md. It runs on its own thread and runtime, so its tasks never
//! show up in the task counts of the contract suite and real sockets need no paused time.
//!
//! The audio of a `Speak` is one binary frame per word (`word_secs` of samples, every byte the
//! first byte of the word, so a test can tell which text a frame came from). Everything the
//! docs leave open is a switch: `auto` (generate when `Speak` arrives, or only on `Flush`),
//! `metadata_on_open`, `clear` (stop at once, or send the already queued audio first, which is
//! what the docs say), `answer_clear`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use brisvox_sim::contract::Fault;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// What the mock does when `Clear` arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClearMode {
    /// Drops everything it had queued (an ideal server).
    Honor,
    /// Keeps sending the audio it already queued, then `Cleared` (what the docs say).
    KeepSending,
}

/// One frame the mock sends.
#[derive(Clone, Debug)]
pub enum Out {
    Text(String),
    Binary(Vec<u8>),
    /// A close frame with this code and reason; the mock then ends the connection.
    Close(u16, String),
}

/// A custom reply to a client message: the frames to send, or `None` for the default reply.
pub type Responder = Arc<dyn Fn(&Value) -> Option<Vec<Out>> + Send + Sync>;

#[derive(Clone)]
pub struct MockConfig {
    pub api_key: String,
    /// Delay between two server frames.
    pub interval: Duration,
    /// Generate audio when a `Speak` arrives. Otherwise text waits for the `Flush`.
    pub auto: bool,
    /// Send `Metadata` right after the upgrade.
    pub metadata_on_open: bool,
    pub clear: ClearMode,
    /// Answer `Clear` with `Cleared`.
    pub answer_clear: bool,
    /// Seconds of audio per word.
    pub word_secs: f64,
    /// Split every audio frame into frames of this many bytes (odd sizes test alignment).
    pub frame_bytes: Option<usize>,
    /// Discard `Speak` text that arrives after a `Clear` and before the `Cleared` has been sent
    /// (the docs say the server may do this, "Clear" section of docs/providers/deepgram-aura.md).
    /// The discarded text shows up in the timeline as `drop:Speak:<text>`.
    pub discard_speak_until_cleared: bool,
    /// Reset every new connection (TCP reset, no close frame) as soon as the upgrade is done.
    pub reset_after_handshake: bool,
    pub responder: Option<Responder>,
}

#[derive(Clone, Debug)]
pub struct Handshake {
    pub uri: String,
    pub auth_ok: bool,
}

#[derive(Default)]
struct State {
    fault: Mutex<Option<Fault>>,
    config: Mutex<Option<MockConfig>>,
    received: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<Handshake>>,
    timeline: Mutex<Vec<(Instant, String)>>,
    connections: AtomicUsize,
    frames: AtomicUsize,
    pings: AtomicUsize,
    kick: Mutex<Option<watch::Sender<u64>>>,
    /// What the next kick does to the connections.
    kick_mode: Mutex<KickMode>,
}

/// How [`Mock::kick`] and its variants end the open connections.
#[derive(Clone, Debug, Default)]
enum KickMode {
    /// A close frame without a code (the server's idle timeout).
    #[default]
    Close,
    /// A close frame with this code and reason.
    CloseWith(u16, String),
    /// An abrupt end with a TCP reset: nothing is said to the client, its next write fails.
    Abort,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Mock {
    pub url: String,
    pub api_key: String,
    state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Mock {
    pub fn start() -> Self {
        let api_key = "test-key-not-a-secret".to_owned();
        let state = Arc::new(State::default());
        *lock(&state.config) = Some(MockConfig {
            api_key: api_key.clone(),
            interval: Duration::from_millis(2),
            auto: true,
            metadata_on_open: true,
            clear: ClearMode::KeepSending,
            answer_clear: true,
            word_secs: 0.2,
            frame_bytes: None,
            discard_speak_until_cleared: false,
            reset_after_handshake: false,
            responder: None,
        });
        let (kick_tx, _) = watch::channel(0_u64);
        *lock(&state.kick) = Some(kick_tx);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        let (stop_tx, stop_rx) = oneshot::channel();
        let thread_state = Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mock runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("listener");
                serve(listener, thread_state, stop_rx).await;
            });
        });
        Self {
            url: format!("ws://{addr}/v1/speak"),
            api_key,
            state,
            stop: Some(stop_tx),
            thread: Some(thread),
        }
    }

    pub fn configure(&self, change: impl FnOnce(&mut MockConfig)) {
        if let Some(config) = lock(&self.state.config).as_mut() {
            change(config);
        }
    }

    /// Faults apply to the next connection.
    pub fn inject(&self, fault: Fault) {
        *lock(&self.state.fault) = Some(fault);
    }

    pub fn reset(&self) {
        *lock(&self.state.fault) = None;
    }

    /// Every JSON frame the clients sent, in order.
    pub fn received(&self) -> Vec<Value> {
        lock(&self.state.received).clone()
    }

    /// The `type`s of the frames received, in order.
    pub fn received_types(&self) -> Vec<String> {
        self.received()
            .iter()
            .map(|v| v["type"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    /// What happened, in order: `recv:Speak:<text>`, `recv:Flush`, `recv:Clear`, `send:audio`,
    /// `send:Flushed`, `send:Cleared`, ... with the time of each.
    pub fn timeline(&self) -> Vec<(Instant, String)> {
        lock(&self.state.timeline).clone()
    }

    /// Just the labels of [`Self::timeline`].
    pub fn labels(&self) -> Vec<String> {
        self.timeline().into_iter().map(|(_, l)| l).collect()
    }

    pub fn handshakes(&self) -> Vec<Handshake> {
        lock(&self.state.handshakes).clone()
    }

    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// WebSocket pings the clients sent (the mock answers them, like any server).
    pub fn pings(&self) -> usize {
        self.state.pings.load(Ordering::SeqCst)
    }

    pub fn frames_sent(&self) -> usize {
        self.state.frames.load(Ordering::SeqCst)
    }

    /// Closes every open connection with a close frame (the server's idle timeout).
    pub fn kick(&self) {
        self.kick_with(KickMode::Close);
    }

    /// Closes every open connection with a close frame carrying `code` and `reason`.
    pub fn kick_with_close(&self, code: u16, reason: &str) {
        self.kick_with(KickMode::CloseWith(code, reason.to_owned()));
    }

    /// Drops every open connection with a TCP reset and no close frame: a client that is not
    /// reading learns of it only when it writes.
    pub fn kick_with_reset(&self) {
        self.kick_with(KickMode::Abort);
    }

    fn kick_with(&self, mode: KickMode) {
        *lock(&self.state.kick_mode) = mode;
        if let Some(tx) = lock(&self.state.kick).as_ref() {
            tx.send_modify(|n| *n += 1);
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn serve(listener: TcpListener, state: Arc<State>, mut stop: oneshot::Receiver<()>) {
    loop {
        tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(handle(stream, Arc::clone(&state)));
                }
            }
        }
    }
}

fn error_response(status: u16) -> ErrorResponse {
    http::Response::builder()
        .status(status)
        .body(Some(String::new()))
        .expect("response")
}

fn query_param(uri: &str, name: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

#[allow(clippy::too_many_lines)]
async fn handle(stream: TcpStream, state: Arc<State>) {
    // Subscribed before the handshake: a kick after the client connected must reach us.
    let mut kick = lock(&state.kick).as_ref().map(watch::Sender::subscribe);
    let fault = lock(&state.fault).take();
    state.connections.fetch_add(1, Ordering::SeqCst);
    if let Some(Fault::ConnectStall { duration }) = fault {
        tokio::time::sleep(duration).await;
    }
    let config = lock(&state.config).clone().expect("config");
    let reject = match fault {
        Some(Fault::Reject { status }) => Some(status),
        _ => None,
    };
    let handshake_state = Arc::clone(&state);
    let expected = format!("Token {}", config.api_key);
    let params = Arc::new(Mutex::new((16_000_u32, 2_usize)));
    let callback_params = Arc::clone(&params);
    #[allow(clippy::result_large_err)]
    let callback = move |request: &Request, response: Response| {
        let auth_ok = request
            .headers()
            .get("authorization")
            .is_some_and(|v| v == expected.as_str());
        let uri = request.uri().to_string();
        let rate = query_param(&uri, "sample_rate")
            .and_then(|v| v.parse().ok())
            .unwrap_or(24_000);
        let bps = if query_param(&uri, "encoding").as_deref() == Some("linear16") {
            2
        } else {
            1
        };
        *lock(&callback_params) = (rate, bps);
        lock(&handshake_state.handshakes).push(Handshake { uri, auth_ok });
        if let Some(status) = reject {
            return Err(error_response(status));
        }
        if !auth_ok {
            return Err(error_response(401));
        }
        Ok(response)
    };
    let Ok(mut ws) = accept_hdr_async(stream, callback).await else {
        return;
    };
    let (rate, bps) = *lock(&params);
    if config.reset_after_handshake {
        #[allow(deprecated)]
        let _ = ws.get_ref().set_linger(Some(Duration::ZERO));
        return;
    }

    let mut pending: VecDeque<Out> = VecDeque::new();
    let mut text_buf = String::new();
    let (mut sent, mut requests, mut sequence) = (0_usize, 0_usize, 0_u64);
    // A `Clear` was received and the `Cleared` has not been sent yet.
    let mut awaiting_cleared = false;
    if config.metadata_on_open {
        pending.push_back(Out::Text(
            json!({
                "type": "Metadata", "request_id": "00000000-0000-0000-0000-000000000000",
                "model_name": "mock", "model_version": "1", "model_uuid": "0"
            })
            .to_string(),
        ));
    }
    let mut next_at = Instant::now();
    loop {
        if let Some(Fault::Disconnect { after_events }) = fault
            && requests > 0
            && sent >= after_events
        {
            return; // abrupt: no close frame
        }
        let kicked = async {
            match kick.as_mut() {
                Some(rx) => {
                    let _ = rx.changed().await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = kicked => {
                let mode = lock(&state.kick_mode).clone();
                match mode {
                    KickMode::Close => {
                        let _ = ws.close(None).await;
                    }
                    KickMode::CloseWith(code, reason) => {
                        let _ = ws
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::from(code),
                                reason: reason.into(),
                            })))
                            .await;
                    }
                    KickMode::Abort => {
                        // Linger zero: dropping the socket sends a reset instead of a FIN.
                        #[allow(deprecated)]
                        let _ = ws.get_ref().set_linger(Some(Duration::ZERO));
                    }
                }
                return;
            }
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let Ok(request) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    requests += 1;
                    lock(&state.received).push(request.clone());
                    let kind = request["type"].as_str().unwrap_or("");
                    let label = if kind == "Speak" {
                        format!("recv:Speak:{}", request["text"].as_str().unwrap_or(""))
                    } else {
                        format!("recv:{kind}")
                    };
                    lock(&state.timeline).push((Instant::now(), label));
                    if let Some(responder) = &config.responder
                        && let Some(frames) = responder(&request)
                    {
                        pending.extend(frames);
                        continue;
                    }
                    match request["type"].as_str().unwrap_or("") {
                        "Speak" => {
                            let text = request["text"].as_str().unwrap_or("");
                            if awaiting_cleared && config.discard_speak_until_cleared {
                                lock(&state.timeline)
                                    .push((Instant::now(), format!("drop:Speak:{text}")));
                            } else if config.auto {
                                audio(&mut pending, text, rate, bps, &config);
                            } else {
                                text_buf.push_str(text);
                            }
                        }
                        "Flush" => {
                            if !config.auto {
                                let text = std::mem::take(&mut text_buf);
                                audio(&mut pending, &text, rate, bps, &config);
                            }
                            pending.push_back(Out::Text(
                                json!({"type": "Flushed", "sequence_id": sequence}).to_string(),
                            ));
                            sequence += 1;
                        }
                        "Clear" => {
                            text_buf.clear();
                            if config.clear == ClearMode::Honor {
                                pending.clear();
                            }
                            if config.answer_clear {
                                awaiting_cleared = true;
                                pending.push_back(Out::Text(
                                    json!({"type": "Cleared", "sequence_id": sequence}).to_string(),
                                ));
                                sequence += 1;
                            }
                        }
                        "Close" => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(Message::Ping(_))) => {
                    // tungstenite queues the pong and sends it on the next read.
                    state.pings.fetch_add(1, Ordering::SeqCst);
                }
                Some(Ok(_)) => {}
            },
            () = until(next_at), if !pending.is_empty() => {
                let Some(out) = pending.pop_front() else { continue };
                // `Metadata` is no TtsEvent: faults count events the adapter will surface.
                let counted = !matches!(&out, Out::Text(t) if t.contains("\"Metadata\""));
                if counted
                    && let Some(Fault::Stall { before_event, duration }) = fault
                    && before_event == sent
                {
                    tokio::time::sleep(duration).await;
                }
                let garbage = counted
                    && matches!(
                        fault,
                        Some(Fault::Protocol { after_events }) if after_events == sent
                    );
                let message = match out {
                    _ if garbage => Message::text("{this is not json"),
                    Out::Text(text) => Message::text(text),
                    Out::Binary(data) => Message::binary(data),
                    Out::Close(code, reason) => {
                        let _ = ws
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::from(code),
                                reason: reason.into(),
                            })))
                            .await;
                        return;
                    }
                };
                let label = match &message {
                    Message::Binary(_) => "send:audio".to_owned(),
                    Message::Text(t) => format!(
                        "send:{}",
                        serde_json::from_str::<Value>(t.as_str())
                            .ok()
                            .and_then(|v| v["type"].as_str().map(str::to_owned))
                            .unwrap_or_default()
                    ),
                    _ => "send:other".to_owned(),
                };
                if ws.send(message).await.is_err() {
                    return;
                }
                if label == "send:Cleared" {
                    awaiting_cleared = false;
                }
                lock(&state.timeline).push((Instant::now(), label));
                if counted {
                    sent += 1;
                }
                state.frames.fetch_add(1, Ordering::SeqCst);
                next_at = Instant::now() + config.interval;
            }
        }
    }
}

/// Queues one binary frame per word (split further if `frame_bytes` is set).
fn audio(pending: &mut VecDeque<Out>, text: &str, rate: u32, bps: usize, config: &MockConfig) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let samples = (f64::from(rate) * config.word_secs) as usize;
    for word in text.split_whitespace() {
        let fill = word.as_bytes().first().copied().unwrap_or(b'?');
        let data = vec![fill; samples * bps];
        match config.frame_bytes {
            Some(size) => {
                for part in data.chunks(size.max(1)) {
                    pending.push_back(Out::Binary(part.to_vec()));
                }
            }
            None => pending.push_back(Out::Binary(data)),
        }
    }
}

/// Waits until `at`; returns at once if it has passed (a timer round trip costs a whole tick).
async fn until(at: Instant) {
    if Instant::now() < at {
        tokio::time::sleep_until(at).await;
    }
}
