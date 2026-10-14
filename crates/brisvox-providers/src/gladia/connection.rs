//! Opening a session: the HTTP init call, then the WebSocket on the returned URL, both inside one
//! deadline, and the mapping of their failures to [`Error`].
//!
//! The init response carries a WebSocket URL with a temporary session token in its query. That
//! URL is a credential: it is never logged, formatted into an error, or stored beyond the
//! handshake.
//!
//! A session that init created but whose WebSocket never opened (upgrade refused or stalled, TLS
//! failure, the connect cancelled or past its deadline) is deleted again on a best-effort basis
//! (`DELETE /v2/live/{id}`, detached, bounded by `close_timeout`), see [`SessionGuard`].

use std::future::Future;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::PROVIDER;
use super::config::{GladiaConfig, is_loopback_host};
use crate::error::{Error, Result};
use crate::net::connect_ws;
use crate::stt::SttConfig;

/// The client side of an open Gladia session.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest server message accepted (transcripts are a few KB; this only bounds a hostile server).
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Largest init response body read.
const MAX_INIT_BODY: usize = 64 * 1024;

/// What every session and every reconnect shares: settings, clock and the HTTP client (with its
/// connection pool and TLS session cache).
pub(crate) struct Shared {
    pub(crate) cfg: GladiaConfig,
    pub(crate) clock: SharedClock,
    client: reqwest::Client,
    init_url: String,
    /// `{base}/v2/live/`: a session id is appended for the clean-up `DELETE`.
    session_url_prefix: String,
    key: HeaderValue,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("cfg", &self.cfg)
            .field("init_url", &self.init_url)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Validates `cfg` and builds the HTTP client (rustls with the shared config of
    /// [`crate::net::tls_config`], no system proxy, no redirects so the key never follows one).
    pub(crate) fn new(cfg: GladiaConfig, clock: SharedClock) -> Result<Self> {
        cfg.validate()?;
        let mut key = HeaderValue::from_str(cfg.api_key.expose()).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "the API key is not a valid HTTP header value".to_owned(),
        })?;
        key.set_sensitive(true);
        let mut tls = (*crate::net::tls_config()?).clone();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("brisvox/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(cfg.io_timeout)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .pool_idle_timeout(cfg.pool_idle_timeout)
            .build()
            .map_err(|e| Error::Protocol {
                provider: PROVIDER,
                detail: format!("cannot build the HTTP client: {e}"),
            })?;
        let init_url = cfg.init_url();
        let session_url_prefix = cfg.session_url_prefix();
        Ok(Self {
            cfg,
            clock,
            client,
            init_url,
            session_url_prefix,
            key,
        })
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.clock
    }

    /// Resolves DNS, completes TLS and pools a connection for the next init call. Sends no
    /// credentials: any HTTP answer (401, 404, 405...) proves the connection works.
    pub(crate) async fn warm(&self) -> Result<()> {
        let head = self.client.head(&self.init_url).send();
        timed(self.clock(), self.cfg.io_timeout, "gladia warm", head)
            .await?
            .map(drop)
            .map_err(|e| transport_error(&e, self.cfg.io_timeout, "gladia warm"))
    }

    /// Opens a new session: init call, then the WebSocket, within `io_timeout` in total.
    ///
    /// # Errors
    /// - `Core(Timeout)` if init plus handshake take longer than the deadline.
    /// - `Rejected { status }` for any HTTP answer other than success (401, 429, 5xx...), at init
    ///   or at the WebSocket upgrade.
    /// - `Disconnected` for I/O failures, `Protocol` for a malformed init response, a URL the
    ///   adapter will not connect to, or TLS problems.
    pub(crate) async fn open(&self, stt: &SttConfig) -> Result<Ws> {
        let body = self.cfg.init_body(stt)?;
        let connect = async {
            let created = self.init(body).await?;
            // From here on the server holds a session. If the socket does not come up (error,
            // timeout or this future being dropped) the guard deletes it again.
            let mut guard = self.guard_for(created.id);
            let ws = self.open_ws(&created.url).await?;
            guard.disarm();
            Ok(ws)
        };
        timed(self.clock(), self.cfg.io_timeout, "gladia connect", connect).await?
    }

    /// A guard that deletes session `id` when dropped armed (nothing to delete without an id).
    fn guard_for(&self, id: Option<SessionId>) -> SessionGuard {
        SessionGuard {
            cleanup: id.map(|id| Cleanup {
                client: self.client.clone(),
                url: brisvox_core::Secret::new(format!("{}{}", self.session_url_prefix, id.0)),
                key: self.key.clone(),
                clock: self.clock.clone(),
                limit: self.cfg.close_timeout,
            }),
        }
    }

    /// `POST /v2/live`: returns the session's WebSocket URL (and its id, for clean-up).
    async fn init(&self, body: Vec<u8>) -> Result<Created> {
        let request = self
            .client
            .post(&self.init_url)
            .header(HeaderName::from_static("x-gladia-key"), self.key.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        let mut response = request
            .send()
            .await
            .map_err(|e| transport_error(&e, self.cfg.io_timeout, "gladia init"))?;
        let status = response.status();
        let bytes = read_body(&mut response).await;
        if !status.is_success() {
            // Only the status and Gladia's request id are logged: never the body (it may echo
            // request fields) and never the request.
            tracing::warn!(
                status = status.as_u16(),
                request_id = error_request_id(&bytes).as_deref(),
                "gladia refused the init call"
            );
            return Err(Error::Rejected {
                provider: PROVIDER,
                status: status.as_u16(),
            });
        }
        parse_init(&bytes)
    }

    /// Completes the WebSocket handshake on `url`.
    async fn open_ws(&self, url: &SessionUrl) -> Result<Ws> {
        let request = url
            .0
            .expose()
            .as_str()
            .into_client_request()
            .map_err(|e| connect_error(&e))?;
        let ws_config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let connector = if url.0.expose().starts_with("wss://") {
            Some(Connector::Rustls(crate::net::tls_config()?))
        } else {
            None
        };
        // Boxed: the TLS/WebSocket handshake future is large and sits inside every caller's state
        // machine (and stack, in debug builds); a connect is rare enough to pay one allocation.
        let handshake = Box::pin(connect_ws(
            request,
            Some(ws_config),
            connector,
            self.clock(),
        ));
        match handshake.await {
            Ok((ws, _response)) => Ok(ws),
            Err(e) => Err(connect_error(&e)),
        }
    }
}

/// A `ws(s)://` URL that carries the session token.
struct SessionUrl(brisvox_core::Secret<String>);

/// A successful init: the URL to open and, if the response had a usable one, the session id.
struct Created {
    url: SessionUrl,
    id: Option<SessionId>,
}

/// The id of a created session, restricted to the characters of a path segment (letters, digits,
/// `-`, `_`; at most 128). Gladia's quickstart shows the id equal to the socket token, so it is
/// treated as a credential: never logged or formatted.
struct SessionId(String);

impl SessionId {
    fn parse(raw: &serde_json::Value) -> Option<Self> {
        let id = raw.as_str()?;
        let valid = !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        valid.then(|| Self(id.to_owned()))
    }
}

/// What the clean-up `DELETE` of an orphaned session needs (owned, to outlive the connect).
struct Cleanup {
    client: reqwest::Client,
    url: brisvox_core::Secret<String>,
    key: HeaderValue,
    clock: SharedClock,
    limit: Duration,
}

/// Deletes a session that init created but that never got a socket. Armed from the moment init
/// answers until the WebSocket is open; dropping it armed (handshake error, deadline, or the
/// connect future being cancelled) spawns one detached `DELETE /v2/live/{id}`, bounded by
/// `close_timeout`. Best effort: the answer is ignored and a failure is only a debug line
/// without the URL. Whether Gladia frees the concurrency slot of a session that never streamed
/// is UNVERIFIED (see `docs/providers/gladia.md`). Without a Tokio runtime nothing is sent.
struct SessionGuard {
    cleanup: Option<Cleanup>,
}

impl SessionGuard {
    /// The socket is open: the session is in use, leave it alone.
    fn disarm(&mut self) {
        self.cleanup = None;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let Some(cleanup) = self.cleanup.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let request = cleanup
                .client
                .delete(cleanup.url.expose().as_str())
                .header(HeaderName::from_static("x-gladia-key"), cleanup.key.clone())
                .send();
            match timed(&*cleanup.clock, cleanup.limit, "gladia cleanup", request).await {
                Ok(Ok(response)) => {
                    tracing::debug!(
                        status = response.status().as_u16(),
                        "deleted an orphaned gladia session"
                    );
                }
                Ok(Err(_)) | Err(_) => {
                    tracing::debug!("could not delete an orphaned gladia session");
                }
            }
        });
    }
}

#[derive(Deserialize)]
struct RawInit {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    id: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    request_id: Option<String>,
}

/// The session URL of a successful init response.
///
/// The adapter connects only to `wss://`, or to `ws://` on a loopback host (tests): the token in
/// the URL must not travel in the clear.
fn parse_init(body: &[u8]) -> Result<Created> {
    let protocol = |detail: &str| Error::Protocol {
        provider: PROVIDER,
        detail: detail.to_owned(),
    };
    let raw: RawInit =
        serde_json::from_slice(body).map_err(|_| protocol("malformed init response"))?;
    let url = raw
        .url
        .ok_or_else(|| protocol("the init response has no url"))?;
    let parsed =
        reqwest::Url::parse(&url).map_err(|_| protocol("the init response url is invalid"))?;
    let allowed = match parsed.scheme() {
        "wss" => true,
        "ws" => parsed.host_str().is_some_and(is_loopback_host),
        _ => false,
    };
    if !allowed || parsed.host_str().is_none() {
        return Err(protocol(
            "the init response url is not wss:// (or ws:// on loopback)",
        ));
    }
    Ok(Created {
        url: SessionUrl(brisvox_core::Secret::new(url)),
        id: raw.id.as_ref().and_then(SessionId::parse),
    })
}

/// Gladia's `request_id` of an error body, sanitised for logging.
fn error_request_id(body: &[u8]) -> Option<String> {
    let raw: RawError = serde_json::from_slice(body).ok()?;
    let id: String = raw
        .request_id?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Reads at most [`MAX_INIT_BODY`] bytes of the response body; a body that fails to read is just
/// shorter. The caller's deadline bounds the wait.
async fn read_body(response: &mut reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_INIT_BODY.saturating_sub(body.len());
        body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if body.len() >= MAX_INIT_BODY {
            break;
        }
    }
    body
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

/// A transport error of the init call as a provider error: a timeout stays a timeout, everything
/// else (refused, reset, TLS failure, truncated body) is a lost connection.
fn transport_error(error: &reqwest::Error, limit: Duration, what: &'static str) -> Error {
    tracing::debug!(%error, "gladia transport error");
    if error.is_timeout() {
        Error::Core(brisvox_core::Error::Timeout { what, after: limit })
    } else {
        Error::Disconnected { provider: PROVIDER }
    }
}

/// Maps a failed WebSocket handshake to the crate error. Only status codes are logged: never the
/// request (its URL holds the token) and never the response body.
fn connect_error(error: &WsError) -> Error {
    match error {
        WsError::Http(response) => {
            tracing::warn!(
                status = response.status().as_u16(),
                "gladia refused the WebSocket connection"
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
        WsError::Url(_) => Error::Protocol {
            provider: PROVIDER,
            detail: "invalid WebSocket URL".to_owned(),
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

    fn url_of(result: Result<Created>) -> Option<String> {
        result.ok().map(|c| c.url.0.expose().clone())
    }

    #[test]
    fn only_wss_or_loopback_ws_urls_are_accepted() {
        let body = |u: &str| format!(r#"{{"id":"x","created_at":"t","url":"{u}"}}"#);
        assert_eq!(
            url_of(parse_init(
                body("wss://api.gladia.io/v2/live?token=t").as_bytes()
            ))
            .as_deref(),
            Some("wss://api.gladia.io/v2/live?token=t")
        );
        assert!(
            url_of(parse_init(
                body("ws://127.0.0.1:9/v2/live?token=t").as_bytes()
            ))
            .is_some()
        );
        assert!(
            url_of(parse_init(
                body("ws://localhost/v2/live?token=t").as_bytes()
            ))
            .is_some()
        );
        for bad in [
            "ws://api.gladia.io/v2/live?token=t",
            "https://api.gladia.io/v2/live?token=t",
            "not a url",
            "wss://",
        ] {
            assert!(parse_init(body(bad).as_bytes()).is_err(), "{bad}");
        }
        assert!(parse_init(br#"{"id":"x"}"#).is_err());
        assert!(parse_init(b"<html>").is_err());
    }

    #[test]
    fn the_session_id_is_kept_only_when_it_is_a_safe_path_segment() {
        let id_of = |body: &str| {
            parse_init(body.as_bytes())
                .ok()
                .and_then(|c| c.id)
                .map(|i| i.0)
        };
        let with =
            |id: &str| format!(r#"{{"id":{id},"url":"wss://api.gladia.io/v2/live?token=t"}}"#);
        assert_eq!(id_of(&with(r#""abc-123_X""#)).as_deref(), Some("abc-123_X"));
        for bad in [
            r#""..%2Fadmin""#,
            r#""a/b""#,
            r#""a?b""#,
            r#""a b""#,
            "12",
            "null",
        ] {
            assert_eq!(id_of(&with(bad)), None, "{bad}");
        }
        assert_eq!(id_of(&with(&format!("\"{}\"", "a".repeat(129)))), None);
        // No id at all does not fail the init.
        assert!(parse_init(br#"{"url":"wss://api.gladia.io/v2/live?token=t"}"#).is_ok());
    }

    #[test]
    fn parse_errors_never_contain_the_url() {
        let err = parse_init(br#"{"url":"ws://evil.example/v2/live?token=SECRET-TOKEN"}"#)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!err.is_empty() && !err.contains("SECRET-TOKEN"));
        assert!(
            !format!(
                "{:?}",
                parse_init(br#"{"url":"wss://a.example/?token=SECRET-TOKEN"}"#)
                    .ok()
                    .map(|c| c.url.0)
            )
            .contains("SECRET-TOKEN")
        );
    }

    #[test]
    fn the_request_id_of_an_error_body_is_sanitised() {
        assert_eq!(
            error_request_id(br#"{"request_id":"G-abc123\n<x>","statusCode":401}"#).as_deref(),
            Some("G-abc123x")
        );
        assert_eq!(error_request_id(b"nope"), None);
        assert_eq!(error_request_id(br#"{"request_id":"!!"}"#), None);
    }
}
