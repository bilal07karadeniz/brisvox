//! `/healthz` and `/readyz` over a minimal HTTP/1.1 listener (`docs/OPERATIONS.md`).
//!
//! * `GET /healthz` is liveness: `200 {"status":"ok"}` for as long as the process serves the
//!   listener, draining or not. A failing liveness probe restarts the container, which a drain
//!   must never cause.
//! * `GET /readyz` is readiness: `200` while the [`SessionManager`] admits sessions, `503` once
//!   draining began (permanent) or while every slot is taken (so a load balancer routes new
//!   calls elsewhere), or while admission control refuses for pool pressure (`overloaded`) or
//!   an exhausted provider budget (`provider_budget`). The body says which, with the
//!   counts and the admission metrics:
//!   `{"status":"ready","accepting":true,"active":3,"reserved":0,"max":256,"rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},"providers":[]}`;
//!   `status` is `ready`, `full`, `overloaded`, `provider_budget` or `draining`. With a pressure
//!   gauge there is also a `"pressure"` object, and each limited provider is listed under
//!   `"providers"` with its limit, calls in use and refusals.
//! * `GET /metrics` is the Prometheus text exposition ([`crate::observability`]): stage
//!   latency histograms, sessions, admission, provider errors and the resilience layer's state.
//!   `404` unless an [`Observability`](crate::observability::Observability) is attached to the
//!   manager and its endpoint is on (`BRISVOX_METRICS`). On a health address that is not
//!   loopback it is also gated by [`MetricsAccess`]: `Authorization: Bearer
//!   <BRISVOX_METRICS_TOKEN>` (`401` without it), or `404` when no token is configured. The
//!   probes stay open: a kubelet or a container runtime probes the pod's address, not loopback,
//!   and they carry only counts.
//! * `POST /admin/reload` reloads the agent definition(s) without a restart (`docs/OPERATIONS.md`,
//!   "Reloading the agent"; [`crate::reload`]). It changes what new calls run, so it has an
//!   access policy of its own, [`AdminAccess`]: a bearer token (`BRISVOX_ADMIN_TOKEN`), or no
//!   token on a loopback address (a request carrying an `Origin` header, which only a browser
//!   sends, is refused then); `404` when neither applies, as if the endpoint were off. The
//!   answer is JSON: `200` applied or unchanged, `422` the new file is invalid (the old
//!   definition stays), `409` the change needs a restart, `503` draining, `504` too slow. At most
//!   [`MAX_ADMIN_REQUESTS`] are handled at once (a reload can take minutes and each holds a
//!   connection): the next gets `429` so the probes keep their slots. A body up to 4 KiB is read
//!   and ignored, a larger one is `413`.
//! * `HEAD` answers like `GET` without the body; any other method is `405`, any other path
//!   `404`, a malformed request `400`, a request head over [`MAX_REQUEST_BYTES`] `431`.
//!
//! # Why not a web framework
//! The workspace has no HTTP server stack (hyper is in the tree only as reqwest's client, and
//! the transports' upgrade handshakes are tungstenite's), and two fixed probe paths do not
//! justify adding one. The listener reads one request head (bounded, under one deadline on
//! the injected clock), writes one response with `Connection: close` and closes. It is not a
//! general HTTP server: no keep-alive, no bodies, no chunked encoding.
//!
//! # Resources
//! At most [`HealthConfig::max_connections`] probes are served at once; a connection beyond
//! that is closed without an answer (overflow policy: drop newest, the prober retries). Each
//! connection gets [`HealthConfig::request_timeout`] for reading the request and writing the
//! answer. Every task is tracked and stops when the server is shut down or dropped.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SharedClock};
use brisvox_transport::auth::{bearer, constant_time_eq};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;

use crate::admission::AdmissionSnapshot;
use crate::session::SessionManager;
use crate::{Error, Result};

/// Largest accepted request head (request line and headers), in bytes.
pub const MAX_REQUEST_BYTES: usize = 4096;

/// Pause after a failed `accept` (descriptor exhaustion), so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Settings of a [`HealthServer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    /// Address to listen on (port 0 picks a free port, see [`HealthServer::local_addr`]).
    pub addr: SocketAddr,
    /// Most probes served at once; more are closed unanswered.
    pub max_connections: usize,
    /// Deadline for reading one request and writing its answer.
    pub request_timeout: Duration,
    /// Deadline for binding the listener.
    pub bind_timeout: Duration,
}

impl HealthConfig {
    /// Defaults for listening on `addr`: 32 connections, 2 s per request, 5 s to bind.
    pub const fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            max_connections: 32,
            request_timeout: Duration::from_secs(2),
            bind_timeout: Duration::from_secs(5),
        }
    }
}

/// Who may read `/metrics`. Counts, error rates and latencies of a voice service are
/// not public data.
#[derive(Clone, Default)]
pub enum MetricsAccess {
    /// Anyone who reaches the listener (what [`HealthServer::bind`] does; right on loopback).
    #[default]
    Open,
    /// Only a request with `Authorization: Bearer <token>`.
    Token(Secret<String>),
    /// Nobody: `/metrics` answers `404`, as if the endpoint were off.
    Disabled,
}

impl std::fmt::Debug for MetricsAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Open => "Open",
            Self::Token(_) => "Token([REDACTED])",
            Self::Disabled => "Disabled",
        })
    }
}

impl MetricsAccess {
    /// The access a process should give `/metrics` on `addr`: open on loopback, behind `token`
    /// when there is one, open when `insecure` says so (`BRISVOX_INSECURE_LISTENERS`), otherwise
    /// off.
    pub fn for_listener(addr: SocketAddr, token: Option<&Secret<String>>, insecure: bool) -> Self {
        if addr.ip().is_loopback() {
            Self::Open
        } else if let Some(token) = token {
            Self::Token(token.clone())
        } else if insecure {
            Self::Open
        } else {
            Self::Disabled
        }
    }

    fn permits(&self, head: &str) -> Option<bool> {
        match self {
            Self::Open => Some(true),
            Self::Disabled => None,
            Self::Token(token) => Some(bearer_matches(head, token)),
        }
    }
}

/// Whether the request head carries `Authorization: Bearer <token>`.
fn bearer_matches(head: &str, token: &Secret<String>) -> bool {
    header(head, "authorization")
        .and_then(bearer)
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.expose().as_bytes()))
}

/// The value of the request header `name` (case-insensitive), trimmed.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Who may `POST /admin/reload`: the one endpoint of the health listener that changes
/// anything. A scrape token ([`MetricsAccess`]) never opens it.
#[derive(Clone, Default)]
pub enum AdminAccess {
    /// Nobody: the endpoint answers `404`, as if it were off.
    #[default]
    Disabled,
    /// A loopback listener without a token: any local process, but not a browser (a request
    /// with an `Origin` header is refused with `403`, so a web page cannot trigger a reload).
    Local,
    /// Only a request with `Authorization: Bearer <token>` (`401` without it).
    Token(Secret<String>),
}

impl std::fmt::Debug for AdminAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "Disabled",
            Self::Local => "Local",
            Self::Token(_) => "Token([REDACTED])",
        })
    }
}

/// The verdict of [`AdminAccess`] on one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminVerdict {
    Allowed,
    NotFound,
    Unauthorized,
    Forbidden,
}

impl AdminAccess {
    /// The access a process should give the admin endpoint on `addr`: `token` when there is
    /// one, open on loopback otherwise, off on any other address. `BRISVOX_INSECURE_LISTENERS`
    /// does not open it: an endpoint that changes what calls run is never served to the network
    /// without a credential.
    pub fn for_listener(addr: SocketAddr, token: Option<&Secret<String>>) -> Self {
        match token {
            Some(token) => Self::Token(token.clone()),
            None if addr.ip().is_loopback() => Self::Local,
            None => Self::Disabled,
        }
    }

    fn check(&self, head: &str) -> AdminVerdict {
        match self {
            Self::Disabled => AdminVerdict::NotFound,
            Self::Local if header(head, "origin").is_some() => AdminVerdict::Forbidden,
            Self::Local => AdminVerdict::Allowed,
            Self::Token(token) if bearer_matches(head, token) => AdminVerdict::Allowed,
            Self::Token(_) => AdminVerdict::Unauthorized,
        }
    }
}

/// What `/readyz` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyStatus {
    /// Admitting sessions.
    Ready,
    /// Every slot is live or reserved; admissions are refused until one frees up.
    Full,
    /// The inference pool is under pressure: admissions are refused until it calms down.
    Overloaded,
    /// A provider is at its concurrency limit, so no call can be admitted until one ends.
    ProviderBudget,
    /// Draining or shut down: admissions are refused for good.
    Draining,
}

impl ReadyStatus {
    /// The `status` value in the `/readyz` body.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Full => "full",
            Self::Overloaded => "overloaded",
            Self::ProviderBudget => "provider_budget",
            Self::Draining => "draining",
        }
    }

    /// The HTTP status code of `/readyz`: 200 when ready, 503 otherwise.
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Ready => 200,
            Self::Full | Self::Overloaded | Self::ProviderBudget | Self::Draining => 503,
        }
    }
}

/// A `/readyz` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// The verdict.
    pub status: ReadyStatus,
    /// Sessions running.
    pub active: usize,
    /// Slots reserved but not yet spawned into.
    pub reserved: usize,
    /// The admission limit.
    pub max: usize,
    /// Admissions refused so far, by reason.
    pub rejected: RejectedCounts,
    /// The admission policy's pressure verdict and provider budgets.
    pub admission: AdmissionSnapshot,
}

/// Admissions refused since the server started, by reason (the JSON `"rejected"` object).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RejectedCounts {
    /// The session limit was reached.
    pub full: u64,
    /// The inference pool was under pressure.
    pub overloaded: u64,
    /// A provider was at its concurrency limit.
    pub provider_budget: u64,
    /// The server was draining.
    pub shutting_down: u64,
}

impl Readiness {
    /// The readiness of `manager` right now.
    pub fn of(manager: &SessionManager) -> Self {
        let stats = manager.stats();
        let admission = manager.admission_snapshot();
        let status = if !manager.is_accepting() {
            ReadyStatus::Draining
        } else if stats.active.saturating_add(stats.reserved) >= stats.max_sessions {
            ReadyStatus::Full
        } else if admission.overloaded() {
            ReadyStatus::Overloaded
        } else if admission.exhausted_provider().is_some() {
            ReadyStatus::ProviderBudget
        } else {
            ReadyStatus::Ready
        };
        Self {
            status,
            active: stats.active,
            reserved: stats.reserved,
            max: stats.max_sessions,
            rejected: RejectedCounts {
                full: stats.rejected_at_capacity,
                overloaded: stats.rejected_overloaded,
                provider_budget: stats.rejected_provider_budget,
                shutting_down: stats.rejected_shutting_down,
            },
            admission,
        }
    }

    /// The JSON body of `/readyz`.
    pub fn to_json(&self) -> String {
        let mut out = format!(
            r#"{{"status":"{}","accepting":{},"active":{},"reserved":{},"max":{},"rejected":{{"full":{},"overloaded":{},"provider_budget":{},"shutting_down":{}}}"#,
            self.status.as_str(),
            self.status != ReadyStatus::Draining,
            self.active,
            self.reserved,
            self.max,
            self.rejected.full,
            self.rejected.overloaded,
            self.rejected.provider_budget,
            self.rejected.shutting_down,
        );
        self.admission.write_json(&mut out);
        out.push('}');
        out
    }
}

/// The running health listener. Dropping it stops the listener without waiting; prefer
/// [`shutdown`](Self::shutdown).
#[derive(Debug)]
pub struct HealthServer {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    clock: SharedClock,
    _stop_on_drop: DropGuard,
}

impl HealthServer {
    /// Binds `config.addr` and serves the probes of `manager` until shut down.
    ///
    /// # Errors
    /// [`Error::Config`] when `max_connections` is zero or the address cannot be bound within
    /// `bind_timeout` (in use, no permission).
    pub async fn bind(
        config: HealthConfig,
        manager: SessionManager,
        clock: SharedClock,
    ) -> Result<Self> {
        Self::bind_with_metrics_access(config, manager, clock, MetricsAccess::Open).await
    }

    /// [`bind`](Self::bind) with `metrics` deciding who may read `/metrics`.
    ///
    /// # Errors
    /// As [`bind`](Self::bind).
    pub async fn bind_with_metrics_access(
        config: HealthConfig,
        manager: SessionManager,
        clock: SharedClock,
        metrics: MetricsAccess,
    ) -> Result<Self> {
        Self::bind_with_access(config, manager, clock, metrics, AdminAccess::Disabled).await
    }

    /// [`bind_with_metrics_access`](Self::bind_with_metrics_access) with `admin` deciding who
    /// may `POST /admin/reload` (the reload hub is attached to the manager,
    /// [`SessionManager::attach_reload`]).
    ///
    /// # Errors
    /// As [`bind`](Self::bind).
    pub async fn bind_with_access(
        config: HealthConfig,
        manager: SessionManager,
        clock: SharedClock,
        metrics: MetricsAccess,
        admin: AdminAccess,
    ) -> Result<Self> {
        if config.max_connections == 0 {
            return Err(Error::Config(
                "health listener max_connections must be at least 1".into(),
            ));
        }
        let listener = tokio::select! {
            biased;
            bound = TcpListener::bind(config.addr) => bound.map_err(|e| {
                Error::Config(format!("cannot bind health listener on {}: {e}", config.addr))
            })?,
            () = clock.sleep(config.bind_timeout) => {
                return Err(Error::Config(format!(
                    "binding the health listener on {} timed out",
                    config.addr
                )));
            }
        };
        let local_addr = listener.local_addr().map_err(|e| {
            Error::Config(format!(
                "health listener on {} has no address: {e}",
                config.addr
            ))
        })?;
        let cancel = CancellationToken::new();
        let tracker = TaskTracker::new();
        let acceptor = Acceptor {
            listener,
            manager,
            clock: clock.clone(),
            config,
            cancel: cancel.clone(),
            tracker: tracker.clone(),
            slots: Arc::new(Semaphore::new(config.max_connections)),
            access: Arc::new(Access::new(metrics, admin)),
        };
        tracker.spawn(acceptor.run());
        tracing::info!(target: "brisvox::server", addr = %local_addr, "health listener up");
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

    /// Stops accepting, ends the connections in flight and waits for every task at most
    /// `timeout` (on the injected clock). `true` if all of them ended.
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

/// The accept loop's state.
struct Acceptor {
    listener: TcpListener,
    manager: SessionManager,
    clock: SharedClock,
    config: HealthConfig,
    cancel: CancellationToken,
    tracker: TaskTracker,
    slots: Arc<Semaphore>,
    access: Arc<Access>,
}

/// Who may read what on the listener.
struct Access {
    metrics: MetricsAccess,
    admin: AdminAccess,
    /// Admin requests in flight: a reload can take minutes (an MCP server that is slow to
    /// answer), and each holds a connection slot, so only a few may wait; the rest get 429 at
    /// once and the probes keep their slots.
    admin_slots: Semaphore,
}

impl Access {
    fn new(metrics: MetricsAccess, admin: AdminAccess) -> Self {
        Self {
            metrics,
            admin,
            admin_slots: Semaphore::new(MAX_ADMIN_REQUESTS),
        }
    }
}

/// Most `POST /admin/reload` requests handled at once on one listener (a running reload and one
/// waiting behind it).
pub const MAX_ADMIN_REQUESTS: usize = 2;

/// Largest request body of `POST /admin/reload` that is read (and ignored); a larger one is
/// answered 413.
const MAX_ADMIN_BODY: usize = 4096;

impl Acceptor {
    async fn run(self) {
        loop {
            let accepted = tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            let stream = match accepted {
                Ok((stream, _peer)) => stream,
                Err(e) => {
                    tracing::warn!(target: "brisvox::server", error = %e, "health accept failed");
                    tokio::select! {
                        biased;
                        () = self.cancel.cancelled() => break,
                        () = self.clock.sleep(ACCEPT_BACKOFF) => continue,
                    }
                }
            };
            let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
                tracing::debug!(target: "brisvox::server", "health listener busy, probe dropped");
                drop(stream);
                continue;
            };
            let manager = self.manager.clone();
            let clock = self.clock.clone();
            let cancel = self.cancel.clone();
            let deadline = self.config.request_timeout;
            let access = Arc::clone(&self.access);
            self.tracker.spawn(async move {
                let _permit = permit;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {}
                    () = serve(stream, &manager, &access, Some((&clock, deadline))) => {}
                }
            });
        }
    }
}

/// Reads one request head from `stream`, writes the answer and closes the write side. The
/// caller bounds it with a deadline and a cancellation token.
pub async fn serve_connection<S>(stream: S, manager: &SessionManager)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_connection_with(stream, manager, &MetricsAccess::Open).await;
}

/// [`serve_connection`] with `metrics` deciding who may read `/metrics`. `/admin/reload` is off.
pub async fn serve_connection_with<S>(stream: S, manager: &SessionManager, metrics: &MetricsAccess)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_connection_admin(stream, manager, metrics, &AdminAccess::Disabled).await;
}

/// [`serve_connection_with`] with `admin` deciding who may `POST /admin/reload`.
pub async fn serve_connection_admin<S>(
    stream: S,
    manager: &SessionManager,
    metrics: &MetricsAccess,
    admin: &AdminAccess,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let access = Access::new(metrics.clone(), admin.clone());
    serve(stream, manager, &access, None).await;
}

/// Runs `future`, giving up when `timing` (a clock and a duration) runs out first; without
/// `timing` it just runs. `None` when it gave up.
async fn within<T>(
    timing: Option<(&SharedClock, Duration)>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match timing {
        None => Some(future.await),
        Some((clock, deadline)) => tokio::select! {
            biased;
            value = future => Some(value),
            () = clock.sleep(deadline) => {
                tracing::debug!(target: "brisvox::server", "health connection timed out");
                None
            }
        },
    }
}

/// One connection: the request head is read, and the answer written, each within `timing`. The
/// work in between has no deadline of its own here: a probe answers at once, and the reload
/// (`POST /admin/reload`) has the hub's own bound.
async fn serve<S>(
    mut stream: S,
    manager: &SessionManager,
    access: &Access,
    timing: Option<(&SharedClock, Duration)>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(head) = within(timing, read_head(&mut stream)).await else {
        return;
    };
    let response = match head {
        Head::Complete(head, already) if is_admin_request(&head) => {
            // The body is read and ignored (a client that sent one would see a reset if the
            // connection closed with it unread), within a small bound.
            if let Some(refusal) = discard_body(&mut stream, &head, already, timing).await {
                refusal
            } else {
                admin_reload(&head, manager, access).await
            }
        }
        Head::Complete(head, _) => respond(&head, manager, &access.metrics),
        Head::TooLarge => Response::plain(431, "request header fields too large"),
        Head::Closed => return,
        Head::Malformed => Response::plain(400, "bad request"),
    };
    let bytes = response.encode();
    match within(timing, stream.write_all(bytes.as_bytes())).await {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            tracing::debug!(target: "brisvox::server", error = %e, "health answer not written");
            return;
        }
        None => return,
    }
    let _ = within(timing, stream.shutdown()).await;
}

/// The path of the agent reload endpoint.
pub const ADMIN_RELOAD_PATH: &str = "/admin/reload";

/// Whether the request line's target is [`ADMIN_RELOAD_PATH`] (any method).
fn is_admin_request(head: &str) -> bool {
    let line = head.split("\r\n").next().unwrap_or_default();
    let mut parts = line.split(' ');
    matches!(
        (parts.next(), parts.next()),
        (Some(_), Some(target)) if target.split('?').next() == Some(ADMIN_RELOAD_PATH)
    )
}

/// Reads the request body announced by `Content-Length` (`already` bytes of it came with the
/// head) and drops it. `Some(413)` when it is larger than [`MAX_ADMIN_BODY`].
async fn discard_body<S>(
    stream: &mut S,
    head: &str,
    already: usize,
    timing: Option<(&SharedClock, Duration)>,
) -> Option<Response>
where
    S: AsyncRead + Unpin,
{
    let length = header(head, "content-length")?.parse::<usize>().ok()?;
    if length > MAX_ADMIN_BODY {
        return Some(Response::plain(413, "request body too large"));
    }
    let mut rest = vec![0u8; length.saturating_sub(already)];
    if !rest.is_empty() {
        let _ = within(timing, stream.read_exact(&mut rest)).await;
    }
    None
}

/// `POST /admin/reload`: access first (a caller without the right gets nothing to learn from
/// the method or the state), then the reload.
async fn admin_reload(head: &str, manager: &SessionManager, access: &Access) -> Response {
    match access.admin.check(head) {
        AdminVerdict::NotFound => return Response::plain(404, "not found"),
        AdminVerdict::Unauthorized => {
            let mut r = Response::plain(401, "unauthorized");
            r.www_authenticate = true;
            return r;
        }
        AdminVerdict::Forbidden => return Response::plain(403, "forbidden"),
        AdminVerdict::Allowed => {}
    }
    let mut parts = head.split("\r\n").next().unwrap_or_default().split(' ');
    let (Some(method), Some(_), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Response::plain(400, "bad request");
    };
    if !version.starts_with("HTTP/1.") {
        return Response::plain(400, "bad request");
    }
    if method != "POST" {
        let mut r = Response::plain(405, "method not allowed");
        r.allow = Some("POST");
        return r;
    }
    let Some(hub) = manager.reload() else {
        // No agent is hosted (or its definition is the environment's): nothing to reload.
        return Response::plain(404, "not found");
    };
    if !manager.is_accepting() {
        return Response::json(503, r#"{"result":"draining"}"#.to_owned());
    }
    let Ok(_slot) = access.admin_slots.try_acquire() else {
        let mut r = Response::plain(429, "a reload is already running and another is waiting");
        r.retry_after = true;
        return r;
    };
    let report = hub.reload(crate::reload::ReloadTrigger::Endpoint).await;
    Response::json(report.http_status(), report.to_json())
}

/// What reading a request head produced.
enum Head {
    /// The head, without the blank line.
    /// The head, and how many bytes after it (the start of a body) were read with it.
    Complete(String, usize),
    /// More than [`MAX_REQUEST_BYTES`] without the end of the head.
    TooLarge,
    /// The peer closed before sending anything.
    Closed,
    /// The peer closed mid-head, or the head is not UTF-8.
    Malformed,
}

async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Head {
    let mut buf = [0u8; MAX_REQUEST_BYTES];
    let mut len = 0usize;
    loop {
        let Some(free) = buf.get_mut(len..) else {
            return Head::TooLarge;
        };
        if free.is_empty() {
            return Head::TooLarge;
        }
        let n = match stream.read(free).await {
            Ok(0) | Err(_) if len == 0 => return Head::Closed,
            Ok(0) | Err(_) => return Head::Malformed,
            Ok(n) => n,
        };
        len = len.saturating_add(n);
        let Some(read) = buf.get(..len) else {
            return Head::Malformed;
        };
        if let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            return match read.get(..end).map(std::str::from_utf8) {
                Some(Ok(head)) => Head::Complete(head.to_owned(), len.saturating_sub(end + 4)),
                _ => Head::Malformed,
            };
        }
    }
}

/// The answer to one request head.
fn respond(head: &str, manager: &SessionManager, metrics: &MetricsAccess) -> Response {
    let line = head.split("\r\n").next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Response::plain(400, "bad request");
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Response::plain(400, "bad request");
    }
    let head_only = match method {
        "GET" => false,
        "HEAD" => true,
        _ => {
            let mut r = Response::plain(405, "method not allowed");
            r.allow = Some("GET, HEAD");
            return r;
        }
    };
    let path = target.split('?').next().unwrap_or(target);
    let mut response = match path {
        "/healthz" => Response::json(200, r#"{"status":"ok"}"#.to_owned()),
        "/readyz" => {
            let ready = Readiness::of(manager);
            Response::json(ready.status.http_status(), ready.to_json())
        }
        "/metrics" => match (manager.observability(), metrics.permits(head)) {
            // Not on, or nobody may read it: indistinguishable from an endpoint that is off.
            (Some(obs), Some(true)) if obs.metrics_endpoint() => Response {
                status: 200,
                content_type: crate::observability::METRICS_CONTENT_TYPE,
                body: obs.render(manager),
                allow: None,
                www_authenticate: false,
                retry_after: false,
                head_only: false,
            },
            (Some(obs), Some(false)) if obs.metrics_endpoint() => {
                let mut r = Response::plain(401, "unauthorized");
                r.www_authenticate = true;
                r
            }
            _ => Response::plain(404, "not found"),
        },
        _ => Response::plain(404, "not found"),
    };
    response.head_only = head_only;
    response
}

/// One HTTP response.
struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
    allow: Option<&'static str>,
    www_authenticate: bool,
    retry_after: bool,
    head_only: bool,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
            allow: None,
            www_authenticate: false,
            retry_after: false,
            head_only: false,
        }
    }

    fn plain(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.to_owned(),
            allow: None,
            www_authenticate: false,
            retry_after: false,
            head_only: false,
        }
    }

    fn encode(&self) -> String {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            413 => "Content Too Large",
            429 => "Too Many Requests",
            405 => "Method Not Allowed",
            409 => "Conflict",
            422 => "Unprocessable Content",
            431 => "Request Header Fields Too Large",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "",
        };
        let mut out = String::with_capacity(160 + self.body.len());
        let _ = write!(
            out,
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        if let Some(allow) = self.allow {
            let _ = write!(out, "Allow: {allow}\r\n");
        }
        if self.retry_after {
            out.push_str("Retry-After: 5\r\n");
        }
        if self.www_authenticate {
            out.push_str("WWW-Authenticate: Bearer\r\n");
        }
        out.push_str("\r\n");
        if !self.head_only {
            out.push_str(&self.body);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::SimClock;
    use tokio::io::duplex;

    use super::*;
    use crate::session::ManagerConfig;

    fn manager(max: usize) -> SessionManager {
        SessionManager::new(ManagerConfig::new(max).unwrap(), SimClock::shared()).unwrap()
    }

    async fn exchange(manager: &SessionManager, request: &[u8]) -> String {
        let (mut client, server) = duplex(16 * 1024);
        let serve = serve_connection(server, manager);
        let talk = async {
            client.write_all(request).await.unwrap();
            client.shutdown().await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        };
        let ((), out) = tokio::join!(serve, talk);
        out
    }

    async fn exchange_with(
        manager: &SessionManager,
        request: &[u8],
        access: &MetricsAccess,
    ) -> String {
        let (mut client, server) = duplex(16 * 1024);
        let serve = serve_connection_with(server, manager, access);
        let talk = async {
            client.write_all(request).await.unwrap();
            client.shutdown().await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        };
        let ((), out) = tokio::join!(serve, talk);
        out
    }

    #[tokio::test(start_paused = true)]
    async fn metrics_follow_the_access_policy_and_the_probes_stay_open() {
        let mgr = manager(4);
        assert!(mgr.attach_observability(Arc::new(crate::observability::Observability::new())));
        let get = b"GET /metrics HTTP/1.1\r\n\r\n";
        let out = exchange_with(&mgr, get, &MetricsAccess::Open).await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        let out = exchange_with(&mgr, get, &MetricsAccess::Disabled).await;
        assert!(out.starts_with("HTTP/1.1 404 "), "{out}");

        let token = MetricsAccess::Token(Secret::new("scrape-token".to_owned()));
        let out = exchange_with(&mgr, get, &token).await;
        assert!(out.starts_with("HTTP/1.1 401 "), "{out}");
        assert!(out.contains("WWW-Authenticate: Bearer\r\n"), "{out}");
        assert!(out.contains("Cache-Control: no-store\r\n"), "{out}");
        for wrong in [
            "Authorization: Bearer scrape-tokeN",
            "Authorization: Bearer scrape-token-and-more",
            "Authorization: Basic scrape-token",
            "Authorization: scrape-token",
            "X-Authorization: Bearer scrape-token",
        ] {
            let request = format!("GET /metrics HTTP/1.1\r\n{wrong}\r\n\r\n");
            let out = exchange_with(&mgr, request.as_bytes(), &token).await;
            assert!(out.starts_with("HTTP/1.1 401 "), "{wrong}: {out}");
        }
        let request = b"GET /metrics HTTP/1.1\r\nauthorization: bearer scrape-token\r\n\r\n";
        let out = exchange_with(&mgr, request, &token).await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        assert!(!out.contains("scrape-token"), "the token is never echoed");

        // The probes need no token: kubelets and container runtimes probe without credentials.
        for path in ["/healthz", "/readyz"] {
            let request = format!("GET {path} HTTP/1.1\r\n\r\n");
            let out = exchange_with(&mgr, request.as_bytes(), &token).await;
            assert!(out.starts_with("HTTP/1.1 200 "), "{path}: {out}");
        }
        assert_eq!(format!("{token:?}"), "Token([REDACTED])");
    }

    #[test]
    fn metrics_access_for_a_listener_address() {
        let secret = Secret::new("t".to_owned());
        let loopback: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:8081".parse().unwrap();
        let v6: SocketAddr = "[::1]:8081".parse().unwrap();
        assert!(matches!(
            MetricsAccess::for_listener(loopback, None, false),
            MetricsAccess::Open
        ));
        assert!(matches!(
            MetricsAccess::for_listener(v6, None, false),
            MetricsAccess::Open
        ));
        assert!(matches!(
            MetricsAccess::for_listener(any, Some(&secret), false),
            MetricsAccess::Token(_)
        ));
        assert!(matches!(
            MetricsAccess::for_listener(any, None, false),
            MetricsAccess::Disabled
        ));
        assert!(matches!(
            MetricsAccess::for_listener(any, None, true),
            MetricsAccess::Open
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn healthz_and_readyz() {
        let mgr = manager(4);
        let out = exchange(&mgr, b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.ends_with("\r\n\r\n{\"status\":\"ok\"}"), "{out}");
        assert!(out.contains("Content-Length: 15\r\n"), "{out}");

        let out = exchange(&mgr, b"GET /readyz?x=1 HTTP/1.0\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(
            out.ends_with(
                r#"{"status":"ready","accepting":true,"active":0,"reserved":0,"max":4,"rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},"providers":[]}"#
            ),
            "{out}"
        );

        mgr.begin_drain();
        let out = exchange(&mgr, b"GET /readyz HTTP/1.1\r\n\r\n").await;
        assert!(
            out.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{out}"
        );
        assert!(
            out.contains(r#""status":"draining","accepting":false"#),
            "{out}"
        );
        let out = exchange(&mgr, b"GET /healthz HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn readyz_is_503_full_at_capacity() {
        let mgr = manager(1);
        let slot = mgr.try_admit().unwrap();
        let out = exchange(&mgr, b"GET /readyz HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
        assert!(
            out.ends_with(
                r#"{"status":"full","accepting":true,"active":0,"reserved":1,"max":1,"rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},"providers":[]}"#
            ),
            "{out}"
        );
        drop(slot);
        let out = exchange(&mgr, b"GET /readyz HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn head_errors_and_limits() {
        let mgr = manager(4);
        let out = exchange(&mgr, b"HEAD /readyz HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.ends_with("Connection: close\r\n\r\n"), "{out}");
        assert!(out.contains("Content-Length: 157\r\n"), "{out}");

        let out = exchange(&mgr, b"POST /readyz HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 405 "), "{out}");
        assert!(out.contains("Allow: GET, HEAD\r\n"), "{out}");

        let out = exchange(&mgr, b"GET /metrics HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 404 "), "{out}");

        for bad in [
            &b"GET /readyz\r\n\r\n"[..],
            b"GET readyz HTTP/1.1\r\n\r\n",
            b"GET /readyz SPDY/3 extra\r\n\r\n",
            b"\xff\xfe /readyz HTTP/1.1\r\n\r\n",
        ] {
            let out = exchange(&mgr, bad).await;
            assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
        }

        let mut huge = b"GET /readyz HTTP/1.1\r\nX: ".to_vec();
        huge.resize(MAX_REQUEST_BYTES + 10, b'a');
        let out = exchange(&mgr, &huge).await;
        assert!(out.starts_with("HTTP/1.1 431 "), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_close_gets_no_answer_and_a_truncated_head_a_400() {
        let mgr = manager(4);
        assert_eq!(exchange(&mgr, b"").await, "");
        let (mut client, server) = duplex(1024);
        client.write_all(b"GET /readyz HTTP/1.1\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let ((), out) = tokio::join!(serve_connection(server, &mgr), async {
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        });
        assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
    }

    /// A reload target that answers `result` and counts the reloads.
    struct Target {
        result: crate::reload::ReloadResult,
        applied: std::sync::atomic::AtomicUsize,
        /// When set, a reload waits for a permit before it answers.
        hold: Option<Arc<Semaphore>>,
    }

    impl crate::reload::ReloadTarget for Target {
        fn apply(&self) -> crate::reload::BoxFuture<'_, crate::reload::ReloadReport> {
            Box::pin(async move {
                if let Some(hold) = &self.hold {
                    hold.acquire().await.unwrap().forget();
                }
                self.applied
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::reload::ReloadReport {
                    result: self.result,
                    generation: 2,
                    message: "the message".to_owned(),
                    slots: Vec::new(),
                }
            })
        }

        fn changed(&self) -> crate::reload::BoxFuture<'_, bool> {
            Box::pin(async { false })
        }

        fn generation(&self) -> u64 {
            1
        }

        fn shutdown(&self) -> crate::reload::BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    fn manager_with_reload(result: crate::reload::ReloadResult) -> (SessionManager, Arc<Target>) {
        let mgr = manager(4);
        let target = Arc::new(Target {
            result,
            applied: std::sync::atomic::AtomicUsize::new(0),
            hold: None,
        });
        let hub = crate::reload::ReloadHub::new(target.clone(), SimClock::shared());
        assert!(mgr.attach_reload(Arc::new(hub)));
        (mgr, target)
    }

    async fn exchange_admin(
        manager: &SessionManager,
        request: &str,
        admin: &AdminAccess,
    ) -> String {
        let (mut client, server) = duplex(16 * 1024);
        let serve = serve_connection_admin(server, manager, &MetricsAccess::Open, admin);
        let talk = async {
            client.write_all(request.as_bytes()).await.unwrap();
            client.shutdown().await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        };
        let ((), out) = tokio::join!(serve, talk);
        out
    }

    fn applied(target: &Target) -> usize {
        target.applied.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn the_reload_endpoint_needs_its_token_and_a_scrape_token_does_not_open_it() {
        let (mgr, target) = manager_with_reload(crate::reload::ReloadResult::Ok);
        let admin = AdminAccess::Token(Secret::new("admin-secret".to_owned()));
        let post = |auth: &str| format!("POST /admin/reload HTTP/1.1\r\n{auth}\r\n");
        for wrong in [
            "",
            "Authorization: Bearer admin-secreT\r\n",
            "Authorization: Bearer admin-secret-and-more\r\n",
            "Authorization: Basic admin-secret\r\n",
            "X-Authorization: Bearer admin-secret\r\n",
            // The metrics scrape token is a different credential.
            "Authorization: Bearer scrape-token\r\n",
        ] {
            let out = exchange_admin(&mgr, &post(wrong), &admin).await;
            assert!(out.starts_with("HTTP/1.1 401 "), "{wrong}: {out}");
            assert!(out.contains("WWW-Authenticate: Bearer\r\n"), "{out}");
        }
        assert_eq!(applied(&target), 0, "nothing reloaded without the token");
        // The method is not even looked at without the token.
        let out = exchange_admin(&mgr, "GET /admin/reload HTTP/1.1\r\n\r\n", &admin).await;
        assert!(out.starts_with("HTTP/1.1 401 "), "{out}");

        let out = exchange_admin(
            &mgr,
            &post("authorization: Bearer admin-secret\r\n"),
            &admin,
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        assert!(out.contains("Content-Type: application/json"), "{out}");
        assert!(out.contains("Cache-Control: no-store"), "{out}");
        assert!(out.contains(r#""result":"ok""#), "{out}");
        assert!(out.contains(r#""generation":2"#), "{out}");
        assert_eq!(applied(&target), 1);

        // With a token the method matters: anything but POST is 405 and names POST.
        for method in ["GET", "HEAD", "PUT"] {
            let request = format!(
                "{method} /admin/reload HTTP/1.1\r\nAuthorization: Bearer admin-secret\r\n\r\n"
            );
            let out = exchange_admin(&mgr, &request, &admin).await;
            assert!(out.starts_with("HTTP/1.1 405 "), "{method}: {out}");
            assert!(out.contains("Allow: POST\r\n"), "{out}");
        }
        assert_eq!(applied(&target), 1);

        // The probes stay as they were.
        let out = exchange_admin(&mgr, "GET /readyz HTTP/1.1\r\n\r\n", &admin).await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_reload_endpoint_is_off_without_access_and_local_only_on_loopback() {
        let (mgr, target) = manager_with_reload(crate::reload::ReloadResult::Ok);
        let post = "POST /admin/reload HTTP/1.1\r\n\r\n";
        // Off: indistinguishable from a path that does not exist (any method).
        for request in [post, "GET /admin/reload HTTP/1.1\r\n\r\n"] {
            let out = exchange_admin(&mgr, request, &AdminAccess::Disabled).await;
            assert!(out.starts_with("HTTP/1.1 404 "), "{out}");
        }
        assert_eq!(applied(&target), 0);

        // Loopback without a token: a local process may; a browser may not (it sends Origin).
        let out = exchange_admin(&mgr, post, &AdminAccess::Local).await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        let from_page = "POST /admin/reload HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n";
        let out = exchange_admin(&mgr, from_page, &AdminAccess::Local).await;
        assert!(out.starts_with("HTTP/1.1 403 "), "{out}");
        assert_eq!(applied(&target), 1, "only the local request reloaded");

        // Which access a listener gets.
        let secret = Secret::new("zzz".to_owned());
        let loopback: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        let public: SocketAddr = "0.0.0.0:8081".parse().unwrap();
        assert!(matches!(
            AdminAccess::for_listener(loopback, None),
            AdminAccess::Local
        ));
        assert!(matches!(
            AdminAccess::for_listener(public, None),
            AdminAccess::Disabled
        ));
        assert!(matches!(
            AdminAccess::for_listener(public, Some(&secret)),
            AdminAccess::Token(_)
        ));
        // A token is required wherever one is set, loopback included.
        assert!(matches!(
            AdminAccess::for_listener(loopback, Some(&secret)),
            AdminAccess::Token(_)
        ));
        assert_eq!(
            format!("{:?}", AdminAccess::Token(secret)),
            "Token([REDACTED])"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_reload_answer_status_follows_the_outcome() {
        use crate::reload::ReloadResult;
        for (result, status) in [
            (ReloadResult::Ok, 200),
            (ReloadResult::Unchanged, 200),
            (ReloadResult::Invalid, 422),
            (ReloadResult::NeedsRestart, 409),
            (ReloadResult::TimedOut, 504),
        ] {
            let (mgr, _) = manager_with_reload(result);
            let out = exchange_admin(
                &mgr,
                "POST /admin/reload HTTP/1.1\r\n\r\n",
                &AdminAccess::Local,
            )
            .await;
            assert!(
                out.starts_with(&format!("HTTP/1.1 {status} ")),
                "{result:?}: {out}"
            );
            assert!(
                out.contains(&format!(r#""result":"{}""#, result.as_str())),
                "{out}"
            );
        }
        // No agent is hosted: no hub, nothing to reload.
        let out = exchange_admin(
            &manager(4),
            "POST /admin/reload HTTP/1.1\r\n\r\n",
            &AdminAccess::Local,
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 404 "), "{out}");
        // Draining: no reload, the answer says why.
        let (mgr, target) = manager_with_reload(ReloadResult::Ok);
        mgr.begin_drain();
        let out = exchange_admin(
            &mgr,
            "POST /admin/reload HTTP/1.1\r\n\r\n",
            &AdminAccess::Local,
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
        assert!(out.contains("draining"), "{out}");
        assert_eq!(applied(&target), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_few_admin_requests_wait_at_once_and_the_rest_are_turned_away() {
        let mgr = manager(4);
        let hold = Arc::new(Semaphore::new(0));
        let target = Arc::new(Target {
            result: crate::reload::ReloadResult::Ok,
            applied: std::sync::atomic::AtomicUsize::new(0),
            hold: Some(Arc::clone(&hold)),
        });
        assert!(mgr.attach_reload(Arc::new(crate::reload::ReloadHub::new(
            target.clone(),
            SimClock::shared()
        ))));
        let post = "POST /admin/reload HTTP/1.1\r\n\r\n";
        // One access for all the connections, like the listener's.
        let access = Access::new(MetricsAccess::Open, AdminAccess::Local);
        let exchange_shared = |request: &'static str| {
            let (mgr, access) = (&mgr, &access);
            async move {
                let (mut client, server) = duplex(16 * 1024);
                let serving = serve(server, mgr, access, None);
                let talk = async {
                    client.write_all(request.as_bytes()).await.unwrap();
                    client.shutdown().await.unwrap();
                    let mut out = String::new();
                    client.read_to_string(&mut out).await.unwrap();
                    out
                };
                let ((), out) = tokio::join!(serving, talk);
                out
            }
        };
        let waiting = async {
            let (a, b) = tokio::join!(exchange_shared(post), exchange_shared(post));
            (a, b)
        };
        let turned_away = async {
            // Let the first two start waiting, then a third arrives.
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            let third = exchange_shared(post).await;
            // Probes are not affected, and the waiting ones finish once the reload may.
            let probe = exchange_shared("GET /healthz HTTP/1.1\r\n\r\n").await;
            hold.add_permits(2);
            (third, probe)
        };
        let ((first, second), (third, probe)) = tokio::join!(waiting, turned_away);
        assert!(third.starts_with("HTTP/1.1 429 "), "{third}");
        assert!(third.contains("Retry-After: 5\r\n"), "{third}");
        assert!(probe.starts_with("HTTP/1.1 200 "), "{probe}");
        assert!(first.starts_with("HTTP/1.1 200 "), "{first}");
        assert!(second.starts_with("HTTP/1.1 200 "), "{second}");
        assert_eq!(
            applied(&target),
            2,
            "the turned-away request reloaded nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_body_is_read_and_ignored_and_a_big_one_is_refused() {
        let (mgr, target) = manager_with_reload(crate::reload::ReloadResult::Ok);
        let body = "x".repeat(100);
        let request = format!(
            "POST /admin/reload HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let out = exchange_admin(&mgr, &request, &AdminAccess::Local).await;
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        assert_eq!(applied(&target), 1);
        // A body that arrives after the head is read as well.
        let (mut client, server) = duplex(16 * 1024);
        let serve = serve_connection_admin(server, &mgr, &MetricsAccess::Open, &AdminAccess::Local);
        let talk = async {
            client
                .write_all(b"POST /admin/reload HTTP/1.1\r\nContent-Length: 600\r\n\r\n")
                .await
                .unwrap();
            client.write_all(&[b'y'; 600]).await.unwrap();
            client.shutdown().await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            out
        };
        let ((), out) = tokio::join!(serve, talk);
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        // Too big: 413, nothing reloaded.
        let big = "POST /admin/reload HTTP/1.1\r\nContent-Length: 100000\r\n\r\n";
        let out = exchange_admin(&mgr, big, &AdminAccess::Local).await;
        assert!(out.starts_with("HTTP/1.1 413 "), "{out}");
        assert_eq!(applied(&target), 2);
    }
}
