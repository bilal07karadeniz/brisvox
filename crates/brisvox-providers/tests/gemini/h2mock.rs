//! A loopback HTTP/2 (h2c, prior knowledge) mock of `streamGenerateContent`, built on the `h2`
//! crate. It exists for the properties HTTP/1.1 cannot show: multiplexing on one connection,
//! `RST_STREAM` when a response is dropped, and a `HEAD` warm-up that leaves the connection
//! usable.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Response, StatusCode};

/// What the mock does with a `POST`.
#[derive(Debug, Clone)]
pub enum Mode {
    /// One text event and a STOP chunk, then the end of the stream.
    Finish,
    /// One text event, then silence until the client resets the stream.
    Hang,
    /// `events` text events, `pace` apart, then a STOP chunk (a burst of concurrent streams).
    Paced { events: usize, pace: Duration },
}

#[derive(Debug)]
struct State {
    mode: Mutex<Mode>,
    connections: AtomicUsize,
    heads: AtomicUsize,
    posts: AtomicUsize,
    resets: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

pub struct H2Mock {
    pub addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for H2Mock {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

fn sse(json: &str) -> Bytes {
    Bytes::from(format!("data: {json}\n\n"))
}

const TEXT: &str =
    r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"index":0}]}"#;
const STOP: &str = r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1}}"#;

impl H2Mock {
    pub async fn start(mode: Mode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(State {
            mode: Mutex::new(mode),
            connections: AtomicUsize::new(0),
            heads: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            resets: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let accept = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    state.connections.fetch_add(1, Ordering::SeqCst);
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let Ok(mut connection) = h2::server::handshake(socket).await else {
                            return;
                        };
                        while let Some(Ok((request, respond))) = connection.accept().await {
                            tokio::spawn(handle(Arc::clone(&state), request, respond));
                        }
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

    pub fn set_mode(&self, mode: Mode) {
        *self.state.mode.lock().expect("lock") = mode;
    }

    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    pub fn heads(&self) -> usize {
        self.state.heads.load(Ordering::SeqCst)
    }

    pub fn posts(&self) -> usize {
        self.state.posts.load(Ordering::SeqCst)
    }

    /// Streams the client ended with `RST_STREAM`.
    pub fn resets(&self) -> usize {
        self.state.resets.load(Ordering::SeqCst)
    }

    /// The most streams that were open at the same time.
    pub fn max_active(&self) -> usize {
        self.state.max_active.load(Ordering::SeqCst)
    }
}

async fn handle(
    state: Arc<State>,
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
) {
    let (parts, mut body) = request.into_parts();
    // Read the request body so flow control keeps moving.
    while let Some(Ok(data)) = body.data().await {
        let _ = body.flow_control().release_capacity(data.len());
    }
    if parts.method == http::Method::HEAD {
        state.heads.fetch_add(1, Ordering::SeqCst);
        let response = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(())
            .expect("response");
        let _ = respond.send_response(response, true);
        return;
    }
    state.posts.fetch_add(1, Ordering::SeqCst);
    let now_active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_active.fetch_max(now_active, Ordering::SeqCst);
    stream_response(&state, &mut respond).await;
    state.active.fetch_sub(1, Ordering::SeqCst);
}

async fn stream_response(state: &State, respond: &mut h2::server::SendResponse<Bytes>) {
    let mode = state.mode.lock().expect("lock").clone();
    let response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(())
        .expect("response");
    let Ok(mut send) = respond.send_response(response, false) else {
        return;
    };
    match mode {
        Mode::Finish => {
            let _ = send.send_data(sse(TEXT), false);
            let _ = send.send_data(sse(STOP), true);
        }
        Mode::Hang => {
            let _ = send.send_data(sse(TEXT), false);
            let reset = tokio::time::timeout(
                Duration::from_secs(10),
                std::future::poll_fn(|cx| send.poll_reset(cx)),
            )
            .await;
            if matches!(reset, Ok(Ok(_))) {
                state.resets.fetch_add(1, Ordering::SeqCst);
            }
        }
        Mode::Paced { events, pace } => {
            for _ in 0..events {
                tokio::time::sleep(pace).await;
                if send.send_data(sse(TEXT), false).is_err() {
                    return;
                }
            }
            let _ = send.send_data(sse(STOP), true);
        }
    }
}
