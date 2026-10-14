//! A local mock of the ElevenLabs multi-context TTS WebSocket
//! (`/v1/text-to-speech/{voice}/multi-stream-input`), speaking the wire protocol of
//! docs/providers/elevenlabs.md. It runs on its own thread and runtime, so its tasks never show
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
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// What the mock does when a `close_context` arrives for a context it is still generating.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelMode {
    /// Stops generating (the ideal server).
    Honor,
    /// Keeps sending everything it already queued (the docs do not say which it is).
    KeepSending,
}

/// Whether and when the mock sends `isFinal` after a `close_context`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FinalMode {
    /// Right after the context's queued output.
    Always,
    /// Never (the docs do not promise one after a flush).
    Never,
    /// After the queued output plus this delay.
    Delayed(Duration),
}

/// Where the mock puts the alignment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlignMode {
    /// `alignment` and `normalizedAlignment` in the message of the audio they describe.
    WithAudio,
    /// An audio-only message, then an alignment-only message.
    Separate,
    /// An alignment-only message, then the audio-only message it describes.
    AlignmentFirst,
    /// Audio only (`alignment` null).
    Absent,
    /// Only `normalizedAlignment`, and with a longer text than the input (numbers spelled out).
    NormalizedOnly,
}

/// A custom reply to a client frame: the raw JSON frames to send, or `None` for the default.
pub type Responder = Arc<dyn Fn(&Value) -> Option<Vec<String>> + Send + Sync>;

#[derive(Clone)]
pub struct MockConfig {
    pub api_key: String,
    /// Delay between two server frames.
    pub interval: Duration,
    pub cancel: CancelMode,
    pub final_mode: FinalMode,
    pub align: AlignMode,
    /// Seconds of audio per word.
    pub word_secs: f64,
    /// Send `is_final` instead of `isFinal` (and `context_id` instead of `contextId`).
    pub snake_case: bool,
    pub responder: Option<Responder>,
}

#[derive(Clone, Debug)]
pub struct Handshake {
    pub uri: String,
    pub key_ok: bool,
    pub key_in_query: bool,
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
    close_code: Mutex<Option<u16>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Mock {
    /// `ws://127.0.0.1:port`
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
            final_mode: FinalMode::Always,
            align: AlignMode::WithAudio,
            word_secs: 0.2,
            snake_case: false,
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
            url: format!("ws://{addr}"),
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

    /// Closes every open connection with a close frame (code 1000, or `code` if given).
    pub fn kick(&self, code: Option<u16>) {
        *lock(&self.state.close_code) = code;
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
    delay: Duration,
}

#[derive(Default)]
struct CtxState {
    initialised: bool,
    closed: bool,
    /// A flush-only frame was received: a later close lets the flush finish.
    flushed: bool,
}

/// Rate and bytes per sample from the `output_format` query value.
fn parse_format(uri: &str) -> (u64, u64) {
    let format = uri
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("output_format="))
        .unwrap_or("pcm_16000");
    match format.split_once('_') {
        Some(("pcm", rate)) => (rate.parse().unwrap_or(16_000), 2),
        Some((_, rate)) => (rate.parse().unwrap_or(8_000), 1),
        None => (16_000, 2),
    }
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
    let uri_slot = Arc::new(Mutex::new(String::new()));
    let callback_slot = Arc::clone(&uri_slot);
    #[allow(clippy::result_large_err)]
    let callback = move |request: &Request, response: Response| {
        let key_ok = request
            .headers()
            .get("xi-api-key")
            .is_some_and(|v| v == expected.as_str());
        let uri = request.uri().to_string();
        *lock(&callback_slot) = uri.clone();
        lock(&handshake_state.handshakes).push(Handshake {
            key_in_query: uri.contains(&expected),
            uri,
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
    let (rate, bytes_per_sample) = parse_format(&lock(&uri_slot));

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
                let code = *lock(&state.close_code);
                let frame = code.map(|c| CloseFrame { code: CloseCode::from(c), reason: "".into() });
                let _ = ws.close(frame).await;
                return;
            }
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    requests += 1;
                    let ctx = Ctx { rate, bytes_per_sample };
                    on_request(&state, &config, &ctx, &mut contexts, &mut pending, text.as_str());
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            () = until(next_at), if !pending.is_empty() => {
                let Some(out) = pending.pop_front() else { continue };
                if !out.delay.is_zero() {
                    tokio::time::sleep(out.delay).await;
                }
                if let Some(Fault::Stall { before_event, duration }) = fault
                    && before_event == sent
                {
                    tokio::time::sleep(duration).await;
                }
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
                next_at = Instant::now() + config.interval;
            }
        }
    }
}

struct Ctx {
    rate: u64,
    bytes_per_sample: u64,
}

/// Splits text into tokens of "a word plus the whitespace after it"; leading whitespace joins
/// the first token.
fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut seen_word = false;
    let mut after_word_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if seen_word {
                after_word_space = true;
            }
            current.push(c);
        } else {
            if after_word_space {
                out.push(std::mem::take(&mut current));
                after_word_space = false;
            }
            seen_word = true;
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn alignment(token: &str, word_ms: u64) -> Value {
    let chars: Vec<String> = token.chars().map(String::from).collect();
    let n = chars.len().max(1) as u64;
    let step = word_ms / n;
    let starts: Vec<u64> = (0..chars.len() as u64).map(|i| i * step).collect();
    let durations: Vec<u64> = chars.iter().map(|_| step).collect();
    json!({"chars": chars, "charStartTimesMs": starts, "charDurationsMs": durations})
}

pub fn audio_msg(ctx: &str, audio: &[u8], align: Option<&Value>, norm: Option<&Value>) -> String {
    json!({
        "audio": base64::engine::general_purpose::STANDARD.encode(audio),
        "alignment": align,
        "normalizedAlignment": norm,
        "contextId": ctx
    })
    .to_string()
}

pub fn final_msg(ctx: &str, snake: bool) -> String {
    if snake {
        json!({"is_final": true, "context_id": ctx}).to_string()
    } else {
        json!({"isFinal": true, "contextId": ctx}).to_string()
    }
}

fn on_request(
    state: &State,
    config: &MockConfig,
    ctx: &Ctx,
    contexts: &mut HashMap<String, CtxState>,
    pending: &mut VecDeque<Out>,
    text: &str,
) {
    let Ok(request) = serde_json::from_str::<Value>(text) else {
        return;
    };
    lock(&state.received).push(request.clone());
    let id = request["context_id"].as_str().unwrap_or("").to_owned();
    let push = |pending: &mut VecDeque<Out>, json: String, delay: Duration| {
        pending.push_back(Out {
            ctx: id.clone(),
            json,
            delay,
        });
    };
    if let Some(responder) = &config.responder
        && let Some(frames) = responder(&request)
    {
        for json in frames {
            push(pending, json, Duration::ZERO);
        }
        return;
    }
    if request["close_context"] == true {
        let flushed = contexts.get(&id).is_some_and(|c| c.flushed);
        if config.cancel == CancelMode::Honor && !flushed {
            pending.retain(|o| o.ctx != id);
        }
        contexts.entry(id.clone()).or_default().closed = true;
        match config.final_mode {
            FinalMode::Always => push(pending, final_msg(&id, config.snake_case), Duration::ZERO),
            FinalMode::Delayed(d) => push(pending, final_msg(&id, config.snake_case), d),
            FinalMode::Never => {}
        }
        return;
    }
    let Some(spoken) = request["text"].as_str() else {
        contexts.entry(id.clone()).or_default().flushed = true;
        return; // flush-only frames: nothing is buffered
    };
    let state_of = contexts.entry(id.clone()).or_default();
    if spoken.is_empty() {
        return; // keepalive
    }
    if !state_of.initialised {
        state_of.initialised = true;
        if spoken.trim().is_empty() {
            return; // the context's first message
        }
    }
    if state_of.closed || request["flush"] != true || spoken.trim().is_empty() {
        return; // buffered without a flush, or text after the close: nothing is generated
    }
    let word_ms = (config.word_secs * 1000.0) as u64;
    let samples_per_word = (ctx.rate as f64 * config.word_secs) as u64;
    for (i, token) in tokens(spoken).iter().enumerate() {
        let audio = vec![(i % 200) as u8 + 1; (samples_per_word * ctx.bytes_per_sample) as usize];
        let aligned = alignment(token, word_ms);
        let normalized = if config.align == AlignMode::NormalizedOnly {
            // Numbers spelled out: more characters than the input.
            let word = token.trim_end();
            alignment(&format!("{word}xx{}", &token[word.len()..]), word_ms)
        } else {
            aligned.clone()
        };
        match config.align {
            AlignMode::WithAudio => push(
                pending,
                audio_msg(&id, &audio, Some(&aligned), Some(&normalized)),
                Duration::ZERO,
            ),
            AlignMode::Separate => {
                push(pending, audio_msg(&id, &audio, None, None), Duration::ZERO);
                push(
                    pending,
                    audio_msg(&id, &[], Some(&aligned), Some(&normalized)),
                    Duration::ZERO,
                );
            }
            AlignMode::AlignmentFirst => {
                push(
                    pending,
                    audio_msg(&id, &[], Some(&aligned), Some(&normalized)),
                    Duration::ZERO,
                );
                push(pending, audio_msg(&id, &audio, None, None), Duration::ZERO);
            }
            AlignMode::Absent => push(pending, audio_msg(&id, &audio, None, None), Duration::ZERO),
            AlignMode::NormalizedOnly => push(
                pending,
                audio_msg(&id, &audio, None, Some(&normalized)),
                Duration::ZERO,
            ),
        }
    }
}

/// Waits until `at`; returns at once if it has passed (a timer round trip costs a whole tick).
async fn until(at: Instant) {
    if Instant::now() < at {
        tokio::time::sleep_until(at).await;
    }
}
