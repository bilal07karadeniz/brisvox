//! Listeners, TLS, routing and the websocket upgrade.
//!
//! One hyper connection loop serves HTTP/1.1 and HTTP/2 (ALPN on the TLS port, HTTP/1.1 on the
//! plain port). The websocket endpoints are upgraded by hand (RFC 6455 handshake) and handed to
//! `tokio-tungstenite` as a raw stream.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{
    CONNECTION, CONTENT_TYPE, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE,
};
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::config::Config;
use crate::metrics::Metrics;
use crate::script::Utterance;
use crate::{llm, stt, tts};

/// The response body type of every route.
pub type Body = UnsyncBoxBody<Bytes, Infallible>;

/// A server-side websocket.
pub type Ws = WebSocketStream<TokioIo<Upgraded>>;

/// A complete body from bytes.
pub fn full(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Time for a TLS handshake or an HTTP/1.1 request head.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// State shared by every connection.
pub struct Shared {
    /// The command line.
    pub config: Arc<Config>,
    /// The fixture's utterances.
    pub script: Arc<Vec<Utterance>>,
    /// Counters.
    pub metrics: Arc<Metrics>,
    /// The CA certificate (`GET /ca.pem`).
    pub ca_pem: String,
    ids: AtomicU64,
}

impl Shared {
    /// State for a run.
    pub fn new(config: Arc<Config>, script: Arc<Vec<Utterance>>, ca_pem: String) -> Self {
        Self {
            config,
            script,
            metrics: Arc::new(Metrics::default()),
            ca_pem,
            ids: AtomicU64::new(0x5eed),
        }
    }

    /// A UUID-shaped id, unique within the run.
    pub fn next_request_id(&self) -> String {
        let n = self.ids.fetch_add(1, Ordering::Relaxed);
        // splitmix64: a different looking value per counter step.
        let mut z = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            z >> 32,
            (z >> 16) & 0xffff,
            z & 0xfff,
            (n >> 4) & 0xfff,
            z.rotate_left(17) & 0xffff_ffff_ffff
        )
    }

    /// Seconds since the epoch (the `created` field of completions).
    pub fn unix_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

fn text_response(status: StatusCode, content_type: &'static str, text: String) -> Response<Body> {
    let mut response = Response::new(full(Bytes::from(text)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    text_response(status, "text/plain; charset=utf-8", text.to_owned())
}

/// Whether the request asks for a websocket upgrade.
fn wants_websocket(request: &Request<Incoming>) -> bool {
    request.method() == Method::GET
        && request
            .headers()
            .get(UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Completes the websocket handshake and runs `handler` on the upgraded stream.
fn upgrade<F, Fut>(
    mut request: Request<Incoming>,
    shared: &Arc<Shared>,
    handler: F,
) -> Response<Body>
where
    F: FnOnce(Ws) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(key) = request
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .map(|k| k.as_bytes().to_vec())
    else {
        shared.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        return plain(StatusCode::BAD_REQUEST, "missing Sec-WebSocket-Key");
    };
    let accept = derive_accept_key(&key);
    let on_upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(Ok(upgraded)) = timeout(HANDSHAKE_TIMEOUT, on_upgrade).await else {
            return;
        };
        let ws = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            Some(ws_config()),
        )
        .await;
        handler(ws).await;
    });
    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    if let Ok(value) = HeaderValue::from_str(&accept) {
        headers.insert(SEC_WEBSOCKET_ACCEPT, value);
    }
    if let Ok(value) = HeaderValue::from_str(&shared.next_request_id()) {
        headers.insert("dg-request-id", value);
    }
    response
}

/// Small per-connection buffers (the defaults are 128 KiB each way): a thousand calls hold two
/// sockets each, and every message is flushed as soon as it is sent.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(8 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024))
}

/// Routes one request.
pub async fn route(request: Request<Incoming>, shared: Arc<Shared>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or("").to_owned();
    match path.as_str() {
        "/healthz" => plain(StatusCode::OK, "ok\n"),
        "/metrics" => text_response(
            StatusCode::OK,
            "text/plain; version=0.0.4; charset=utf-8",
            shared.metrics.render(),
        ),
        "/ca.pem" => text_response(
            StatusCode::OK,
            "application/x-pem-file",
            shared.ca_pem.clone(),
        ),
        "/v1/listen" if wants_websocket(&request) => {
            let params = stt::Params::from_query(&query);
            let task_shared = Arc::clone(&shared);
            upgrade(request, &shared, move |ws| {
                stt::run(ws, params, task_shared)
            })
        }
        "/v1/speak" if wants_websocket(&request) => {
            let params = tts::Params::from_query(&query);
            let task_shared = Arc::clone(&shared);
            upgrade(request, &shared, move |ws| {
                tts::run(ws, params, task_shared)
            })
        }
        "/v1/chat/completions" | "/chat/completions" => llm::chat(request, shared).await,
        "/v1/models" => llm::models(),
        _ => {
            shared.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
            plain(StatusCode::NOT_FOUND, "not found\n")
        }
    }
}

async fn serve_connection<I>(io: I, shared: Arc<Shared>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(shared.config.h2_max_streams)
        .keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(20));
    let service = service_fn(move |request| {
        let shared = Arc::clone(&shared);
        async move { Ok::<_, Infallible>(route(request, shared).await) }
    });
    // A connection error is a client going away; nothing to do.
    let _ = builder
        .serve_connection_with_upgrades(TokioIo::new(io), service)
        .await;
}

async fn handle_tcp(stream: TcpStream, tls: Option<TlsAcceptor>, shared: Arc<Shared>) {
    let _open = shared.metrics.connections.enter();
    let _ = stream.set_nodelay(true);
    match tls {
        None => serve_connection(stream, shared).await,
        Some(acceptor) => match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
            Ok(Ok(tls_stream)) => {
                shared
                    .metrics
                    .tls_handshakes
                    .fetch_add(1, Ordering::Relaxed);
                serve_connection(tls_stream, shared).await;
            }
            _ => {
                shared.metrics.tls_failures.fetch_add(1, Ordering::Relaxed);
            }
        },
    }
}

/// Accepts connections on `listener` until `shutdown` changes. At most `limit` connections are
/// open; further ones are closed at accept and counted.
pub async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    shared: Arc<Shared>,
    limit: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else {
                    // Out of file descriptors or a reset while accepting: back off briefly.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
                    shared.metrics.refused.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let (tls, shared) = (tls.clone(), Arc::clone(&shared));
                tokio::spawn(async move {
                    handle_tcp(stream, tls, shared).await;
                    drop(permit);
                });
            }
            _ = shutdown.changed() => break,
        }
    }
}

/// Binds `addr`, for the caller to hand to [`serve`].
///
/// # Errors
/// A message naming the address.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener, String> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))
}
