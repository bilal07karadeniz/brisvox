//! A mock of an OpenAI-compatible `/v1/audio/speech` server (HTTP/1.1 with chunked bodies), for
//! the tests of the `openai_speech` adapter. It runs on its own thread and runtime, so its tasks
//! never show up in the leak counters of the contract suite.
//!
//! The audio of a request is one chunk per word of the `input`, every byte of the chunk of word
//! `i` equal to `i + 1`, so a test can see order and loss from the bytes alone.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use brisvox_sim::contract::Fault;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A request the mock received.
#[derive(Debug, Clone, Default)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `input` field of the JSON body.
    pub fn input(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("input").and_then(|i| i.as_str()).map(str::to_owned))
            .unwrap_or_default()
    }
}

/// What the body of a successful response looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Headerless PCM, one chunk per word.
    Pcm,
    /// A WAV header (sizes unknown, as a streaming server writes it) at this rate, written in
    /// pieces of `header_piece` bytes, then PCM chunks.
    Wav { rate: u32, header_piece: usize },
    /// The whole audio in one chunk, with no pacing (a server that synthesizes first).
    Whole,
}

#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Bytes of audio per word.
    pub chunk_bytes: usize,
    /// Pause before every chunk.
    pub chunk_delay: Duration,
    pub mode: Mode,
    /// Texts containing the key are answered with the status.
    pub refuse: Vec<(String, u16)>,
    /// Authorization the server demands, if any.
    pub require_auth: Option<String>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: 4_800,
            chunk_delay: Duration::from_millis(20),
            mode: Mode::Pcm,
            refuse: Vec::new(),
            require_auth: None,
        }
    }
}

#[derive(Default)]
struct State {
    config: Mutex<MockConfig>,
    faults: Mutex<VecDeque<Fault>>,
    recorded: Mutex<Vec<Recorded>>,
    accepted: AtomicUsize,
    aborted: AtomicUsize,
    completed: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Mock {
    pub base_url: String,
    state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Mock {
    pub fn start() -> Self {
        let state = Arc::new(State::default());
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
            base_url: format!("http://{addr}/v1"),
            state,
            stop: Some(stop_tx),
            thread: Some(thread),
        }
    }

    pub fn configure(&self, change: impl FnOnce(&mut MockConfig)) {
        change(&mut lock(&self.state.config));
    }

    /// Faults apply to the next requests, one each, in order.
    pub fn inject(&self, fault: Fault) {
        lock(&self.state.faults).push_back(fault);
    }

    /// Forgets queued faults and restores the default behaviour (recordings are kept).
    pub fn reset(&self) {
        lock(&self.state.faults).clear();
        *lock(&self.state.config) = MockConfig::default();
    }

    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.state.recorded).clone()
    }

    /// Connections accepted so far.
    pub fn accepted(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    /// Responses the client abandoned before they ended.
    pub fn aborted(&self) -> usize {
        self.state.aborted.load(Ordering::SeqCst)
    }

    /// Responses written to the end.
    pub fn completed(&self) -> usize {
        self.state.completed.load(Ordering::SeqCst)
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
                let Ok((socket, _)) = accepted else { continue };
                state.accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(connection(socket, Arc::clone(&state)));
            }
        }
    }
}

/// Reads one request (head and body); `None` when the client closed.
async fn read_request(rd: &mut OwnedReadHalf, carry: &mut Vec<u8>) -> Option<Recorded> {
    let head_end = loop {
        if let Some(at) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let mut buf = [0_u8; 4096];
        let n = rd.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        carry.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&carry[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while carry.len() < body_start + length {
        let mut buf = [0_u8; 4096];
        let n = rd.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        carry.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&carry[body_start..body_start + length]).into_owned();
    carry.drain(..body_start + length);
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

async fn connection(socket: TcpStream, state: Arc<State>) {
    let (mut rd, mut wr) = socket.into_split();
    let mut carry = Vec::new();
    while let Some(request) = read_request(&mut rd, &mut carry).await {
        lock(&state.recorded).push(request.clone());
        if !respond(&mut rd, &mut wr, &state, &request).await {
            return;
        }
    }
}

async fn plain(wr: &mut OwnedWriteHalf, status: u16, content_type: &str, body: &str) -> bool {
    let text = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    wr.write_all(text.as_bytes()).await.is_ok()
}

/// Sleeps `delay`, returning `false` if the client hung up meanwhile.
async fn pause(rd: &mut OwnedReadHalf, state: &State, delay: Duration) -> bool {
    let mut probe = [0_u8; 1];
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        read = rd.read(&mut probe) => {
            // EOF or an error: the client dropped the response. (A byte would be a pipelined
            // request, which the adapter never sends.)
            let _ = read;
            state.aborted.fetch_add(1, Ordering::SeqCst);
            false
        }
    }
}

async fn write_chunk(wr: &mut OwnedWriteHalf, state: &State, data: &[u8]) -> bool {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    if wr.write_all(&out).await.is_err() || wr.flush().await.is_err() {
        state.aborted.fetch_add(1, Ordering::SeqCst);
        return false;
    }
    true
}

fn wav_header(rate: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16_u32.to_le_bytes());
    v.extend_from_slice(&1_u16.to_le_bytes());
    v.extend_from_slice(&1_u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2_u16.to_le_bytes());
    v.extend_from_slice(&16_u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
    v
}

/// Answers one request. `false` ends the connection.
async fn respond(
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
    state: &State,
    request: &Recorded,
) -> bool {
    if request.method == "HEAD" {
        return plain(wr, 405, "text/plain", "").await;
    }
    let config = lock(&state.config).clone();
    if let Some(expected) = &config.require_auth
        && request.header("authorization") != Some(expected.as_str())
    {
        return plain(wr, 401, "application/json", "{\"detail\":\"no\"}").await;
    }
    let fault = lock(&state.faults).pop_front();
    let input = request.input();
    if let Some((_, status)) = config.refuse.iter().find(|(key, _)| input.contains(key)) {
        return plain(wr, *status, "application/json", "{\"detail\":\"refused\"}").await;
    }
    let mut disconnect_after = None;
    let mut stall = None;
    match fault {
        Some(Fault::Reject { status }) => {
            return plain(wr, status, "application/json", "{\"error\":\"injected\"}").await;
        }
        Some(Fault::Protocol { .. }) => {
            return plain(wr, 200, "application/json", "{\"oops\":true}").await;
        }
        Some(Fault::ConnectStall { duration }) => {
            if !pause(rd, state, duration).await {
                return false;
            }
        }
        Some(Fault::Disconnect { after_events }) => disconnect_after = Some(after_events),
        Some(Fault::Stall {
            before_event,
            duration,
        }) => stall = Some((before_event, duration)),
        None => {}
    }
    let content_type = match config.mode {
        Mode::Wav { .. } => "audio/wav",
        _ => "audio/pcm",
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n"
    );
    if wr.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut event = 0_usize;
    if let Mode::Wav { rate, header_piece } = config.mode {
        for piece in wav_header(rate).chunks(header_piece.max(1)) {
            if !write_chunk(wr, state, piece).await {
                return false;
            }
        }
    }
    let whole = config.mode == Mode::Whole;
    let mut whole_audio = Vec::new();
    for (i, _) in words.iter().enumerate() {
        if disconnect_after == Some(event) {
            return false; // The connection is dropped in the middle of the body.
        }
        if let Some((before, duration)) = stall
            && before == event
            && !pause(rd, state, duration).await
        {
            return false;
        }
        let fill = u8::try_from(i + 1).unwrap_or(255);
        let data = vec![fill; config.chunk_bytes];
        if whole {
            whole_audio.extend_from_slice(&data);
        } else {
            if !pause(rd, state, config.chunk_delay).await {
                return false;
            }
            if !write_chunk(wr, state, &data).await {
                return false;
            }
        }
        event += 1;
    }
    if whole && !whole_audio.is_empty() && !write_chunk(wr, state, &whole_audio).await {
        return false;
    }
    if disconnect_after == Some(event) {
        return false;
    }
    let ended = wr.write_all(b"0\r\n\r\n").await.is_ok();
    if ended {
        state.completed.fetch_add(1, Ordering::SeqCst);
    }
    ended
}
