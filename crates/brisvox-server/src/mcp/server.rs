//! One MCP server: how to reach it, the connection, and the task that keeps it connected.
//!
//! A server is connected once at startup ([`Server::open`], bounded by its startup timeout). A
//! supervisor task then owns the connection: when it ends (a stdio child exited, an HTTP
//! transport closed for good) the supervisor marks the server as restarting, so calls fail at
//! once, and reconnects with a growing delay that stops growing at [`RESTART_DELAY_MAX`]. It
//! never gives up: a server that is down for an hour is tried every ten seconds until it comes
//! back or the process stops (a deploy of the MCP server must not need a restart of this one).

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess, which_command};
use rmcp::{ClientLifecycleMode, ClientServiceExt, Peer, RoleClient};
use tokio::io::AsyncReadExt as _;
use tokio::process::ChildStderr;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::{McpServerSettings, McpTransportSettings};

/// A running connection to an MCP server.
pub(crate) type Service = RunningService<RoleClient, ClientConfig>;

/// The first reconnection delay; it doubles up to [`RESTART_DELAY_MAX`].
const RESTART_DELAY_FIRST: Duration = Duration::from_millis(250);

/// The longest reconnection delay, kept for as long as the server stays down.
const RESTART_DELAY_MAX: Duration = Duration::from_secs(10);

/// A connection that lasted this long was healthy: the next outage starts its delays afresh.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// While a server stays down, failed attempts are logged at `warn` level for the first
/// [`WARN_FIRST_ATTEMPTS`] attempts and then once in every this many (about a minute at the
/// longest delay).
const WARN_EVERY: u32 = 6;

/// See [`WARN_EVERY`].
const WARN_FIRST_ATTEMPTS: u32 = 3;

/// How long a closing connection (stdin closed, then the child waited for) may take.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP connect deadline of an HTTP server.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an idle HTTP connection to a server is kept.
const HTTP_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Longest line of a stdio server's standard error that is logged as one event.
const STDERR_LINE_MAX: usize = 2048;

/// The environment variables a stdio server inherits from the brisvox-server process, besides
/// `PATH`: on Windows, the ones the system needs to start a program at all. Nothing else is
/// passed on (the process holds the provider keys).
#[cfg(windows)]
const INHERITED_ENV: [&str; 7] = [
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SYSTEMROOT",
    "COMSPEC",
    "TEMP",
    "TMP",
];
#[cfg(not(windows))]
const INHERITED_ENV: [&str; 1] = ["PATH"];

/// Where a server's connection is.
#[derive(Debug, Clone)]
pub(crate) enum Link {
    /// Connected: calls go to this peer.
    Up(Peer<RoleClient>),
    /// Not connected, being reconnected.
    Restarting,
    /// Closed (the server is shutting down): calls fail at once.
    Down,
}

/// What the status of a server says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpLinkState {
    /// Connected.
    Up,
    /// Lost; reconnecting, without end, at most every ten seconds. Calls fail at once.
    Restarting,
    /// Closed: the server is shutting down. Calls fail at once.
    Down,
}

/// A connection just opened: the service and the server's tool list.
pub(crate) struct Opened {
    pub(crate) service: Service,
    pub(crate) tools: Vec<rmcp::model::Tool>,
}

/// The future of one connection attempt.
pub(crate) type OpenFuture<'a> = Pin<Box<dyn Future<Output = Result<Opened, String>> + Send + 'a>>;

/// How a connection to a server is made (once, and again after every loss). The real one speaks
/// stdio or HTTP; tests make in-memory ones.
pub(crate) trait Connect: Send + Sync {
    /// Connects, handshakes and lists the tools, without a time limit of its own.
    ///
    /// # Errors
    /// What went wrong, as one line without any secret.
    fn connect(&self) -> OpenFuture<'_>;
}

/// A connected (or reconnecting) MCP server.
pub(crate) struct Server {
    /// The server's name (logs, errors).
    pub(crate) name: Arc<str>,
    connector: Box<dyn Connect>,
    /// Longest wait for a (re)connection to start and list its tools.
    startup_timeout: Duration,
    /// The tool names that must stay offered (the allowlist).
    expected: Vec<String>,
    link: Mutex<Link>,
    /// Completed reconnections.
    restarts: AtomicU64,
    /// One permit per call in flight (`max_in_flight`).
    in_flight: Arc<Semaphore>,
    max_in_flight: usize,
    pub(crate) clock: SharedClock,
}

impl Server {
    pub(crate) fn new(settings: &McpServerSettings, clock: SharedClock) -> Self {
        let name: Arc<str> = Arc::from(settings.name.as_str());
        Self::with_connector(
            name.clone(),
            Box::new(Transport {
                name,
                settings: settings.transport.clone(),
            }),
            settings.startup_timeout,
            settings.tools.iter().map(|t| t.name.clone()).collect(),
            settings.max_in_flight,
            clock,
        )
    }

    pub(crate) fn with_connector(
        name: Arc<str>,
        connector: Box<dyn Connect>,
        startup_timeout: Duration,
        expected: Vec<String>,
        max_in_flight: usize,
        clock: SharedClock,
    ) -> Self {
        Self {
            name,
            connector,
            startup_timeout,
            expected,
            link: Mutex::new(Link::Restarting),
            restarts: AtomicU64::new(0),
            in_flight: Arc::new(Semaphore::new(max_in_flight)),
            max_in_flight,
            clock,
        }
    }

    fn set_link(&self, link: Link) {
        *self.link.lock().unwrap_or_else(PoisonError::into_inner) = link;
    }

    /// Marks the server connected through `service` (before its supervisor runs, so a call
    /// right after the start finds it).
    pub(crate) fn mark_up(&self, service: &Service) {
        self.set_link(Link::Up(service.peer().clone()));
    }

    /// The peer to call, when the server is connected.
    pub(crate) fn peer(&self) -> Option<Peer<RoleClient>> {
        match &*self.link.lock().unwrap_or_else(PoisonError::into_inner) {
            Link::Up(peer) if !peer.is_transport_closed() => Some(peer.clone()),
            _ => None,
        }
    }

    /// The permits of the calls in flight, and their limit.
    pub(crate) fn in_flight(&self) -> (&Arc<Semaphore>, usize) {
        (&self.in_flight, self.max_in_flight)
    }

    /// The state for the status report.
    pub(crate) fn state(&self) -> McpLinkState {
        match &*self.link.lock().unwrap_or_else(PoisonError::into_inner) {
            Link::Up(peer) if !peer.is_transport_closed() => McpLinkState::Up,
            Link::Up(_) | Link::Restarting => McpLinkState::Restarting,
            Link::Down => McpLinkState::Down,
        }
    }

    /// Completed reconnections so far.
    pub(crate) fn restarts(&self) -> u64 {
        self.restarts.load(Ordering::Relaxed)
    }

    /// Connects, handshakes and lists the server's tools, within the startup timeout.
    ///
    /// # Errors
    /// What went wrong, as one line without any secret.
    pub(crate) async fn open(&self) -> Result<Opened, String> {
        tokio::select! {
            biased;
            opened = self.connector.connect() => opened,
            () = self.clock.sleep(self.startup_timeout) => Err(format!(
                "no answer within the startup timeout of {} ms (startup_timeout_ms)",
                self.startup_timeout.as_millis()
            )),
        }
    }

    /// The allowlisted tools that `tools` (a fresh `tools/list`) no longer offers.
    fn missing<'a>(&'a self, tools: &[rmcp::model::Tool]) -> Vec<&'a str> {
        self.expected
            .iter()
            .filter(|name| !tools.iter().any(|t| t.name == name.as_str()))
            .map(String::as_str)
            .collect()
    }

    /// Owns the connection until `cancel` fires: publishes the peer while the connection
    /// lives, reconnects with backoff when it ends, for as long as it takes. Returns when
    /// closed.
    pub(crate) async fn supervise(self: Arc<Self>, first: Service, cancel: CancellationToken) {
        let mut service = first;
        let mut failures: u32 = 0;
        loop {
            let started = self.clock.now();
            self.set_link(Link::Up(service.peer().clone()));
            let closer = service.cancellation_token();
            let waiting = service.waiting();
            tokio::pin!(waiting);
            tokio::select! {
                _ = &mut waiting => {}
                () = cancel.cancelled() => {
                    closer.cancel();
                    // The service closes its transport (stdin, then the child) when cancelled.
                    tokio::select! {
                        _ = &mut waiting => {}
                        () = self.clock.sleep(CLOSE_TIMEOUT) => {
                            tracing::warn!(server = %self.name, "the MCP connection did not close in time");
                        }
                    }
                    self.set_link(Link::Down);
                    return;
                }
            }
            self.set_link(Link::Restarting);
            if self.clock.now().saturating_duration_since(started) >= HEALTHY_AFTER {
                failures = 0;
            }
            tracing::warn!(server = %self.name, "the MCP connection ended; reconnecting");
            service = loop {
                failures = failures.saturating_add(1);
                let delay = restart_delay(failures);
                tokio::select! {
                    () = self.clock.sleep(delay) => {}
                    () = cancel.cancelled() => {
                        self.set_link(Link::Down);
                        return;
                    }
                }
                let attempt = tokio::select! {
                    opened = self.open() => opened,
                    () = cancel.cancelled() => {
                        self.set_link(Link::Down);
                        return;
                    }
                };
                let reason = match attempt {
                    Ok(opened) => {
                        let missing = self.missing(&opened.tools);
                        if missing.is_empty() {
                            self.restarts.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(server = %self.name, attempts = failures, "the MCP server is connected again");
                            break opened.service;
                        }
                        format!("it no longer offers the allowlisted tools {missing:?}")
                    }
                    Err(reason) => reason,
                };
                if failures <= WARN_FIRST_ATTEMPTS || failures.is_multiple_of(WARN_EVERY) {
                    tracing::warn!(server = %self.name, attempt = failures, %reason, "reconnecting to the MCP server failed; trying again");
                } else {
                    tracing::debug!(server = %self.name, attempt = failures, %reason, "reconnecting to the MCP server failed");
                }
            };
        }
    }
}

/// The delay before reconnection attempt `n` (1-based): doubling from the first delay, capped.
pub(crate) fn restart_delay(n: u32) -> Duration {
    let doublings = n.saturating_sub(1).min(16);
    RESTART_DELAY_FIRST
        .saturating_mul(1u32 << doublings)
        .min(RESTART_DELAY_MAX)
}

/// The real way to a server: a child process or an HTTP endpoint.
struct Transport {
    name: Arc<str>,
    settings: McpTransportSettings,
}

impl Connect for Transport {
    fn connect(&self) -> OpenFuture<'_> {
        Box::pin(async move {
            let service = match &self.settings {
                McpTransportSettings::Stdio { command, args, env } => {
                    let (transport, stderr) = spawn_stdio(command, args, env)
                        .map_err(|e| format!("cannot start `{command}`: {e}"))?;
                    if let Some(stderr) = stderr {
                        forward_stderr(Arc::clone(&self.name), stderr);
                    }
                    client_config()
                        .serve_with_lifecycle(transport, lifecycle())
                        .await
                        .map_err(|e| format!("the MCP handshake failed: {e}"))?
                }
                McpTransportSettings::Http { url, bearer } => {
                    // The URL has no query, fragment or user info (checked when the file is
                    // read), so the SDK's messages, which quote it, hold nothing secret.
                    let client = brisvox_providers::net::shared_client(
                        &brisvox_providers::net::HttpClientSpec {
                            provider: "mcp",
                            base_url: url,
                            connect_timeout: HTTP_CONNECT_TIMEOUT,
                            pool_idle_timeout: Some(HTTP_IDLE_TIMEOUT),
                            http2_prior_knowledge: false,
                        },
                    )
                    .map_err(|e| format!("cannot build the HTTP client: {e}"))?;
                    // A server of the session era that restarts forgets its sessions and answers
                    // 404 to the old one: start a new session (a request that got the 404 was not
                    // processed, so its single retry cannot run a tool twice).
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .reinit_on_expired_session(true);
                    if let Some(token) = bearer {
                        config = config.auth_header(token.expose().clone());
                    }
                    let transport = StreamableHttpClientTransport::with_client(client, config);
                    client_config()
                        .serve_with_lifecycle(transport, lifecycle())
                        .await
                        .map_err(|e| format!("the MCP handshake failed: {e}"))?
                }
            };
            // A closed-on-error service closes its transport (and kills a child) when dropped.
            let tools = service
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| format!("tools/list failed: {e}"))?;
            Ok(Opened { service, tools })
        })
    }
}

/// What this client says about itself: no capabilities (no sampling, roots or elicitation: a
/// server that needs them cannot be used by a voice agent).
pub(crate) fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("brisvox", crate::VERSION),
    )
}

/// Both protocol eras: probe `server/discover` and fall back to the `initialize` handshake of
/// the 2025-11-25 revision, so the many servers that do not speak the newest one work too.
pub(crate) fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// Starts the program of a stdio server with a clean environment: `PATH` (and the Windows
/// essentials), then what the entry sets.
fn spawn_stdio(
    command: &str,
    args: &[String],
    env: &[(String, brisvox_core::Secret<String>)],
) -> std::io::Result<(TokioChildProcess, Option<ChildStderr>)> {
    let mut cmd = which_command(command)?;
    cmd.args(args).env_clear();
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    for (name, value) in env {
        cmd.env(name, value.expose());
    }
    cmd.kill_on_drop(true);
    TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
}

/// Logs what a stdio server writes to its standard error, a line per event at debug level,
/// until it closes.
fn forward_stderr(name: Arc<str>, mut stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(buf.get(..n).unwrap_or_default());
                    drain_lines(&mut pending, |line| {
                        tracing::debug!(server = %name, "stderr: {line}");
                    });
                }
            }
        }
        // What was written without a final line break.
        if !pending.is_empty() {
            let line = String::from_utf8_lossy(&pending);
            tracing::debug!(server = %name, "stderr: {}", line.trim_end());
        }
    });
}

/// Takes the complete lines out of `pending` and hands each (decoded as a whole, so a character
/// split between two reads is not mangled) to `log`. A line longer than [`STDERR_LINE_MAX`] is
/// handed over in parts, cut at a character boundary.
fn drain_lines(pending: &mut Vec<u8>, mut log: impl FnMut(&str)) {
    loop {
        let end = match pending.iter().position(|b| *b == b'\n') {
            Some(i) => i + 1,
            None if pending.len() > STDERR_LINE_MAX => match std::str::from_utf8(pending) {
                // A character cut by the end of the buffer waits for its rest.
                Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => e.valid_up_to(),
                _ => pending.len(),
            },
            None => return,
        };
        let line: Vec<u8> = pending.drain(..end).collect();
        let text = String::from_utf8_lossy(&line);
        log(text.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restart_delay_doubles_and_is_capped() {
        let delays: Vec<u64> = (1..=8)
            .map(|n| u64::try_from(restart_delay(n).as_millis()).unwrap())
            .collect();
        assert_eq!(delays, [250, 500, 1000, 2000, 4000, 8000, 10_000, 10_000]);
        assert_eq!(restart_delay(0), RESTART_DELAY_FIRST);
        assert_eq!(restart_delay(u32::MAX), RESTART_DELAY_MAX);
    }

    fn lines_of(chunks: &[&[u8]]) -> Vec<String> {
        let mut pending = Vec::new();
        let mut out = Vec::new();
        for chunk in chunks {
            pending.extend_from_slice(chunk);
            drain_lines(&mut pending, |l| out.push(l.to_owned()));
        }
        out
    }

    #[test]
    fn stderr_is_logged_by_line_and_a_split_character_survives() {
        // "é" is 0xC3 0xA9; the read ends between its bytes.
        let out = lines_of(&[b"caf\xC3", b"\xA9 ready\nsecond ", b"line\r\n"]);
        assert_eq!(out, ["café ready", "second line"]);
        // Nothing complete yet: nothing logged.
        assert!(lines_of(&[b"no newline"]).is_empty());
    }

    #[test]
    fn an_endless_line_is_cut_at_a_character_boundary() {
        let long = "é".repeat(STDERR_LINE_MAX); // 2 bytes each
        let mut bytes = long.into_bytes();
        bytes.push(0xC3); // half a character at the end of the buffer
        let mut pending = bytes;
        let mut out = Vec::new();
        drain_lines(&mut pending, |l| out.push(l.to_owned()));
        assert_eq!(out.len(), 1);
        assert!(!out[0].contains('\u{FFFD}'), "no mangled character");
        assert_eq!(pending, [0xC3], "the half character waits for its rest");
    }
}
