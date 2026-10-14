//! A local mock of the Cartesia TTS WebSocket (`/tts/websocket`), speaking the wire protocol of
//! docs/providers/cartesia.md. It runs on its own thread and runtime, so its tasks never show
//! up in the task counts of the contract suite and real sockets need no paused time.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine as _;
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

/// What the mock does when a cancel arrives for a context it is still generating.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelMode {
    /// Stops generating (the ideal server).
    Honor,
    /// Keeps sending everything it already queued (what the docs say a generating request does).
    KeepSending,
}

/// Which time base the mock's word timestamps use for continuation chunks (UNVERIFIED upstream).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeBase {
    /// Seconds from the first sample of the context.
    Context,
    /// Seconds from the first sample of each request's own audio.
    Chunk,
}

/// Where the mock puts the `timestamps` of a text chunk relative to the chunk's `flush_done`
/// (the docs leave the order open).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TsOrder {
    /// Audio, `timestamps`, then `flush_done`.
    BeforeFlush,
    /// Audio, `flush_done`, then `timestamps`.
    AfterFlush,
}

/// What the mock does to a context that has been quiet for the expiry (the docs: a context
/// expires after its last audio; input on it is "not supported", the reaction is unknown).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpiryMode {
    /// The next request on the context is answered with an `error` message.
    Error,
    /// The server sends an unsolicited `done` when the context expires and ignores later input.
    Done,
}

/// A custom reply to a request: the raw JSON frames to send, or `None` for the default reply.
pub type Responder = Arc<dyn Fn(&Value) -> Option<Vec<String>> + Send + Sync>;

#[derive(Clone)]
pub struct MockConfig {
    pub api_key: String,
    /// Delay between two server frames.
    pub interval: Duration,
    pub cancel: CancelMode,
    pub time_base: TimeBase,
    /// Seconds of audio per word.
    pub word_secs: f64,
    pub ts_order: TsOrder,
    /// Contexts expire this long after the last audio the mock sent for them.
    pub expiry: Option<(Duration, ExpiryMode)>,
    pub responder: Option<Responder>,
}

#[derive(Clone, Debug)]
pub struct Handshake {
    pub uri: String,
    pub key_ok: bool,
}

#[derive(Default)]
struct State {
    fault: Mutex<Option<Fault>>,
    config: Mutex<Option<MockConfig>>,
    received: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<Handshake>>,
    connections: AtomicUsize,
    frames: AtomicUsize,
    kick: Mutex<Option<watch::Sender<u64>>>,
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
            cancel: CancelMode::KeepSending,
            time_base: TimeBase::Context,
            word_secs: 0.2,
            ts_order: TsOrder::BeforeFlush,
            expiry: None,
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
            url: format!("ws://{addr}/tts/websocket"),
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

    pub fn handshakes(&self) -> Vec<Handshake> {
        lock(&self.state.handshakes).clone()
    }

    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    pub fn frames_sent(&self) -> usize {
        self.state.frames.load(Ordering::SeqCst)
    }

    /// Closes every open connection with a close frame (the server's idle timeout).
    pub fn kick(&self) {
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

/// One frame waiting to be sent, tagged with its context for cancel handling.
struct Out {
    ctx: String,
    json: String,
}

#[derive(Default)]
struct CtxState {
    words: usize,
    flush_id: u32,
    /// `timestamps` held back until the next flush marker or the close.
    held_ts: Vec<String>,
    /// When the last audio of the context was sent, plus the expiry.
    expires_at: Option<Instant>,
    expired: bool,
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
    let expected = config.api_key.clone();
    #[allow(clippy::result_large_err)]
    let callback = move |request: &Request, response: Response| {
        let key_ok = request
            .headers()
            .get("x-api-key")
            .is_some_and(|v| v == expected.as_str());
        lock(&handshake_state.handshakes).push(Handshake {
            uri: request.uri().to_string(),
            key_ok,
        });
        if let Some(status) = reject {
            return Err(error_response(status));
        }
        if !key_ok {
            return Err(error_response(401));
        }
        Ok(response)
    };
    let Ok(mut ws) = accept_hdr_async(stream, callback).await else {
        return;
    };

    let mut pending: VecDeque<Out> = VecDeque::new();
    let mut contexts: HashMap<String, CtxState> = HashMap::new();
    let (mut sent, mut requests) = (0_usize, 0_usize);
    let mut next_at = Instant::now();
    loop {
        if let Some(Fault::Disconnect { after_events }) = fault
            && requests > 0
            && sent >= after_events
        {
            return; // abrupt: no close frame
        }
        let next_expiry = match config.expiry {
            Some((_, ExpiryMode::Done)) => contexts
                .iter()
                .filter(|(ctx, c)| !c.expired && !pending.iter().any(|o| &o.ctx == *ctx))
                .filter_map(|(_, c)| c.expires_at)
                .min(),
            _ => None,
        };
        let expiry_timer = async {
            match next_expiry {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
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
                let _ = ws.close(None).await;
                return;
            }
            () = expiry_timer => {
                let now = Instant::now();
                let due: Vec<String> = contexts
                    .iter()
                    .filter(|(ctx, c)| {
                        !c.expired
                            && c.expires_at.is_some_and(|t| t <= now)
                            && !pending.iter().any(|o| &o.ctx == *ctx)
                    })
                    .map(|(ctx, _)| ctx.clone())
                    .collect();
                for ctx in due {
                    if let Some(c) = contexts.get_mut(&ctx) {
                        c.expired = true;
                    }
                    if ws.send(Message::text(done(&ctx))).await.is_err() {
                        return;
                    }
                    state.frames.fetch_add(1, Ordering::SeqCst);
                }
            }
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    requests += 1;
                    on_request(&state, &config, &mut contexts, &mut pending, text.as_str());
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            () = until(next_at), if !pending.is_empty() => {
                let Some(out) = pending.pop_front() else { continue };
                if let Some(Fault::Stall { before_event, duration }) = fault
                    && before_event == sent
                {
                    tokio::time::sleep(duration).await;
                }
                let is_chunk = out.json.contains("\"type\":\"chunk\"");
                let text = match fault {
                    Some(Fault::Protocol { after_events }) if after_events == sent => {
                        "{this is not json".to_owned()
                    }
                    _ => out.json,
                };
                if ws.send(Message::text(text)).await.is_err() {
                    return;
                }
                sent += 1;
                state.frames.fetch_add(1, Ordering::SeqCst);
                if let Some((after, _)) = config.expiry
                    && is_chunk
                    && let Some(c) = contexts.get_mut(&out.ctx)
                {
                    c.expires_at = Some(Instant::now() + after);
                }
                next_at = Instant::now() + config.interval;
            }
        }
    }
}

fn on_request(
    state: &State,
    config: &MockConfig,
    contexts: &mut HashMap<String, CtxState>,
    pending: &mut VecDeque<Out>,
    text: &str,
) {
    let Ok(request) = serde_json::from_str::<Value>(text) else {
        return;
    };
    lock(&state.received).push(request.clone());
    let ctx = request["context_id"].as_str().unwrap_or("").to_owned();
    if let Some(responder) = &config.responder
        && let Some(frames) = responder(&request)
    {
        for json in frames {
            pending.push_back(Out {
                ctx: ctx.clone(),
                json,
            });
        }
        return;
    }
    if request["cancel"] == true {
        if config.cancel == CancelMode::Honor {
            pending.retain(|o| o.ctx != ctx);
        }
        return;
    }
    let state_of = contexts.entry(ctx.clone()).or_default();
    if let Some((_, mode)) = config.expiry
        && (state_of.expired || state_of.expires_at.is_some_and(|t| Instant::now() >= t))
    {
        state_of.expired = true;
        if mode == ExpiryMode::Error {
            pending.push_back(Out {
                ctx: ctx.clone(),
                json: json!({
                    "type": "error", "done": true, "status_code": 400,
                    "title": "Context expired", "message": "the context has expired",
                    "context_id": ctx
                })
                .to_string(),
            });
        }
        return;
    }
    if request["flush"] == true {
        state_of.flush_id += 1;
        pending.push_back(Out {
            ctx: ctx.clone(),
            json: json!({
                "type": "flush_done", "done": false, "flush_done": true,
                "flush_id": state_of.flush_id, "status_code": 206, "context_id": ctx
            })
            .to_string(),
        });
        for json in state_of.held_ts.drain(..) {
            pending.push_back(Out {
                ctx: ctx.clone(),
                json,
            });
        }
        return;
    }
    let rate = request["output_format"]["sample_rate"]
        .as_u64()
        .unwrap_or(16_000);
    let bytes_per_sample: u64 = if request["output_format"]["encoding"] == "pcm_s16le" {
        2
    } else {
        1
    };
    let transcript = request["transcript"].as_str().unwrap_or("");
    let words: Vec<&str> = transcript.split_whitespace().collect();
    let samples_per_word = (rate as f64 * config.word_secs) as u64;
    let (mut starts, mut ends) = (Vec::new(), Vec::new());
    for (i, _) in words.iter().enumerate() {
        let index = match config.time_base {
            TimeBase::Context => state_of.words + i,
            TimeBase::Chunk => i,
        };
        let start = index as f64 * config.word_secs + 0.02;
        starts.push(start);
        ends.push(start + config.word_secs - 0.02);
        let audio = vec![(i % 200) as u8 + 1; (samples_per_word * bytes_per_sample) as usize];
        pending.push_back(Out {
            ctx: ctx.clone(),
            json: chunk(&ctx, &audio),
        });
    }
    state_of.words += words.len();
    if !words.is_empty() {
        let json = json!({
            "type": "timestamps", "done": false, "status_code": 206, "context_id": ctx,
            "word_timestamps": {"words": words, "start": starts, "end": ends}
        })
        .to_string();
        if config.ts_order == TsOrder::AfterFlush && request["continue"] != false {
            state_of.held_ts.push(json);
        } else {
            pending.push_back(Out {
                ctx: ctx.clone(),
                json,
            });
        }
    }
    if request["continue"] == false {
        for json in state_of.held_ts.drain(..) {
            pending.push_back(Out {
                ctx: ctx.clone(),
                json,
            });
        }
        pending.push_back(Out {
            ctx: ctx.clone(),
            json: done(&ctx),
        });
    }
}

pub fn chunk(ctx: &str, audio: &[u8]) -> String {
    json!({
        "type": "chunk",
        "data": base64::engine::general_purpose::STANDARD.encode(audio),
        "done": false, "status_code": 206, "step_time": 1, "context_id": ctx
    })
    .to_string()
}

pub fn done(ctx: &str) -> String {
    json!({"type": "done", "done": true, "status_code": 206, "context_id": ctx}).to_string()
}

/// Waits until `at`; returns at once if it has passed (a timer round trip costs a whole tick).
async fn until(at: Instant) {
    if Instant::now() < at {
        tokio::time::sleep_until(at).await;
    }
}
