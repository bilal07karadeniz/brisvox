//! Agent definition reload without a restart (`docs/OPERATIONS.md`, "Reloading the agent").
//!
//! An operator edits the agent file (prompt, voice, tools, MCP servers, providers...) and the
//! running server picks it up: **new calls run the new definition, calls in progress keep theirs
//! until they end.**
//!
//! # Pieces
//! * [`SwapRunner`] is the [`SessionRunner`] the listeners hold. It owns the current
//!   [`EngineRunner`] (a *snapshot*: the definition, its providers, its tools and MCP
//!   connections) behind a lock and hands every call an `Arc` of it **when the call is
//!   accepted**. The call keeps that `Arc` until it ends, so a swap changes nothing for it.
//! * [`RunnerRecipe`] builds a snapshot from a definition. The start and every reload build the
//!   same way, so a reloaded definition is checked exactly as the start checks it (providers,
//!   tools, and connecting its MCP servers with their startup bounds).
//! * [`AgentSet`] is the set of reloadable definitions (the main agent, and the generic
//!   websocket listener's own one): it re-reads the files, validates them, builds the new
//!   snapshots and swaps them in, all or nothing.
//! * [`ReloadHub`] is what the triggers talk to: `POST /admin/reload` ([`crate::health`]),
//!   `SIGHUP` on Unix, and the optional file poll ([`ReloadSettings`]). It runs one reload at a
//!   time under an overall bound, counts it (`brisvox_agent_reloads_total{result}`) and logs one
//!   line per reload.
//!
//! # The rules
//! * A reload parses and validates the new file(s) like the start does. **Any error keeps the
//!   old definition**: the endpoint answers with the error (positions included), the log gets a
//!   warning, `result="invalid"` is counted. A reload applies completely or not at all.
//! * A snapshot's resources are owned by the snapshot. When the last call on a replaced snapshot
//!   ends, its `Arc` drops, and its MCP connections (stdio children, HTTP sessions) and warm
//!   provider connections close (bounded: a child gets its stdin closed, a few seconds, then is
//!   killed). Nothing is closed while a call still uses it.
//! * Some things cannot change while the process runs; a reload that would change them answers
//!   `needs_restart` and changes **nothing** (see [`Veto`]): the agent's `[recording]` table,
//!   a provider that a `BRISVOX_PROVIDER_LIMITS` limit named but the start did not use, a
//!   Smart Turn model the start did not load. The listeners' addresses and TLS, every
//!   environment setting, and a definition given as `BRISVOX_AGENT_DEFINITION` (it is the
//!   environment) are outside the reload. A provider this binary was built without is an error
//!   of the new file, like at the start.
//! * Time comes from the injected clock: the poll interval and every bound.

use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use brisvox_core::SharedClock;
use brisvox_engine::Offload;
use brisvox_inference::smart_turn::TurnModel;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent_links::AgentLinks;
use crate::config::{self, AgentConfig, ConfigError, Env, Format};
use crate::observability::Observability;
use crate::privacy::Recording;
use crate::providers::ProviderFactory;
use crate::runner::{AcceptedCall, EngineRunner, RunnerConfig, Vad};
use crate::session::{SessionContext, SessionRunner};
use crate::settings::var;
use crate::tools::ToolSetup;
use crate::{Error, Result};

/// Environment variable: poll the agent file(s) every this many milliseconds and reload when the
/// content changed (default off). The mtime is checked first, then the content hash; a change is
/// applied once the file stayed the same for one more interval.
pub const ENV_AGENT_RELOAD_POLL_MS: &str = "BRISVOX_AGENT_RELOAD_POLL_MS";

/// Shortest accepted poll interval.
pub const MIN_POLL: Duration = Duration::from_millis(100);
/// Longest accepted poll interval (one day).
pub const MAX_POLL: Duration = Duration::from_secs(86_400);

/// How long one whole reload may take (waiting for another reload, reading, building the new
/// snapshots including the MCP connections of the new definition) before it is given up. The
/// old definition stays.
pub const RELOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a file read (on a blocking thread) may take.
const FILE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`ReloadHub::shutdown`] waits for a reload in progress.
const RELOAD_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`ReloadHub::shutdown`] waits for its watcher tasks.
const WATCHER_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// A boxed future, so the reload target can be used through `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------------------------
// Settings

/// The reload settings of the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReloadSettings {
    /// Poll the agent file(s) this often; `None` (the default): never.
    pub poll: Option<Duration>,
}

impl ReloadSettings {
    /// Reads `BRISVOX_AGENT_RELOAD_POLL_MS` from `env` (unset, blank or `0`: off).
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable when the value is not a whole number of
    /// milliseconds between 100 and 86 400 000.
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let Some(value) = var(env, ENV_AGENT_RELOAD_POLL_MS)? else {
            return Ok(Self::default());
        };
        let ms = value.parse::<u64>().map_err(|_| {
            Error::Config(format!(
                "{ENV_AGENT_RELOAD_POLL_MS}: {value:?} is not a whole number of milliseconds"
            ))
        })?;
        if ms == 0 {
            return Ok(Self::default());
        }
        let poll = Duration::from_millis(ms);
        if poll < MIN_POLL || poll > MAX_POLL {
            return Err(Error::Config(format!(
                "{ENV_AGENT_RELOAD_POLL_MS}: {ms} is outside {} to {} milliseconds (0 turns the poll off)",
                MIN_POLL.as_millis(),
                MAX_POLL.as_millis()
            )));
        }
        Ok(Self { poll: Some(poll) })
    }
}

// ---------------------------------------------------------------------------------------------
// Reports

/// What asked for a reload (a label of the log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadTrigger {
    /// `POST /admin/reload`.
    Endpoint,
    /// `SIGHUP`.
    Signal,
    /// The file poll ([`ENV_AGENT_RELOAD_POLL_MS`]).
    Poll,
}

impl ReloadTrigger {
    /// A short name for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Signal => "sighup",
            Self::Poll => "poll",
        }
    }
}

/// How a reload ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadResult {
    /// A new definition is live: new calls run it.
    Ok,
    /// Nothing changed since the last load: nothing was done.
    Unchanged,
    /// The new file is invalid, or its snapshot could not be built (a provider, a tool, an MCP
    /// server that does not answer): the old definition stays.
    Invalid,
    /// The change needs a restart (it touches something that is fixed while the process
    /// runs): the old definition stays.
    NeedsRestart,
    /// The reload did not finish within [`RELOAD_TIMEOUT`]: the old definition stays.
    TimedOut,
    /// The server is draining: no reload is made.
    Closed,
}

impl ReloadResult {
    /// The `result` value of the JSON answer.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unchanged => "unchanged",
            Self::Invalid => "invalid",
            Self::NeedsRestart => "needs_restart",
            Self::TimedOut => "timed_out",
            Self::Closed => "draining",
        }
    }

    /// The `result` label of `brisvox_agent_reloads_total`; none for a refused reload.
    pub const fn metric_label(self) -> Option<&'static str> {
        match self {
            Self::Ok => Some("ok"),
            Self::Unchanged => Some("unchanged"),
            Self::Invalid | Self::TimedOut => Some("invalid"),
            Self::NeedsRestart => Some("needs_restart"),
            Self::Closed => None,
        }
    }
}

/// What a reload did to one definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotResult {
    /// The new definition is live.
    Applied,
    /// The file has not changed (or cannot change).
    Unchanged,
    /// The file is invalid, or its snapshot could not be built.
    Invalid,
    /// The change needs a restart.
    NeedsRestart,
    /// Valid, but not applied because another definition of the same reload was not.
    Held,
}

impl SlotResult {
    /// The value in the JSON answer.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Invalid => "invalid",
            Self::NeedsRestart => "needs_restart",
            Self::Held => "held",
        }
    }
}

/// What a reload did to one definition (the main agent, or the websocket listener's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotReport {
    /// Which definition: `agent` or `websocket_agent`.
    pub slot: String,
    /// The name (`agent.name`) of the definition that new calls run now.
    pub agent: String,
    /// What happened to it.
    pub result: SlotResult,
    /// Why, in a line or more (the error with its file positions for an invalid file).
    pub detail: String,
}

/// The outcome of one reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// How it ended.
    pub result: ReloadResult,
    /// The generation new calls run: 1 at the start, one more per applied reload.
    pub generation: u64,
    /// One line for a person: what happened, or the first problem.
    pub message: String,
    /// Per definition.
    pub slots: Vec<SlotReport>,
}

impl ReloadReport {
    fn refused(result: ReloadResult, generation: u64, message: &str) -> Self {
        Self {
            result,
            generation,
            message: message.to_owned(),
            slots: Vec::new(),
        }
    }

    /// The HTTP status of `POST /admin/reload` for this outcome: 200 (applied, unchanged), 422
    /// (invalid), 409 (needs a restart), 504 (too slow), 503 (draining).
    pub const fn http_status(&self) -> u16 {
        match self.result {
            ReloadResult::Ok | ReloadResult::Unchanged => 200,
            ReloadResult::Invalid => 422,
            ReloadResult::NeedsRestart => 409,
            ReloadResult::TimedOut => 504,
            ReloadResult::Closed => 503,
        }
    }

    /// The JSON body of `POST /admin/reload`.
    pub fn to_json(&self) -> String {
        let agents: Vec<serde_json::Value> = self
            .slots
            .iter()
            .map(|s| {
                serde_json::json!({
                    "slot": s.slot,
                    "agent": s.agent,
                    "result": s.result.as_str(),
                    "detail": s.detail,
                })
            })
            .collect();
        serde_json::json!({
            "result": self.result.as_str(),
            "generation": self.generation,
            "message": self.message,
            "agents": agents,
        })
        .to_string()
    }

    /// `slot=agent(result)` for the log line.
    fn summary(&self) -> String {
        self.slots
            .iter()
            .map(|s| format!("{}={}({})", s.slot, s.agent, s.result.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ---------------------------------------------------------------------------------------------
// The hub

/// What a [`ReloadHub`] reloads. [`AgentSet`] is the implementation of the server; the trait
/// keeps the triggers testable without providers.
pub trait ReloadTarget: Send + Sync + 'static {
    /// Reads, validates and applies the new definition(s): all of them or none. Must be safe to
    /// drop at any await (the hub drops it when the reload times out): nothing is swapped until
    /// the last step, which does not await.
    fn apply(&self) -> BoxFuture<'_, ReloadReport>;

    /// For the poll: whether a definition changed on disk and stayed the same for one more
    /// check. Cheap while nothing changed.
    fn changed(&self) -> BoxFuture<'_, bool>;

    /// The generation new calls run.
    fn generation(&self) -> u64;

    /// Closes what the live snapshots hold open (MCP connections), at the end of the process.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

/// Runs the reloads: one at a time, bounded, counted and logged. The server attaches one to its
/// session manager ([`SessionManager::attach_reload`](crate::SessionManager::attach_reload)).
pub struct ReloadHub {
    target: Arc<dyn ReloadTarget>,
    clock: SharedClock,
    observability: Option<Arc<Observability>>,
    timeout: Duration,
    /// One reload at a time; the others wait (bounded by `timeout`).
    gate: tokio::sync::Mutex<()>,
    closed: AtomicBool,
    stop: CancellationToken,
    tasks: TaskTracker,
}

impl std::fmt::Debug for ReloadHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadHub")
            .field("generation", &self.target.generation())
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ReloadHub {
    /// A hub over `target`, on `clock`, with the default bound ([`RELOAD_TIMEOUT`]).
    pub fn new(target: Arc<dyn ReloadTarget>, clock: SharedClock) -> Self {
        Self {
            target,
            clock,
            observability: None,
            timeout: RELOAD_TIMEOUT,
            gate: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    /// Counts the reloads (`brisvox_agent_reloads_total`) and publishes the generation
    /// (`brisvox_agent_generation`) in `observability`.
    #[must_use]
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        observability
            .metrics()
            .set_agent_generation(self.target.generation());
        self.observability = Some(observability);
        self
    }

    /// Gives one reload at most `timeout` instead of [`RELOAD_TIMEOUT`].
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The generation new calls run.
    pub fn generation(&self) -> u64 {
        self.target.generation()
    }

    /// Reloads the agent definition(s) now. Waits for a reload in progress first; the whole
    /// call is bounded. Never panics and never leaves a half-applied definition.
    pub async fn reload(&self, trigger: ReloadTrigger) -> ReloadReport {
        if self.closed.load(Ordering::Acquire) {
            return ReloadReport::refused(
                ReloadResult::Closed,
                self.target.generation(),
                "the server is draining: no reload",
            );
        }
        let started = self.clock.now();
        let report = tokio::select! {
            biased;
            report = async {
                let _turn = self.gate.lock().await;
                self.target.apply().await
            } => report,
            () = self.clock.sleep(self.timeout) => ReloadReport::refused(
                ReloadResult::TimedOut,
                self.target.generation(),
                &format!(
                    "the reload did not finish within {} s: the old definition stays",
                    self.timeout.as_secs()
                ),
            ),
        };
        let elapsed_ms = u64::try_from(
            self.clock
                .now()
                .saturating_duration_since(started)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        if let Some(obs) = &self.observability {
            if let Some(label) = report.result.metric_label() {
                obs.metrics().observe_agent_reload(label);
            }
            if report.result == ReloadResult::Ok {
                obs.metrics().set_agent_generation(report.generation);
            }
        }
        match report.result {
            ReloadResult::Ok | ReloadResult::Unchanged | ReloadResult::Closed => tracing::info!(
                target: "brisvox::server",
                trigger = trigger.as_str(),
                result = report.result.as_str(),
                generation = report.generation,
                elapsed_ms,
                agents = %report.summary(),
                "agent reload"
            ),
            ReloadResult::Invalid | ReloadResult::NeedsRestart | ReloadResult::TimedOut => {
                tracing::warn!(
                    target: "brisvox::server",
                    trigger = trigger.as_str(),
                    result = report.result.as_str(),
                    generation = report.generation,
                    elapsed_ms,
                    agents = %report.summary(),
                    problem = %report.message,
                    "agent reload refused: the old definition stays"
                );
            }
        }
        report
    }

    /// Reloads when the definition files change: checks `target.changed()` every `interval`
    /// (on the clock) until the hub is closed.
    pub fn watch_files(self: &Arc<Self>, interval: Duration) {
        let hub = Arc::clone(self);
        self.tasks.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = hub.stop.cancelled() => return,
                    () = hub.clock.sleep(interval) => {}
                }
                let changed = tokio::select! {
                    biased;
                    () = hub.stop.cancelled() => return,
                    changed = hub.target.changed() => changed,
                };
                if changed {
                    hub.reload(ReloadTrigger::Poll).await;
                }
            }
        });
        tracing::info!(
            target: "brisvox::server",
            interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
            "agent files are polled for changes"
        );
    }

    /// Reloads on `SIGHUP` until the hub is closed. The handler is installed before this
    /// returns, so a signal that follows is not lost.
    ///
    /// # Errors
    /// [`Error::Config`] when the signal handler cannot be installed.
    #[cfg(unix)]
    pub fn watch_hangup(self: &Arc<Self>) -> Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hangup = signal(SignalKind::hangup())
            .map_err(|e| Error::Config(format!("cannot install the SIGHUP handler: {e}")))?;
        let hub = Arc::clone(self);
        self.tasks.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = hub.stop.cancelled() => return,
                    got = hangup.recv() => {
                        if got.is_none() {
                            return;
                        }
                    }
                }
                hub.reload(ReloadTrigger::Signal).await;
            }
        });
        Ok(())
    }

    /// No more reloads: the watchers stop and [`Self::reload`] answers "draining". The server
    /// calls it when the drain begins.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.stop.cancel();
    }

    /// [`Self::close`], then waits (bounded) for the watchers and closes what the live
    /// snapshots hold open.
    pub async fn shutdown(&self) {
        self.close();
        self.tasks.close();
        tokio::select! {
            biased;
            () = self.tasks.wait() => {}
            () = self.clock.sleep(WATCHER_STOP_TIMEOUT) => {
                tracing::warn!(target: "brisvox::server", "a reload watcher did not stop in time");
            }
        }
        // A reload in progress finishes (or is given up) first, so it cannot swap in a snapshot
        // after the live ones were closed; the target also refuses a swap once it is shut down.
        let turn = tokio::select! {
            biased;
            turn = self.gate.lock() => Some(turn),
            () = self.clock.sleep(RELOAD_STOP_TIMEOUT) => {
                tracing::warn!(target: "brisvox::server", "a reload was still running at shutdown");
                None
            }
        };
        self.target.shutdown().await;
        drop(turn);
    }
}

// ---------------------------------------------------------------------------------------------
// The swappable runner

/// The runner the listeners hold: it runs each call on the snapshot that is current **when the
/// call is accepted** and keeps that snapshot alive for the call's whole life, whatever is
/// swapped in meanwhile (see the [module docs](self)).
pub struct SwapRunner<F: ProviderFactory> {
    current: RwLock<Arc<EngineRunner<F>>>,
}

impl<F: ProviderFactory> std::fmt::Debug for SwapRunner<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwapRunner")
            .field("agent", &self.current().agent().name())
            .finish_non_exhaustive()
    }
}

impl<F: ProviderFactory> SwapRunner<F> {
    /// A swappable runner that starts with `runner`.
    pub fn new(runner: EngineRunner<F>) -> Self {
        Self {
            current: RwLock::new(Arc::new(runner)),
        }
    }

    /// The snapshot new calls run now.
    pub fn current(&self) -> Arc<EngineRunner<F>> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Makes `runner` the snapshot of new calls and returns the previous one. Calls that run
    /// on the previous one keep it until they end; it is closed when the last of them is gone
    /// (and the returned `Arc` dropped).
    pub fn replace(&self, runner: EngineRunner<F>) -> Arc<EngineRunner<F>> {
        std::mem::replace(
            &mut *self.current.write().unwrap_or_else(PoisonError::into_inner),
            Arc::new(runner),
        )
    }
}

impl<F: ProviderFactory> SessionRunner for SwapRunner<F> {
    type Request = AcceptedCall;
    type Output = ();
    type Error = Error;

    fn run(
        self: Arc<Self>,
        ctx: SessionContext,
        request: AcceptedCall,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        // The snapshot is taken here, when the call is spawned, not when the future is first
        // polled: the call runs the definition that was current when it was accepted.
        let snapshot = self.current();
        drop(self);
        SessionRunner::run(snapshot, ctx, request)
    }
}

// ---------------------------------------------------------------------------------------------
// Building a snapshot

type FactoryFn<F> = Arc<dyn Fn(&AgentConfig) -> Result<F> + Send + Sync>;
type LinksFn = Arc<dyn Fn(&AgentConfig) -> Result<AgentLinks> + Send + Sync>;

/// Runs `work` on the blocking pool and waits for it. The build of a snapshot (TLS configurations,
/// HTTP clients, the tools' JSON Schemas) is CPU work that must not run on a tokio worker while
/// calls are live; the reload's overall bound ([`RELOAD_TIMEOUT`]) covers the wait.
async fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::Config(format!("building the agent definition failed: {e}")))?
}

/// Everything a snapshot is built from besides the definition: the shared, process-wide parts
/// (inference pool, VAD, Smart Turn model, observability, recording) and the way to make the
/// definition's providers and agent links. The start builds the first snapshot with it and
/// every reload builds the next one, so both check a definition the same way.
pub struct RunnerRecipe<F: ProviderFactory> {
    factory: FactoryFn<F>,
    offload: Offload,
    vad: Vad,
    config: RunnerConfig,
    /// The tools of the process without any MCP connection: each snapshot gets a copy
    /// ([`ToolSetup::without_mcp`]) and connects its own definition's MCP servers.
    tools: ToolSetup,
    clock: SharedClock,
    smart_turn: Option<Arc<dyn TurnModel>>,
    turn_offload: Option<Offload>,
    links: Option<LinksFn>,
    observability: Option<Arc<Observability>>,
    recording: Option<Arc<Recording>>,
    redact_caller_numbers: bool,
}

impl<F: ProviderFactory> RunnerRecipe<F> {
    /// A recipe: `factory` makes the providers of a definition (it runs on a tokio worker and
    /// must not block), the calls' heavy jobs go to `offload`, `tools` is the process's tool
    /// setup **before** [`ToolSetup::with_mcp`] (the recipe connects the MCP servers of each
    /// definition itself).
    pub fn new(
        factory: impl Fn(&AgentConfig) -> Result<F> + Send + Sync + 'static,
        offload: Offload,
        vad: Vad,
        config: RunnerConfig,
        tools: ToolSetup,
        clock: &SharedClock,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
            offload,
            vad,
            config,
            tools,
            clock: clock.clone(),
            smart_turn: None,
            turn_offload: None,
            links: None,
            observability: None,
            recording: None,
            redact_caller_numbers: false,
        }
    }

    /// Decides adaptive turns with `model`, its analyses on `offload` when given
    /// ([`EngineRunner::with_smart_turn`]).
    #[must_use]
    pub fn with_smart_turn(mut self, model: Arc<dyn TurnModel>, offload: Option<Offload>) -> Self {
        self.smart_turn = Some(model);
        self.turn_offload = offload;
        self
    }

    /// Resolves each definition's agent processes with `links` ([`AgentLinks::resolve`]).
    #[must_use]
    pub fn with_links(
        mut self,
        links: impl Fn(&AgentConfig) -> Result<AgentLinks> + Send + Sync + 'static,
    ) -> Self {
        self.links = Some(Arc::new(links));
        self
    }

    /// Feeds the calls into `observability`.
    #[must_use]
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        self.observability = Some(observability);
        self
    }

    /// Records the calls as `recording` is set up to.
    #[must_use]
    pub fn with_recording(mut self, recording: Arc<Recording>) -> Self {
        self.recording = Some(recording);
        self
    }

    /// Whether the server redacts phone numbers ([`EngineRunner::with_redact_caller_numbers`]).
    #[must_use]
    pub const fn with_redact_caller_numbers(mut self, redact: bool) -> Self {
        self.redact_caller_numbers = redact;
        self
    }

    /// The process's Smart Turn model, if one was loaded.
    pub fn smart_turn_model(&self) -> Option<&Arc<dyn TurnModel>> {
        self.smart_turn.as_ref()
    }

    /// Builds the snapshot of `agent`: its providers, its tools (connecting its MCP servers
    /// with their startup bounds) and its agent links. Nothing outlives a failure: whatever was
    /// connected is closed before the error returns.
    ///
    /// # Errors
    /// [`Error::Config`] as the start reports it: a provider that does not build, a tool the
    /// process does not offer, an MCP server that does not answer or does not offer an
    /// allowlisted tool.
    pub async fn build(&self, agent: AgentConfig) -> Result<EngineRunner<F>> {
        // The steps that cannot leave anything behind first, then the MCP connections. The
        // synchronous parts run on the blocking pool.
        let (factory, links) = {
            let factory = Arc::clone(&self.factory);
            let links = self.links.clone();
            let agent = agent.clone();
            off_thread(move || {
                let providers = factory(&agent)?;
                let links = match &links {
                    Some(links) => links(&agent)?,
                    None => AgentLinks::default(),
                };
                Ok((providers, links))
            })
            .await?
        };
        let tools = self
            .tools
            .without_mcp()
            .with_mcp(&agent, &self.clock)
            .await?;
        #[cfg(feature = "mcp")]
        let mcp = tools.mcp_host();
        let built = {
            let offload = self.offload.clone();
            let vad = self.vad.clone();
            let config = self.config;
            let clock = self.clock.clone();
            let smart_turn = self.smart_turn.clone();
            let turn_offload = self.turn_offload.clone();
            let observability = self.observability.clone();
            let recording = self.recording.clone();
            let redact = self.redact_caller_numbers;
            off_thread(move || {
                let mut runner =
                    EngineRunner::with_tools(agent, factory, offload, vad, config, tools, &clock)?;
                if let Some(model) = smart_turn {
                    runner = runner.with_smart_turn(model);
                    if let Some(offload) = turn_offload {
                        runner = runner.with_smart_turn_offload(offload);
                    }
                }
                if let Some(observability) = observability {
                    runner = runner.with_observability(observability);
                }
                runner = runner.with_redact_caller_numbers(redact);
                if let Some(recording) = recording {
                    runner = runner.with_recording(recording);
                }
                if !links.is_empty() {
                    runner = runner.with_agent_links(links);
                }
                Ok(runner)
            })
            .await
        };
        match built {
            Ok(runner) => Ok(runner),
            Err(error) => {
                // Nothing may be left running from a snapshot that is not used.
                #[cfg(feature = "mcp")]
                if let Some(mcp) = mcp {
                    mcp.shutdown().await;
                }
                Err(error)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The reloadable definitions

/// Where a definition comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionSource {
    /// A file the operator can edit (`BRISVOX_AGENT_FILE`, `BRISVOX_WS_AGENT_FILE`).
    File(PathBuf),
    /// The environment (`BRISVOX_AGENT_DEFINITION`): it cannot change while the process runs.
    Environment,
}

/// A rule that holds a change back: given the live definition and the new one, a reason when the
/// change needs a restart. Registered per definition with [`AgentSlot::with_veto`].
pub type Veto = Box<dyn Fn(&AgentConfig, &AgentConfig) -> Option<String> + Send + Sync>;

struct SlotState {
    /// Hash of the text the live definition was loaded from.
    loaded: u64,
    /// The file's (mtime, size) at the last poll.
    stat: Option<(SystemTime, u64)>,
    /// A change the poll saw once, waiting to stay the same for one more interval.
    seen: Option<u64>,
    /// The hash of the last text the poll asked a reload for and that did not become live (a
    /// refusal, a restart rule, a timeout): the poll does not ask again until the file changes.
    rejected: Option<u64>,
}

impl SlotState {
    /// What one poll saw of the file; `true` when a reload should be asked for. A change is
    /// asked for **once**: the text is remembered as `rejected` at that moment, so a reload that
    /// does not make it live (invalid, needs a restart, held back, timed out) is not retried at
    /// every interval. A reload that does make it live replaces `loaded` and clears `rejected`;
    /// the endpoint and `SIGHUP` can always ask again.
    fn observe(&mut self, observed: Option<Observation>) -> bool {
        match observed {
            // Unreadable or too slow: look again at the next poll.
            None | Some(Observation::Gone) => false,
            // Untouched for one more interval: the change is stable.
            Some(Observation::Same) => match self.seen.take() {
                Some(hash) => {
                    self.rejected = Some(hash);
                    true
                }
                None => false,
            },
            Some(Observation::Moved { stat, hash }) => {
                self.stat = stat;
                self.seen = match hash {
                    Some(hash) if hash != self.loaded && self.rejected != Some(hash) => Some(hash),
                    _ => None,
                };
                false
            }
        }
    }
}

/// One reloadable definition: the swappable runner, the way to build its snapshots and the rules
/// that hold a change back.
pub struct AgentSlot<F: ProviderFactory> {
    label: &'static str,
    source: DefinitionSource,
    swap: Arc<SwapRunner<F>>,
    recipe: RunnerRecipe<F>,
    vetoes: Vec<Veto>,
    state: Mutex<SlotState>,
}

impl<F: ProviderFactory> AgentSlot<F> {
    /// A definition called `label` (`agent`, `websocket_agent`), read from `source`, run by
    /// `swap`, whose next snapshots `recipe` builds. `loaded_hash` is [`text_hash`] of the text
    /// the live definition was loaded from ([`load_file_hashed`]).
    pub fn new(
        label: &'static str,
        source: DefinitionSource,
        loaded_hash: u64,
        swap: Arc<SwapRunner<F>>,
        recipe: RunnerRecipe<F>,
    ) -> Self {
        Self {
            label,
            source,
            swap,
            recipe,
            vetoes: Vec::new(),
            state: Mutex::new(SlotState {
                loaded: loaded_hash,
                stat: None,
                seen: None,
                rejected: None,
            }),
        }
    }

    /// Holds back a change for which `veto` gives a reason ("needs a restart").
    #[must_use]
    pub fn with_veto(
        mut self,
        veto: impl Fn(&AgentConfig, &AgentConfig) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.vetoes.push(Box::new(veto));
        self
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current_name(&self) -> String {
        self.swap.current().agent().name().to_owned()
    }
}

/// A hash of definition text, to tell whether a file changed (not a security measure).
pub fn text_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// [`config::load_file`] that also returns the [`text_hash`] of the text it parsed (`None` when
/// the file could not be read), so the reload can tell later whether the file changed. Blocking
/// file IO: call at startup or from `spawn_blocking`.
pub fn load_file_hashed(
    path: &Path,
    env: &dyn Env,
) -> (Option<u64>, std::result::Result<AgentConfig, ConfigError>) {
    let name = path.display().to_string();
    match config::read_bounded(path) {
        Ok(text) => (
            Some(text_hash(&text)),
            config::load_str_as(&text, &name, Format::from_path(path), env),
        ),
        Err(message) => (None, Err(ConfigError::single(&name, "", message))),
    }
}

/// What a slot's file read says.
enum Plan {
    /// Nothing to do, and why.
    Unchanged(String),
    /// The text is not usable (the hash tells the poll not to retry it).
    Invalid(String, Option<u64>),
    /// A restart is needed, and why.
    NeedsRestart(String),
    /// A valid new definition.
    Apply(Box<AgentConfig>, u64),
}

/// The reloadable definitions of a server; the [`ReloadTarget`] of its [`ReloadHub`].
pub struct AgentSet<F: ProviderFactory> {
    slots: Vec<AgentSlot<F>>,
    env: Arc<dyn Env + Send + Sync>,
    generation: AtomicU64,
    /// Set by [`ReloadTarget::shutdown`]: no snapshot is swapped in afterwards.
    closed: AtomicBool,
}

impl<F: ProviderFactory> AgentSet<F> {
    /// The reloadable `slots` (the main agent first); secrets and settings in a new file come
    /// from `env`, the environment the start read.
    pub fn new(slots: Vec<AgentSlot<F>>, env: Arc<dyn Env + Send + Sync>) -> Self {
        Self {
            slots,
            env,
            generation: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    /// Re-reads the slot's file, validates it and checks the rules that need a restart.
    async fn plan(&self, slot: &AgentSlot<F>) -> Plan {
        let DefinitionSource::File(path) = &slot.source else {
            return Plan::Unchanged(
                "the definition is BRISVOX_AGENT_DEFINITION, part of the environment, which \
                 cannot change while the server runs: restart to change it"
                    .to_owned(),
            );
        };
        let loaded = slot.state().loaded;
        let read = {
            let path = path.clone();
            let env = Arc::clone(&self.env);
            blocking(&slot.recipe.clock, move || {
                let (hash, agent) = load_file_hashed(&path, &*env);
                if hash == Some(loaded) && agent.is_ok() {
                    return (hash, None);
                }
                (hash, Some(agent))
            })
            .await
        };
        let Some((hash, agent)) = read else {
            return Plan::Invalid(
                format!(
                    "{}: reading the file did not finish in time or failed",
                    path.display()
                ),
                None,
            );
        };
        let agent = match agent {
            None => {
                return Plan::Unchanged("the file has not changed since it was loaded".to_owned());
            }
            Some(Err(error)) => return Plan::Invalid(error.to_string(), hash),
            Some(Ok(agent)) => agent,
        };
        let live = slot.swap.current();
        for veto in &slot.vetoes {
            if let Some(reason) = veto(live.agent(), &agent) {
                return Plan::NeedsRestart(reason);
            }
        }
        Plan::Apply(Box::new(agent), hash.unwrap_or_default())
    }

    async fn apply_inner(&self) -> ReloadReport {
        let mut plans = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            plans.push(self.plan(slot).await);
        }
        // A reload applies completely or not at all: once one definition is refused (or fails
        // to build), the valid others are held back and not even built.
        let mut holds_back = plans
            .iter()
            .any(|p| matches!(p, Plan::Invalid(..) | Plan::NeedsRestart(_)));
        let mut outcomes: Vec<Outcome<F>> = Vec::with_capacity(plans.len());
        for (slot, plan) in self.slots.iter().zip(plans) {
            outcomes.push(match plan {
                Plan::Unchanged(detail) => Outcome::Unchanged(detail),
                Plan::Invalid(detail, hash) => Outcome::Invalid { detail, hash },
                Plan::NeedsRestart(detail) => Outcome::NeedsRestart(detail),
                Plan::Apply(..) if holds_back => Outcome::Held,
                Plan::Apply(agent, hash) => match slot.recipe.build(*agent).await {
                    Ok(runner) => Outcome::Built {
                        runner: Box::new(runner),
                        hash,
                    },
                    Err(error) => {
                        holds_back = true;
                        Outcome::Invalid {
                            detail: error.to_string(),
                            hash: Some(hash),
                        }
                    }
                },
            });
        }
        // The process is shutting down: whatever was built is closed again, nothing is swapped in
        // (the live snapshots were closed, or are being).
        if self.closed.load(Ordering::SeqCst) {
            #[cfg(feature = "mcp")]
            for outcome in &outcomes {
                if let Outcome::Built { runner, .. } = outcome
                    && let Some(mcp) = runner.mcp_host()
                {
                    mcp.shutdown().await;
                }
            }
            return ReloadReport {
                result: ReloadResult::Closed,
                generation: self.generation.load(Ordering::Acquire),
                message: "the server is shutting down: no reload".to_owned(),
                slots: Vec::new(),
            };
        }
        let refused = outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Invalid { .. } | Outcome::NeedsRestart(_)));
        let built = outcomes.iter().any(|o| matches!(o, Outcome::Built { .. }));
        let swap_now = built && !refused;
        let mut message = String::new();
        let mut note = |text: String| {
            if message.is_empty() {
                message = text;
            }
        };
        // From here nothing awaits: the swap is one step, whatever drops this future.
        let generation = if swap_now {
            self.generation
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1)
        } else {
            self.generation.load(Ordering::Acquire)
        };
        let mut retired_calls = 0usize;
        let mut slots = Vec::with_capacity(outcomes.len());
        for (slot, outcome) in self.slots.iter().zip(outcomes) {
            let (result, detail) = match outcome {
                Outcome::Unchanged(detail) => (SlotResult::Unchanged, detail),
                Outcome::Invalid { detail, hash } => {
                    let mut state = slot.state();
                    if hash.is_some() {
                        state.rejected = hash;
                    }
                    state.seen = None;
                    drop(state);
                    note(format!("{}: {detail}", slot.label));
                    (SlotResult::Invalid, detail)
                }
                Outcome::NeedsRestart(detail) => {
                    note(format!("{}: needs a restart: {detail}", slot.label));
                    (SlotResult::NeedsRestart, detail)
                }
                Outcome::Held => (SlotResult::Held, HELD_DETAIL.to_owned()),
                Outcome::Built { runner, hash } if swap_now => {
                    let new_name = runner.agent().name().to_owned();
                    let old = slot.swap.replace(*runner);
                    let mut state = slot.state();
                    state.loaded = hash;
                    state.seen = None;
                    state.rejected = None;
                    drop(state);
                    let live_calls = Arc::strong_count(&old).saturating_sub(1);
                    retired_calls = retired_calls.saturating_add(live_calls);
                    (
                        SlotResult::Applied,
                        format!(
                            "{} -> {new_name}; {live_calls} call(s) keep the old definition until they end",
                            old.agent().name()
                        ),
                    )
                }
                // Built, but another definition was refused after it: dropped (its
                // connections close).
                Outcome::Built { .. } => (SlotResult::Held, HELD_DETAIL.to_owned()),
            };
            slots.push(SlotReport {
                slot: slot.label.to_owned(),
                agent: slot.current_name(),
                result,
                detail,
            });
        }
        let result = if swap_now {
            ReloadResult::Ok
        } else if slots.iter().any(|s| s.result == SlotResult::Invalid) {
            ReloadResult::Invalid
        } else if slots.iter().any(|s| s.result == SlotResult::NeedsRestart) {
            ReloadResult::NeedsRestart
        } else {
            ReloadResult::Unchanged
        };
        if message.is_empty() {
            message = if swap_now {
                format!(
                    "reloaded: new calls run generation {generation}; {retired_calls} call(s) in progress keep the previous definition until they end"
                )
            } else {
                "nothing changed".to_owned()
            };
        }
        ReloadReport {
            result,
            generation,
            message,
            slots,
        }
    }
}

/// Why a valid definition was not applied.
const HELD_DETAIL: &str = "valid, but not applied because another definition of this reload was not (a reload applies completely or not at all)";

/// What became of one slot's [`Plan`] in a reload.
enum Outcome<F: ProviderFactory> {
    Unchanged(String),
    Invalid {
        detail: String,
        hash: Option<u64>,
    },
    NeedsRestart(String),
    /// Valid, not built: another definition was refused.
    Held,
    /// The new snapshot, ready to swap in.
    Built {
        runner: Box<EngineRunner<F>>,
        hash: u64,
    },
}

/// Runs `work` on a blocking thread, bounded by [`FILE_TIMEOUT`] on `clock`; `None` when it did
/// not finish in time or panicked.
async fn blocking<T: Send + 'static>(
    clock: &SharedClock,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let task = tokio::task::spawn_blocking(work);
    tokio::select! {
        biased;
        joined = task => joined.ok(),
        () = clock.sleep(FILE_TIMEOUT) => None,
    }
}

impl<F: ProviderFactory> ReloadTarget for AgentSet<F> {
    fn apply(&self) -> BoxFuture<'_, ReloadReport> {
        Box::pin(self.apply_inner())
    }

    fn changed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let mut confirmed = false;
            for slot in &self.slots {
                let DefinitionSource::File(path) = &slot.source else {
                    continue;
                };
                let previous = slot.state().stat;
                let observed = {
                    let path = path.clone();
                    blocking(&slot.recipe.clock, move || observe_file(&path, previous)).await
                };
                confirmed |= slot.state().observe(observed);
            }
            confirmed
        })
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            #[cfg(feature = "mcp")]
            for slot in &self.slots {
                if let Some(mcp) = slot.swap.current().mcp_host() {
                    mcp.shutdown().await;
                }
            }
        })
    }
}

/// What the poll saw of a file.
enum Observation {
    /// Same mtime and size as at the last poll.
    Same,
    /// The file is gone or unreadable (mid-save): nothing to conclude.
    Gone,
    /// A different mtime or size: the new stat and the hash of the content (`None` when it could
    /// not be read as text).
    Moved {
        stat: Option<(SystemTime, u64)>,
        hash: Option<u64>,
    },
}

/// Blocking: compares the file's (mtime, size) with `previous` and hashes the content when they
/// differ.
fn observe_file(path: &Path, previous: Option<(SystemTime, u64)>) -> Observation {
    let Ok(meta) = std::fs::metadata(path) else {
        return Observation::Gone;
    };
    let stat = meta.modified().ok().map(|modified| (modified, meta.len()));
    if stat.is_some() && stat == previous {
        return Observation::Same;
    }
    let hash = config::read_bounded(path).ok().map(|text| text_hash(&text));
    Observation::Moved { stat, hash }
}

/// Why a reload that would change the agent's `[recording]` table is held back: recording is set
/// up once (directory, retention sweeper, encryption).
pub fn recording_veto(live: &AgentConfig, new: &AgentConfig) -> Option<String> {
    (live.recording() != new.recording()).then(|| {
        "the [recording] table changed: recording is set up when the server starts".to_owned()
    })
}

/// Why a reload that would use a provider whose `BRISVOX_PROVIDER_LIMITS` limit the start
/// dropped (no call used the provider then) is held back: the limit is applied at admission,
/// which is built once. `dropped` is the list of the dropped limits' provider names.
pub fn provider_limit_veto(
    dropped: Vec<String>,
) -> impl Fn(&AgentConfig, &AgentConfig) -> Option<String> + Send + Sync + 'static {
    move |_, new| {
        crate::admission::agent_providers(new)
            .into_iter()
            .find(|name| dropped.iter().any(|d| d == name))
            .map(|name| {
                format!(
                    "the new definition uses the provider `{name}`, which has a limit in \
                     BRISVOX_PROVIDER_LIMITS that only takes effect for providers in use at the \
                     start"
                )
            })
    }
}

/// Why a reload to an adaptive turn policy is held back when the start loaded no Smart Turn
/// model: the model and its executor are set up once. `has_model` says whether the process has
/// one.
pub fn smart_turn_veto(
    env: Arc<dyn Env + Send + Sync>,
    has_model: bool,
) -> impl Fn(&AgentConfig, &AgentConfig) -> Option<String> + Send + Sync + 'static {
    move |_, new| {
        if has_model {
            return None;
        }
        match crate::runner::wants_smart_turn_model(&*env, new) {
            Ok(true) => Some(
                "the new definition has an adaptive turn policy and the server started without a \
                 Smart Turn model: the model and its executor are set up when the server starts"
                    .to_owned(),
            ),
            _ => None,
        }
    }
}

/// Wires the triggers of a reload hub: the poll when [`ReloadSettings`] asks for it and `SIGHUP`
/// on Unix. (`POST /admin/reload` needs only the hub on the session manager.)
///
/// # Errors
/// [`Error::Config`] when the `SIGHUP` handler cannot be installed.
pub fn start_watchers(hub: &Arc<ReloadHub>, settings: &ReloadSettings) -> Result<()> {
    #[cfg(unix)]
    hub.watch_hangup()?;
    if let Some(interval) = settings.poll {
        hub.watch_files(interval);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
