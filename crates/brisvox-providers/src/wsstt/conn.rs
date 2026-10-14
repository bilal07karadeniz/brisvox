//! Opening the WebSocket for the local-speech adapters: URL checks, the optional bearer key,
//! TLS, one deadline for the whole handshake, and the mapping of connect failures to [`Error`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Clock, Secret};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::Settings;
use crate::error::{Error, Result};
use crate::net::connect_ws;

/// The client side of an open connection.
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where and how to connect.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Full URL including the query (never contains the key).
    pub(crate) url: String,
    /// `Authorization: Bearer <key>` when the server wants a key.
    pub(crate) bearer: Option<Secret<String>>,
    /// The shared TLS config for `wss://`; `None` for plain `ws://`.
    pub(crate) tls: Option<Arc<rustls::ClientConfig>>,
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

/// Opens a connection and completes the WebSocket handshake within `settings.io_timeout`.
///
/// # Errors
/// - `Core(Timeout)` if the handshake takes longer than the deadline.
/// - `Rejected { status }` for any HTTP answer other than the upgrade (401, 404, 429, 503...).
/// - `Disconnected` for I/O failures, `Protocol` for TLS or request problems.
pub(crate) async fn open(settings: &Settings, clock: &dyn Clock, target: &Target) -> Result<Ws> {
    let provider = settings.provider;
    let mut request = target
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| connect_error(provider, &e))?;
    if let Some(key) = &target.bearer {
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| {
                Error::Protocol {
                    provider,
                    detail: "the API key is not a valid HTTP header value".to_owned(),
                }
            })?;
        auth.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, auth);
    }
    let ws_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_message_size(Some(settings.max_message_bytes))
        .max_frame_size(Some(settings.max_message_bytes));
    let connector = target.tls.clone().map(Connector::Rustls);
    // Boxed: the TLS/WebSocket handshake future is large and sits inside every caller's state
    // machine; a connect is rare enough to pay one allocation.
    let handshake = Box::pin(connect_ws(request, Some(ws_config), connector, clock));
    match timed(
        clock,
        settings.io_timeout,
        settings.connect_label,
        handshake,
    )
    .await?
    {
        Ok((ws, _response)) => Ok(ws),
        Err(e) => Err(connect_error(provider, &e)),
    }
}

/// Maps a failed handshake to the crate error. Only status codes are logged: never the request
/// (which may hold the key) and never the response body.
fn connect_error(provider: &'static str, error: &WsError) -> Error {
    match error {
        WsError::Http(response) => {
            tracing::warn!(
                provider,
                status = response.status().as_u16(),
                "the server refused the connection"
            );
            Error::Rejected {
                provider,
                status: response.status().as_u16(),
            }
        }
        WsError::Io(io) => {
            tracing::debug!(provider, kind = ?io.kind(), "connect failed with an I/O error");
            Error::Disconnected { provider }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => Error::Disconnected { provider },
        WsError::Tls(e) => Error::Protocol {
            provider,
            detail: format!("TLS error: {e}"),
        },
        WsError::Url(e) => Error::Protocol {
            provider,
            detail: format!("invalid URL: {e}"),
        },
        other => Error::Protocol {
            provider,
            detail: format!("handshake failed: {other}"),
        },
    }
}

/// Resolves the host of `url` (bounded by `io_timeout`) and builds the shared TLS config for
/// `wss://`: what makes the next connect fast.
///
/// # Errors
/// `Core(Timeout)` when the lookup is slow, `Disconnected` when it fails.
pub(crate) async fn warm(settings: &Settings, clock: &dyn Clock, target: &Target) -> Result<()> {
    if target.tls.is_some() {
        crate::net::tls_config()?;
    }
    let Some((authority, tls)) = authority_of(&target.url) else {
        return Err(Error::Protocol {
            provider: settings.provider,
            detail: "the URL has no host".to_owned(),
        });
    };
    let address = with_default_port(authority, if tls { 443 } else { 80 });
    let what = "warm lookup";
    let lookup = timed(
        clock,
        settings.io_timeout,
        what,
        tokio::net::lookup_host(address),
    )
    .await?;
    lookup.map(drop).map_err(|_| Error::Disconnected {
        provider: settings.provider,
    })
}

/// `(host[:port], tls)` of a `ws://` / `wss://` URL.
fn authority_of(url: &str) -> Option<(&str, bool)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    Some((authority, scheme == "wss"))
}

/// `host[:port]` with `default` appended when no port is given (IPv6 literals are bracketed).
fn with_default_port(authority: &str, default: u16) -> String {
    let has_port = match authority.strip_prefix('[') {
        Some(rest) => rest.contains("]:"),
        None => authority.contains(':'),
    };
    if has_port {
        authority.to_owned()
    } else {
        format!("{authority}:{default}")
    }
}

// ---------------------------------------------------------------------------------------------
// Settings helpers shared by the adapters' `validate`
// ---------------------------------------------------------------------------------------------

/// Letters, digits, `.`, `_` and `-`, at least one character.
pub(crate) fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Strictly parses `host[:port]` (a bracketed IPv6 literal, or a name / IPv4 address of
/// letters, digits, `-` and `.`; a digits-only port) and returns the host. Anything else
/// (userinfo `@`, backslash, path, query, spaces, a stray `:`) is `None`, so the host that is
/// checked is exactly the host that is connected to.
pub(crate) fn parse_authority(authority: &str) -> Option<&str> {
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (inner, after) = v6.split_once(']')?;
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':')?),
            };
            if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
                return None;
            }
            (inner, port)
        }
        None => {
            let (host, port) = match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            {
                return None;
            }
            (host, port)
        }
    };
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(host)
}

/// Checks a `base_url` (`ws://host[:port]` or `wss://host[:port]`, nothing else) and returns
/// `(authority, tls)`.
///
/// A server that wants a key (`has_key`) is only dialed in plain text on loopback, so the key
/// can never travel in the clear; a keyless local server may be anywhere (a LAN GPU box, a
/// Docker network).
///
/// # Errors
/// [`Error::Protocol`] naming the problem.
pub(crate) fn check_base_url<'a>(
    provider: &'static str,
    base: &'a str,
    has_key: bool,
) -> Result<(&'a str, bool)> {
    let bad = |detail: &str| {
        Err(Error::Protocol {
            provider,
            detail: format!("invalid config: {detail}"),
        })
    };
    let Some((scheme, rest)) = base.split_once("://") else {
        return bad("base_url must be ws:// or wss://");
    };
    if !matches!(scheme, "ws" | "wss") {
        return bad("base_url must be ws:// or wss://");
    }
    let Some(host) = parse_authority(rest) else {
        return bad("base_url must be scheme and host[:port] only");
    };
    if scheme == "ws" && has_key && !crate::loopback::is_loopback_host(host) {
        return bad("an API key is only sent over wss:// (plain ws:// is allowed for loopback)");
    }
    Ok((rest, scheme == "wss"))
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
#[cfg(feature = "provider-openai-realtime-stt")]
pub(crate) fn percent_encode(value: &str) -> String {
    let hex = |nibble: u8| {
        char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase())
    };
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 0x0F));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_are_appended_only_when_missing() {
        assert_eq!(with_default_port("gpu.lan", 443), "gpu.lan:443");
        assert_eq!(with_default_port("127.0.0.1:9", 80), "127.0.0.1:9");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
        assert_eq!(with_default_port("[::1]:9", 80), "[::1]:9");
    }

    #[test]
    fn the_authority_is_read_from_a_url() {
        assert_eq!(
            authority_of("ws://127.0.0.1:6006/path?x=1"),
            Some(("127.0.0.1:6006", false))
        );
        assert_eq!(
            authority_of("wss://api.example.com/v1/realtime?model=m"),
            Some(("api.example.com", true))
        );
        assert_eq!(authority_of("no scheme"), None);
    }

    #[test]
    fn plain_ws_with_a_key_is_loopback_only_and_a_keyless_server_may_be_anywhere() {
        assert!(check_base_url("p", "ws://127.0.0.1:6006", true).is_ok());
        assert!(check_base_url("p", "ws://localhost", true).is_ok());
        assert!(check_base_url("p", "wss://gpu.example.com", true).is_ok());
        assert!(check_base_url("p", "ws://10.0.0.5:6006", true).is_err());
        assert!(check_base_url("p", "ws://10.0.0.5:6006", false).is_ok());
        for bad in [
            "http://x",
            "ws://",
            "ws://user@host",
            "ws://host/path",
            "ws://host:",
            "ws://host:99999999",
            "host",
        ] {
            assert!(check_base_url("p", bad, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_are_plain_words() {
        assert!(is_token("gpt-4o.mini_1"));
        assert!(!is_token(""));
        assert!(!is_token("a b"));
        assert!(!is_token("a&b"));
    }

    #[cfg(feature = "provider-openai-realtime-stt")]
    #[test]
    fn percent_encoding_keeps_unreserved_characters_only() {
        assert_eq!(percent_encode("tr-TR"), "tr-TR");
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn the_debug_output_of_a_target_hides_the_key() {
        let target = Target {
            url: "ws://127.0.0.1:1/".to_owned(),
            bearer: Some(Secret::new("sk-very-secret".to_owned())),
            tls: None,
        };
        let shown = format!("{target:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
    }
}
