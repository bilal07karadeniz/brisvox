//! A small mock WebSocket server for the local-speech adapter tests: one `TcpListener`, one
//! handler per connection, a log of everything the clients sent, the upgrade requests, and the
//! faults the contract suite injects. Real sockets do not work with paused tokio time, so tests
//! run on the real `SystemClock` with short deadlines.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::result_large_err
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the server saw from a client.
#[derive(Debug, Clone)]
pub enum What {
    /// A binary message.
    Audio(Bytes),
    /// A text message.
    Text(String),
    /// A WebSocket ping (the library answers it with a pong).
    Ping,
}

/// One logged client message.
#[derive(Debug, Clone)]
pub struct Seen {
    pub conn: usize,
    pub at: Instant,
    pub what: What,
}

/// An upgrade request.
#[derive(Debug, Clone)]
pub struct Req {
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Everything the server saw on its WebSockets, shared with the test.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<Seen>>>);

impl Log {
    fn push(&self, conn: usize, what: What) {
        lock(&self.0).push(Seen {
            conn,
            at: Instant::now(),
            what,
        });
    }

    pub fn all(&self) -> Vec<Seen> {
        lock(&self.0).clone()
    }

    /// Binary messages of connection `conn`, in order.
    pub fn audio(&self, conn: usize) -> Vec<Bytes> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn)
            .filter_map(|s| match s.what {
                What::Audio(b) => Some(b),
                _ => None,
            })
            .collect()
    }

    /// Text messages of connection `conn`, in order.
    pub fn texts(&self, conn: usize) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn)
            .filter_map(|s| match s.what {
                What::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    pub fn pings(&self, conn: usize) -> usize {
        self.all()
            .into_iter()
            .filter(|s| s.conn == conn && matches!(s.what, What::Ping))
            .count()
    }
}

#[derive(Default)]
struct Shared {
    connections: AtomicUsize,
    requests: Mutex<Vec<Req>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// A connection accepted at TCP level, not yet answered.
pub struct Conn {
    stream: tokio::net::TcpStream,
    /// 0-based number of this connection.
    pub index: usize,
    shared: Arc<Shared>,
    log: Log,
}

type Ws = WebSocketStream<tokio::net::TcpStream>;

impl Conn {
    /// Completes the WebSocket handshake.
    pub async fn accept(self) -> Option<Server> {
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
    pub async fn reject(self, status: u16) {
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
pub struct Server {
    ws: Ws,
    pub index: usize,
    log: Log,
    pub audio_frames: usize,
}

impl Server {
    /// Next client message, logged. `None` when the connection is over.
    pub async fn next(&mut self) -> Option<What> {
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

    /// Reads until `n` binary messages arrived on this connection.
    pub async fn until_audio(&mut self, n: usize) -> bool {
        while self.audio_frames < n {
            if self.next().await.is_none() {
                return false;
            }
        }
        true
    }

    /// Reads until the client goes away.
    pub async fn drain(&mut self) {
        while self.next().await.is_some() {}
    }

    pub async fn send(&mut self, text: impl Into<String>) {
        let _ = self.ws.send(Message::text(text.into())).await;
    }

    pub async fn send_ping(&mut self) {
        let _ = self.ws.send(Message::Ping(Bytes::new())).await;
    }

    /// Closes with a close frame and lets the client read it.
    pub async fn close_with(&mut self, code: u16, reason: &str) {
        let _ = self
            .ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: reason.to_owned().into(),
            })))
            .await;
        let _ = tokio::time::timeout(Duration::from_millis(300), self.drain()).await;
    }
}

type Handler = Arc<dyn Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync>;

/// The mock server.
pub struct MockServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    pub log: Log,
    accept_task: AbortHandle,
}

impl MockServer {
    pub async fn start<F>(ws_handler: F) -> Self
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
                    Err(_) => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let index = accept_shared.connections.fetch_add(1, Ordering::SeqCst);
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

    /// `ws://127.0.0.1:port`.
    pub fn base_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    /// The upgrade requests that reached the server, in order.
    pub fn requests(&self) -> Vec<Req> {
        lock(&self.shared.requests).clone()
    }

    /// Aborts every connection task (stalled ones included).
    pub fn abort_handlers(&self) {
        for handle in lock(&self.shared.tasks).drain(..) {
            handle.abort();
        }
    }

    /// Waits (real time) until at least `n` connections were accepted.
    pub async fn wait_connections(&self, n: usize) -> bool {
        for _ in 0..500 {
            if self.connections() >= n {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.abort_handlers();
    }
}

/// Turns an async closure into the handler type.
pub fn handler<F, Fut>(f: F) -> impl Fn(Conn) -> BoxFuture<'static, ()> + Send + Sync + 'static
where
    F: Fn(Conn) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    move |conn| Box::pin(f(conn))
}
