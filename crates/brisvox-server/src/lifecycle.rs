//! Process lifecycle of the server binary (`docs/OPERATIONS.md`): shutdown signals, the
//! graceful drain and the exit status.
//!
//! # Sequence
//! 1. [`Server::start`] creates the [`SessionManager`] and binds the health listener
//!    ([`crate::health`]); `/readyz` is 200 from then on.
//! 2. [`Server::run`] waits for the first shutdown signal: `SIGTERM` or `SIGINT` on Unix,
//!    Ctrl-C or Ctrl-Break on Windows ([`ShutdownSignals::install`]).
//! 3. [`drain`]: the manager stops admitting ([`SessionManager::begin_drain`]), so `/readyz`
//!    turns 503 `draining` at once and [`SessionManager::try_admit`] returns
//!    [`crate::AdmissionError::ShuttingDown`]. The hosted transports ([`crate::hosting`])
//!    admit through the manager, so from that moment every new connection is answered
//!    with HTTP 503 before its upgrade; calls already running keep their connections. `/healthz`
//!    stays 200. Running sessions get [`ShutdownPolicy::drain_timeout`] (on the
//!    injected clock) to end by themselves; a second signal cuts this stage short.
//! 4. Whatever is left is cancelled, then aborted ([`SessionManager::shutdown`] with the
//!    policy's `cancel_grace` and `abort_wait`). The transports run under the manager's
//!    [`SessionManager::shutdown_token`], so the cancel stage also closes their connections.
//! 5. The listeners stop ([`Listeners::stop`]), the health listener closes and the process
//!    exits with [`DrainOutcome::exit_code`]: 0 when
//!    every session ended by itself, 3 when some had to be cancelled, 4 when some had to be
//!    aborted, 5 when some are still registered (a bug). Startup failures exit with 1. The
//!    binary then shuts the tokio runtime down with [`shutdown_runtime`], so a blocking
//!    task stuck in an aborted session cannot hold the exit back.

use std::future::{Future, pending};
use std::net::SocketAddr;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::admission::AdmissionPolicy;
use crate::health::{HealthConfig, HealthServer, MetricsAccess};
use crate::hosting::{HostSettings, LISTENER_STOP_TIMEOUT, ListenerAddrs, Listeners};
use crate::runner::AcceptedCall;
use crate::session::{SessionManager, SessionRunner, ShutdownPolicy};
use crate::settings::ServerSettings;
use crate::{Error, Result};

/// Exit status of a process that failed to start (bad settings, port in use, no signal
/// handler).
pub const EXIT_STARTUP_FAILURE: u8 = 1;

/// How long the binary waits for the tokio runtime to shut down after the drain
/// (`Runtime::shutdown_timeout`). Aborting a session does not stop a `spawn_blocking` closure
/// it started (inference, an offload pool shutdown); dropping the runtime would wait for such a
/// closure without limit, so the exit could miss its status and be cut by `SIGKILL` instead.
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long the health listener gets to close after the drain.
const HEALTH_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// How long the callback listener gets to close after the drain.
#[cfg(feature = "call-control")]
const CALLBACK_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// A request to shut down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// `SIGTERM` (Unix): what orchestrators send.
    Terminate,
    /// `SIGINT` (Unix).
    Interrupt,
    /// Ctrl-C (Windows console).
    CtrlC,
    /// Ctrl-Break (Windows console).
    CtrlBreak,
    /// Asked for in-process (tests, embedding), see [`ShutdownSignals::manual`].
    Requested,
}

impl ShutdownSignal {
    /// A short name for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Interrupt => "SIGINT",
            Self::CtrlC => "ctrl-c",
            Self::CtrlBreak => "ctrl-break",
            Self::Requested => "requested",
        }
    }
}

/// Sends shutdown requests to a [`ShutdownSignals`] made by [`ShutdownSignals::manual`].
#[derive(Debug, Clone)]
pub struct ShutdownTrigger {
    tx: mpsc::Sender<ShutdownSignal>,
}

impl ShutdownTrigger {
    /// Delivers `signal`. `false` if it was dropped: the receiver is gone, or
    /// [`ShutdownSignals::CAPACITY`] signals are already pending (only the first two matter).
    pub fn trigger(&self, signal: ShutdownSignal) -> bool {
        self.tx.try_send(signal).is_ok()
    }
}

/// The stream of shutdown requests the server reacts to.
#[derive(Debug)]
pub struct ShutdownSignals {
    rx: mpsc::Receiver<ShutdownSignal>,
    /// Stops the OS signal forwarder when this is dropped.
    _stop: Option<DropGuard>,
}

impl ShutdownSignals {
    /// Pending signals kept; more are dropped (overflow policy: drop newest; the server only
    /// looks at the first two).
    pub const CAPACITY: usize = 4;

    /// Signals that come only from the returned [`ShutdownTrigger`].
    pub fn manual() -> (ShutdownTrigger, Self) {
        let (tx, rx) = mpsc::channel(Self::CAPACITY);
        (ShutdownTrigger { tx }, Self { rx, _stop: None })
    }

    /// Listens for the operating system's shutdown signals: `SIGTERM` and `SIGINT` on Unix,
    /// Ctrl-C and Ctrl-Break on Windows. Install it before serving, so a signal is never
    /// missed and a failure stops the start.
    ///
    /// # Errors
    /// [`Error::Config`] outside a tokio runtime or when a handler cannot be registered.
    pub fn install() -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Config("signal handlers need a tokio runtime".into()))?;
        let (tx, rx) = mpsc::channel(Self::CAPACITY);
        let cancel = CancellationToken::new();
        let forward = os::forwarder(tx, cancel.clone())
            .map_err(|e| Error::Config(format!("cannot install signal handlers: {e}")))?;
        runtime.spawn(forward);
        Ok(Self {
            rx,
            _stop: Some(cancel.drop_guard()),
        })
    }

    /// The next request; `None` once no more can come (every trigger dropped).
    pub async fn recv(&mut self) -> Option<ShutdownSignal> {
        self.rx.recv().await
    }
}

#[cfg(unix)]
mod os {
    use std::future::Future;
    use std::io;

    use tokio::signal::unix::{SignalKind, signal};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::ShutdownSignal;

    pub(super) fn forwarder(
        tx: mpsc::Sender<ShutdownSignal>,
        cancel: CancellationToken,
    ) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        Ok(async move {
            loop {
                let got = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    Some(()) = term.recv() => ShutdownSignal::Terminate,
                    Some(()) = int.recv() => ShutdownSignal::Interrupt,
                    else => break,
                };
                let _ = tx.try_send(got);
            }
        })
    }
}

#[cfg(windows)]
mod os {
    use std::future::Future;
    use std::io;

    use tokio::signal::windows::{ctrl_break, ctrl_c};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::ShutdownSignal;

    pub(super) fn forwarder(
        tx: mpsc::Sender<ShutdownSignal>,
        cancel: CancellationToken,
    ) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let mut c = ctrl_c()?;
        let mut brk = ctrl_break()?;
        Ok(async move {
            loop {
                let got = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    Some(()) = c.recv() => ShutdownSignal::CtrlC,
                    Some(()) = brk.recv() => ShutdownSignal::CtrlBreak,
                    else => break,
                };
                let _ = tx.try_send(got);
            }
        })
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    use std::future::Future;
    use std::io;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::ShutdownSignal;

    pub(super) fn forwarder(
        tx: mpsc::Sender<ShutdownSignal>,
        cancel: CancellationToken,
    ) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        Ok(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    r = tokio::signal::ctrl_c() => {
                        if r.is_err() {
                            break;
                        }
                        let _ = tx.try_send(ShutdownSignal::CtrlC);
                    }
                }
            }
        })
    }
}

/// How a drain ended; decides the exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every session ended by itself within the drain timeout (or there were none).
    Drained,
    /// Some sessions were cancelled (drain timeout or a second signal) and returned in time.
    Cancelled,
    /// Some sessions ignored cancellation and were aborted.
    Aborted,
    /// Some sessions were still registered after the abort stage (a bug worth an alert).
    Leaked,
}

impl DrainOutcome {
    /// The process exit status: 0, 3, 4 or 5 (1 is [`EXIT_STARTUP_FAILURE`]).
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Drained => 0,
            Self::Cancelled => 3,
            Self::Aborted => 4,
            Self::Leaked => 5,
        }
    }

    /// A short name for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drained => "drained",
            Self::Cancelled => "cancelled",
            Self::Aborted => "aborted",
            Self::Leaked => "leaked",
        }
    }
}

/// What [`drain`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Sessions running or reserved when the drain began.
    pub initial: usize,
    /// Ended by themselves before the drain stage ended.
    pub drained: usize,
    /// Ended after their token was cancelled.
    pub cancelled: usize,
    /// Had to be aborted.
    pub aborted: usize,
    /// Still registered at the end.
    pub leaked: usize,
    /// A second signal cut the drain stage short.
    pub forced: bool,
    /// Time from the start of the drain to its end, on the injected clock.
    pub elapsed: Duration,
}

impl DrainReport {
    /// The worst thing that happened.
    pub const fn outcome(&self) -> DrainOutcome {
        if self.leaked > 0 {
            DrainOutcome::Leaked
        } else if self.aborted > 0 {
            DrainOutcome::Aborted
        } else if self.cancelled > 0 {
            DrainOutcome::Cancelled
        } else {
            DrainOutcome::Drained
        }
    }
}

/// Shuts `runtime` down after the drain, waiting at most [`RUNTIME_SHUTDOWN_TIMEOUT`] for its
/// tasks. A `spawn_blocking` closure still running then (an aborted session's inference or
/// offload pool shutdown) is abandoned, not waited for: dropping the runtime instead would block
/// until that closure returns, possibly forever, and the process would miss its exit status.
/// Call it from outside the runtime (the binary's `main`), never from one of its tasks.
pub fn shutdown_runtime(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
}

/// Drains `manager`: stops admissions at once, lets sessions end by themselves for
/// `policy.drain_timeout` (or until `force` completes), then cancels and aborts the rest per
/// `policy`. Never takes longer than the sum of the policy's timeouts.
pub async fn drain<F>(
    manager: &SessionManager,
    policy: ShutdownPolicy,
    clock: &dyn Clock,
    force: F,
) -> DrainReport
where
    F: Future<Output = ()>,
{
    let start = clock.now();
    manager.begin_drain();
    let initial = {
        let stats = manager.stats();
        stats.active.saturating_add(stats.reserved)
    };
    tracing::info!(
        target: "brisvox::server",
        sessions = initial,
        timeout_ms = u64::try_from(policy.drain_timeout.as_millis()).unwrap_or(u64::MAX),
        "drain started: admissions stopped, waiting for sessions to end"
    );
    let forced = tokio::select! {
        biased;
        _ = manager.wait_idle(policy.drain_timeout) => false,
        () = force => true,
    };
    if forced {
        tracing::warn!(target: "brisvox::server", "second shutdown signal: cancelling sessions now");
    }
    let rest = manager
        .shutdown(ShutdownPolicy {
            drain_timeout: Duration::ZERO,
            ..policy
        })
        .await;
    let report = DrainReport {
        initial,
        drained: initial
            .saturating_sub(rest.initial)
            .saturating_add(rest.drained),
        cancelled: rest.cancelled,
        aborted: rest.aborted,
        leaked: rest.leaked,
        forced,
        elapsed: clock.now().saturating_duration_since(start),
    };
    tracing::info!(
        target: "brisvox::server",
        outcome = report.outcome().as_str(),
        ?report,
        "drain finished"
    );
    report
}

/// The server process: a session manager, its health listener, the hosted transports and the
/// drain on shutdown.
#[derive(Debug)]
pub struct Server {
    manager: SessionManager,
    clock: SharedClock,
    health: Option<HealthServer>,
    policy: ShutdownPolicy,
    listeners: Option<Listeners>,
    /// The callback listener, when `BRISVOX_CALLBACK_ADDR` is set.
    #[cfg(feature = "call-control")]
    callbacks: Option<crate::callbacks::CallbackServer>,
    /// The MCP connections of the agents' tools, closed after the drain.
    #[cfg(feature = "mcp")]
    mcp: Vec<std::sync::Arc<crate::mcp::McpHost>>,
}

impl Server {
    /// Creates the session manager and binds the health listener (unless disabled).
    ///
    /// # Errors
    /// [`Error::Config`] outside a tokio runtime or when the health address cannot be bound.
    pub async fn start(settings: ServerSettings, clock: SharedClock) -> Result<Self> {
        Self::start_with_admission(settings, AdmissionPolicy::none(), clock).await
    }

    /// [`start`](Self::start) with an admission policy (pool-pressure shedding and provider
    /// budgets): the manager the transports admit through applies it, and `/readyz`
    /// reports it.
    ///
    /// # Errors
    /// As [`start`](Self::start).
    pub async fn start_with_admission(
        settings: ServerSettings,
        policy: AdmissionPolicy,
        clock: SharedClock,
    ) -> Result<Self> {
        let manager = SessionManager::with_admission(settings.manager, clock.clone(), policy)?;
        let health = match settings.health_addr {
            Some(addr) => {
                let metrics = MetricsAccess::for_listener(
                    addr,
                    settings.metrics_token.as_ref(),
                    settings.insecure_listeners,
                );
                match &metrics {
                    MetricsAccess::Disabled => tracing::warn!(
                        target: "brisvox::server",
                        %addr,
                        "/metrics is off: the health listener is not on loopback and BRISVOX_METRICS_TOKEN is unset (set it, or BRISVOX_INSECURE_LISTENERS=1 for an open /metrics)"
                    ),
                    MetricsAccess::Open if !addr.ip().is_loopback() => tracing::warn!(
                        target: "brisvox::server",
                        %addr,
                        "/metrics is served without authentication on a non-loopback address (BRISVOX_INSECURE_LISTENERS=1)"
                    ),
                    _ => {}
                }
                Some(
                    HealthServer::bind_with_metrics_access(
                        HealthConfig::new(addr),
                        manager.clone(),
                        clock.clone(),
                        metrics,
                    )
                    .await?,
                )
            }
            None => None,
        };
        Ok(Self {
            manager,
            clock,
            health,
            policy: settings.shutdown,
            listeners: None,
            #[cfg(feature = "call-control")]
            callbacks: None,
            #[cfg(feature = "mcp")]
            mcp: Vec::new(),
        })
    }

    /// Closes the MCP connections `mcp` with the server: they serve through the drain (a call
    /// still draining may use their tools) and close after the transports.
    #[cfg(feature = "mcp")]
    pub fn attach_mcp(&mut self, mcp: std::sync::Arc<crate::mcp::McpHost>) {
        self.mcp.push(mcp);
    }

    /// Runs the callback listener `callbacks` with the server: it serves through the drain (a
    /// call still draining may need its callbacks) and stops after the transports.
    #[cfg(feature = "call-control")]
    pub fn attach_callbacks(&mut self, callbacks: crate::callbacks::CallbackServer) {
        self.callbacks = Some(callbacks);
    }

    /// Where the callback listener is bound, if it runs.
    #[cfg(feature = "call-control")]
    pub fn callback_addr(&self) -> Option<SocketAddr> {
        self.callbacks
            .as_ref()
            .map(crate::callbacks::CallbackServer::local_addr)
    }

    /// [`start`](Self::start), then binds the listeners `host` turns on; their calls run as
    /// sessions of `runner` (the binary's [`EngineRunner`](crate::runner::EngineRunner)). See
    /// [`crate::hosting`].
    ///
    /// # Errors
    /// As [`start`](Self::start), and [`Error::Config`] when a listener cannot be bound.
    pub async fn start_hosting<R>(
        settings: ServerSettings,
        host: &HostSettings,
        clock: SharedClock,
        runner: &std::sync::Arc<R>,
    ) -> Result<Self>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        Self::start_hosting_with_admission(settings, AdmissionPolicy::none(), host, clock, runner)
            .await
    }

    /// [`start_hosting`](Self::start_hosting) with an admission policy
    /// ([`start_with_admission`](Self::start_with_admission)).
    ///
    /// # Errors
    /// As [`start_hosting`](Self::start_hosting).
    pub async fn start_hosting_with_admission<R>(
        settings: ServerSettings,
        policy: AdmissionPolicy,
        host: &HostSettings,
        clock: SharedClock,
        runner: &std::sync::Arc<R>,
    ) -> Result<Self>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        Self::start_hosting_routed(settings, policy, host, clock, runner, None).await
    }

    /// [`start_hosting_with_admission`](Self::start_hosting_with_admission), with the generic
    /// websocket listener's calls run by `websocket_runner` when given
    /// ([`HostSettings::websocket_agent_file`]). One manager admits the calls of both.
    ///
    /// # Errors
    /// As [`start_hosting`](Self::start_hosting).
    pub async fn start_hosting_routed<R>(
        settings: ServerSettings,
        policy: AdmissionPolicy,
        host: &HostSettings,
        clock: SharedClock,
        runner: &std::sync::Arc<R>,
        websocket_runner: Option<&std::sync::Arc<R>>,
    ) -> Result<Self>
    where
        R: SessionRunner<Request = AcceptedCall>,
    {
        let mut server = Self::start_with_admission(settings, policy, clock.clone()).await?;
        match Listeners::bind_routed(host, &server.manager, runner, websocket_runner, clock).await {
            Ok(listeners) => {
                server.listeners = Some(listeners);
                Ok(server)
            }
            Err(e) => {
                if let Some(health) = server.health.take() {
                    let _ = health.shutdown(HEALTH_STOP_TIMEOUT).await;
                }
                Err(e)
            }
        }
    }

    /// Where the hosted listeners are bound (all `None` without [`start_hosting`](Self::start_hosting)).
    pub fn listener_addrs(&self) -> ListenerAddrs {
        self.listeners
            .as_ref()
            .map(Listeners::addrs)
            .unwrap_or_default()
    }

    /// The session manager the transports admit calls through.
    pub const fn manager(&self) -> &SessionManager {
        &self.manager
    }

    /// Where the health listener is bound, if it runs.
    pub fn health_addr(&self) -> Option<SocketAddr> {
        self.health.as_ref().map(HealthServer::local_addr)
    }

    /// Serves until the first shutdown signal, drains (a second signal skips the rest of the
    /// drain stage), stops the listeners, closes the health listener and reports how the drain
    /// ended. A signal
    /// stream that ends counts as a shutdown request.
    pub async fn run(self, signals: &mut ShutdownSignals) -> DrainReport {
        let first = signals.recv().await;
        tracing::info!(
            target: "brisvox::server",
            signal = first.map_or("signal stream closed", ShutdownSignal::as_str),
            "shutdown requested"
        );
        let force = async {
            match signals.recv().await {
                Some(signal) => {
                    tracing::info!(target: "brisvox::server", signal = signal.as_str(), "second shutdown signal");
                }
                None => pending::<()>().await,
            }
        };
        let report = drain(&self.manager, self.policy, &*self.clock, force).await;
        // After the drain: every session has ended, so stopping the transports cuts no call.
        if let Some(listeners) = self.listeners
            && !listeners.stop(LISTENER_STOP_TIMEOUT).await
        {
            tracing::warn!(target: "brisvox::server", "listeners did not stop in time");
        }
        #[cfg(feature = "call-control")]
        if let Some(callbacks) = self.callbacks
            && !callbacks.shutdown(CALLBACK_STOP_TIMEOUT).await
        {
            tracing::warn!(target: "brisvox::server", "callback listener did not stop in time");
        }
        #[cfg(feature = "mcp")]
        for mcp in self.mcp {
            mcp.shutdown().await;
        }
        if let Some(health) = self.health
            && !health.shutdown(HEALTH_STOP_TIMEOUT).await
        {
            tracing::warn!(target: "brisvox::server", "health listener did not stop in time");
        }
        report
    }
}
