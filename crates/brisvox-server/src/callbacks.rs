//! The callback listener (`docs/OPERATIONS.md`, `docs/TELEPHONY.md`): the HTTP endpoints
//! through which the world tells a live call what the carriers' REST call control cannot return.
//! Feature `call-control`; off unless `BRISVOX_CALLBACK_ADDR` is set.
//!
//! | Request | What it does |
//! |---|---|
//! | `POST /v1/calls/{carrier}/{call}/warm/complete` | The receiving party accepted: join them to the caller (`BoundCall::complete_warm`). The bot leaves the call. |
//! | `POST /v1/calls/{carrier}/{call}/warm/abort` | The receiving party did not answer, declined or is a machine: end their leg and put the caller back (`BoundCall::abort_warm`). |
//! | `POST /v1/amd/twilio` | A Twilio AMD callback (`AsyncAmdStatusCallback`, form body): the verdict goes to the call's voicemail handler. `200 delivered`, or `202 queued` when the call has not started yet (it waits up to a minute). |
//! | `POST /v1/amd/telnyx` | A Telnyx AMD webhook (JSON body). |
//!
//! `{carrier}` is `twilio` or `telnyx`; `{call}` is the carrier's id of the call: Twilio's
//! `CallSid` of the caller's call, Telnyx's `call_control_id` (it is in the call's stream
//! metadata and in the application's own webhooks).
//!
//! # Authentication
//! Every request needs the shared secret `BRISVOX_CALLBACK_TOKEN` (at least 16 characters), as
//! `Authorization: Bearer <secret>` or, for carriers that can only put credentials in a URL
//! (`https://user:<secret>@host/...`), as the password of `Authorization: Basic`. The compare is
//! constant time; a request without valid credentials gets `401` before its body is read. The
//! secret is never logged. Carrier webhooks are not signed by this listener: put it behind TLS
//! (the secret travels in a header) and keep it off the public internet when possible (loopback
//! by default is not enforced; the operator chooses the address).
//!
//! # Why not the health listener, and why not a web framework
//! The health responder ([`crate::health`]) reads a request head and answers `GET`/`HEAD`; these
//! endpoints take a body and mutate calls, and belong on a listener of their own (a private
//! address, a secret), not on the unauthenticated probe port. The workspace has no HTTP server
//! stack, so this is the same hand-written HTTP/1.1 as the health listener: one request per
//! connection, `Connection: close`, no keep-alive, no chunked bodies, bodies of
//! [`MAX_BODY_BYTES`] at most.
//!
//! # Resources
//! At most [`CallbackSettings::max_connections`] requests run at once (a connection beyond that
//! is closed without an answer: drop newest, the carrier retries). The head is read under
//! [`CallbackSettings::read_timeout`], the handler runs under
//! [`CallbackSettings::handler_timeout`] and the answer is written under
//! [`CallbackSettings::read_timeout`], all on the injected clock. Every task is tracked and ends
//! when the listener is shut down or dropped.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock};
use brisvox_transport::call_control::{
    CallerState, parse_telnyx_amd_event, parse_twilio_amd_callback,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;

use crate::config::Env;
use crate::settings::{addr_var, var};
use crate::tools::Carrier;
use crate::tools::telephony::{CallHub, HubError, VerdictDelivery};
use crate::{Error, Result};

/// Environment variable: where the callback listener binds (`host:port`), or `off` (default).
pub const ENV_CALLBACK_ADDR: &str = "BRISVOX_CALLBACK_ADDR";
/// Environment variable: the shared secret every callback request must carry (at least
/// [`MIN_TOKEN_CHARS`] characters). Required with `BRISVOX_CALLBACK_ADDR`.
pub const ENV_CALLBACK_TOKEN: &str = "BRISVOX_CALLBACK_TOKEN";

/// Shortest accepted shared secret, in characters.
pub const MIN_TOKEN_CHARS: usize = 16;
/// Largest accepted request head (request line and headers), in bytes.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Largest accepted request body, in bytes (an AMD webhook is a few hundred bytes).
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Pause after a failed `accept` (descriptor exhaustion), so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Settings of the callback listener.
#[derive(Clone)]
pub struct CallbackSettings {
    /// Address to listen on (port 0 picks a free port, see [`CallbackServer::local_addr`]).
    pub addr: SocketAddr,
    /// The shared secret.
    pub token: Secret<String>,
    /// Most requests served at once; more are closed unanswered.
    pub max_connections: usize,
    /// Deadline for reading one request head and body, and for writing the answer.
    pub read_timeout: Duration,
    /// Deadline for the handler: it makes carrier REST calls (each bounded by the binding's own
    /// request timeout), so it is longer than the read.
    pub handler_timeout: Duration,
    /// Deadline for binding the listener.
    pub bind_timeout: Duration,
}

impl std::fmt::Debug for CallbackSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackSettings")
            .field("addr", &self.addr)
            .field("token", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

impl CallbackSettings {
    /// Settings for `addr` with the secret `token`: 32 requests at once, 5 s to read or write,
    /// 25 s for the handler, 5 s to bind.
    pub fn new(addr: SocketAddr, token: impl Into<String>) -> Self {
        Self {
            addr,
            token: Secret::new(token.into()),
            max_connections: 32,
            read_timeout: Duration::from_secs(5),
            handler_timeout: Duration::from_secs(25),
            bind_timeout: Duration::from_secs(5),
        }
    }

    /// Reads the settings from `env`: `None` when [`ENV_CALLBACK_ADDR`] is unset or `off`.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when the address does not parse, the secret is
    /// missing or shorter than [`MIN_TOKEN_CHARS`].
    pub fn from_env(env: &dyn Env) -> Result<Option<Self>> {
        let Some(value) = addr_var(env, ENV_CALLBACK_ADDR)? else {
            return Ok(None);
        };
        if value.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let addr: SocketAddr = value.parse().map_err(|e| {
            Error::Config(format!(
                "{ENV_CALLBACK_ADDR}: {value:?} is not an address like 127.0.0.1:8090 ({e})"
            ))
        })?;
        let token = var(env, ENV_CALLBACK_TOKEN)?.ok_or_else(|| {
            Error::Config(format!(
                "{ENV_CALLBACK_ADDR} is set: {ENV_CALLBACK_TOKEN} is required (every callback request is authenticated)"
            ))
        })?;
        if token.chars().count() < MIN_TOKEN_CHARS {
            return Err(Error::Config(format!(
                "{ENV_CALLBACK_TOKEN}: at least {MIN_TOKEN_CHARS} characters"
            )));
        }
        Ok(Some(Self::new(addr, token)))
    }
}

/// The running callback listener. Dropping it stops the listener without waiting; prefer
/// [`shutdown`](Self::shutdown).
#[derive(Debug)]
pub struct CallbackServer {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    clock: SharedClock,
    _stop_on_drop: DropGuard,
}

impl CallbackServer {
    /// Binds `settings.addr` and serves the callbacks of the calls in `hub`.
    ///
    /// # Errors
    /// [`Error::Config`] when `max_connections` is zero or the address cannot be bound within
    /// `bind_timeout`.
    pub async fn bind(
        settings: CallbackSettings,
        hub: CallHub,
        clock: SharedClock,
    ) -> Result<Self> {
        if settings.max_connections == 0 {
            return Err(Error::Config(
                "callback listener max_connections must be at least 1".into(),
            ));
        }
        let listener = tokio::select! {
            biased;
            bound = TcpListener::bind(settings.addr) => bound.map_err(|e| {
                Error::Config(format!("cannot bind the callback listener on {}: {e}", settings.addr))
            })?,
            () = clock.sleep(settings.bind_timeout) => {
                return Err(Error::Config(format!(
                    "binding the callback listener on {} timed out",
                    settings.addr
                )));
            }
        };
        let local_addr = listener.local_addr().map_err(|e| {
            Error::Config(format!(
                "callback listener on {} has no address: {e}",
                settings.addr
            ))
        })?;
        let cancel = CancellationToken::new();
        let tracker = TaskTracker::new();
        let acceptor = Acceptor {
            listener,
            hub,
            clock: clock.clone(),
            slots: Arc::new(Semaphore::new(settings.max_connections)),
            settings,
            cancel: cancel.clone(),
            tracker: tracker.clone(),
        };
        tracker.spawn(acceptor.run());
        tracing::info!(target: "brisvox::server", addr = %local_addr, "callback listener up");
        Ok(Self {
            local_addr,
            _stop_on_drop: cancel.clone().drop_guard(),
            cancel,
            tracker,
            clock,
        })
    }

    /// The bound address.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting, ends the requests in flight and waits for every task at most `timeout`
    /// (on the injected clock). `true` if all of them ended.
    pub async fn shutdown(self, timeout: Duration) -> bool {
        self.cancel.cancel();
        self.tracker.close();
        tokio::select! {
            biased;
            () = self.tracker.wait() => true,
            () = self.clock.sleep(timeout) => false,
        }
    }
}

struct Acceptor {
    listener: TcpListener,
    hub: CallHub,
    clock: SharedClock,
    settings: CallbackSettings,
    slots: Arc<Semaphore>,
    cancel: CancellationToken,
    tracker: TaskTracker,
}

impl Acceptor {
    async fn run(self) {
        let settings = Arc::new(self.settings.clone());
        loop {
            let accepted = tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            let stream = match accepted {
                Ok((stream, _peer)) => stream,
                Err(e) => {
                    tracing::warn!(target: "brisvox::server", error = %e, "callback accept failed");
                    tokio::select! {
                        biased;
                        () = self.cancel.cancelled() => break,
                        () = self.clock.sleep(ACCEPT_BACKOFF) => continue,
                    }
                }
            };
            let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
                tracing::debug!(target: "brisvox::server", "callback listener busy, request dropped");
                drop(stream);
                continue;
            };
            let hub = self.hub.clone();
            let clock = self.clock.clone();
            let cancel = self.cancel.clone();
            let settings = Arc::clone(&settings);
            self.tracker.spawn(async move {
                let _permit = permit;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {}
                    () = serve_connection(stream, &hub, &settings, &clock) => {}
                }
            });
        }
    }
}

/// Serves one request on `stream`: read, authenticate, handle, answer, close.
pub async fn serve_connection<S>(
    stream: S,
    hub: &CallHub,
    settings: &CallbackSettings,
    clock: &SharedClock,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_with(stream, settings, clock, |request| async move {
        route(&request, hub).await
    })
    .await;
}

/// [`serve_connection`] with the handler given (the tests stall it to see the deadline).
async fn serve_with<S, H, F>(
    mut stream: S,
    settings: &CallbackSettings,
    clock: &SharedClock,
    handle: H,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    H: FnOnce(Request) -> F,
    F: std::future::Future<Output = Response>,
{
    let request = tokio::select! {
        biased;
        read = read_request(&mut stream, settings.token.expose()) => read,
        () = clock.sleep(settings.read_timeout) => {
            tracing::debug!(target: "brisvox::server", "callback request timed out");
            return;
        }
    };
    let response = match request {
        Read::Closed => return,
        Read::Reject(response) => response,
        Read::Request(request) => tokio::select! {
            biased;
            response = handle(request) => response,
            () = clock.sleep(settings.handler_timeout) => {
                Response::error(504, "timeout", "the carrier did not answer in time")
            }
        },
    };
    let bytes = response.encode();
    tokio::select! {
        biased;
        written = stream.write_all(bytes.as_bytes()) => {
            if let Err(e) = written {
                tracing::debug!(target: "brisvox::server", error = %e, "callback answer not written");
                return;
            }
        }
        () = clock.sleep(settings.read_timeout) => return,
    }
    let _ = stream.shutdown().await;
}

/// A parsed, authenticated request.
#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// What reading a request produced.
enum Read {
    Request(Request),
    /// Answer this without handling anything (bad request, bad credentials, too large).
    Reject(Response),
    /// The peer went away before sending anything.
    Closed,
}

/// Reads the head, checks the credentials against `token`, then reads the body.
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S, token: &str) -> Read {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Read::Reject(Response::error(
                431,
                "head_too_large",
                "request header fields too large",
            ));
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) if buf.is_empty() => return Read::Closed,
            Ok(0) | Err(_) => return bad_request("the request ended inside its head"),
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    };
    let Some(head) = buf
        .get(..head_end)
        .and_then(|head| std::str::from_utf8(head).ok())
    else {
        return bad_request("the request head is not text");
    };
    let head = match Head::parse(head) {
        Ok(head) => head,
        Err(reason) => return bad_request(reason),
    };
    // Credentials before anything else is read or done.
    if !head.authorized(token) {
        let mut response = Response::error(401, "unauthorized", "credentials required");
        response.www_authenticate = true;
        return Read::Reject(response);
    }
    if head.chunked {
        return Read::Reject(Response::error(
            501,
            "chunked_body",
            "chunked request bodies are not supported",
        ));
    }
    if head.content_length > MAX_BODY_BYTES {
        return Read::Reject(Response::error(
            413,
            "body_too_large",
            "request body too large",
        ));
    }
    let body_start = head_end.saturating_add(4);
    let mut body: Vec<u8> = buf.get(body_start..).unwrap_or_default().to_vec();
    while body.len() < head.content_length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return bad_request("the request ended inside its body"),
            Ok(n) => body.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    }
    body.truncate(head.content_length);
    Read::Request(Request {
        method: head.method,
        path: head.path,
        body,
    })
}

fn bad_request(reason: &str) -> Read {
    Read::Reject(Response::error(400, "bad_request", reason))
}

/// The request line and the headers the listener looks at.
#[derive(Debug)]
struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: usize,
    chunked: bool,
}

impl Head {
    fn parse(head: &str) -> std::result::Result<Self, &'static str> {
        let mut lines = head.split("\r\n");
        let line = lines.next().unwrap_or_default();
        let mut parts = line.split(' ');
        let (Some(method), Some(target), Some(version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed request line");
        };
        if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
            return Err("malformed request line");
        }
        let path = target.split('?').next().unwrap_or(target).to_owned();
        let mut authorization = None;
        let mut content_length = 0usize;
        let mut chunked = false;
        for header in lines {
            let Some((name, value)) = header.split_once(':') else {
                return Err("malformed header");
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_owned());
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().map_err(|_| "malformed content-length")?;
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = !value.eq_ignore_ascii_case("identity");
            }
        }
        Ok(Self {
            method: method.to_owned(),
            path,
            authorization,
            content_length,
            chunked,
        })
    }

    /// Whether the request carries the shared secret (Bearer, or the password of Basic).
    fn authorized(&self, token: &str) -> bool {
        let Some(value) = &self.authorization else {
            return false;
        };
        let (scheme, credentials) = value.split_once(' ').unwrap_or((value.as_str(), ""));
        let credentials = credentials.trim();
        if scheme.eq_ignore_ascii_case("bearer") {
            return constant_time_eq(credentials.as_bytes(), token.as_bytes());
        }
        if scheme.eq_ignore_ascii_case("basic") {
            return match base64_decode(credentials) {
                Some(decoded) => {
                    let password = decoded
                        .iter()
                        .position(|b| *b == b':')
                        .and_then(|i| decoded.get(i.saturating_add(1)..))
                        .unwrap_or_default();
                    constant_time_eq(password, token.as_bytes())
                }
                None => false,
            };
        }
        false
    }
}

/// Compares two byte strings in time that depends on their lengths only.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len = a.len().max(b.len());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Decodes standard base64 (padding optional); `None` for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// Runs the request against the hub.
async fn route(request: &Request, hub: &CallHub) -> Response {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        [
            "v1",
            "calls",
            carrier,
            call,
            "warm",
            action @ ("complete" | "abort"),
        ] => {
            if request.method != "POST" {
                return Response::method_not_allowed();
            }
            let Some(carrier) = parse_carrier(carrier) else {
                return Response::error(404, "unknown_carrier", "carrier is twilio or telnyx");
            };
            // Clients percent-encode the `:` of a Telnyx id.
            let call = percent_decode(call);
            let Some(call) = call.filter(|call| valid_call_id(call)) else {
                return Response::error(400, "bad_call_id", "not a valid call id");
            };
            if *action == "complete" {
                warm_complete(hub, carrier, &call).await
            } else {
                warm_abort(hub, carrier, &call).await
            }
        }
        ["v1", "amd", carrier @ ("twilio" | "telnyx")] => {
            if request.method != "POST" {
                return Response::method_not_allowed();
            }
            amd(hub, carrier, &request.body)
        }
        _ => Response::error(404, "not_found", "not found"),
    }
}

fn parse_carrier(name: &str) -> Option<Carrier> {
    match name {
        "twilio" => Some(Carrier::Twilio),
        "telnyx" => Some(Carrier::Telnyx),
        _ => None,
    }
}

/// `segment` with its `%XX` escapes decoded; `None` for a malformed escape or text that is not
/// UTF-8.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The characters of a carrier call id (`CallRef` accepts the same).
fn valid_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'=' | b'.'))
}

async fn warm_complete(hub: &CallHub, carrier: Carrier, call: &str) -> Response {
    match hub.complete_warm(carrier, call).await {
        Ok(()) => {
            tracing::info!(target: "brisvox::server", carrier = ?carrier, "warm transfer completed by callback");
            Response::json(200, &json!({"status": "joined"}))
        }
        Err(error) => hub_error(&error),
    }
}

async fn warm_abort(hub: &CallHub, carrier: Carrier, call: &str) -> Response {
    match hub.abort_warm(carrier, call).await {
        Ok(state) => {
            tracing::info!(target: "brisvox::server", carrier = ?carrier, ?state, "warm transfer aborted by callback");
            let caller = match state {
                CallerState::Resumed => "resumed",
                CallerState::Parked => "parked",
                CallerState::Unchanged => "unchanged",
            };
            Response::json(200, &json!({"status": "aborted", "caller": caller}))
        }
        Err(error) => hub_error(&error),
    }
}

/// An AMD callback: parse, find the call, hand over the verdict.
fn amd(hub: &CallHub, carrier: &str, body: &[u8]) -> Response {
    let (carrier, call, verdict) = match carrier {
        "twilio" => {
            let Ok(text) = std::str::from_utf8(body) else {
                return Response::error(400, "bad_body", "the body is not text");
            };
            match parse_twilio_amd_callback(text) {
                Ok(result) => (Carrier::Twilio, result.call_sid, result.answered_by),
                Err(error) => return Response::error(400, "bad_body", &error.to_string()),
            }
        }
        _ => match parse_telnyx_amd_event(body) {
            Ok(Some(event)) => (Carrier::Telnyx, event.call_control_id, event.answered_by),
            Ok(None) => return Response::json(200, &json!({"status": "ignored"})),
            Err(error) => return Response::error(400, "bad_body", &error.to_string()),
        },
    };
    match hub.deliver_verdict(carrier, &call, verdict) {
        Ok(VerdictDelivery::Delivered) => Response::json(200, &json!({"status": "delivered"})),
        // The call has not started: the verdict waits for it.
        Ok(VerdictDelivery::Queued) => Response::json(202, &json!({"status": "queued"})),
        Err(error) => hub_error(&error),
    }
}

fn hub_error(error: &HubError) -> Response {
    match error {
        HubError::NoSuchCall => Response::error(404, "no_such_call", "no live call with that id"),
        HubError::NoWarmTransfer => Response::error(
            409,
            "no_warm_transfer",
            "the call has no warm transfer in progress",
        ),
        HubError::NoVoicemail => Response::error(
            409,
            "no_voicemail",
            "the call's agent has no [telephony.voicemail] section",
        ),
        HubError::Provider(detail) => Response::error(502, "carrier_error", detail),
        HubError::Timeout => Response::error(504, "timeout", "the carrier did not answer in time"),
    }
}

/// One HTTP response.
struct Response {
    status: u16,
    body: String,
    allow_post: bool,
    www_authenticate: bool,
}

impl Response {
    fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            body: body.to_string(),
            allow_post: false,
            www_authenticate: false,
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(status, &json!({"error": code, "message": message}))
    }

    fn method_not_allowed() -> Self {
        let mut response = Self::error(405, "method_not_allowed", "use POST");
        response.allow_post = true;
        response
    }

    fn encode(&self) -> String {
        let reason = match self.status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Content Too Large",
            431 => "Request Header Fields Too Large",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            504 => "Gateway Timeout",
            _ => "",
        };
        let mut out = String::with_capacity(180 + self.body.len());
        let _ = write!(
            out,
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n",
            self.status,
            self.body.len()
        );
        if self.allow_post {
            out.push_str("Allow: POST\r\n");
        }
        if self.www_authenticate {
            out.push_str("WWW-Authenticate: Bearer\r\n");
        }
        out.push_str("\r\n");
        out.push_str(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one request through `serve_with` over an in-memory stream with a handler that never
    /// answers; what the client got back.
    async fn stalled_handler_answer(handler_timeout: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut settings = CallbackSettings::new(([127, 0, 0, 1], 0).into(), "0123456789abcdef");
        settings.handler_timeout = handler_timeout;
        let clock = brisvox_core::SystemClock::shared();
        let (mut client, server) = tokio::io::duplex(4096);
        let serving = tokio::spawn(async move {
            serve_with(server, &settings, &clock, |_| {
                std::future::pending::<Response>()
            })
            .await;
        });
        client
            .write_all(b"POST /x HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut answer))
            .await
            .expect("the listener never answered a stalled handler")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the connection task did not end")
            .unwrap();
        answer
    }

    #[tokio::test]
    async fn a_handler_that_never_answers_gets_a_504_at_its_deadline() {
        let answer = stalled_handler_answer(Duration::from_millis(100)).await;
        assert!(answer.starts_with("HTTP/1.1 504 "), "{answer}");
        assert!(answer.contains("\"timeout\""), "{answer}");
    }

    #[test]
    fn the_compare_is_exact_and_length_sensitive() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret!"));
        assert!(!constant_time_eq(b"secret!", b"secret"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn base64_decodes_what_basic_auth_sends() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(base64_decode("dXNlcjpwYXNzMQ==").unwrap(), b"user:pass1");
        assert_eq!(base64_decode("dXNlcjpwYXNzMQ").unwrap(), b"user:pass1");
        assert!(base64_decode("not base64!").is_none());
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    fn head(auth: Option<&str>) -> Head {
        Head {
            method: "POST".to_owned(),
            path: "/".to_owned(),
            authorization: auth.map(str::to_owned),
            content_length: 0,
            chunked: false,
        }
    }

    #[test]
    fn bearer_and_basic_carry_the_secret_and_nothing_else_does() {
        let token = "0123456789abcdef";
        assert!(head(Some("Bearer 0123456789abcdef")).authorized(token));
        assert!(head(Some("bearer 0123456789abcdef")).authorized(token));
        assert!(!head(Some("Bearer 0123456789abcdeX")).authorized(token));
        assert!(!head(Some("Bearer")).authorized(token));
        assert!(!head(None).authorized(token));
        assert!(!head(Some("Token 0123456789abcdef")).authorized(token));
        // Basic: any user name, the secret as the password ("twilio:0123456789abcdef").
        assert!(head(Some("Basic dHdpbGlvOjAxMjM0NTY3ODlhYmNkZWY=")).authorized(token));
        // The secret as the user name, no password.
        assert!(!head(Some("Basic MDEyMzQ1Njc4OWFiY2RlZjo=")).authorized(token));
        assert!(!head(Some("Basic ???")).authorized(token));
    }

    #[test]
    fn a_request_head_is_parsed_strictly() {
        let ok = Head::parse(
            "POST /v1/amd/twilio?x=1 HTTP/1.1\r\nHost: h\r\nContent-Length: 12\r\nauthorization: Bearer t",
        )
        .unwrap();
        assert_eq!(ok.method, "POST");
        assert_eq!(ok.path, "/v1/amd/twilio");
        assert_eq!(ok.content_length, 12);
        assert_eq!(ok.authorization.as_deref(), Some("Bearer t"));
        assert!(
            Head::parse("POST /x HTTP/1.1\r\nTransfer-Encoding: chunked")
                .unwrap()
                .chunked
        );
        for bad in [
            "",
            "POST",
            "POST /x",
            "POST x HTTP/1.1",
            "POST /x HTTP/2",
            "POST /x HTTP/1.1 extra",
            "POST /x HTTP/1.1\r\nno colon",
            "POST /x HTTP/1.1\r\nContent-Length: nope",
        ] {
            assert!(Head::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn path_segments_are_percent_decoded() {
        assert_eq!(percent_decode("v3%3AAgD").as_deref(), Some("v3:AgD"));
        assert_eq!(percent_decode("plain-id_1").as_deref(), Some("plain-id_1"));
        assert_eq!(
            percent_decode("a%20b").as_deref(),
            Some("a b"),
            "checked later"
        );
        for bad in ["%", "%4", "%zz", "%ff"] {
            assert!(percent_decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn call_ids_are_checked() {
        assert!(valid_call_id("CA0123456789abcdef"));
        assert!(valid_call_id("v3:AgD-Pq_x=="));
        assert!(!valid_call_id(""));
        assert!(!valid_call_id("a b"));
        assert!(!valid_call_id("a/b"));
        assert!(!valid_call_id(&"a".repeat(257)));
    }

    #[test]
    fn the_settings_come_from_the_environment_and_never_show_the_secret() {
        use std::collections::HashMap;
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert!(CallbackSettings::from_env(&env(&[])).unwrap().is_none());
        assert!(
            CallbackSettings::from_env(&env(&[(ENV_CALLBACK_ADDR, "off")]))
                .unwrap()
                .is_none()
        );
        let good = env(&[
            (ENV_CALLBACK_ADDR, "127.0.0.1:8090"),
            (ENV_CALLBACK_TOKEN, "0123456789abcdef-secret"),
        ]);
        let settings = CallbackSettings::from_env(&good).unwrap().unwrap();
        assert_eq!(settings.addr.port(), 8090);
        assert!(!format!("{settings:?}").contains("secret"));
        let with_port = env(&[
            (ENV_CALLBACK_ADDR, "127.0.0.1:${PORT}"),
            (ENV_CALLBACK_TOKEN, "0123456789abcdef-secret"),
            ("PORT", "8123"),
        ]);
        let settings = CallbackSettings::from_env(&with_port).unwrap().unwrap();
        assert_eq!(settings.addr.port(), 8123);
        for (pairs, needle) in [
            (
                vec![
                    (ENV_CALLBACK_ADDR, "127.0.0.1:${PORT}"),
                    (ENV_CALLBACK_TOKEN, "0123456789abcdef-secret"),
                ],
                "PORT",
            ),
            (vec![(ENV_CALLBACK_ADDR, "nonsense")], ENV_CALLBACK_ADDR),
            (vec![(ENV_CALLBACK_ADDR, "127.0.0.1:1")], ENV_CALLBACK_TOKEN),
            (
                vec![
                    (ENV_CALLBACK_ADDR, "127.0.0.1:1"),
                    (ENV_CALLBACK_TOKEN, "short"),
                ],
                "at least 16",
            ),
        ] {
            let error = CallbackSettings::from_env(&env(&pairs))
                .unwrap_err()
                .to_string();
            assert!(error.contains(needle), "{error}");
        }
    }
}
