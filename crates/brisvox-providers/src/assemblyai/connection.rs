//! Opening the WebSocket: URL, auth header, TLS, one deadline for the whole handshake including
//! the `Begin` message, and the mapping of connect failures and close codes to [`Error`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Clock;
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::PROVIDER;
use super::config::AssemblyAiConfig;
use super::protocol::{self, Incoming};
use crate::error::{Error, Result};
use crate::net::connect_ws;

/// The client side of an open AssemblyAI connection.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted (turns are a few KB; this only bounds a hostile server).
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Where and how to connect.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Full URL including the query (never contains the key).
    pub(crate) url: String,
    /// The shared TLS config for `wss://`; `None` for loopback `ws://`.
    pub(crate) tls: Option<Arc<rustls::ClientConfig>>,
    /// The model that was asked for, to compare with the one `Begin` reports.
    pub(crate) model: String,
}

impl Target {
    /// Target of a session.
    pub(crate) fn new(url: String, tls: Option<Arc<rustls::ClientConfig>>, model: String) -> Self {
        Self { url, tls, model }
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

/// How the adapter treats a close code of the server (`docs/providers/assemblyai.md`, "Close
/// codes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// The server refused the session and a retry would not help (or is the caller's business):
    /// `Rejected { status }`. Rate limiting is reported as 429 whichever of the two documented
    /// codes (1008, 3009) carried it.
    Rejected(u16),
    /// The adapter sent something the server does not accept (`3006` invalid message, `3007`
    /// chunk size): `Protocol`.
    Protocol(u16),
    /// A transient end (`1000`, `1011`, `3005`, `3008`, no code...): a drop worth a reconnect.
    /// Also `3006` for inactivity (a delayed `KeepAlive`, not a malformed message) and `3007` for
    /// the transmission rate (a replay that was a little too fast): both are survivable with a
    /// new connection, whose replay is paced again.
    Retry,
}

/// Classifies a close frame by its code and reason.
pub(crate) fn classify_close(code: u16, reason: &str) -> Closed {
    match code {
        3009 => Closed::Rejected(429),
        1008 if reason.to_ascii_lowercase().contains("too many") => Closed::Rejected(429),
        1008 => Closed::Rejected(1008),
        3006 if reason.to_ascii_lowercase().contains("inactivity") => Closed::Retry,
        3007 if reason.to_ascii_lowercase().contains("transmission rate") => {
            tracing::warn!(
                "assemblyai closed the session for audio sent faster than real time;                  reconnecting"
            );
            Closed::Retry
        }
        3006 | 3007 => Closed::Protocol(code),
        _ => Closed::Retry,
    }
}

/// Opens a connection, completes the WebSocket handshake and receives `Begin`, all within
/// `cfg.io_timeout`.
///
/// # Errors
/// - `Core(Timeout)` if this takes longer than the deadline.
/// - `Rejected { status }` for any HTTP answer other than the upgrade (401, 402, 429, 5xx...) and
///   for a close frame instead of `Begin` (1008 for credentials and billing, 429 for rate
///   limiting, other codes as they are).
/// - `Disconnected` for I/O failures, `Protocol` for TLS or request problems and for a first
///   message that is not `Begin`.
pub(crate) async fn open(cfg: &AssemblyAiConfig, clock: &dyn Clock, target: &Target) -> Result<Ws> {
    let mut request = target
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| connect_error(&e))?;
    // The key is the whole header value, without a `Bearer` prefix.
    let mut auth = HeaderValue::from_str(cfg.api_key.expose()).map_err(|_| Error::Protocol {
        provider: PROVIDER,
        detail: "the API key is not a valid HTTP header value".to_owned(),
    })?;
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let connector = target.tls.clone().map(Connector::Rustls);
    let model = target.model.as_str();
    let connect = async move {
        // Boxed: the TLS/WebSocket handshake future is large and sits inside every caller's
        // state machine (and stack, in debug builds); a connect is rare enough to pay one
        // allocation.
        let handshake = Box::pin(connect_ws(request, Some(ws_config), connector, clock));
        let (mut ws, _response) = handshake.await.map_err(|e| connect_error(&e))?;
        await_begin(&mut ws, model).await?;
        Ok::<Ws, Error>(ws)
    };
    timed(clock, cfg.io_timeout, "assemblyai connect", connect).await?
}

/// Reads until the `Begin` message. A close frame here is how a bad key or a rate limit may be
/// reported (the docs do not say whether that happens before or after the upgrade).
async fn await_begin(ws: &mut Ws, model: &str) -> Result<()> {
    loop {
        match ws.next().await {
            None | Some(Err(_)) => return Err(Error::Disconnected { provider: PROVIDER }),
            Some(Ok(Message::Text(text))) => match protocol::parse(text.as_str()) {
                Ok(Incoming::Begin { model: reported }) => {
                    if reported.as_deref().is_some_and(|m| m != model) {
                        tracing::warn!(
                            requested = model,
                            reported = reported.as_deref().unwrap_or(""),
                            "assemblyai runs another speech model than requested"
                        );
                    }
                    return Ok(());
                }
                // An `Error` frame precedes the close that says what it was.
                Ok(Incoming::Error { description }) => {
                    tracing::debug!(description = %description, "assemblyai error before Begin");
                }
                Ok(_) | Err(_) => {
                    return Err(Error::Protocol {
                        provider: PROVIDER,
                        detail: "the first message was not Begin".to_owned(),
                    });
                }
            },
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = frame.map_or((1005_u16, String::new()), |f| {
                    (u16::from(f.code), f.reason.to_string())
                });
                tracing::warn!(code, "assemblyai closed the connection before Begin");
                return Err(match classify_close(code, &reason) {
                    Closed::Protocol(code) => Error::Protocol {
                        provider: PROVIDER,
                        detail: format!("rejected at connect with close code {code}"),
                    },
                    Closed::Rejected(status) => Error::Rejected {
                        provider: PROVIDER,
                        status,
                    },
                    Closed::Retry => Error::Rejected {
                        provider: PROVIDER,
                        status: code,
                    },
                });
            }
            Some(Ok(_)) => {}
        }
    }
}

/// Maps a failed handshake to the crate error. Only status codes are logged: never the request
/// (which holds the key) and never the response body.
fn connect_error(error: &WsError) -> Error {
    match error {
        WsError::Http(response) => {
            tracing::warn!(
                status = response.status().as_u16(),
                "assemblyai refused the connection"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_are_classified_as_documented() {
        assert_eq!(classify_close(3009, "x"), Closed::Rejected(429));
        assert_eq!(
            classify_close(
                1008,
                "Unauthorized Connection: Too many concurrent sessions"
            ),
            Closed::Rejected(429)
        );
        assert_eq!(
            classify_close(
                1008,
                "Unauthorized Connection: Missing Authorization header"
            ),
            Closed::Rejected(1008)
        );
        assert_eq!(classify_close(3006, ""), Closed::Protocol(3006));
        assert_eq!(classify_close(3007, ""), Closed::Protocol(3007));
        assert_eq!(
            classify_close(
                3007,
                "Input duration violation: 20 ms. Expected between 50 and 1000 ms"
            ),
            Closed::Protocol(3007)
        );
        assert_eq!(
            classify_close(
                3006,
                "Session terminated due to inactivity: No messages received for 10 seconds"
            ),
            Closed::Retry
        );
        assert_eq!(
            classify_close(
                3007,
                "Audio Transmission Rate Exceeded: Received 3.0 sec. audio in 1.0 sec"
            ),
            Closed::Retry
        );
        for code in [1000, 1005, 1011, 3005, 3008] {
            assert_eq!(classify_close(code, ""), Closed::Retry, "{code}");
        }
    }
}
