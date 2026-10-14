//! The WebSocket connect shared by the adapters: a raced TCP dial ([`dial`]), then TLS and the
//! handshake over that socket.
//!
//! This replaces `tokio_tungstenite::connect_async_tls_with_config`, which resolves the host and
//! tries the addresses sequentially: one blackholed IPv6 route then costs the whole handshake
//! deadline. The result types are the ones that function returns, so adapters keep their error
//! mapping unchanged.

use brisvox_core::Clock;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::error::UrlError;
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config};

use super::dial::dial;

/// The socket [`connect_ws`] returns (plain or TLS).
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connects to the `ws://` / `wss://` URL of `request`: the TCP connect races all resolved
/// addresses (families alternate, a new attempt every 250 ms or when the last failed, at most
/// 8; Nagle off), then TLS (`connector`) and the WebSocket handshake run over the winning
/// socket.
///
/// There is no deadline inside: the caller bounds the whole call (name lookup, dial, TLS,
/// handshake) with its own timeout. Dropping the future cancels every attempt in flight.
///
/// # Errors
/// The same [`WsError`]s as `connect_async_tls_with_config`: `Url` for a missing host or an
/// unsupported scheme, `Io` when no address connects, `Tls`, `Http` (a refused upgrade), ...
pub async fn connect_ws(
    request: Request,
    config: Option<WebSocketConfig>,
    connector: Option<Connector>,
    clock: &dyn Clock,
) -> Result<(WsStream, Response), WsError> {
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or(WsError::Url(UrlError::NoHostName))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = match uri.scheme_str() {
        Some("wss") => uri.port_u16().unwrap_or(443),
        Some("ws") => uri.port_u16().unwrap_or(80),
        _ => return Err(WsError::Url(UrlError::UnsupportedUrlScheme)),
    };
    let tcp = dial(&host, port, clock).await.map_err(WsError::Io)?;
    client_async_tls_with_config(request, tcp, config, connector).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    async fn listener() -> (tokio::net::TcpListener, u16) {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    #[tokio::test]
    async fn a_url_without_a_ws_scheme_is_a_url_error_and_nothing_is_dialed() {
        let clock = brisvox_core::clock::SystemClock;
        let request = "http://127.0.0.1:1/".into_client_request().unwrap();
        let err = connect_ws(request, None, None, &clock).await.unwrap_err();
        assert!(matches!(err, WsError::Url(UrlError::UnsupportedUrlScheme)));
    }

    #[tokio::test]
    async fn plain_ws_connects_to_an_ipv4_loopback_listener_by_ip_and_by_name() {
        let clock = brisvox_core::clock::SystemClock;
        let (l, port) = listener().await;
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = l.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                ws.close(None).await.ok();
            }
        });
        // `localhost` may resolve to ::1 first: the racer must fall through to 127.0.0.1.
        for host in ["127.0.0.1", "localhost"] {
            let request = format!("ws://{host}:{port}/")
                .into_client_request()
                .unwrap();
            let (_ws, response) = connect_ws(request, None, None, &clock).await.unwrap();
            assert_eq!(response.status().as_u16(), 101);
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_port_is_an_io_error() {
        let clock = brisvox_core::clock::SystemClock;
        let (l, port) = listener().await;
        drop(l);
        let request = format!("ws://127.0.0.1:{port}/")
            .into_client_request()
            .unwrap();
        let err = connect_ws(request, None, None, &clock).await.unwrap_err();
        assert!(matches!(err, WsError::Io(_)));
    }
}
