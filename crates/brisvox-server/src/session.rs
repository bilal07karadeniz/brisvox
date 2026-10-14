//! The session manager: bounded admission, one supervised task per session, panic isolation,
//! guaranteed registry cleanup and a staged shutdown.
//!
//! # Model
//!
//! A [`SessionManager`] owns a registry of live sessions. A session is any future
//! `FnOnce(SessionContext) -> impl Future<Output = Result<T, E>>`; the manager does not know what
//! runs inside, so it does not depend on the engine's `run_call` (the engine plugs in through
//! [`SessionRunner`], see below).
//!
//! * **Admission.** At most [`ManagerConfig::max_sessions`] sessions are live or reserved. A full
//!   manager answers [`AdmissionError::AtCapacity`], a draining one
//!   [`AdmissionError::ShuttingDown`]; nothing queues. An [`AdmissionPolicy`]
//!   ([`SessionManager::with_admission`]) adds two more refusals after those:
//!   [`AdmissionError::Overloaded`] while the inference pool is under pressure and
//!   [`AdmissionError::ProviderBudget`] when a provider the call uses is at its concurrency
//!   limit; the provider permits ride in the [`SessionSlot`] and then with the session, and
//!   come back on every end path. A transport that wants to refuse before
//!   it spends work (a WebSocket upgrade, a SIP 200 OK) calls [`SessionManager::try_admit`],
//!   gets a [`SessionSlot`], and spawns into it once the connection is accepted. Dropping the
//!   slot gives the reservation back. A reservation is honoured even if draining begins before
//!   it is spawned into ([`SessionSlot::spawn`] cannot fail): the transport has already
//!   accepted the call, so the session runs and is drained, cancelled or aborted by the
//!   shutdown like any other. The shutdown also waits for outstanding reservations, so no
//!   session can start unnoticed after it returned.
//! * **Isolation.** Every session runs in its own tokio task. Building the session future,
//!   running it and turning its result into a [`SessionEnded`] (the error's `Display`, the drop
//!   of the output or error) all run under `catch_unwind`, so a panic in any of them ends only
//!   that session ([`SessionEnded::Panicked`], counted); the worker thread, the other sessions
//!   and the manager are unaffected. A panic while *dropping* the panic payload is contained
//!   too.
//! * **Cleanup on every exit path.** The registry entry is removed by a drop guard owned by the
//!   task, so normal return, error, panic, cancellation, `abort()` (drain timeout) and a runtime
//!   that drops the task unpolled all remove it exactly once, update the stats and wake
//!   [`SessionManager::wait_idle`]. Each session has a [`CancellationToken`] that is a child of
//!   the manager-wide token. Cancellation is cooperative: the session is expected to return soon
//!   after its token fires; the manager aborts the task only at the last shutdown stage.
//! * **Shutdown.** [`SessionManager::shutdown`] runs three stages with timeouts on the injected
//!   [`brisvox_core::Clock`]: (1) stop admitting and let sessions finish by themselves for
//!   [`ShutdownPolicy::drain_timeout`]; (2) cancel every session token and wait
//!   [`ShutdownPolicy::cancel_grace`]; (3) abort what is left and wait
//!   [`ShutdownPolicy::abort_wait`]. The [`ShutdownReport`] says how many sessions ended in each
//!   stage and how many (should be zero) were still registered at the end.
//!
//! # Classifying an end
//!
//! * a panic is [`SessionEnded::Panicked`];
//! * otherwise, if the session's token was cancelled by the time its future returned, the end is
//!   [`SessionEnded::Cancelled`] whatever the future returned (errors during teardown are
//!   expected, and a session that finished in the same instant as the cancel is
//!   indistinguishable from one that obeyed it);
//! * otherwise `Ok` is [`SessionEnded::Completed`] and `Err` is [`SessionEnded::Failed`] with
//!   the error's `Display` text (bounded to 256 characters; error text must never carry
//!   credentials, `docs/TRACING.md`);
//! * a task dropped before it returned (abort, runtime shutdown) is [`SessionEnded::Aborted`].
//!
//! # Adapter point for the engine (`run_call`)
//!
//! Built: [`crate::runner::EngineRunner`], fed by the hosted transports of
//! [`crate::hosting`] (which reserve the slot before the upgrade, through an admission gate).
//! The sketch below is the shape it follows.
//!
//! Implement [`SessionRunner`] for a type holding the process-wide pieces (provider factories,
//! `EngineConfig`, the inference pool handle) and let `run` build and drive one call:
//!
//! ```ignore
//! impl SessionRunner for EngineRunner {
//!     type Request = AcceptedCall;          // CallLink / TransportSession + agent definition
//!     type Output = CallOutcome;
//!     type Error = brisvox_engine::Error;
//!     fn run(self: Arc<Self>, ctx: SessionContext, call: AcceptedCall)
//!         -> impl Future<Output = Result<CallOutcome, Self::Error>> + Send + 'static
//!     {
//!         let cfg = self.config.clone();
//!         async move {
//!             let running = run_call(call, cfg, ctx.clock.clone(), ctx.cancel.clone()).await?;
//!             running.wait().await            // returns when the call ends or ctx.cancel fires
//!         }
//!     }
//! }
//! // accept loop: reserve first, so a refusal can still be answered on the link it owns.
//! while let Some(link) = transport.accept(&cancel).await? {
//!     match manager.try_admit() {
//!         Ok(slot) => { slot.spawn_with(&runner, AcceptedCall::new(link)); } // never refused
//!         Err(refused) => link.reject(refused), // WebSocket close 1013, SIP 503, HTTP 503
//!     }
//! }
//! ```
//!
//! `run` must watch `ctx.cancel` (pass it, or a child, as the pipeline's cancel token) and
//! stop the pipeline with its own timeout when it fires; dropping the future is the abort path
//! and must leave nothing running (the `RunningPipeline` drop guard does).

use std::collections::HashMap;
use std::fmt::{self, Display};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use futures::FutureExt;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::admission::{AdmissionPolicy, AdmissionSnapshot, Permits};
use crate::observability::Observability;
use crate::{Error, Result};

/// Longest error or panic text kept in a [`SessionEnded`], in characters.
const MAX_MESSAGE_CHARS: usize = 256;

/// Stand-in used when a panic payload is neither `&str` nor `String`.
const UNKNOWN_PANIC: &str = "panic with a non-string payload";

/// Identifies one session for its whole life. Ids are never reused within a manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    /// The numeric value (monotonic per manager, starting at 1).
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Why a session was not admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AdmissionError {
    /// The manager already holds [`ManagerConfig::max_sessions`] live or reserved sessions.
    /// Transports should refuse the call cleanly (HTTP 503, WebSocket close 1013, SIP 503).
    #[error("server at capacity ({max} sessions)")]
    AtCapacity {
        /// The configured limit.
        max: usize,
    },
    /// The manager is draining or shut down and admits nothing new.
    #[error("server is shutting down")]
    ShuttingDown,
    /// The inference pool is under pressure ([`crate::admission::OverloadConfig`]): a new call
    /// would degrade the running ones. Transports answer like a busy server, with a longer
    /// retry hint.
    #[error("server overloaded")]
    Overloaded,
    /// A provider the call would use is at its concurrency limit
    /// ([`crate::admission::AdmissionSettings::provider_limits`]).
    #[error("provider {provider} at its limit of {limit} concurrent calls")]
    ProviderBudget {
        /// The exhausted provider.
        provider: Arc<str>,
        /// Its configured limit.
        limit: usize,
    },
}

impl From<AdmissionError> for Error {
    fn from(e: AdmissionError) -> Self {
        match e {
            AdmissionError::AtCapacity { .. } => Self::AtCapacity,
            AdmissionError::ShuttingDown => Self::ShuttingDown,
            AdmissionError::Overloaded => Self::Overloaded,
            AdmissionError::ProviderBudget { provider, limit } => Self::ProviderBudget {
                provider: provider.to_string(),
                limit,
            },
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEnded {
    /// The session future returned `Ok` and nobody had cancelled it.
    Completed,
    /// The session future returned `Err`; the error's `Display` text, bounded to 256 characters.
    Failed(String),
    /// The session panicked; the panic message, bounded to 256 characters. Other sessions were
    /// not affected.
    Panicked(String),
    /// The session's token was cancelled (per session or by shutdown) and it returned.
    Cancelled,
    /// The task was dropped before the future returned: aborted at the last shutdown stage, or
    /// the runtime went away.
    Aborted,
}

impl SessionEnded {
    /// The payload-free kind, as counted in [`EndCounts`].
    pub const fn reason(&self) -> EndReason {
        match self {
            Self::Completed => EndReason::Completed,
            Self::Failed(_) => EndReason::Failed,
            Self::Panicked(_) => EndReason::Panicked,
            Self::Cancelled => EndReason::Cancelled,
            Self::Aborted => EndReason::Aborted,
        }
    }
}

/// The kind of a [`SessionEnded`], without its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndReason {
    /// See [`SessionEnded::Completed`].
    Completed,
    /// See [`SessionEnded::Failed`].
    Failed,
    /// See [`SessionEnded::Panicked`].
    Panicked,
    /// See [`SessionEnded::Cancelled`].
    Cancelled,
    /// See [`SessionEnded::Aborted`].
    Aborted,
}

/// Sessions ended so far, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndCounts {
    /// Returned `Ok`.
    pub completed: u64,
    /// Returned `Err`.
    pub failed: u64,
    /// Panicked.
    pub panicked: u64,
    /// Ended after a cancel.
    pub cancelled: u64,
    /// Task dropped before returning.
    pub aborted: u64,
}

impl EndCounts {
    /// Total sessions ended, any reason.
    pub const fn total(&self) -> u64 {
        self.completed + self.failed + self.panicked + self.cancelled + self.aborted
    }

    /// The count for one reason.
    pub const fn get(&self, reason: EndReason) -> u64 {
        match reason {
            EndReason::Completed => self.completed,
            EndReason::Failed => self.failed,
            EndReason::Panicked => self.panicked,
            EndReason::Cancelled => self.cancelled,
            EndReason::Aborted => self.aborted,
        }
    }

    fn bump(&mut self, reason: EndReason) {
        let slot = match reason {
            EndReason::Completed => &mut self.completed,
            EndReason::Failed => &mut self.failed,
            EndReason::Panicked => &mut self.panicked,
            EndReason::Cancelled => &mut self.cancelled,
            EndReason::Aborted => &mut self.aborted,
        };
        *slot = slot.saturating_add(1);
    }
}

/// A consistent snapshot of the manager's counters (taken under the registry lock, so
/// `started == active + ended.total()` always holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// Sessions registered right now.
    pub active: usize,
    /// Most sessions registered at once so far.
    pub peak_active: usize,
    /// Slots reserved by [`SessionManager::try_admit`] and not yet spawned or dropped.
    pub reserved: usize,
    /// The admission limit.
    pub max_sessions: usize,
    /// Sessions started since the manager was created.
    pub started: u64,
    /// Admissions refused because the manager was full.
    pub rejected_at_capacity: u64,
    /// Admissions refused because the manager was draining.
    pub rejected_shutting_down: u64,
    /// Admissions refused because the inference pool was under pressure.
    pub rejected_overloaded: u64,
    /// Admissions refused because a provider was at its concurrency limit; the
    /// per-provider split is in [`SessionManager::admission_snapshot`].
    pub rejected_provider_budget: u64,
    /// Sessions ended, by reason.
    pub ended: EndCounts,
}

/// Configuration of a [`SessionManager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerConfig {
    /// Most sessions live or reserved at once. Further admissions are refused, not queued.
    pub max_sessions: usize,
}

impl ManagerConfig {
    /// Default limit (`BRISVOX_MAX_SESSIONS` overrides it). The pool-pressure and provider-budget
    /// rules of [`AdmissionPolicy`] refuse earlier than this when they apply.
    pub const DEFAULT_MAX_SESSIONS: usize = 256;

    /// The largest accepted limit: far above what one process serves, and small enough that the
    /// limits derived from it (the hosted transports' socket bounds, twice this) stay valid.
    pub const MAX_SESSIONS: usize = 100_000;

    /// A config with the given limit.
    ///
    /// # Errors
    /// [`Error::Config`] if `max_sessions` is zero (such a server could never serve a call) or
    /// above [`Self::MAX_SESSIONS`].
    pub fn new(max_sessions: usize) -> Result<Self> {
        if max_sessions == 0 {
            return Err(Error::Config("max_sessions must be at least 1".into()));
        }
        if max_sessions > Self::MAX_SESSIONS {
            return Err(Error::Config(format!(
                "max_sessions must be at most {}",
                Self::MAX_SESSIONS
            )));
        }
        Ok(Self { max_sessions })
    }
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            max_sessions: Self::DEFAULT_MAX_SESSIONS,
        }
    }
}

/// Timeouts of the three shutdown stages. All run on the manager's injected clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownPolicy {
    /// Stage 1: admit nothing and let sessions finish by themselves (a call in progress ends
    /// naturally). Zero skips straight to cancellation.
    pub drain_timeout: Duration,
    /// Stage 2: after every session token is cancelled, how long sessions get to return.
    pub cancel_grace: Duration,
    /// Stage 3: after the remaining tasks are aborted, how long to wait for their drop guards
    /// to clean up (this is only scheduler latency; tasks are dropped at their next poll).
    pub abort_wait: Duration,
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            drain_timeout: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(5),
            abort_wait: Duration::from_secs(1),
        }
    }
}

/// What [`SessionManager::shutdown`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Sessions registered or reserved when the shutdown began.
    pub initial: usize,
    /// Ended by themselves during the drain stage.
    pub drained: usize,
    /// Ended after their token was cancelled.
    pub cancelled: usize,
    /// Had to be aborted.
    pub aborted: usize,
    /// Still registered or reserved after every stage. Anything but zero is a bug worth an
    /// alert.
    pub leaked: usize,
}

/// What a session receives when it starts.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// The session's id.
    pub id: SessionId,
    /// Fires when this session (or the whole manager) is cancelled. Pass it, or a child, to the
    /// pipeline; return soon after it fires.
    pub cancel: CancellationToken,
    /// The injected clock, for every timeout and timestamp inside the session.
    pub clock: SharedClock,
}

/// Builds and drives one kind of session from an accepted request; the adapter point for the
/// engine's `run_call` (see the module docs).
pub trait SessionRunner: Send + Sync + 'static {
    /// What a transport hands over on accept (link, agent definition, ...).
    type Request: Send + 'static;
    /// What the session produces; dropped by the manager (log or record it inside `run`).
    type Output: Send + 'static;
    /// Why a session failed. Its `Display` text is logged and kept in
    /// [`SessionEnded::Failed`]: it must not contain credentials.
    type Error: Display + Send + 'static;

    /// The future that runs one session to its end. Takes the runner by `Arc` so the future
    /// can be `'static` without borrowing it.
    fn run(
        self: Arc<Self>,
        ctx: SessionContext,
        request: Self::Request,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send + 'static;
}

/// Handle to a started session. Dropping it detaches: the session keeps running.
#[derive(Debug)]
pub struct SessionHandle {
    id: SessionId,
    cancel: CancellationToken,
    ended: oneshot::Receiver<SessionEnded>,
}

impl SessionHandle {
    /// The session's id.
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Asks the session to stop (cancels its token). Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Waits until the session has ended and the registry no longer holds it.
    pub async fn ended(self) -> SessionEnded {
        self.ended.await.unwrap_or(SessionEnded::Aborted)
    }
}

/// A reserved admission. Spawn a session into it, or drop it to give the reservation back.
#[derive(Debug)]
#[must_use = "a slot holds a capacity reservation until it is spawned into or dropped"]
pub struct SessionSlot {
    inner: Arc<Inner>,
    id: SessionId,
    live: bool,
    /// The provider budgets this call holds; they move to the session when it spawns.
    permits: Permits,
}

impl SessionSlot {
    /// The id the session will have.
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Starts the session in its own task.
    ///
    /// `make` runs inside the task, under the panic guard, so a panic while *building* the
    /// session future is contained like one while running it.
    ///
    /// Never refused: a slot reserved before the manager began draining still starts its
    /// session (see the module docs). If the shutdown has already cancelled every session, the
    /// new session's token starts cancelled; if it has reached the abort stage, the task is
    /// aborted at once. Either way the session is registered, classified and counted.
    pub fn spawn<M, F, T, E>(mut self, make: M) -> SessionHandle
    where
        M: FnOnce(SessionContext) -> F + Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Display + Send + 'static,
    {
        self.live = false; // from here the slot's reservation is consumed or released below
        let permits = std::mem::take(&mut self.permits);
        self.inner.clone().spawn_session(self.id, permits, make)
    }

    /// Starts a session driven by `runner` in this slot (the engine adapter point; see the
    /// module docs). Never refused, as [`spawn`](Self::spawn): `request` always reaches
    /// [`SessionRunner::run`].
    pub fn spawn_with<R: SessionRunner>(
        self,
        runner: &Arc<R>,
        request: R::Request,
    ) -> SessionHandle {
        let runner = Arc::clone(runner);
        self.spawn(move |ctx| runner.run(ctx, request))
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        if self.live {
            // Budgets first, so a waiter woken by the reservation's release sees them free.
            drop(std::mem::take(&mut self.permits));
            self.inner.release_reservation();
        }
    }
}

/// Admission control, supervision and registry of live sessions. Cheap to clone (all clones
/// share one registry). Dropping every clone does not stop running sessions; call
/// [`shutdown`](Self::shutdown).
#[derive(Debug, Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl SessionManager {
    /// A manager that spawns sessions on the current tokio runtime and takes time from `clock`.
    ///
    /// # Errors
    /// [`Error::Config`] when called outside a tokio runtime, or when `config` breaks the rules
    /// of [`ManagerConfig::new`] (its field is public).
    pub fn new(config: ManagerConfig, clock: SharedClock) -> Result<Self> {
        Self::with_admission(config, clock, AdmissionPolicy::none())
    }

    /// [`new`](Self::new) with an [`AdmissionPolicy`]: pool-pressure shedding and provider
    /// budgets on top of the session limit.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn with_admission(
        config: ManagerConfig,
        clock: SharedClock,
        policy: AdmissionPolicy,
    ) -> Result<Self> {
        ManagerConfig::new(config.max_sessions)?;
        let runtime = Handle::try_current()
            .map_err(|_| Error::Config("SessionManager::new needs a tokio runtime".into()))?;
        let (outstanding_tx, _) = watch::channel(0);
        Ok(Self {
            inner: Arc::new(Inner {
                runtime,
                clock,
                max_sessions: config.max_sessions,
                policy,
                next_id: AtomicU64::new(1),
                shutdown: CancellationToken::new(),
                outstanding_tx,
                registry: Mutex::new(Registry::default()),
                observability: OnceLock::new(),
            }),
        })
    }

    /// Reserves capacity for one session without starting it (see the module docs).
    ///
    /// Refusals are checked in this order: draining, session limit, pool pressure, provider
    /// budgets.
    ///
    /// # Errors
    /// [`AdmissionError::ShuttingDown`], [`AdmissionError::AtCapacity`],
    /// [`AdmissionError::Overloaded`] or [`AdmissionError::ProviderBudget`]; the refusal is
    /// counted.
    pub fn try_admit(&self) -> Result<SessionSlot, AdmissionError> {
        // Sampled before the registry lock: the pool is read, the registry is not touched.
        let overloaded = self.inner.policy.overloaded(self.inner.clock.now());
        let mut reg = self.inner.lock();
        if reg.phase != Phase::Open {
            reg.rejected_shutting_down = reg.rejected_shutting_down.saturating_add(1);
            return Err(AdmissionError::ShuttingDown);
        }
        if reg.entries.len().saturating_add(reg.reserved) >= self.inner.max_sessions {
            reg.rejected_at_capacity = reg.rejected_at_capacity.saturating_add(1);
            return Err(AdmissionError::AtCapacity {
                max: self.inner.max_sessions,
            });
        }
        if overloaded {
            reg.rejected_overloaded = reg.rejected_overloaded.saturating_add(1);
            return Err(AdmissionError::Overloaded);
        }
        let permits = match self.inner.policy.acquire() {
            Ok(permits) => permits,
            Err(refused) => {
                reg.rejected_provider_budget = reg.rejected_provider_budget.saturating_add(1);
                return Err(refused);
            }
        };
        reg.reserved = reg.reserved.saturating_add(1);
        self.inner.publish(&reg);
        drop(reg);
        let id = SessionId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        Ok(SessionSlot {
            inner: Arc::clone(&self.inner),
            id,
            live: true,
            permits,
        })
    }

    /// Admits and starts one session: [`try_admit`](Self::try_admit) then
    /// [`SessionSlot::spawn`].
    ///
    /// # Errors
    /// See [`try_admit`](Self::try_admit). On refusal `make` is dropped unused.
    pub fn start<M, F, T, E>(&self, make: M) -> Result<SessionHandle, AdmissionError>
    where
        M: FnOnce(SessionContext) -> F + Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Display + Send + 'static,
    {
        Ok(self.try_admit()?.spawn(make))
    }

    /// Admits and starts a session driven by `runner` (the engine adapter point).
    ///
    /// # Errors
    /// See [`try_admit`](Self::try_admit). On refusal `request` is dropped, which closes a
    /// transport link it owns without a reason; a transport that wants to answer the refusal
    /// on the link (close 1013, SIP 503) uses [`try_admit`](Self::try_admit) and
    /// [`SessionSlot::spawn_with`] instead.
    pub fn start_with<R: SessionRunner>(
        &self,
        runner: &Arc<R>,
        request: R::Request,
    ) -> Result<SessionHandle, AdmissionError> {
        Ok(self.try_admit()?.spawn_with(runner, request))
    }

    /// Cancels one session's token. `false` if it is not registered (already ended).
    pub fn cancel(&self, id: SessionId) -> bool {
        let token = self.inner.lock().entries.get(&id).map(|e| e.cancel.clone());
        token.is_some_and(|t| {
            t.cancel();
            true
        })
    }

    /// Cancels every session registered right now. Sessions started later (including those
    /// spawned into a slot reserved before this call) are not affected, so the manager keeps
    /// serving new calls normally. To stop admissions as well use
    /// [`begin_drain`](Self::begin_drain) or [`shutdown`](Self::shutdown).
    pub fn cancel_all(&self) {
        let tokens: Vec<CancellationToken> = self
            .inner
            .lock()
            .entries
            .values()
            .map(|e| e.cancel.clone())
            .collect();
        for token in tokens {
            token.cancel();
        }
    }

    /// Stops admitting new sessions (`/readyz` turns false); running ones continue. Permanent.
    pub fn begin_drain(&self) {
        let mut reg = self.inner.lock();
        if reg.phase == Phase::Open {
            reg.phase = Phase::Draining;
        }
    }

    /// A token that fires when [`shutdown`](Self::shutdown) reaches its cancel stage (and stays
    /// quiet through the drain stage and [`cancel_all`](Self::cancel_all)). Hosted transports
    /// run under it: their live calls keep their connections while the drain lets
    /// them end by themselves, and at the cancel stage the connections go too, so one still in
    /// its handshake gives its reservation back instead of holding the shutdown up.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.child_token()
    }

    /// Whether new sessions are admitted (false once draining began).
    pub fn is_accepting(&self) -> bool {
        self.inner.lock().phase == Phase::Open
    }

    /// Whether `id` is registered right now.
    pub fn contains(&self, id: SessionId) -> bool {
        self.inner.lock().entries.contains_key(&id)
    }

    /// Sessions registered right now.
    pub fn active(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// Ids of the registered sessions, oldest first.
    pub fn active_ids(&self) -> Vec<SessionId> {
        let mut ids: Vec<SessionId> = self.inner.lock().entries.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// A consistent snapshot of the counters.
    pub fn stats(&self) -> SessionStats {
        let reg = self.inner.lock();
        SessionStats {
            active: reg.entries.len(),
            peak_active: reg.peak_active,
            reserved: reg.reserved,
            max_sessions: self.inner.max_sessions,
            started: reg.started,
            rejected_at_capacity: reg.rejected_at_capacity,
            rejected_shutting_down: reg.rejected_shutting_down,
            rejected_overloaded: reg.rejected_overloaded,
            rejected_provider_budget: reg.rejected_provider_budget,
            ended: reg.ended,
        }
    }

    /// The admission policy's state now: the pressure verdict (sampling the pool) and each
    /// limited provider's budget. Empty without a policy.
    pub fn admission_snapshot(&self) -> AdmissionSnapshot {
        self.inner.policy.snapshot(self.inner.clock.now())
    }

    /// Attaches the process's [`Observability`]: the health listener then serves `GET /metrics`
    /// from it. Set once; `false` when one was attached already.
    pub fn attach_observability(&self, observability: Arc<Observability>) -> bool {
        self.inner.observability.set(observability).is_ok()
    }

    /// The attached [`Observability`], if any.
    pub fn observability(&self) -> Option<&Arc<Observability>> {
        self.inner.observability.get()
    }

    /// Waits until no session is registered or reserved, at most `timeout` (on the injected
    /// clock). `true` if none is.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        self.inner.wait_idle(timeout).await
    }

    /// Stops admissions and ends every session in three stages (see [`ShutdownPolicy`]).
    /// Returns when the registry is empty or the last stage timed out; never blocks longer than
    /// the sum of the policy's three timeouts. Calling it again is harmless.
    pub async fn shutdown(&self, policy: ShutdownPolicy) -> ShutdownReport {
        let inner = &self.inner;
        let initial = {
            let mut reg = inner.lock();
            reg.phase = Phase::Draining;
            reg.outstanding()
        };
        tracing::info!(target: "brisvox::server", sessions = initial, "draining sessions");

        inner.wait_idle(policy.drain_timeout).await;
        let after_drain = inner.outstanding();

        if after_drain > 0 {
            tracing::warn!(target: "brisvox::server", sessions = after_drain, "drain timeout, cancelling sessions");
            inner.shutdown.cancel();
            inner.wait_idle(policy.cancel_grace).await;
        }
        let after_cancel = inner.outstanding();

        if after_cancel > 0 {
            tracing::error!(target: "brisvox::server", sessions = after_cancel, "sessions ignored cancellation, aborting");
            inner.abort_all();
            inner.wait_idle(policy.abort_wait).await;
        }
        let leaked = inner.outstanding();

        let report = ShutdownReport {
            initial,
            drained: initial.saturating_sub(after_drain),
            cancelled: after_drain.saturating_sub(after_cancel),
            aborted: after_cancel.saturating_sub(leaked),
            leaked,
        };
        tracing::info!(target: "brisvox::server", ?report, "session shutdown finished");
        report
    }
}

// ---------------------------------------------------------------------------------------------
// Internals.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Draining,
}

#[derive(Debug)]
struct Entry {
    cancel: CancellationToken,
    /// Set right after the task is spawned (spawning happens outside the lock, see
    /// [`Inner::spawn_session`]).
    abort: Option<AbortHandle>,
    started_at: Instant,
}

#[derive(Debug)]
struct Registry {
    entries: HashMap<SessionId, Entry>,
    phase: Phase,
    /// Set by the abort stage: tasks spawned but not yet holding an abort handle are aborted as
    /// soon as they get one.
    aborting: bool,
    reserved: usize,
    peak_active: usize,
    started: u64,
    rejected_at_capacity: u64,
    rejected_shutting_down: u64,
    rejected_overloaded: u64,
    rejected_provider_budget: u64,
    ended: EndCounts,
}

impl Registry {
    /// Registered sessions plus reserved slots: what the shutdown waits for.
    fn outstanding(&self) -> usize {
        self.entries.len().saturating_add(self.reserved)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            phase: Phase::Open,
            aborting: false,
            reserved: 0,
            peak_active: 0,
            started: 0,
            rejected_at_capacity: 0,
            rejected_shutting_down: 0,
            rejected_overloaded: 0,
            rejected_provider_budget: 0,
            ended: EndCounts::default(),
        }
    }
}

struct Inner {
    runtime: Handle,
    clock: SharedClock,
    max_sessions: usize,
    /// Pool-pressure and provider-budget rules beyond `max_sessions`.
    policy: AdmissionPolicy,
    next_id: AtomicU64,
    /// Parent of every session token.
    shutdown: CancellationToken,
    /// [`Registry::outstanding`], for [`Inner::wait_idle`]. Written only under the registry
    /// lock, by [`Inner::publish`].
    outstanding_tx: watch::Sender<usize>,
    registry: Mutex<Registry>,
    /// What `/metrics` serves; set once by the binary.
    observability: OnceLock<Arc<Observability>>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("max_sessions", &self.max_sessions)
            .field("outstanding", &*self.outstanding_tx.borrow())
            .finish_non_exhaustive()
    }
}

impl Inner {
    /// The registry lock. Never held across an await or a spawn; a poisoned lock (a panic while
    /// holding it, which this module's code cannot cause) is recovered rather than propagated,
    /// because the registry stays structurally valid.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release_reservation(&self) {
        let mut reg = self.lock();
        reg.reserved = reg.reserved.saturating_sub(1);
        self.publish(&reg);
    }

    /// Publishes the outstanding count; call with the registry lock held, after every change.
    fn publish(&self, reg: &Registry) {
        self.outstanding_tx.send_replace(reg.outstanding());
    }

    /// Registered plus reserved, right now.
    fn outstanding(&self) -> usize {
        self.lock().outstanding()
    }

    fn abort_all(&self) {
        let handles: Vec<AbortHandle> = {
            let mut reg = self.lock();
            reg.aborting = true;
            reg.entries
                .values()
                .filter_map(|e| e.abort.clone())
                .collect()
        };
        // Outside the lock: dropping an aborted task runs its drop guard, which takes the lock.
        for handle in handles {
            handle.abort();
        }
    }

    async fn wait_idle(&self, timeout: Duration) -> bool {
        let mut rx = self.outstanding_tx.subscribe();
        tokio::select! {
            idle = async { rx.wait_for(|n| *n == 0).await.is_ok() } => idle,
            () = self.clock.sleep(timeout) => *self.outstanding_tx.borrow() == 0,
        }
    }

    /// Turns a reservation into a registered session, then spawns its task. Registering first
    /// (and spawning outside the lock) means the task's drop guard can always take the lock,
    /// even if the runtime drops the task during `spawn` itself.
    fn spawn_session<M, F, T, E>(
        self: Arc<Self>,
        id: SessionId,
        permits: Permits,
        make: M,
    ) -> SessionHandle
    where
        M: FnOnce(SessionContext) -> F + Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Display + Send + 'static,
    {
        let cancel = self.shutdown.child_token();
        {
            let mut reg = self.lock();
            // The reservation was admitted; it is honoured whatever the phase is now.
            reg.reserved = reg.reserved.saturating_sub(1);
            reg.entries.insert(
                id,
                Entry {
                    cancel: cancel.clone(),
                    abort: None,
                    started_at: self.clock.now(),
                },
            );
            reg.started = reg.started.saturating_add(1);
            reg.peak_active = reg.peak_active.max(reg.entries.len());
            self.publish(&reg);
        }

        let (end_tx, end_rx) = oneshot::channel();
        let guard = EndGuard {
            inner: Arc::clone(&self),
            id,
            cancel: cancel.clone(),
            end_tx: Some(end_tx),
            permits,
        };
        let ctx = SessionContext {
            id,
            cancel: cancel.clone(),
            clock: Arc::clone(&self.clock),
        };
        let span = brisvox_core::telemetry::session_span(&id.to_string());
        let task = async move {
            let mut guard = guard;
            let cancel = guard.cancel.clone();
            // Classification runs under the guard too: the error's `Display` and the drop of
            // the output or error are user code and may panic.
            let ended = AssertUnwindSafe(async move {
                let result = make(ctx).await;
                classify(result, cancel.is_cancelled())
            })
            .catch_unwind()
            .await
            .unwrap_or_else(panicked);
            guard.finish(ended);
        }
        .instrument(span);

        let join = self.runtime.spawn(task);
        {
            let mut reg = self.lock();
            let aborting = reg.aborting;
            if let Some(entry) = reg.entries.get_mut(&id) {
                // Absent: the task already ended (or was dropped during spawn) and cleaned up.
                let abort = join.abort_handle();
                if aborting {
                    abort.abort();
                }
                entry.abort = Some(abort);
            }
        }
        SessionHandle {
            id,
            cancel,
            ended: end_rx,
        }
    }

    /// Removes the session and records how it ended. A session already finished is ignored.
    fn finish(&self, id: SessionId, reason: EndReason) -> Option<Instant> {
        let mut reg = self.lock();
        let entry = reg.entries.remove(&id)?;
        reg.ended.bump(reason);
        self.publish(&reg);
        Some(entry.started_at)
    }
}

/// Owned by the session task. Removes the registry entry on every exit path: `finish` on the
/// normal and panic paths, `Drop` when the task is dropped before it got there.
struct EndGuard {
    inner: Arc<Inner>,
    id: SessionId,
    cancel: CancellationToken,
    /// `None` once the end was recorded.
    end_tx: Option<oneshot::Sender<SessionEnded>>,
    /// The call's provider budgets: given back before the end is recorded (and, if the task is
    /// dropped without reaching it, with the guard).
    permits: Permits,
}

impl EndGuard {
    fn finish(&mut self, ended: SessionEnded) {
        // Before the registry shows the session gone, so anything that saw it end (a
        // `wait_idle`, an admission retry) also finds its budgets free.
        drop(std::mem::take(&mut self.permits));
        let Some(end_tx) = self.end_tx.take() else {
            return;
        };
        let reason = ended.reason();
        let started_at = self.inner.finish(self.id, reason);
        let lived = started_at.map(|s| self.inner.clock.now().saturating_duration_since(s));
        match &ended {
            SessionEnded::Panicked(message) => {
                tracing::error!(target: "brisvox::server", id = %self.id, ?lived, %message, "session panicked");
            }
            SessionEnded::Failed(message) => {
                tracing::warn!(target: "brisvox::server", id = %self.id, ?lived, %message, "session failed");
            }
            other => {
                tracing::debug!(target: "brisvox::server", id = %self.id, ?lived, reason = ?other.reason(), "session ended");
            }
        }
        // A detached handle has nobody to tell; skip the send (and keep `ended` unmoved).
        if !end_tx.is_closed() {
            let _ = end_tx.send(ended);
        }
    }
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        // Reached with `end_tx` still set only if the task was dropped before finishing.
        self.finish(SessionEnded::Aborted);
    }
}

/// Maps what the session future returned to how it ended (see the module docs). Formats the
/// error and drops the output or error, so it must run under the task's panic guard.
fn classify<T, E: Display>(result: Result<T, E>, cancelled: bool) -> SessionEnded {
    match result {
        _ if cancelled => SessionEnded::Cancelled,
        Ok(_) => SessionEnded::Completed,
        Err(e) => SessionEnded::Failed(bounded(e.to_string())),
    }
}

/// The end of a session whose future, or whose classification, panicked.
fn panicked(payload: Box<dyn std::any::Any + Send>) -> SessionEnded {
    let message = panic_message(payload.as_ref());
    // A destructor that panics while the payload drops must not take the task down.
    let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
    SessionEnded::Panicked(message)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(UNKNOWN_PANIC);
    bounded(text.to_owned())
}

/// Truncates to [`MAX_MESSAGE_CHARS`] characters, on a character boundary.
fn bounded(mut text: String) -> String {
    if let Some((index, _)) = text.char_indices().nth(MAX_MESSAGE_CHARS) {
        text.truncate(index);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_cuts_on_char_boundary() {
        let long = "ğ".repeat(MAX_MESSAGE_CHARS + 10);
        let cut = bounded(long);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS);
        assert_eq!(bounded("short".into()), "short");
    }

    #[test]
    fn end_counts_total_and_get() {
        let mut c = EndCounts::default();
        c.bump(EndReason::Panicked);
        c.bump(EndReason::Panicked);
        c.bump(EndReason::Aborted);
        assert_eq!(c.total(), 3);
        assert_eq!(c.get(EndReason::Panicked), 2);
        assert_eq!(c.get(EndReason::Completed), 0);
    }

    #[test]
    fn config_rejects_zero() {
        assert!(ManagerConfig::new(0).is_err());
        assert_eq!(ManagerConfig::new(3).unwrap().max_sessions, 3);
        // Capped: the hosted transports' bounds are derived from it (twice the limit).
        let max = ManagerConfig::MAX_SESSIONS;
        assert_eq!(ManagerConfig::new(max).unwrap().max_sessions, max);
        assert!(ManagerConfig::new(max + 1).is_err());
        assert!(ManagerConfig::new(usize::MAX).is_err());
    }

    #[tokio::test]
    async fn a_manager_refuses_a_config_built_past_the_constructor() {
        let config = ManagerConfig {
            max_sessions: usize::MAX,
        };
        assert!(SessionManager::new(config, brisvox_core::SystemClock::shared()).is_err());
    }

    #[test]
    fn classify_prefers_cancel_and_panic_payloads_are_read() {
        assert_eq!(
            panicked(Box::new("boom")),
            SessionEnded::Panicked("boom".into())
        );
        assert_eq!(
            panicked(Box::new(String::from("owned"))),
            SessionEnded::Panicked("owned".into())
        );
        assert_eq!(
            panicked(Box::new(42_u8)),
            SessionEnded::Panicked(UNKNOWN_PANIC.into())
        );
        let cancelled = classify::<(), String>(Err("late error".into()), true);
        assert_eq!(cancelled, SessionEnded::Cancelled);
        let failed = classify::<(), String>(Err("bad".into()), false);
        assert_eq!(failed, SessionEnded::Failed("bad".into()));
        assert_eq!(
            classify::<(), String>(Ok(()), false),
            SessionEnded::Completed
        );
    }
}
