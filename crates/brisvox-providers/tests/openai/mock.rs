//! A small local mock of the OpenAI Responses endpoint: HTTP/1.1 over a loopback `TcpListener`,
//! chunked `text/event-stream` responses, scripted step by step. No HTTP crate: the wire is
//! written by hand so tests control chunk boundaries, delays and abrupt closes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

/// One step of a scripted response body.
#[derive(Debug, Clone)]
pub enum Step {
    /// An SSE event (`data: <json>\n\n`) in its own HTTP chunk; counts toward `events_sent`
    /// when `counted`.
    Event { json: String, counted: bool },
    /// Raw bytes in one HTTP chunk (comments, split events, garbage).
    Raw(Vec<u8>),
    /// Wait (ends early when the client hangs up).
    Sleep(Duration),
    /// Hang up without finishing the chunked body.
    Close,
}

impl Step {
    pub fn event(json: impl Into<String>) -> Self {
        Self::Event {
            json: json.into(),
            counted: false,
        }
    }

    pub fn counted(json: impl Into<String>) -> Self {
        Self::Event {
            json: json.into(),
            counted: true,
        }
    }

    pub fn raw(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Raw(bytes.into())
    }
}

/// A scripted response to the next `POST /v1/responses`.
#[derive(Debug, Clone)]
pub struct Script {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Body for a non-200 status (sent with a content length).
    pub error_body: String,
    /// Wait after reading the request, before the response headers.
    pub pre_delay: Duration,
    /// For a non-200 status: the `Content-Length` to declare instead of the real body length,
    /// and the connection then stalls (the rest of the body never comes).
    pub error_stalls_after_body_declaring: Option<usize>,
    pub steps: Vec<Step>,
}

impl Script {
    pub fn stream(steps: Vec<Step>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            error_body: String::new(),
            pre_delay: Duration::ZERO,
            error_stalls_after_body_declaring: None,
            steps,
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            error_body: body.to_owned(),
            pre_delay: Duration::ZERO,
            error_stalls_after_body_declaring: None,
            steps: Vec::new(),
        }
    }

    /// A refusal whose body is cut short: `body` is sent, `declared_len` bytes are announced,
    /// and the server then goes silent (until the client hangs up).
    pub fn status_then_stall(status: u16, body: &str, declared_len: usize) -> Self {
        Self {
            error_stalls_after_body_declaring: Some(declared_len),
            ..Self::status(status, body)
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// A request the mock received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("the request body is JSON")
    }
}

#[derive(Debug, Default)]
struct State {
    scripts: Mutex<Vec<Script>>,
    requests: Mutex<Vec<Recorded>>,
    events_sent: AtomicUsize,
    connections: AtomicUsize,
    /// Connections whose handler ended (the client hung up, or a script ended it).
    closed: AtomicUsize,
    keepalive: AtomicBool,
}

/// The mock server. Dropping it stops accepting connections.
pub struct Mock {
    pub addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Mock {
    /// Starts the mock on a loopback port. Responses close the connection after each request
    /// unless `keepalive` is set.
    pub async fn start(keepalive: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(State {
            keepalive: AtomicBool::new(keepalive),
            ..State::default()
        });
        let accept = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    state.connections.fetch_add(1, Ordering::SeqCst);
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        serve(socket, Arc::clone(&state)).await;
                        state.closed.fetch_add(1, Ordering::SeqCst);
                    });
                }
            }
        });
        Self {
            addr,
            state,
            accept,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queues a script for the next `POST /v1/responses` (FIFO).
    pub fn script(&self, script: Script) {
        self.state.scripts.lock().expect("lock").push(script);
    }

    /// Forgets queued scripts.
    pub fn clear_scripts(&self) {
        self.state.scripts.lock().expect("lock").clear();
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().expect("lock").clone()
    }

    /// The `POST /v1/responses` requests, in order.
    pub fn posts(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect()
    }

    pub fn events_sent(&self) -> usize {
        self.state.events_sent.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Connections that have ended. With scripts that never finish on their own, each one is a
    /// hang-up by the client.
    pub fn closed_connections(&self) -> usize {
        self.state.closed.load(Ordering::SeqCst)
    }
}

/// Reads one request; `None` when the client closed first.
async fn read_request(rd: &mut OwnedReadHalf, carry: &mut Vec<u8>) -> Option<Recorded> {
    let head_end = loop {
        if let Some(pos) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut buf = [0u8; 4096];
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
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while carry.len() < body_start + length {
        let mut buf = [0u8; 4096];
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

/// True when the client has closed its side (EOF or error), without waiting.
fn client_gone(rd: &OwnedReadHalf) -> bool {
    let mut buf = [0u8; 64];
    match rd.try_read(&mut buf) {
        Ok(0) => true,
        // Unread bytes (a pipelined request) are not a hang-up; nothing to read is not either.
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    }
}

/// Sleeps `duration`, or until the client hangs up (returns false then).
async fn sleep_unless_gone(rd: &mut OwnedReadHalf, duration: Duration) -> bool {
    let mut buf = [0u8; 64];
    tokio::select! {
        () = tokio::time::sleep(duration) => true,
        r = rd.read(&mut buf) => !matches!(r, Ok(0) | Err(_)),
    }
}

async fn write_chunk(wr: &mut OwnedWriteHalf, data: &[u8]) -> bool {
    let mut frame = format!("{:x}\r\n", data.len()).into_bytes();
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\r\n");
    wr.write_all(&frame).await.is_ok() && wr.flush().await.is_ok()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn serve(socket: TcpStream, state: Arc<State>) {
    let _ = socket.set_nodelay(true);
    let (mut rd, mut wr) = socket.into_split();
    let mut carry = Vec::new();
    loop {
        let Some(request) = read_request(&mut rd, &mut carry).await else {
            return;
        };
        let streaming = request.method == "POST" && request.path == "/v1/responses";
        state.requests.lock().expect("lock").push(request);
        let keepalive = state.keepalive.load(Ordering::SeqCst);
        let connection = if keepalive { "keep-alive" } else { "close" };
        if !streaming {
            // Warm-up and anything else: an empty 401, like an unauthenticated probe.
            let head = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: {connection}\r\n\r\n"
            );
            if wr.write_all(head.as_bytes()).await.is_err() || !keepalive {
                return;
            }
            continue;
        }
        let script = {
            let mut scripts = state.scripts.lock().expect("lock");
            if scripts.is_empty() {
                None
            } else {
                Some(scripts.remove(0))
            }
        };
        let script = script.unwrap_or_else(|| {
            Script::stream(vec![Step::counted(
                r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
            )])
        });
        if !script.pre_delay.is_zero() && !sleep_unless_gone(&mut rd, script.pre_delay).await {
            return;
        }
        if script.status != 200 {
            let mut head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n",
                script.status,
                reason(script.status),
                script
                    .error_stalls_after_body_declaring
                    .unwrap_or(script.error_body.len())
            );
            for (n, v) in &script.headers {
                head.push_str(&format!("{n}: {v}\r\n"));
            }
            head.push_str("\r\n");
            head.push_str(&script.error_body);
            if script.error_stalls_after_body_declaring.is_some() {
                if wr.write_all(head.as_bytes()).await.is_ok() {
                    let _ = wr.flush().await;
                    sleep_unless_gone(&mut rd, Duration::from_secs(30)).await;
                }
                return;
            }
            if wr.write_all(head.as_bytes()).await.is_err() || !keepalive {
                return;
            }
            continue;
        }
        let mut head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: {connection}\r\n"
        );
        for (n, v) in &script.headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if wr.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for step in script.steps {
            if client_gone(&rd) {
                return;
            }
            match step {
                Step::Event { json, counted } => {
                    let data = format!("data: {json}\n\n");
                    if !write_chunk(&mut wr, data.as_bytes()).await {
                        return;
                    }
                    if counted {
                        state.events_sent.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Step::Raw(bytes) => {
                    if !write_chunk(&mut wr, &bytes).await {
                        return;
                    }
                }
                Step::Sleep(duration) => {
                    if !sleep_unless_gone(&mut rd, duration).await {
                        return;
                    }
                }
                Step::Close => return,
            }
        }
        if wr.write_all(b"0\r\n\r\n").await.is_err() || !keepalive {
            return;
        }
    }
}
