//! Opening the WebSocket: URL, auth header, TLS, one deadline for the whole handshake, and the
//! mapping of connect failures to [`Error`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Clock;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::PROVIDER;
use super::config::DeepgramConfig;
use crate::error::{Error, Result};
use crate::net::connect_ws;

/// The client side of an open Deepgram connection.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted (results are a few KB; this only bounds a hostile server).
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Where and how to connect.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Full URL including the query (never contains the key).
    pub(crate) url: String,
    /// The shared TLS config for `wss://`; `None` for loopback `ws://`.
    pub(crate) tls: Option<Arc<rustls::ClientConfig>>,
}

impl Target {
    /// Target of a session.
    pub(crate) fn new(url: String, tls: Option<Arc<rustls::ClientConfig>>) -> Self {
        Self { url, tls }
    }
}

/// Awaits `future` for at most `limit` on `clock`.
///
/// # Errors
/// [`brisvox_core::Error::Timeout`] naming `what` when the deadline passes first.
pub(crate) async fn timed<T>(
    clock: &dyn Clock,
    limit: Duration,
    what: &'static str,
    future: impl Future<Output = T>,
) -> std::result::Result<T, brisvox_core::Error> {
    tokio::select! {
        biased;
        out = future => Ok(out),
        () = clock.sleep(limit) => Err(brisvox_core::Error::Timeout { what, after: limit }),
    }
}

/// Opens a connection and completes the WebSocket handshake within `cfg.io_timeout`.
///
/// # Errors
/// - `Core(Timeout)` if the handshake takes longer than the deadline.
/// - `Rejected { status }` for any HTTP answer other than the upgrade (401, 402, 429, 5xx...).
/// - `Disconnected` for I/O failures, `Protocol` for TLS or request problems.
pub(crate) async fn open(cfg: &DeepgramConfig, clock: &dyn Clock, target: &Target) -> Result<Ws> {
    let mut request = target
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| connect_error(&e))?;
    let mut auth =
        HeaderValue::from_str(&format!("Token {}", cfg.api_key.expose())).map_err(|_| {
            Error::Protocol {
                provider: PROVIDER,
                detail: "the API key is not a valid HTTP header value".to_owned(),
            }
        })?;
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let connector = target.tls.clone().map(Connector::Rustls);
    // Boxed: the TLS/WebSocket handshake future is large and sits inside every caller's state
    // machine (and stack, in debug builds); a connect is rare enough to pay one allocation.
    let handshake = Box::pin(connect_ws(request, Some(ws_config), connector, clock));
    match timed(clock, cfg.io_timeout, "deepgram connect", handshake).await? {
        Ok((ws, _response)) => Ok(ws),
        Err(e) => Err(connect_error(&e)),
    }
}

/// Maps a failed handshake to the crate error. Only status codes and ids are logged: never the
/// request (which holds the key) and never the response body.
fn connect_error(error: &WsError) -> Error {
    match error {
        WsError::Http(response) => {
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
            };
            tracing::warn!(
                status = response.status().as_u16(),
                request_id = header("dg-request-id"),
                dg_error = header("dg-error"),
                "deepgram refused the connection"
            );
            Error::Rejected {
                provider: PROVIDER,
                status: response.status().as_u16(),
            }
        }
        WsError::Io(io) => {
            tracing::debug!(kind = ?io.kind(), "connect failed with an I/O error");
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            Error::Disconnected { provider: PROVIDER }
        }
        WsError::Tls(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("TLS error: {e}"),
        },
        WsError::Url(e) => Error::Protocol {
            provider: PROVIDER,
            detail: format!("invalid URL: {e}"),
        },
        other => Error::Protocol {
            provider: PROVIDER,
            detail: format!("handshake failed: {other}"),
        },
    }
}
