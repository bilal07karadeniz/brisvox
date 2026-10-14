//! Credentials at the websocket upgrade: where the token is read from, how a refusal
//! looks, and the callback of the generic listener. The RTVI listener adds its own path and origin
//! checks around the same pieces.
//!
//! A token may travel in (first one found wins):
//! 1. `Authorization: Bearer <token>`;
//! 2. the subprotocol `brisvox-token.<token>` of `Sec-WebSocket-Protocol` (a browser's
//!    `WebSocket` constructor cannot set headers; offer `["brisvox", "brisvox-token.<token>"]` and
//!    the server selects `brisvox`);
//! 3. the `?token=` query parameter (what the Pipecat client SDKs send). A query string is logged
//!    by proxies and kept in browser history: prefer 1 or 2.
//!
//! Every answer carries `Cache-Control: no-store`.

use std::sync::{Arc, Mutex, PoisonError};

use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};

use crate::auth::{self, AuthError, Authenticator, Grant};

/// The value of `key` in a query string, percent-decoded (`+` stays a plus: the JS transport
/// encodes with `encodeURIComponent`).
pub(crate) fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            percent_decode(value)
        } else {
            None
        }
    })
}

pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte == b'%' {
            let hex = text.get(at + 1..at + 3)?;
            // `from_str_radix` also accepts a leading sign (`%+1`): only two hex digits count.
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The `Sec-WebSocket-Protocol` values of the request, joined into one list.
fn subprotocols(request: &Request) -> Option<String> {
    let values: Vec<&str> = request
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(","))
}

/// The token the request carries, wherever it travels (see the module docs).
pub(crate) fn credential(request: &Request) -> Option<String> {
    request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(auth::bearer)
        .map(str::to_owned)
        .or_else(|| {
            subprotocols(request).and_then(|list| auth::subprotocol_token(&list).map(str::to_owned))
        })
        .or_else(|| {
            request
                .uri()
                .query()
                .and_then(|query| query_param(query, "token"))
        })
}

/// A refusal answer: the status and a short text, never cached.
pub(crate) fn refusal(status: StatusCode, body: &str) -> Box<ErrorResponse> {
    let mut response = ErrorResponse::new(Some(body.to_owned()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Box::new(response)
}

/// Checks the credential of an upgrade request, using a session token up. A missing credential is
/// `401`, an invalid one `403`, an oversize one `413`, a full single-use set `503`.
pub(crate) fn authenticate(
    auth: &Authenticator,
    request: &Request,
) -> Result<Grant, Box<ErrorResponse>> {
    let presented = credential(request);
    auth.check(presented.as_deref()).map_err(|error| {
        tracing::debug!(%error, "websocket upgrade refused: credential");
        match error {
            AuthError::Missing => refusal(StatusCode::UNAUTHORIZED, "token required"),
            AuthError::TooLarge => refusal(StatusCode::PAYLOAD_TOO_LARGE, "token too large"),
            AuthError::Busy => refusal(StatusCode::SERVICE_UNAVAILABLE, "server busy"),
            _ => refusal(StatusCode::FORBIDDEN, "invalid token"),
        }
    })
}

/// The accepted upgrade's answer: `Cache-Control: no-store`, and when the client offered the
/// token subprotocol, the subprotocol to select (a browser fails the connection otherwise).
pub(crate) fn accept(request: &Request, mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Some(selected) = subprotocols(request).and_then(|list| auth::selected_subprotocol(&list))
        && let Ok(value) = HeaderValue::from_str(&selected)
    {
        response
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }
    response
}

/// Where the callback leaves the grant for the connection.
pub(crate) type GrantSlot = Arc<Mutex<Option<Grant>>>;

/// The upgrade callback of the generic listener: checks the credential and leaves the grant in
/// the slot.
pub(crate) struct AuthCallback {
    pub(crate) auth: Arc<Authenticator>,
    pub(crate) slot: GrantSlot,
}

impl Callback for AuthCallback {
    #[allow(clippy::result_large_err)] // the error type is fixed by tungstenite's `Callback`
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        match authenticate(&self.auth, request) {
            Ok(grant) => {
                *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(grant);
                Ok(accept(request, response))
            }
            Err(refusal) => Err(*refusal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_accepts_two_hex_digits_and_nothing_else() {
        assert_eq!(percent_decode("abc").as_deref(), Some("abc"));
        assert_eq!(percent_decode("a%20b%2Bc+d").as_deref(), Some("a b+c+d"));
        assert_eq!(percent_decode("%C3%BC").as_deref(), Some("\u{fc}"));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("abc%"), None);
        // `from_str_radix` would take a sign: `%+1` must not decode to 0x01.
        assert_eq!(percent_decode("%+1"), None);
        // Not valid UTF-8 after decoding.
        assert_eq!(percent_decode("%ff"), None);
        assert_eq!(
            query_param("a=1&token=x%20y", "token").as_deref(),
            Some("x y")
        );
        assert_eq!(query_param("a=1", "token"), None);
        assert_eq!(query_param("token=%zz", "token"), None);
    }

    fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = tokio_tungstenite::tungstenite::http::Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn the_credential_is_read_from_the_header_the_subprotocol_or_the_query_in_that_order() {
        let all = request(
            "/ws?token=query",
            &[
                ("authorization", "Bearer header"),
                ("sec-websocket-protocol", "brisvox, brisvox-token.proto"),
            ],
        );
        assert_eq!(credential(&all).as_deref(), Some("header"));
        let proto = request(
            "/ws?token=query",
            &[("sec-websocket-protocol", "brisvox, brisvox-token.proto")],
        );
        assert_eq!(credential(&proto).as_deref(), Some("proto"));
        assert_eq!(
            credential(&request("/ws?token=query", &[])).as_deref(),
            Some("query")
        );
        assert_eq!(credential(&request("/ws", &[])), None);
        // Two Sec-WebSocket-Protocol headers are one list.
        let two = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri("/ws")
            .header("sec-websocket-protocol", "brisvox")
            .header("sec-websocket-protocol", "brisvox-token.second")
            .body(())
            .unwrap();
        assert_eq!(credential(&two).as_deref(), Some("second"));
    }

    #[test]
    fn the_accepted_answer_selects_the_plain_subprotocol_and_is_never_cached() {
        let req = request(
            "/ws",
            &[("sec-websocket-protocol", "brisvox-token.t, brisvox")],
        );
        let response = accept(&req, Response::new(()));
        assert_eq!(
            response.headers()["sec-websocket-protocol"],
            HeaderValue::from_static("brisvox")
        );
        assert_eq!(
            response.headers()["cache-control"],
            HeaderValue::from_static("no-store")
        );
        let bare = accept(&request("/ws", &[]), Response::new(()));
        assert!(bare.headers().get("sec-websocket-protocol").is_none());
    }
}
