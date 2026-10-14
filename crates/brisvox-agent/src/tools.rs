//! The tool engine: the tools the LLM may call, argument validation against JSON Schema,
//! parallel execution with per-tool timeouts, interruption, idempotency and the retry budget for
//! malformed calls. The contract is `docs/TOOLS.md` (items 1 and 4 to 8, and the "Tool engine
//!" section); this module implements it.
//!
//! One LLM response is handled in one of two ways. A committed response goes through
//! [`ToolEngine::execute`]. A speculative one goes through [`ToolEngine::speculate`], and its
//! SideEffect calls wait in the returned [`Speculation`] until [`ToolEngine::commit`] runs them, or
//! are discarded with it. The caller pushes the response's assistant message
//! ([`crate::assemble::Response::assistant_message`]) before either, and appends the results
//! ([`Execution::append_to`]). Results are always in the order the LLM issued the calls, whatever
//! order they completed in.
//!
//! Error results are JSON objects `{"error": kind, "tool": name, "detail": text}` (plus `"issues"`
//! and `"truncated"` for invalid arguments), so the LLM can read and correct them. The kinds are
//! listed in `docs/TOOLS.md`.

mod confirm;
mod filler;
mod schema;
mod stats;
mod store;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::pin::{Pin, pin};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use brisvox_core::{SharedClock, TurnEpoch};
use futures::FutureExt;
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::assemble::{Assembled, DEFAULT_MAX_CALLS};
use crate::context::{Context, Message, ToolCall, ToolCallId, late_tool_result_note};

pub use self::confirm::{
    CONFIRM_DESCRIPTION, CONFIRM_TOOL, ConfirmStats, Confirmations, DEFAULT_CONFIRM_TTL,
    MAX_PENDING, REQUEST_MESSAGE,
};
use self::confirm::{ConfirmPendingTool, GatedTool};
pub use self::filler::{
    ClipKey, DEFAULT_FILLER_DELAY, DEFAULT_FILLER_TEXT, FillerChoice, FillerClips, FillerConfig,
    FillerPhrase, FillerPlan, FillerPolicy, StaticClips,
};
pub use self::stats::ToolStatsSnapshot;
use self::stats::{ArgsDigest, ToolStats};
use self::store::{Begin, Entry, Store};

/// How long an interrupted or timed-out SideEffect call stays a candidate for suppressing an
/// identical call of a later response run: 120 seconds (see [`EngineConfig::orphan_window`]).
pub const DEFAULT_ORPHAN_WINDOW: Duration = Duration::from_secs(120);

/// Calls remembered as orphan candidates per session (the oldest is forgotten past this).
pub const ORPHAN_CAPACITY: usize = 32;

/// Timeout of a tool that does not set its own: 5 seconds.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(5);
/// Filler delay of a tool that does not set its own (TOOLS.md item 9): 700 ms.
pub const DEFAULT_FILLER_AFTER: Duration = Duration::from_millis(700);
/// Malformed-call rounds a turn may spend on retries (TOOLS.md item 4). See [`ToolEngine::execute`].
pub const DEFAULT_MAX_INVALID_ROUNDS: u32 = 2;
/// Largest tool result given to the LLM, in bytes. A larger ReadOnly result becomes an error; a
/// larger SideEffect result is cut to this size (see [`ToolEngine::execute`]).
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024;
/// SideEffect calls recorded per session for idempotency (TOOLS.md item 6).
pub const DEFAULT_IDEMPOTENCY_CAPACITY: usize = 256;
/// Bytes of recorded results kept per session for idempotency: 4 MiB. Together with the entry
/// count this bounds a session's store.
pub const DEFAULT_IDEMPOTENCY_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Largest tool-call arguments the engine validates and runs, in serialized JSON bytes: 16 KiB.
/// Larger arguments are refused as malformed (`arguments_too_large`) before any schema check, so
/// validation (inline on the caller's task) stays bounded. See [`EngineConfig::max_argument_bytes`].
pub const DEFAULT_MAX_ARGUMENT_BYTES: usize = 16 * 1024;
/// Hard backstop for a SideEffect tool future: 120 s. See [`EngineConfig::max_side_effect_runtime`].
pub const DEFAULT_MAX_SIDE_EFFECT_RUNTIME: Duration = Duration::from_secs(120);
/// Late results kept for the next turn. A result that finds the mailbox full is counted in
/// [`ToolEngine::late_notes_dropped`] and not reported.
pub const MAILBOX_CAPACITY: usize = 64;
/// Longest tool name the registry accepts (the providers' limit).
pub const MAX_TOOL_NAME_LEN: usize = 64;
/// Bytes of the `detail` of an error result; longer text is cut (ellipsis included).
const MAX_DETAIL_BYTES: usize = 512;
/// Appended to a SideEffect result cut to the size limit.
const TRUNCATION_NOTE: &str = "...[truncated]";

/// Identity of a session. Keys the idempotency store, so two sessions never share results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// The session with number `id`.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The session number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// How a tool may be run (TOOLS.md items 7 and 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    /// Has no side effects. May run speculatively. Dropped and answered `interrupted` when the
    /// user interrupts while it runs, or when its timeout passes.
    ReadOnly,
    /// Changes something outside the agent (a booking, a payment, a message). Never runs
    /// speculatively and never runs twice for one call id. Not dropped by the engine because of a
    /// timeout, an interruption or a dropped `execute` future: it runs to completion and its
    /// result is recorded and reported. Only two things end it early: the hard backstop
    /// ([`EngineConfig::max_side_effect_runtime`]) and the drop of the [`ToolEngine`] (the end of
    /// the session). Either way its outcome is then recorded as unknown, never as a result.
    SideEffect,
}

/// What the LLM is told about a tool, and how the engine runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// The name the LLM calls the tool by: 1 to [`MAX_TOOL_NAME_LEN`] ASCII letters, digits, `_`
    /// or `-`.
    pub name: Arc<str>,
    /// What the tool does, for the LLM.
    pub description: Arc<str>,
    /// The arguments as a JSON Schema object (`"type": "object"`), compiled at registration.
    pub parameters: Value,
    /// Read-only or side-effecting.
    pub kind: ToolKind,
    /// Longest the engine waits for a result before it answers with a `timeout` error. Non-zero.
    /// For a ReadOnly tool the wait ends the tool's future. For a SideEffect tool the future keeps
    /// running (up to [`EngineConfig::max_side_effect_runtime`]) and its result is reported later
    /// as a note (see [`Tool`]).
    pub timeout: Duration,
    /// After this long without a result the caller should play a filler phrase (TOOLS.md item 9).
    /// The engine only carries the value; the [`FillerPolicy`] reads it (as the delay under
    /// [`FillerPolicy::per_tool`], and only as an on/off switch under a fixed-delay policy) and
    /// playing the phrase is the caller's job. `None` disables fillers for this tool.
    pub filler_after: Option<Duration>,
    /// Hold the tool for the user's confirmation (`docs/TOOLS.md`, "Confirmation gate"). A call
    /// is not run: the model gets a `confirmation_required` result and must ask the user; the
    /// tool runs only when the model calls the built-in [`CONFIRM_TOOL`] after the user's *next*
    /// reply. Only a [`ToolKind::SideEffect`] tool can ask for it. Default `false`.
    pub confirm: bool,
    /// A [`ToolKind::SideEffect`] tool whose identical call (same arguments) may legitimately
    /// repeat in later responses of one turn, such as pressing a digit twice in an IVR menu.
    /// By default the engine refuses such a repeat as `already_done` after the first one
    /// succeeded, since a model that read the confirmation and asks again is repeating itself
    /// (a second booking). Two identical calls of one response always run. Default `false`.
    pub repeatable: bool,
}

impl ToolSpec {
    /// A spec with the default timeout ([`DEFAULT_TOOL_TIMEOUT`]) and filler delay
    /// ([`DEFAULT_FILLER_AFTER`]).
    pub fn new(
        name: impl Into<Arc<str>>,
        description: impl Into<Arc<str>>,
        parameters: Value,
        kind: ToolKind,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            kind,
            timeout: DEFAULT_TOOL_TIMEOUT,
            filler_after: Some(DEFAULT_FILLER_AFTER),
            confirm: false,
            repeatable: false,
        }
    }

    /// Sets whether an identical call may repeat within a turn (see [`Self::repeatable`]).
    #[must_use]
    pub fn with_repeatable(mut self, repeatable: bool) -> Self {
        self.repeatable = repeatable;
        self
    }

    /// Sets whether the tool needs the user's confirmation (see [`Self::confirm`]).
    #[must_use]
    pub fn with_confirm(mut self, confirm: bool) -> Self {
        self.confirm = confirm;
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the filler delay (`None` disables it).
    #[must_use]
    pub fn with_filler_after(mut self, filler_after: Option<Duration>) -> Self {
        self.filler_after = filler_after;
        self
    }
}

/// The successful result of a tool.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutput {
    /// A JSON value, sent to the LLM in compact form.
    Json(Value),
    /// Text, sent to the LLM as it is.
    Text(String),
}

impl From<Value> for ToolOutput {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

/// A failure reported by a tool. Its message is sent to the LLM in a `tool_failed` error result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    message: String,
}

impl ToolError {
    /// A failure with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// What a running tool is told about its call.
#[derive(Debug, Clone)]
pub struct ToolCallCtx {
    /// The session the call belongs to.
    pub session: SessionId,
    /// The turn the call belongs to.
    pub epoch: TurnEpoch,
    /// The call's internal id.
    pub call: ToolCallId,
    /// Cancelled when the user interrupts. The engine drops a ReadOnly tool when it fires. A
    /// SideEffect tool is not dropped by it (only by the engine's backstop or its drop); it may
    /// watch this token to answer sooner, but only when it can still finish consistently.
    pub cancel: CancellationToken,
    /// The session's pending confirmations (used by the gate's own tools; an ordinary tool has
    /// no use for it).
    pub confirmations: Arc<Confirmations>,
}

impl ToolCallCtx {
    /// The idempotency key of this call (TOOLS.md item 6). Pass its [`IdempotencyKey::digest`] to
    /// external APIs that accept an idempotency key.
    pub fn idempotency_key(&self) -> IdempotencyKey {
        IdempotencyKey::new(self.session, self.epoch, self.call.clone())
    }
}

/// Boxed future returned by [`Tool::call`]. Boxed so that [`Tool`] stays object-safe.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

/// A tool the engine can run.
///
/// Implementations receive arguments that already match their schema. They must not block the
/// thread (CONTRIBUTING.md: no CPU-heavy or blocking work on tokio workers) and must bound every
/// network or IO await with a timeout of their own.
///
/// The engine's timeout ends the wait of the caller. For a ReadOnly tool it also drops the
/// future. For a SideEffect tool it does not drop the future: the call keeps running after its
/// slot was answered with a `timeout` or `interrupted` error, and its result is recorded and
/// reported later as a note. A SideEffect tool must still bound its own IO: the engine stops the
/// future (a hard backstop, [`EngineConfig::max_side_effect_runtime`]) and aborts it when the
/// engine is dropped, at the next await point, so a SideEffect tool must tolerate being dropped
/// there. A ReadOnly or SideEffect tool that panics is answered `tool_failed` or
/// `unknown_outcome` and does not affect the caller.
pub trait Tool: Send + Sync {
    /// Runs the tool. Use `Box::pin(async move { ... })` for the body.
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a>;
}

/// Why a tool cannot be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The name is empty, too long, or has characters outside `[A-Za-z0-9_-]`.
    #[error(
        "invalid tool name {0:?}: use 1 to {MAX_TOOL_NAME_LEN} ASCII letters, digits, '_' or '-'"
    )]
    InvalidName(String),
    /// A tool with this name is already registered.
    #[error("tool {0:?} is already registered")]
    Duplicate(String),
    /// The parameters are not a JSON Schema object that compiles offline.
    #[error("tool {tool:?} has an invalid parameter schema: {reason}")]
    InvalidSchema {
        /// The tool.
        tool: String,
        /// Why the schema is rejected.
        reason: String,
    },
    /// The timeout is zero.
    #[error("tool {0:?} has a zero timeout")]
    ZeroTimeout(String),
    /// `confirm` was asked for a tool that is not a SideEffect tool: a ReadOnly tool changes
    /// nothing, so there is nothing to confirm.
    #[error("tool {0:?} asks for confirmation but is not a SideEffect tool")]
    ConfirmNeedsSideEffect(String),
    /// `repeatable` was asked for a tool that is not a SideEffect tool: the repeat refusal only
    /// concerns SideEffect tools (a ReadOnly tool may always repeat), so the flag has no meaning.
    #[error("tool {0:?} is marked repeatable but is not a SideEffect tool")]
    RepeatableNeedsSideEffect(String),
    /// The name belongs to the engine's built-in tool.
    #[error("the tool name {0:?} is reserved")]
    Reserved(String),
    /// No tool with this name is registered.
    #[error("no tool named {0:?} is registered")]
    Unknown(String),
}

/// A registered tool: its spec, its compiled schema and the implementation.
struct Registered {
    spec: ToolSpec,
    validator: jsonschema::Validator,
    /// Shared, so a SideEffect call can run on its own task.
    tool: Arc<dyn Tool>,
}

impl fmt::Debug for Registered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered")
            .field("name", &self.spec.name)
            .field("kind", &self.spec.kind)
            .finish_non_exhaustive()
    }
}

/// The tools of one agent, by name. Registration compiles each schema once; the LLM request
/// lists them with [`Self::specs`] in name order, so requests are reproducible.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<Arc<str>, Arc<Registered>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `tool` under `spec`.
    ///
    /// # Errors
    /// [`RegistryError`] when the name is invalid or taken, the timeout is zero, or the
    /// parameters are not an object schema (`"type": "object"`) that compiles without network
    /// access. A rejected tool is not added.
    pub fn register<T: Tool + 'static>(
        &mut self,
        spec: ToolSpec,
        tool: T,
    ) -> Result<(), RegistryError> {
        let name = spec.name.to_string();
        if !valid_name(&spec.name) {
            return Err(RegistryError::InvalidName(name));
        }
        if self.tools.contains_key(&spec.name) {
            return Err(RegistryError::Duplicate(name));
        }
        if &*spec.name == CONFIRM_TOOL {
            return Err(RegistryError::Reserved(name));
        }
        if spec.timeout.is_zero() {
            return Err(RegistryError::ZeroTimeout(name));
        }
        if spec.confirm && spec.kind != ToolKind::SideEffect {
            return Err(RegistryError::ConfirmNeedsSideEffect(name));
        }
        if spec.repeatable && spec.kind != ToolKind::SideEffect {
            return Err(RegistryError::RepeatableNeedsSideEffect(name));
        }
        if spec.parameters.get("type").and_then(Value::as_str) != Some("object") {
            return Err(RegistryError::InvalidSchema {
                tool: name,
                reason: "the parameters must be a JSON Schema with \"type\": \"object\"".to_owned(),
            });
        }
        let validator = schema::compile(&spec.parameters)
            .map_err(|reason| RegistryError::InvalidSchema { tool: name, reason })?;
        let mut tool: Arc<dyn Tool> = Arc::new(tool);
        if spec.confirm {
            tool = Arc::new(GatedTool {
                inner: tool,
                name: Arc::clone(&spec.name),
            });
        }
        self.tools.insert(
            spec.name.clone(),
            Arc::new(Registered {
                spec,
                validator,
                tool,
            }),
        );
        self.sync_confirm_tool();
        Ok(())
    }

    /// Keeps the built-in [`CONFIRM_TOOL`] in step with the tools: present exactly when some tool
    /// asks for confirmation, with a timeout that covers the slowest of them (it runs them).
    fn sync_confirm_tool(&mut self) {
        let timeout = self
            .tools
            .values()
            .filter(|entry| entry.spec.confirm)
            .map(|entry| entry.spec.timeout)
            .max();
        self.tools.remove(CONFIRM_TOOL);
        let Some(timeout) = timeout else {
            return;
        };
        let parameters = json!({
            "type": "object",
            "properties": {
                "call_id": {
                    "type": "string",
                    "description": "The call_id of the confirmation_required result."
                }
            },
            "required": ["call_id"],
            "additionalProperties": false,
        });
        let spec = ToolSpec::new(
            CONFIRM_TOOL,
            CONFIRM_DESCRIPTION,
            parameters,
            ToolKind::SideEffect,
        )
        .with_timeout(timeout);
        // The schema is the constant above: it compiles; if it ever did not, the tool is absent
        // and the held calls can never run (fail closed).
        if let Ok(validator) = schema::compile(&spec.parameters) {
            self.tools.insert(
                Arc::clone(&spec.name),
                Arc::new(Registered {
                    spec,
                    validator,
                    tool: Arc::new(ConfirmPendingTool),
                }),
            );
        }
    }

    /// Holds the tools in `names` for confirmation ([`ToolSpec::confirm`]):
    /// how an agent definition's `confirm_tools` list takes effect on tools the server registered
    /// without the option. A tool that already asks for it stays as it is.
    ///
    /// # Errors
    /// [`RegistryError::Unknown`] for a name that is not registered,
    /// [`RegistryError::ConfirmNeedsSideEffect`] for a ReadOnly tool,
    /// [`RegistryError::Reserved`] for the built-in [`CONFIRM_TOOL`] itself (gating the
    /// confirmation would make every confirmation need a confirmation).
    pub fn require_confirmation<'a>(
        &mut self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), RegistryError> {
        for name in names {
            if name == CONFIRM_TOOL {
                return Err(RegistryError::Reserved(name.to_owned()));
            }
            let Some(entry) = self.tools.get(name).map(Arc::clone) else {
                return Err(RegistryError::Unknown(name.to_owned()));
            };
            if entry.spec.confirm {
                continue;
            }
            if entry.spec.kind != ToolKind::SideEffect {
                return Err(RegistryError::ConfirmNeedsSideEffect(name.to_owned()));
            }
            let spec = entry.spec.clone().with_confirm(true);
            let validator = schema::compile(&spec.parameters).map_err(|reason| {
                RegistryError::InvalidSchema {
                    tool: name.to_owned(),
                    reason,
                }
            })?;
            let tool: Arc<dyn Tool> = Arc::new(GatedTool {
                inner: Arc::clone(&entry.tool),
                name: Arc::clone(&spec.name),
            });
            self.tools.insert(
                Arc::clone(&spec.name),
                Arc::new(Registered {
                    spec,
                    validator,
                    tool,
                }),
            );
        }
        self.sync_confirm_tool();
        Ok(())
    }

    /// The specs of all tools, in name order (for the LLM request).
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> + '_ {
        self.tools.values().map(|entry| &entry.spec)
    }

    /// The spec of the tool called `name`.
    pub fn spec(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name).map(|entry| &entry.spec)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// A registry of the tools for which `keep` returns `true` (for example the tools one agent
    /// definition names, out of every tool a server registers). The tools and their compiled
    /// schemas are shared with `self`, not copied or recompiled.
    #[must_use]
    pub fn filtered(&self, mut keep: impl FnMut(&ToolSpec) -> bool) -> Self {
        // The built-in confirmation tool follows the tools that need it, not the filter.
        let mut filtered = Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| &***name != CONFIRM_TOOL)
                .filter(|(_, entry)| keep(&entry.spec))
                .map(|(name, entry)| (Arc::clone(name), Arc::clone(entry)))
                .collect(),
        };
        filtered.sync_confirm_tool();
        filtered
    }
}

/// `[A-Za-z0-9_-]{1,64}`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Identity of one SideEffect call: (session, turn epoch, call id) (TOOLS.md item 6).
///
/// The store keys on the tuple itself, so two different calls never share a result. For external
/// APIs and logs, [`Self::digest`] gives a stable 64-bit value of the same tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    session: SessionId,
    epoch: TurnEpoch,
    call: ToolCallId,
}

impl IdempotencyKey {
    /// The key of call `call` of turn `epoch` in `session`.
    pub fn new(session: SessionId, epoch: TurnEpoch, call: ToolCallId) -> Self {
        Self {
            session,
            epoch,
            call,
        }
    }

    /// A stable 64-bit digest (FNV-1a over the session, the epoch and the call id). Equal keys
    /// have equal digests; distinct keys may collide, so the store never uses the digest as
    /// identity.
    pub fn digest(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(PRIME);
            }
        };
        feed(&self.session.get().to_le_bytes());
        feed(&self.epoch.get().to_le_bytes());
        feed(&[0xff]);
        feed(self.call.as_str().as_bytes());
        hash
    }
}

/// Engine settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Malformed-call rounds a turn may spend before the budget is exhausted. Default
    /// [`DEFAULT_MAX_INVALID_ROUNDS`].
    pub max_invalid_rounds: u32,
    /// Largest result given to the LLM, in bytes. Default [`DEFAULT_MAX_RESULT_BYTES`].
    pub max_result_bytes: usize,
    /// SideEffect calls recorded per session (at least 1). Default
    /// [`DEFAULT_IDEMPOTENCY_CAPACITY`].
    pub idempotency_capacity: usize,
    /// Bytes of recorded SideEffect results per session. Default
    /// [`DEFAULT_IDEMPOTENCY_MAX_BYTES`]. The engine raises it to at least
    /// [`Self::max_result_bytes`], so the store can always hold one result.
    pub idempotency_max_bytes: usize,
    /// Largest serialized arguments of one call, in bytes. A call with larger arguments is
    /// malformed (`arguments_too_large`): it is not validated and does not run. Bounds the CPU
    /// spent validating LLM-controlled input on the caller's task. Default
    /// [`DEFAULT_MAX_ARGUMENT_BYTES`].
    pub max_argument_bytes: usize,
    /// Hard backstop for a SideEffect tool future, on the injected clock. At least the tool's
    /// own timeout is used (a shorter value is raised to it). When it passes, the future is
    /// dropped, the call is recorded as lost (`unknown_outcome`, never run again) and the late
    /// note says so. Default [`DEFAULT_MAX_SIDE_EFFECT_RUNTIME`].
    pub max_side_effect_runtime: Duration,
    /// How long, on the injected clock, an *orphaned* SideEffect call (one whose result did not
    /// reach the LLM as a normal tool result: interrupted or timed out while running, dropped
    /// with its `execute` future, or carried by an [`Execution`] dropped undelivered) suppresses an
    /// identical call (same tool, same arguments) of a *later* response run: a later turn, or a
    /// later round of the same turn. The later call is answered with the recorded result, or with
    /// `duplicate_call` while the first run is still in flight, and the tool does not run again.
    /// This is how a regenerated response after a barge-in does not book twice. `ZERO` turns the
    /// suppression off. Default [`DEFAULT_ORPHAN_WINDOW`].
    pub orphan_window: Duration,
    /// How long, on the injected clock, a held confirmation ([`ToolSpec::confirm`]) can still be
    /// confirmed. Default [`DEFAULT_CONFIRM_TTL`].
    pub confirm_ttl: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_invalid_rounds: DEFAULT_MAX_INVALID_ROUNDS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            idempotency_capacity: DEFAULT_IDEMPOTENCY_CAPACITY,
            idempotency_max_bytes: DEFAULT_IDEMPOTENCY_MAX_BYTES,
            max_argument_bytes: DEFAULT_MAX_ARGUMENT_BYTES,
            max_side_effect_runtime: DEFAULT_MAX_SIDE_EFFECT_RUNTIME,
            orphan_window: DEFAULT_ORPHAN_WINDOW,
            confirm_ttl: DEFAULT_CONFIRM_TTL,
        }
    }
}

/// The outcome of one committed [`ToolEngine::execute`] or [`ToolEngine::commit`].
#[derive(Debug, Default)]
pub struct Execution {
    /// One tool message per call of the response, in the order the LLM issued the calls.
    pub results: Vec<Message>,
    /// System notes for SideEffect results that arrived after their slot was answered (the call
    /// timed out or was interrupted). They follow the results, so the context stays valid. The
    /// note of an orphan whose result this execution replays is left out (the LLM reads it once).
    pub late_notes: Vec<Message>,
    /// The turn's retry budget is spent (see [`ToolEngine::execute`]). No call ran in this
    /// execution; the caller should speak a fallback instead of asking the LLM again.
    pub budget_exhausted: bool,
    /// What the engine must do if this execution never reaches the context (see [`Delivery`]).
    /// Built by the engine; `Default` (for an execution built by hand) does nothing.
    pub delivery: Delivery,
}

impl Execution {
    /// Pushes the results, then the late notes, to `context`, and marks the execution delivered
    /// (see [`Delivery`]). Messages are cloned (their text is shared), so the execution stays
    /// readable.
    pub fn append_to(&mut self, context: &mut Context) {
        for message in self.results.iter().chain(&self.late_notes) {
            context.push(message.clone());
        }
        self.mark_delivered();
    }

    /// Marks the execution delivered without appending it: the caller put the results and notes
    /// into its context another way. [`Self::append_to`] does this itself.
    pub fn mark_delivered(&mut self) {
        self.delivery.delivered();
    }
}

/// The engine's hold on an [`Execution`] until it reaches the context.
///
/// An execution that is dropped before [`Execution::append_to`] (or
/// [`Execution::mark_delivered`]) never reached the LLM: the runner discarded it at a barge-in,
/// or its future was aborted after `execute` returned. Dropping it then:
/// - reports each SideEffect result it carried as a late note and marks the call an orphan, so
///   the result is not lost and a regenerated identical call is answered from the record instead
///   of running again;
/// - marks every other SideEffect call of the execution an orphan;
/// - puts the late notes it took from the mailbox back;
/// - keeps the orphan entries (and their notes) that the execution replayed, so the next
///   regeneration is suppressed too.
///
/// Delivery forgets the replayed orphans (their result is now a tool result in the context) and
/// their notes. `Default` holds nothing and does nothing.
#[derive(Default)]
pub struct Delivery {
    claims: Option<(Arc<Shared>, Claims)>,
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("Delivery");
        if let Some((_, claims)) = &self.claims {
            out.field("ran", &claims.ran.len())
                .field("held", &claims.held.len())
                .field("replays", &claims.replays.len())
                .field("notes", &claims.notes.len());
        }
        out.finish()
    }
}

impl Delivery {
    /// The execution reached the context.
    fn delivered(&mut self) {
        if let Some((shared, claims)) = self.claims.take() {
            claims.delivered(&shared);
        }
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if let Some((shared, claims)) = self.claims.take() {
            claims.abandon(&shared);
        }
    }
}

/// One call of a speculative response, after the speculative run.
#[derive(Debug)]
enum Slot {
    /// Answered already (a ReadOnly call that ran, or a call refused or malformed).
    Answered(Message),
    /// A SideEffect call held until the turn commits.
    Held(ToolCall, Arc<Registered>),
}

/// A speculative run: the ReadOnly calls have run, the SideEffect calls are held (TOOLS.md item
/// 7). Commit it with [`ToolEngine::commit`] when the turn commits; drop it to discard the
/// speculation (its held calls never run).
#[derive(Debug)]
pub struct Speculation {
    epoch: TurnEpoch,
    slots: Vec<Slot>,
    budget_exhausted: bool,
    /// Whether this speculation spent a malformed round of its turn's budget.
    spent_round: bool,
}

impl Speculation {
    /// The turn the speculation belongs to.
    pub fn epoch(&self) -> TurnEpoch {
        self.epoch
    }

    /// The SideEffect calls held until commit, in issue order.
    pub fn held_calls(&self) -> impl Iterator<Item = &ToolCall> + '_ {
        self.slots.iter().filter_map(|slot| match slot {
            Slot::Held(call, _) => Some(call),
            Slot::Answered(_) => None,
        })
    }

    /// The results already settled by the speculative run (ReadOnly calls and refusals), in
    /// issue order, skipping held calls. They reach the context only through [`ToolEngine::commit`].
    pub fn answered(&self) -> impl Iterator<Item = &Message> + '_ {
        self.slots.iter().filter_map(|slot| match slot {
            Slot::Answered(message) => Some(message),
            Slot::Held(..) => None,
        })
    }

    /// Whether the turn's retry budget was spent by this speculation.
    pub fn budget_exhausted(&self) -> bool {
        self.budget_exhausted
    }

    /// Whether this speculation spent a malformed round of the turn's retry budget (its response
    /// held a malformed call). A caller that discards the speculation gives the round back with
    /// [`ToolEngine::refund_speculation`], so a discarded guess cannot make the real run hit the
    /// budget earlier than a run without speculation.
    pub fn spent_round(&self) -> bool {
        self.spent_round
    }
}

/// The malformed round a speculative run has spent, given back when the run is dropped before it
/// finished (see [`ToolEngine::speculate`]).
struct InFlightRound<'a> {
    shared: &'a Shared,
    epoch: TurnEpoch,
    armed: bool,
}

impl Drop for InFlightRound<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = lock(&self.shared.state);
            if state.newest == Some(self.epoch) {
                state.invalid_rounds = state.invalid_rounds.saturating_sub(1);
            }
        }
    }
}

/// Per-session state behind the engine's lock. Never held across an await.
#[derive(Debug)]
struct EngineState {
    /// The newest turn the engine has seen. Calls of older turns are refused.
    newest: Option<TurnEpoch>,
    /// Malformed rounds of the newest turn so far.
    invalid_rounds: u32,
    store: Store,
    /// Recent SideEffect calls, newest last, at most [`ORPHAN_CAPACITY`]. A call is an orphan once
    /// its result can no longer reach the LLM as a tool result of its own turn.
    recent: VecDeque<Recent>,
    /// Runs of responses started so far (each `execute` and `commit` is one): the ordinal of the
    /// next one.
    rounds: u64,
}

/// A SideEffect call remembered for the orphan check.
#[derive(Debug)]
struct Recent {
    key: IdempotencyKey,
    tool: Arc<str>,
    args: ArgsDigest,
    at: Instant,
    /// The ordinal of the response run that started the call ([`Round::ordinal`]).
    round: u64,
    /// Interrupted, timed out while running, or dropped with its `execute` future or its
    /// undelivered [`Execution`]: the LLM was told the call was cut off (or never read its
    /// result), so an identical call of a later response run is the same intent, repeated.
    orphan: bool,
    /// The orphan's late note reached the context (taken by [`ToolEngine::take_late_notes`] or
    /// carried by a delivered [`Execution`]): the LLM has read its result, so a repeat of the
    /// call is answered `already_reported` instead of with the result again.
    reported: bool,
}

/// Late SideEffect results waiting for the next execution or [`ToolEngine::take_late_notes`].
#[derive(Debug, Default)]
struct Mailbox {
    /// Each note with the key of the call it reports, so a replay can take back the note of its
    /// call and a delivered note marks its orphan reported.
    notes: VecDeque<(IdempotencyKey, Message)>,
    dropped: u64,
}

/// State shared with the tasks of SideEffect calls.
#[derive(Debug)]
struct Shared {
    state: Mutex<EngineState>,
    mailbox: Mutex<Mailbox>,
    stats: ToolStats,
}

/// Runs the tools of one session. One engine per session; it is shared by reference.
pub struct ToolEngine {
    registry: Arc<ToolRegistry>,
    session: SessionId,
    clock: SharedClock,
    config: EngineConfig,
    shared: Arc<Shared>,
    /// Held confirmations of this session (the confirmation gate).
    confirmations: Arc<Confirmations>,
    /// The tasks of SideEffect calls. Dropping the engine aborts them (each marks its
    /// idempotency entry lost). Finished tasks are reaped on every spawn, so the set holds only
    /// running calls, which the idempotency store bounds.
    tasks: Mutex<JoinSet<()>>,
}

/// Size of the idempotency store, see [`ToolEngine::idempotency_usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdempotencyUsage {
    /// Entries recorded: in flight, settled or lost.
    pub entries: usize,
    /// Bytes of the recorded results.
    pub bytes: usize,
}

/// Admission of a round by the turn's retry budget.
enum Admission {
    /// Runs; `retries` malformed rounds have been spent by the turn so far.
    Admitted { retries: u32 },
    /// The budget is spent: nothing runs.
    Exhausted { retries: u32 },
    /// A newer turn has started: nothing runs.
    Stale,
}

/// Why calls are answered without running them.
#[derive(Clone, Copy)]
struct Refusal {
    kind: &'static str,
    detail: &'static str,
}

const STALE: Refusal = Refusal {
    kind: "stale_turn",
    detail: "a newer turn has started; this call belongs to a superseded turn and was not run",
};

const EXHAUSTED: Refusal = Refusal {
    kind: "budget_exhausted",
    detail: "the tool-call retry budget of this turn is spent, so no tool ran; answer the user without calling tools",
};

/// A call after validation, before anything runs.
enum Step {
    /// Already answered: a malformed call, or a refusal. `outcome` names it in the trace.
    Answer {
        id: ToolCallId,
        tool: Arc<str>,
        message: Message,
        outcome: &'static str,
    },
    /// Answered by an earlier run of the same speculation; the message is passed on untouched.
    Passed(Message),
    /// Runs now.
    Run(ToolCall, Arc<Registered>),
    /// A SideEffect call of a speculative run, held until commit.
    Hold(ToolCall, Arc<Registered>),
}

/// What the calls of one admitted round share: the turn, its malformed-round count so far, and
/// the turn's cancellation token.
#[derive(Clone, Copy)]
struct Round<'a> {
    epoch: TurnEpoch,
    retries: u32,
    cancel: &'a CancellationToken,
    /// Which run of a response this is, counted per session: two calls of one response share it,
    /// a later round of the same turn has another.
    ordinal: u64,
}

/// A call's work, prepared synchronously: nothing has been awaited yet.
enum Item {
    /// Settled already.
    Ready(Slot),
    /// A ReadOnly call to run in place.
    ReadOnly {
        call: ToolCall,
        entry: Arc<Registered>,
        started: Instant,
    },
    /// A SideEffect call running on its own task. Its result arrives through `wait`.
    Effect {
        id: ToolCallId,
        tool: Arc<str>,
        timeout: Duration,
        started: Instant,
        wait: EffectWait,
    },
}

/// What the start of a new SideEffect call decided.
enum Decision {
    /// The idempotency store's answer for the call's own key.
    Begin(Begin),
    /// An identical call already ran in an earlier response of this turn and succeeded.
    Repeat,
    /// An identical call is an orphan of an earlier turn.
    Orphan(OrphanAnswer),
}

/// How a call that repeats an orphaned SideEffect call is answered.
enum OrphanAnswer {
    /// The orphan (under the key) finished: its recorded result.
    Replayed(Message, IdempotencyKey),
    /// The orphan is still running; its result follows as a note.
    InFlight,
    /// The orphan (under the key) ended without a result.
    Lost(IdempotencyKey),
    /// The orphan finished and its late note is already in the context: the result is not
    /// repeated.
    Reported,
}

impl EngineState {
    /// Remembers a SideEffect call for the orphan check, forgetting the oldest settled one
    /// (an orphan last) past [`ORPHAN_CAPACITY`].
    fn remember(&mut self, recent: Recent) {
        if self.recent.len() >= ORPHAN_CAPACITY {
            let at = self.recent.iter().position(|r| !r.orphan).unwrap_or(0);
            self.recent.remove(at);
        }
        self.recent.push_back(recent);
    }
}

impl Shared {
    /// Marks the call under `key` as an orphan: its result will not reach the LLM as the tool
    /// result of its own turn.
    fn mark_orphan(&self, key: &IdempotencyKey) {
        let mut state = lock(&self.state);
        if let Some(recent) = state.recent.iter_mut().rev().find(|r| r.key == *key) {
            recent.orphan = true;
        }
    }
}

impl EngineState {
    /// The late note of the call under `key` reached the context (see [`Recent::reported`]).
    fn note_reported(&mut self, key: &IdempotencyKey) {
        if let Some(recent) = self.recent.iter_mut().rev().find(|r| r.key == *key) {
            recent.reported = true;
        }
    }
}

/// `message` as the answer to call `id` (a recorded result is replayed under the new call's id).
fn with_call_id(message: &Message, id: &ToolCallId) -> Message {
    match message {
        Message::Tool {
            content, is_error, ..
        } => Message::Tool {
            call_id: id.clone(),
            content: Arc::clone(content),
            is_error: *is_error,
        },
        other => other.clone(),
    }
}

/// The final result of a SideEffect call, sent by its task.
#[derive(Debug)]
struct Settled {
    message: Message,
    outcome: &'static str,
}

impl ToolEngine {
    /// An engine for `session` over `registry`, timing calls with `clock`. SideEffect calls run on
    /// tokio tasks, so the engine must be used inside a tokio runtime with the `rt` feature.
    pub fn new(
        registry: Arc<ToolRegistry>,
        session: SessionId,
        clock: SharedClock,
        config: EngineConfig,
    ) -> Self {
        let config = EngineConfig {
            idempotency_max_bytes: config.idempotency_max_bytes.max(config.max_result_bytes),
            ..config
        };
        let state = EngineState {
            newest: None,
            invalid_rounds: 0,
            store: Store::new(config.idempotency_capacity, config.idempotency_max_bytes),
            recent: VecDeque::new(),
            rounds: 0,
        };
        let confirmations = Arc::new(Confirmations::new(clock.clone(), config.confirm_ttl));
        Self {
            registry,
            session,
            clock,
            config,
            confirmations,
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                mailbox: Mutex::new(Mailbox::default()),
                stats: ToolStats::default(),
            }),
            tasks: Mutex::new(JoinSet::new()),
        }
    }

    /// The session's held confirmations. The LLM runner reports every ended user turn
    /// ([`Confirmations::note_user_turn`]) and every interruption ([`Confirmations::clear`]) to it.
    pub fn confirmations(&self) -> &Arc<Confirmations> {
        &self.confirmations
    }

    /// The settings this engine runs with (its idempotency byte bound raised to the result
    /// bound, see [`Self::new`]).
    pub const fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Malformed rounds spent by turn `epoch` so far (0 for a turn older than the newest seen).
    pub fn invalid_rounds(&self, epoch: TurnEpoch) -> u32 {
        let state = lock(&self.shared.state);
        if state.newest == Some(epoch) {
            state.invalid_rounds
        } else {
            0
        }
    }

    /// Entries and bytes of the idempotency store.
    pub fn idempotency_usage(&self) -> IdempotencyUsage {
        let state = lock(&self.shared.state);
        IdempotencyUsage {
            entries: state.store.len(),
            bytes: state.store.bytes(),
        }
    }

    /// Late results waiting for the next execution or [`Self::take_late_notes`], removed.
    /// Call it only when no tool call is open in the context (after an [`Execution`] was
    /// appended), so the notes never separate a call from its result.
    pub fn take_late_notes(&self) -> Vec<Message> {
        let taken: Vec<(IdempotencyKey, Message)> =
            lock(&self.shared.mailbox).notes.drain(..).collect();
        if !taken.is_empty() {
            let mut state = lock(&self.shared.state);
            for (key, _) in &taken {
                state.note_reported(key);
            }
        }
        taken.into_iter().map(|(_, note)| note).collect()
    }

    /// Counters of the engine's interruption behaviour: cancelled ReadOnly calls, SideEffect
    /// calls that outlived their turn, replays and suppressed duplicates.
    pub fn stats(&self) -> ToolStatsSnapshot {
        self.shared.stats.snapshot()
    }

    /// Late results not kept because the mailbox was full ([`MAILBOX_CAPACITY`]).
    pub fn late_notes_dropped(&self) -> u64 {
        lock(&self.shared.mailbox).dropped
    }

    /// Executes the calls of one committed LLM response (TOOLS.md items 4 to 8).
    ///
    /// Each call is handled as follows, in issue order:
    /// - A call the assembler could not parse, a call to an unregistered tool, a call whose
    ///   arguments fail the tool's schema or exceed [`EngineConfig::max_argument_bytes`], and a call
    ///   beyond [`DEFAULT_MAX_CALLS`] in one response are malformed. Each gets an error result (for
    ///   schema failures, the issues by path and reason) and counts toward the turn's retry budget.
    /// - A SideEffect call whose (session, epoch, call id) was already recorded returns the recorded
    ///   result and does not run again. A call whose earlier run ended without a result answers
    ///   `unknown_outcome` and does not run again. A call in flight answers `duplicate_call`.
    /// - A call that has not started when `cancel` is already cancelled answers `interrupted` and
    ///   does not run.
    /// - Other valid calls run concurrently. Each is raced against its timeout (from the injected
    ///   clock) and, for a ReadOnly call, against `cancel`:
    ///   - A ReadOnly call is dropped at its timeout (`timeout`) or when `cancel` fires
    ///     (`interrupted`).
    ///   - A SideEffect call is not dropped by the timeout, the interruption or the drop of this
    ///     future. It runs on its own task, and its result is recorded in the idempotency store. If it settles before the timeout or the interruption, its
    ///     result is the answer. If not, its slot is answered `timeout` or `interrupted` with a
    ///     detail that says the call keeps running, and its real result follows as a late note
    ///     (see [`Self::take_late_notes`] and [`Execution::late_notes`]). A result that exists but
    ///     was not read when this future is dropped is reported the same way. Results over
    ///     [`EngineConfig::max_result_bytes`] are cut, with a note, so the effect is still
    ///     reported as done. A tool that never finishes is stopped at
    ///     [`EngineConfig::max_side_effect_runtime`] and recorded as lost.
    ///   - A panic of a ReadOnly tool is answered `tool_failed`; the other calls are unaffected.
    /// - A SideEffect call that repeats (same tool, same arguments) an *orphaned* SideEffect call of
    ///   another response run (an earlier round or turn) within [`EngineConfig::orphan_window`]
    ///   does not run: it is answered with the
    ///   orphan's recorded result, or `duplicate_call` while the orphan still runs (see
    ///   `docs/TOOLS.md`).
    ///
    /// The retry budget is per turn. Every execution with a malformed call spends one round; valid
    /// rounds do not refund it. The budget allows [`EngineConfig::max_invalid_rounds`] rounds of
    /// retries: the round that would exceed it sets [`Execution::budget_exhausted`], runs nothing
    /// (every call, valid ones included, is answered `budget_exhausted`), and every later execution
    /// of the same turn is exhausted too. A new `epoch` starts a new budget.
    ///
    /// A call of a turn older than the newest turn the engine has seen is stale: every call of it is
    /// answered `stale_turn`, and nothing runs and no budget is spent. So a late execution of a
    /// superseded turn never runs a tool, even if its token was never cancelled.
    ///
    /// The engine never reads the context. Push the response's assistant message first, then append
    /// the execution with [`Execution::append_to`].
    pub async fn execute(
        &self,
        calls: Vec<Assembled>,
        epoch: TurnEpoch,
        cancel: &CancellationToken,
    ) -> Execution {
        let (steps, retries, budget_exhausted, _) = self.admit_round(calls, epoch, false);
        let (slots, delivery) = self.run_steps(steps, epoch, cancel, retries).await;
        self.execution(slots, delivery, budget_exhausted)
    }

    /// Runs the ReadOnly calls of a speculative LLM response and holds its SideEffect calls
    /// (TOOLS.md item 7). Malformed calls, refusals and the retry budget work as in
    /// [`Self::execute`], and a speculation spends a round like any other response, even if it is
    /// later dropped; a caller that discards it gives the round back with
    /// [`Self::refund_speculation`] (a run cancelled before it finished refunds by itself).
    ///
    /// Nothing is written to the context. The caller pushes the assistant message, then either
    /// commits the speculation with [`Self::commit`] and appends its result, or discards it. If
    /// discarded, the caller closes the open calls with `Context::interrupt_pending_tool_calls`,
    /// so the context stays valid.
    pub async fn speculate(
        &self,
        calls: Vec<Assembled>,
        epoch: TurnEpoch,
        cancel: &CancellationToken,
    ) -> Speculation {
        let (steps, retries, budget_exhausted, spent_round) = self.admit_round(calls, epoch, true);
        // A speculative run that is cancelled (its future dropped) before it finishes returns no
        // `Speculation` to refund, so the spent round is given back here.
        let mut in_flight = InFlightRound {
            shared: &self.shared,
            epoch,
            armed: spent_round,
        };
        // A speculative run holds its SideEffect calls, so it claims nothing (`delivery` is empty).
        let (slots, _delivery) = self.run_steps(steps, epoch, cancel, retries).await;
        in_flight.armed = false;
        Speculation {
            epoch,
            slots,
            budget_exhausted,
            spent_round,
        }
    }

    /// Gives back the malformed round a discarded speculation spent
    /// ([`Speculation::spent_round`]). Dropping a [`Speculation`] alone keeps the spend (the
    /// round was counted when it was speculated); a caller that discards a guess calls this so
    /// the turn's retry budget is what a run without speculation would have left. Does nothing
    /// for a speculation that spent nothing, for a turn that is no longer the newest, or when the
    /// budget is already zero. Call it at most once per speculation, and never for a committed
    /// one.
    pub fn refund_speculation(&self, speculation: &Speculation) {
        if !speculation.spent_round {
            return;
        }
        let mut state = lock(&self.shared.state);
        if state.newest == Some(speculation.epoch) {
            state.invalid_rounds = state.invalid_rounds.saturating_sub(1);
        }
    }

    /// Commits a speculation: runs its held SideEffect calls and returns every result of the
    /// response in issue order, ready for [`Execution::append_to`]. A speculation of a superseded
    /// turn runs nothing (`stale_turn`), and a turn whose budget is spent answers its held calls
    /// `budget_exhausted`. Commit spends no budget: the round was counted when it was speculated.
    pub async fn commit(&self, speculation: Speculation, cancel: &CancellationToken) -> Execution {
        let Speculation {
            epoch,
            slots,
            budget_exhausted,
            ..
        } = speculation;
        let steps: Vec<Step> = slots
            .into_iter()
            .map(|slot| match slot {
                Slot::Answered(message) => Step::Passed(message),
                Slot::Held(call, entry) => Step::Run(call, entry),
            })
            .collect();
        let (steps, retries, exhausted) = match self.admit(epoch, false) {
            Admission::Admitted { retries } => (steps, retries, false),
            Admission::Exhausted { retries } => (refuse(steps, EXHAUSTED), retries, true),
            Admission::Stale => (refuse(steps, STALE), 0, false),
        };
        let (slots, delivery) = self.run_steps(steps, epoch, cancel, retries).await;
        self.execution(slots, delivery, budget_exhausted || exhausted)
    }

    /// Plans one response's calls, admits the round and returns its steps.
    fn admit_round(
        &self,
        calls: Vec<Assembled>,
        epoch: TurnEpoch,
        speculative: bool,
    ) -> (Vec<Step>, u32, bool, bool) {
        let (steps, malformed) = self.plan(calls, speculative);
        match self.admit(epoch, malformed) {
            Admission::Admitted { retries } => (steps, retries, false, malformed),
            Admission::Exhausted { retries } => {
                (refuse(steps, EXHAUSTED), retries, true, malformed)
            }
            Admission::Stale => (refuse(steps, STALE), 0, false, false),
        }
    }

    /// Checks the turn against the newest seen and spends one malformed round if `malformed`.
    fn admit(&self, epoch: TurnEpoch, malformed: bool) -> Admission {
        let mut state = lock(&self.shared.state);
        match state.newest {
            Some(newest) if epoch < newest => return Admission::Stale,
            Some(newest) if epoch == newest => {}
            _ => {
                state.newest = Some(epoch);
                state.invalid_rounds = 0;
            }
        }
        if malformed {
            state.invalid_rounds = state.invalid_rounds.saturating_add(1);
        }
        let retries = state.invalid_rounds;
        if retries > self.config.max_invalid_rounds {
            Admission::Exhausted { retries }
        } else {
            Admission::Admitted { retries }
        }
    }

    /// Validates every call (synchronous, nothing runs). Returns the steps in issue order and
    /// whether the response holds a malformed call.
    fn plan(&self, calls: Vec<Assembled>, speculative: bool) -> (Vec<Step>, bool) {
        let mut malformed = false;
        let mut steps = Vec::with_capacity(calls.len());
        for (index, assembled) in calls.into_iter().enumerate() {
            let step = if index >= DEFAULT_MAX_CALLS {
                malformed = true;
                let (id, tool) = (assembled.id().clone(), assembled_name(&assembled));
                let message = error_message(
                    &id,
                    "too_many_calls",
                    &tool,
                    "a response may carry at most 32 calls; this one was not run",
                );
                Step::Answer {
                    id,
                    tool,
                    message,
                    outcome: "too_many_calls",
                }
            } else {
                match assembled {
                    Assembled::Invalid(invalid) => {
                        malformed = true;
                        Step::Answer {
                            id: invalid.id.clone(),
                            tool: assembled_name_of_invalid(&invalid),
                            message: invalid.error_result(),
                            outcome: "malformed",
                        }
                    }
                    Assembled::Complete(call) => {
                        let (step, bad) = self.plan_call(call, speculative);
                        malformed |= bad;
                        step
                    }
                }
            };
            steps.push(step);
        }
        (steps, malformed)
    }

    /// Plans one complete call. The flag says whether it is malformed.
    fn plan_call(&self, call: ToolCall, speculative: bool) -> (Step, bool) {
        let Some(entry) = self.registry.tools.get(&call.name).map(Arc::clone) else {
            let message = error_message(
                &call.id,
                "unknown_tool",
                &call.name,
                "no tool with this name is registered",
            );
            let step = Step::Answer {
                id: call.id,
                tool: call.name,
                message,
                outcome: "unknown_tool",
            };
            return (step, true);
        };
        let limit = self.config.max_argument_bytes;
        if !fits(&call.arguments, limit) {
            let message = error_message(
                &call.id,
                "arguments_too_large",
                &call.name,
                &format!(
                    "the arguments are more than {limit} bytes; call again with smaller arguments"
                ),
            );
            let step = Step::Answer {
                id: call.id,
                tool: call.name,
                message,
                outcome: "arguments_too_large",
            };
            return (step, true);
        }
        let issues = schema::issues(&entry.validator, &call.arguments);
        if !issues.is_empty() {
            let issues_json: Vec<Value> = issues
                .items
                .iter()
                .map(|issue| json!({ "path": issue.path, "reason": issue.reason }))
                .collect();
            let body = json!({
                "error": "invalid_arguments",
                "tool": bounded(&call.name, MAX_TOOL_NAME_LEN),
                "detail": bounded(
                    "the arguments do not match the tool's JSON Schema; call again with corrected arguments",
                    MAX_DETAIL_BYTES,
                ),
                "issues": issues_json,
                "truncated": issues.truncated,
            });
            let message = Message::tool_error(call.id.clone(), body.to_string());
            let step = Step::Answer {
                id: call.id,
                tool: call.name,
                message,
                outcome: "invalid_arguments",
            };
            return (step, true);
        }
        let held = speculative && entry.spec.kind == ToolKind::SideEffect;
        let step = if held {
            Step::Hold(call, entry)
        } else {
            Step::Run(call, entry)
        };
        (step, false)
    }

    /// Starts every step in issue order, then waits for all of them. Returns one slot per step,
    /// in the same order.
    async fn run_steps(
        &self,
        steps: Vec<Step>,
        epoch: TurnEpoch,
        cancel: &CancellationToken,
        retries: u32,
    ) -> (Vec<Slot>, Delivery) {
        let ordinal = {
            let mut state = lock(&self.shared.state);
            let ordinal = state.rounds;
            state.rounds = state.rounds.wrapping_add(1);
            ordinal
        };
        let round = Round {
            epoch,
            retries,
            cancel,
            ordinal,
        };
        // SideEffect calls started and results taken from their tasks but not yet handed to the
        // caller. If this future is dropped before it returns them, they are reported as late
        // notes instead of being lost with the future; once returned, the execution's
        // `Delivery` holds them until the execution reaches the context.
        let unclaimed = Unclaimed::new(&self.shared);
        let items: Vec<Item> = steps
            .into_iter()
            .map(|step| self.start(step, round, &unclaimed))
            .collect();
        let slots = join_all(
            items
                .into_iter()
                .map(|item| self.settle(item, round, &unclaimed)),
        )
        .await;
        (slots, unclaimed.release())
    }

    /// The execution of a response's slots: the results in issue order and the late notes, minus
    /// the notes of orphans this run replayed (their result is a tool result already). The notes
    /// taken go to `delivery`, so a discarded execution puts them back.
    fn execution(
        &self,
        slots: Vec<Slot>,
        mut delivery: Delivery,
        budget_exhausted: bool,
    ) -> Execution {
        let results = slots
            .into_iter()
            .filter_map(|slot| match slot {
                Slot::Answered(message) => Some(message),
                Slot::Held(..) => None,
            })
            .collect();
        let taken: Vec<(IdempotencyKey, Message)> =
            lock(&self.shared.mailbox).notes.drain(..).collect();
        let replayed = |key: &IdempotencyKey| {
            delivery
                .claims
                .as_ref()
                .is_some_and(|(_, claims)| claims.replays.contains(key))
        };
        let late_notes = taken
            .iter()
            .filter(|(key, _)| !replayed(key))
            .map(|(_, note)| note.clone())
            .collect();
        if !taken.is_empty() {
            match &mut delivery.claims {
                Some((_, claims)) => claims.notes = taken,
                None => {
                    delivery.claims = Some((
                        Arc::clone(&self.shared),
                        Claims {
                            notes: taken,
                            ..Claims::default()
                        },
                    ));
                }
            }
        }
        Execution {
            results,
            late_notes,
            budget_exhausted,
            delivery,
        }
    }

    /// Synchronous part of one step: traces what is already decided, records SideEffect calls
    /// and spawns their tasks.
    fn start(&self, step: Step, round: Round<'_>, unclaimed: &Unclaimed) -> Item {
        let Round { epoch, retries, .. } = round;
        match step {
            Step::Passed(message) => Item::Ready(Slot::Answered(message)),
            Step::Answer {
                id,
                tool,
                message,
                outcome,
            } => {
                self.trace(epoch, &id, &tool, outcome, Duration::ZERO, retries);
                Item::Ready(Slot::Answered(message))
            }
            Step::Hold(call, entry) => {
                self.trace(epoch, &call.id, &call.name, "held", Duration::ZERO, retries);
                Item::Ready(Slot::Held(call, entry))
            }
            Step::Run(call, entry) => {
                let started = self.clock.now();
                if round.cancel.is_cancelled() {
                    ToolStats::bump(&self.shared.stats.not_started_cancelled);
                    let message = error_message(
                        &call.id,
                        "interrupted",
                        &call.name,
                        "the user interrupted before the tool started",
                    );
                    self.trace(
                        epoch,
                        &call.id,
                        &call.name,
                        "not_started",
                        Duration::ZERO,
                        retries,
                    );
                    return Item::Ready(Slot::Answered(message));
                }
                match entry.spec.kind {
                    ToolKind::ReadOnly => Item::ReadOnly {
                        call,
                        entry,
                        started,
                    },
                    ToolKind::SideEffect => {
                        self.start_side_effect(call, entry, round, started, unclaimed)
                    }
                }
            }
        }
    }

    /// Records a SideEffect call and, if it is new, spawns its task.
    fn start_side_effect(
        &self,
        call: ToolCall,
        entry: Arc<Registered>,
        round: Round<'_>,
        started: Instant,
        unclaimed: &Unclaimed,
    ) -> Item {
        let Round { epoch, retries, .. } = round;
        let id = call.id.clone();
        let tool = Arc::clone(&call.name);
        let timeout = entry.spec.timeout;
        let key = IdempotencyKey::new(self.session, epoch, id.clone());
        let args = ArgsDigest::of(&call.arguments);
        // A gated call only records a request (the confirmation gate), and an interruption
        // clears the request: it takes no part in the orphan replay, so the model repeating
        // the identical call after a barge-in records a fresh request instead of being handed
        // the cleared one's `call_id`.
        let gated = entry.spec.confirm;
        let decision = {
            let mut state = lock(&self.shared.state);
            let orphan = if gated {
                None
            } else {
                self.orphan_answer(&mut state, &key, &tool, args, started, round.ordinal)
            };
            match orphan {
                Some(answer) => Decision::Orphan(answer),
                None if !gated
                    && !entry.spec.repeatable
                    && self.repeats_earlier_success(&state, &key, &tool, args, round.ordinal) =>
                {
                    Decision::Repeat
                }
                None => {
                    let begun = state.store.begin(&key);
                    if matches!(begun, Begin::Started) && !gated {
                        state.remember(Recent {
                            key: key.clone(),
                            tool: Arc::clone(&tool),
                            args,
                            at: started,
                            round: round.ordinal,
                            orphan: false,
                            reported: false,
                        });
                    }
                    Decision::Begin(begun)
                }
            }
        };
        let begun = match decision {
            Decision::Begin(begun) => begun,
            Decision::Repeat => {
                ToolStats::bump(&self.shared.stats.side_effect_repeats);
                self.trace(
                    epoch,
                    &id,
                    &tool,
                    "repeat_refused",
                    self.clock.now().saturating_duration_since(started),
                    retries,
                );
                return Item::Ready(Slot::Answered(error_message(
                    &id,
                    "already_done",
                    &tool,
                    "an identical call already ran earlier in this turn and succeeded; its result is above. It was not run again; do not repeat this call, tell the caller what happened",
                )));
            }
            Decision::Orphan(answer) => {
                if let OrphanAnswer::Replayed(_, orphan) | OrphanAnswer::Lost(orphan) = &answer {
                    unclaimed.replayed(orphan.clone());
                }
                let (message, outcome) = self.orphan_message(&id, &tool, answer);
                self.trace(
                    epoch,
                    &id,
                    &tool,
                    outcome,
                    self.clock.now().saturating_duration_since(started),
                    retries,
                );
                return Item::Ready(Slot::Answered(message));
            }
        };
        let (message, outcome) = match begun {
            Begin::Started => {
                let wait = self.spawn_side_effect(call, &entry, key, round, started);
                unclaimed.ran(&wait.report);
                return Item::Effect {
                    id,
                    tool,
                    timeout,
                    started,
                    wait,
                };
            }
            Begin::Recorded(message) => {
                ToolStats::bump(&self.shared.stats.side_effect_replays);
                (message, "recorded")
            }
            Begin::InFlight => (
                error_message(
                    &id,
                    "duplicate_call",
                    &tool,
                    "a call with this id is already running; its result will not be repeated",
                ),
                "duplicate",
            ),
            Begin::Lost => (
                error_message(
                    &id,
                    "unknown_outcome",
                    &tool,
                    "an earlier run of this call ended without a result, so its outcome is unknown; it is not run again",
                ),
                "lost",
            ),
            Begin::Full => (
                error_message(
                    &id,
                    "store_full",
                    &tool,
                    "too many side-effect calls are in flight to record their results",
                ),
                "store_full",
            ),
        };
        self.trace(
            epoch,
            &id,
            &tool,
            outcome,
            self.clock.now().saturating_duration_since(started),
            retries,
        );
        Item::Ready(Slot::Answered(message))
    }

    /// Whether a SideEffect call repeats one that already ran, and succeeded, in an earlier
    /// response of the same turn: same tool, same arguments, a different response run.
    /// The model has read that call's result (an orphan, whose result it has not, is
    /// [`Self::orphan_answer`]'s), so the identical call is the model repeating itself, not a
    /// second booking; running it would do the effect twice. A call that failed is not a
    /// success: a retry of it runs (see "Side effects" in `docs/TOOLS.md`). Two identical calls
    /// of one response are separate calls, so the model can still ask for the same effect twice
    /// by asking in one response. A new turn starts clean. Runs under the state lock.
    fn repeats_earlier_success(
        &self,
        state: &EngineState,
        key: &IdempotencyKey,
        tool: &str,
        args: ArgsDigest,
        round: u64,
    ) -> bool {
        // A call whose own key is known is a replay (answered by the store), not a repeat.
        if state.store.get(key).is_some() {
            return false;
        }
        state.recent.iter().any(|recent| {
            !recent.orphan
                && recent.round != round
                && recent.key.epoch == key.epoch
                && *recent.tool == *tool
                && recent.args == args
                && matches!(
                    state.store.get(&recent.key),
                    Some(Entry::Done {
                        message: Message::Tool {
                            is_error: false,
                            ..
                        },
                        ..
                    })
                )
        })
    }

    /// The orphan check: whether a call that is new by key repeats a SideEffect call whose
    /// result did not reach the LLM as a tool result (see [`EngineConfig::orphan_window`]). Runs
    /// under the state lock, which it never holds across an await.
    fn orphan_answer(
        &self,
        state: &mut EngineState,
        key: &IdempotencyKey,
        tool: &str,
        args: ArgsDigest,
        now: Instant,
        round: u64,
    ) -> Option<OrphanAnswer> {
        let window = self.config.orphan_window;
        if window.is_zero() || state.store.get(key).is_some() {
            return None;
        }
        // A call of another response run only: two identical calls of one response are separate
        // calls, but a later round repeats the cut-off call (a regeneration after a barge-in, or a
        // retry after a timeout although the LLM was told not to). Newest first: the latest
        // orphan is the one the call repeats.
        let at = state.recent.iter().rposition(|recent| {
            recent.orphan
                && recent.round != round
                && *recent.tool == *tool
                && recent.args == args
                && now.saturating_duration_since(recent.at) <= window
        })?;
        let orphan = state.recent.get(at)?;
        let (orphan_key, reported) = (orphan.key.clone(), orphan.reported);
        match state.store.get(&orphan_key).cloned() {
            Some(Entry::Done { .. }) if reported => {
                // The orphan's note already told the LLM the result: replaying it would put the
                // result in the context twice.
                ToolStats::bump(&self.shared.stats.orphan_reported);
                Some(OrphanAnswer::Reported)
            }
            Some(Entry::Done { message, .. }) => {
                // The orphan entry and its note stay until the execution that carries the replay
                // reaches the context (`Delivery`): if it is discarded, the next regeneration is
                // suppressed too and the note still reports the result.
                ToolStats::bump(&self.shared.stats.orphan_replays);
                Some(OrphanAnswer::Replayed(message, orphan_key))
            }
            Some(Entry::InFlight) => {
                ToolStats::bump(&self.shared.stats.orphan_in_flight);
                Some(OrphanAnswer::InFlight)
            }
            Some(Entry::Lost) => {
                ToolStats::bump(&self.shared.stats.orphan_lost);
                Some(OrphanAnswer::Lost(orphan_key))
            }
            None => {
                // Evicted from the store: nothing is known about it any more.
                state.recent.remove(at);
                None
            }
        }
    }

    /// The tool message and trace outcome for an orphan answer.
    fn orphan_message(
        &self,
        id: &ToolCallId,
        tool: &str,
        answer: OrphanAnswer,
    ) -> (Message, &'static str) {
        match answer {
            OrphanAnswer::Replayed(message, _) => (with_call_id(&message, id), "orphan_replayed"),
            OrphanAnswer::InFlight => (
                error_message(
                    id,
                    "duplicate_call",
                    tool,
                    "an identical call from an interrupted turn is still running; its result follows as a note and is not repeated; do not repeat this call",
                ),
                "orphan_in_flight",
            ),
            OrphanAnswer::Lost(_) => (
                error_message(
                    id,
                    "unknown_outcome",
                    tool,
                    "an identical call from an interrupted turn ended without a result, so its outcome is unknown; it is not run again",
                ),
                "orphan_lost",
            ),
            OrphanAnswer::Reported => (
                error_message(
                    id,
                    "already_reported",
                    tool,
                    "an identical call from an interrupted turn already finished and its result is in the note above; it is not run again and its result is not repeated; do not repeat this call",
                ),
                "orphan_reported",
            ),
        }
    }

    /// Runs a SideEffect call on its own task. The task owns the tool future, so dropping the
    /// caller's future does not drop the call. The task records the result, and reports it on the
    /// mailbox when nobody is waiting for it any more. The task ends early only at the hard
    /// backstop or when the engine is dropped (see [`ToolKind::SideEffect`]).
    fn spawn_side_effect(
        &self,
        call: ToolCall,
        entry: &Registered,
        key: IdempotencyKey,
        round: Round<'_>,
        started: Instant,
    ) -> EffectWait {
        let Round {
            epoch,
            retries,
            cancel,
            ..
        } = round;
        let (tx, rx) = oneshot::channel();
        let guard = StoreGuard {
            shared: Arc::clone(&self.shared),
            key: key.clone(),
            armed: true,
        };
        let tool = Arc::clone(&entry.tool);
        let clock = Arc::clone(&self.clock);
        let max = self.config.max_result_bytes;
        let backstop = self.config.max_side_effect_runtime.max(entry.spec.timeout);
        let ToolCall {
            id,
            name,
            arguments,
            ..
        } = call;
        let ctx = ToolCallCtx {
            session: self.session,
            epoch,
            call: id.clone(),
            cancel: cancel.clone(),
            confirmations: Arc::clone(&self.confirmations),
        };
        // Every SideEffect run is logged with its idempotency key, never with its arguments
        // (they may hold personal data).
        tracing::info!(
            target: "brisvox::tools",
            tool = %name,
            epoch = epoch.get(),
            idempotency_key = ctx.idempotency_key().digest(),
            "side-effect tool started"
        );
        let report = Arc::new(LateReport {
            shared: Arc::clone(&self.shared),
            clock: Arc::clone(&self.clock),
            session: self.session,
            epoch,
            retries,
            id: id.clone(),
            name: Arc::clone(&name),
            started,
            key: key.clone(),
            gated: entry.spec.confirm,
        });
        let task_report = Arc::clone(&report);
        let task = async move {
            let raced = race(tool.call(arguments, ctx), clock.sleep(backstop), None).await;
            let (message, outcome) = if let Raced::Done(output) = raced {
                let (message, outcome) = finish_output(&id, &name, output, max, true);
                guard.complete(message.clone());
                (message, outcome)
            } else {
                // The backstop passed: the future is dropped, so the effect may or may not have
                // happened. Record the call as lost (the guard does it on drop).
                drop(guard);
                let detail = format!(
                    "no result within {backstop:?}; the tool was stopped and its outcome is unknown; it is not run again"
                );
                (
                    error_message(&id, "unknown_outcome", &name, &detail),
                    "lost",
                )
            };
            if let Err(settled) = tx.send(Settled { message, outcome }) {
                // Nobody waits for this result any more: its slot was answered. Report it later.
                task_report.push(&settled.message);
            }
        };
        {
            let mut tasks = lock(&self.tasks);
            while tasks.try_join_next().is_some() {}
            tasks.spawn(task);
        }
        EffectWait {
            rx,
            done: false,
            report,
        }
    }

    /// Waits for one prepared call and returns its slot.
    async fn settle(&self, item: Item, round: Round<'_>, unclaimed: &Unclaimed) -> Slot {
        let Round {
            epoch,
            retries,
            cancel,
            ..
        } = round;
        match item {
            Item::Ready(slot) => slot,
            Item::ReadOnly {
                call,
                entry,
                started,
            } => {
                let id = call.id.clone();
                let name = Arc::clone(&call.name);
                let timeout = entry.spec.timeout;
                let ctx = ToolCallCtx {
                    session: self.session,
                    epoch,
                    call: id.clone(),
                    cancel: cancel.clone(),
                    confirmations: Arc::clone(&self.confirmations),
                };
                // A panic of third-party tool code is answered like any failure and must not
                // unwind through the caller (CONTRIBUTING.md: one session's panic must not affect
                // others), nor discard the sibling calls' results.
                let work = AssertUnwindSafe(async { entry.tool.call(call.arguments, ctx).await })
                    .catch_unwind();
                let mut cancelled_on_drop =
                    CountOnDrop::new(&self.shared.stats.read_only_cancelled);
                let (message, outcome) =
                    match race(work, self.clock.sleep(timeout), Some(cancel)).await {
                        Raced::Done(Ok(output)) => {
                            finish_output(&id, &name, output, self.config.max_result_bytes, false)
                        }
                        Raced::Done(Err(_panic)) => (
                            error_message(&id, "tool_failed", &name, "the tool panicked"),
                            "failed",
                        ),
                        Raced::TimedOut => (
                            error_message(
                                &id,
                                "timeout",
                                &name,
                                &format!("no result within {timeout:?}"),
                            ),
                            "timed_out",
                        ),
                        Raced::Cancelled => {
                            ToolStats::bump(&self.shared.stats.read_only_cancelled);
                            (
                                error_message(
                                    &id,
                                    "interrupted",
                                    &name,
                                    "the user interrupted while the tool was running",
                                ),
                                "interrupted",
                            )
                        }
                    };
                cancelled_on_drop.disarm();
                let duration = self.clock.now().saturating_duration_since(started);
                self.trace(epoch, &id, &name, outcome, duration, retries);
                Slot::Answered(message)
            }
            Item::Effect {
                id,
                tool,
                timeout,
                started,
                mut wait,
            } => {
                let raced = race(&mut wait.rx, self.clock.sleep(timeout), Some(cancel)).await;
                let raced = match raced {
                    Raced::Done(received) => {
                        wait.done = true;
                        Raced::Done(received)
                    }
                    // The result may have been sent between the last poll and now. Close the
                    // channel and take it, so it is never dropped with the receiver. Closing
                    // also makes the task report a later result itself.
                    other => match wait.take_arrived() {
                        Some(settled) => Raced::Done(Ok(settled)),
                        None => other,
                    },
                };
                let (message, outcome) = match raced {
                    Raced::Done(Ok(settled)) => {
                        unclaimed.hold(&wait.report, &settled.message);
                        (settled.message, settled.outcome)
                    }
                    Raced::Done(Err(_closed)) => (
                        error_message(
                            &id,
                            "unknown_outcome",
                            &tool,
                            "the tool ended without a result, so its outcome is unknown; it is not run again",
                        ),
                        "lost",
                    ),
                    Raced::TimedOut => {
                        wait.report.orphaned();
                        (
                            error_message(
                                &id,
                                "timeout",
                                &tool,
                                &format!(
                                    "no result within {timeout:?}; the call keeps running and its result follows as a note; do not repeat this call"
                                ),
                            ),
                            "timed_out_running",
                        )
                    }
                    Raced::Cancelled => {
                        wait.report.orphaned();
                        ToolStats::bump(&self.shared.stats.side_effect_interrupted);
                        (
                            error_message(
                                &id,
                                "interrupted",
                                &tool,
                                "the user interrupted while the tool was running; its result follows as a note",
                            ),
                            "interrupted_running",
                        )
                    }
                };
                let duration = self.clock.now().saturating_duration_since(started);
                self.trace(epoch, &id, &tool, outcome, duration, retries);
                Slot::Answered(message)
            }
        }
    }

    /// Traces one settled call (arguments are never traced). Also used for calls that were
    /// answered without running.
    fn trace(
        &self,
        epoch: TurnEpoch,
        id: &ToolCallId,
        tool: &str,
        outcome: &'static str,
        duration: Duration,
        retries: u32,
    ) {
        trace_call(self.session, epoch, retries, id, tool, outcome, duration);
    }
}

impl fmt::Debug for ToolEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolEngine")
            .field("session", &self.session)
            .field("tools", &self.registry.len())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The name a call is reported under: its name, or `unknown_tool` when none arrived.
fn assembled_name(assembled: &Assembled) -> Arc<str> {
    match assembled {
        Assembled::Complete(call) => Arc::clone(&call.name),
        Assembled::Invalid(invalid) => assembled_name_of_invalid(invalid),
    }
}

fn assembled_name_of_invalid(invalid: &crate::assemble::InvalidCall) -> Arc<str> {
    invalid
        .name
        .clone()
        .unwrap_or_else(|| Arc::from("unknown_tool"))
}

/// Answers every call of `steps` that is not already answered with `refusal`.
fn refuse(steps: Vec<Step>, refusal: Refusal) -> Vec<Step> {
    steps
        .into_iter()
        .map(|step| {
            let (id, tool) = match &step {
                Step::Answer { id, tool, .. } => (id.clone(), Arc::clone(tool)),
                Step::Run(call, _) | Step::Hold(call, _) => {
                    (call.id.clone(), Arc::clone(&call.name))
                }
                Step::Passed(message) => return Step::Passed(message.clone()),
            };
            let message = error_message(&id, refusal.kind, &tool, refusal.detail);
            Step::Answer {
                id,
                tool,
                message,
                outcome: refusal.kind,
            }
        })
        .collect()
}

/// Outcome of a tool future raced against its timeout and, when given, a cancellation.
enum Raced<T> {
    Done(T),
    TimedOut,
    Cancelled,
}

/// Polls `work` until it completes, `timeout` fires, or `cancel` (if any) is cancelled. Whichever
/// comes first wins; when several are ready, the work's own result is preferred. `work` is dropped
/// when the race ends unless it completed.
async fn race<F: Future>(
    work: F,
    timeout: Sleep,
    cancel: Option<&CancellationToken>,
) -> Raced<F::Output> {
    let mut work = pin!(work);
    let mut timeout = pin!(timeout);
    let mut cancelled = pin!(wait_cancelled(cancel));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = work.as_mut().poll(cx) {
            return Poll::Ready(Raced::Done(output));
        }
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Raced::Cancelled);
        }
        if timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Raced::TimedOut);
        }
        Poll::Pending
    })
    .await
}

/// Completes when `cancel` is cancelled; never completes without a token.
async fn wait_cancelled(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Everything needed to report a SideEffect result as a late note: where it goes, and the call's
/// identity for the trace and the note text.
struct LateReport {
    shared: Arc<Shared>,
    clock: SharedClock,
    session: SessionId,
    epoch: TurnEpoch,
    retries: u32,
    id: ToolCallId,
    name: Arc<str>,
    started: Instant,
    key: IdempotencyKey,
    /// A gated call (the confirmation gate): its result is a request that an interruption
    /// clears, so it is never reported as a late note.
    gated: bool,
}

impl LateReport {
    /// The call's result will not reach the LLM as the tool result of its own turn: an
    /// identical call of a later response run is a repeat of it and must not run again.
    fn orphaned(&self) {
        self.shared.mark_orphan(&self.key);
    }

    /// Reports `message`, the result of this call, on the mailbox and the trace.
    fn push(&self, message: &Message) {
        if self.gated {
            return;
        }
        self.orphaned();
        ToolStats::bump(&self.shared.stats.side_effect_late_results);
        let duration = self.clock.now().saturating_duration_since(self.started);
        trace_call(
            self.session,
            self.epoch,
            self.retries,
            &self.id,
            &self.name,
            "late_result",
            duration,
        );
        lock(&self.shared.mailbox).push(
            self.key.clone(),
            late_tool_result_note(&self.id, &self.name, message.text()),
        );
    }
}

/// The caller's side of a SideEffect call: the channel its task answers on. A result that reached
/// the channel but was not taken (the caller's future was dropped, or the wait ended in the same
/// instant) is never lost with the receiver: [`Drop`] reports it as a late note. A result the task
/// sends after the channel is closed is reported by the task itself, so each result is reported
/// or delivered exactly once.
struct EffectWait {
    rx: oneshot::Receiver<Settled>,
    /// Whether the channel completed and was read; a completed receiver must not be read again.
    done: bool,
    report: Arc<LateReport>,
}

impl EffectWait {
    /// Closes the channel and takes a result that already arrived, if any.
    fn take_arrived(&mut self) -> Option<Settled> {
        if self.done {
            return None;
        }
        self.done = true;
        self.rx.close();
        self.rx.try_recv().ok()
    }
}

impl Drop for EffectWait {
    fn drop(&mut self) {
        if !self.done {
            // The wait ends without a result being read: the call outlives its caller.
            self.report.orphaned();
            ToolStats::bump(&self.report.shared.stats.side_effect_interrupted);
        }
        if let Some(settled) = self.take_arrived() {
            self.report.push(&settled.message);
        }
    }
}

/// What one run of a response owes the engine until its execution reaches the context: the
/// SideEffect calls it ran, the results it took from their tasks, the orphans it replayed and the
/// late notes it took from the mailbox.
#[derive(Default)]
struct Claims {
    /// Every SideEffect call the run started.
    ran: Vec<Arc<LateReport>>,
    /// SideEffect results taken from their tasks (each also in `ran`).
    held: Vec<(Arc<LateReport>, Message)>,
    /// Keys of the orphans whose record answered a call of the run.
    replays: Vec<IdempotencyKey>,
    /// Late notes taken from the mailbox, in mailbox order.
    notes: Vec<(IdempotencyKey, Message)>,
}

impl Claims {
    fn is_empty(&self) -> bool {
        self.ran.is_empty()
            && self.held.is_empty()
            && self.replays.is_empty()
            && self.notes.is_empty()
    }

    /// The execution reached the context: the replayed orphans are answered for good, and the
    /// orphans whose notes it carried are reported.
    fn delivered(self, shared: &Shared) {
        if self.replays.is_empty() && self.notes.is_empty() {
            return;
        }
        {
            let mut state = lock(&shared.state);
            for key in &self.replays {
                state.recent.retain(|recent| recent.key != *key);
            }
            for (key, _) in &self.notes {
                state.note_reported(key);
            }
        }
        if self.replays.is_empty() {
            return;
        }
        // A note of a replayed orphan that arrived after the execution was built: the replayed
        // result already told the LLM.
        let mut mailbox = lock(&shared.mailbox);
        for key in &self.replays {
            mailbox.take_note_of(key);
        }
    }

    /// The execution never reached the context: give everything back (see [`Delivery`]).
    fn abandon(self, shared: &Shared) {
        if !self.notes.is_empty() {
            lock(&shared.mailbox).restore(self.notes);
        }
        for report in &self.ran {
            report.orphaned();
        }
        for (report, message) in &self.held {
            report.push(message);
        }
    }
}

/// [`Claims`] of one run while its calls settle. The run's slots are returned only when every
/// call has settled, and the future may be dropped before that (a barge-in `select!`).
/// [`Self::release`] hands the claims to the [`Delivery`] of the execution; dropping the holder
/// first gives them back as [`Claims::abandon`] does.
struct Unclaimed {
    shared: Arc<Shared>,
    claims: Mutex<Claims>,
}

impl Unclaimed {
    fn new(shared: &Arc<Shared>) -> Self {
        Self {
            shared: Arc::clone(shared),
            claims: Mutex::new(Claims::default()),
        }
    }

    /// The SideEffect call behind `report` was started by this run.
    fn ran(&self, report: &Arc<LateReport>) {
        lock(&self.claims).ran.push(Arc::clone(report));
    }

    /// Holds `message`, the result of the call behind `report`.
    fn hold(&self, report: &Arc<LateReport>, message: &Message) {
        lock(&self.claims)
            .held
            .push((Arc::clone(report), message.clone()));
    }

    /// The orphan under `key` answered a call of this run.
    fn replayed(&self, key: IdempotencyKey) {
        lock(&self.claims).replays.push(key);
    }

    /// The slots are being returned: the claims go to the execution's [`Delivery`].
    fn release(self) -> Delivery {
        let claims = std::mem::take(&mut *lock(&self.claims));
        Delivery {
            claims: (!claims.is_empty()).then(|| (Arc::clone(&self.shared), claims)),
        }
    }
}

impl Drop for Unclaimed {
    fn drop(&mut self) {
        let claims = std::mem::take(&mut *lock(&self.claims));
        claims.abandon(&self.shared);
    }
}

/// Counts a ReadOnly call whose wait is dropped before it ended (the caller's future was dropped
/// or aborted at a barge-in): the tool's future goes with it, so the call was cancelled even though
/// the engine never saw the token fire.
struct CountOnDrop<'a> {
    counter: &'a AtomicU64,
    armed: bool,
}

impl<'a> CountOnDrop<'a> {
    fn new(counter: &'a AtomicU64) -> Self {
        Self {
            counter,
            armed: true,
        }
    }

    /// The wait ended by itself: nothing to count.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CountOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            ToolStats::bump(self.counter);
        }
    }
}

/// Whether `value` serializes to at most `limit` bytes. Stops serializing past the limit, so the
/// check costs at most `limit` bytes of work.
fn fits(value: &Value, limit: usize) -> bool {
    struct Counter {
        left: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            match self.left.checked_sub(data.len()) {
                Some(left) => {
                    self.left = left;
                    Ok(data.len())
                }
                None => Err(io::Error::other("over the limit")),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { left: limit }, value).is_ok()
}

/// Guards an in-flight idempotency entry: if its task ends without a result (panic, abort), the
/// entry is marked lost, so it neither stays in flight forever nor runs again.
struct StoreGuard {
    shared: Arc<Shared>,
    key: IdempotencyKey,
    armed: bool,
}

impl StoreGuard {
    /// Records the settled result of the call.
    fn complete(mut self, message: Message) {
        self.armed = false;
        lock(&self.shared.state).store.finish(&self.key, message);
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.shared.state).store.lost(&self.key);
        }
    }
}

impl Mailbox {
    /// Keeps a late note, or counts it as dropped when the mailbox is full.
    fn push(&mut self, call: IdempotencyKey, note: Message) {
        if self.notes.len() >= MAILBOX_CAPACITY {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.notes.push_back((call, note));
        }
    }

    /// Removes the note that reports the call under `call`, if it is still waiting.
    fn take_note_of(&mut self, call: &IdempotencyKey) {
        self.notes.retain(|(key, _)| key != call);
    }

    /// Puts notes taken earlier back in front (they are older than any note pushed since), in
    /// their order. A note that no longer fits is counted as dropped.
    fn restore(&mut self, notes: Vec<(IdempotencyKey, Message)>) {
        for note in notes.into_iter().rev() {
            if self.notes.len() >= MAILBOX_CAPACITY {
                self.dropped = self.dropped.saturating_add(1);
            } else {
                self.notes.push_front(note);
            }
        }
    }
}

/// The final result of a tool future: its output, or a `tool_failed` error.
fn finish_output(
    id: &ToolCallId,
    name: &str,
    output: Result<ToolOutput, ToolError>,
    max: usize,
    side_effect: bool,
) -> (Message, &'static str) {
    match output {
        Ok(output) => render(id, name, output, max, side_effect),
        Err(error) => (
            error_message(id, "tool_failed", name, error.message()),
            "failed",
        ),
    }
}

/// The tool message for a successful output. A ReadOnly output over `max` bytes becomes a
/// `result_too_large` error. A SideEffect output over `max` bytes is cut to `max` bytes (with
/// [`TRUNCATION_NOTE`]) and kept, because the effect happened.
fn render(
    id: &ToolCallId,
    name: &str,
    output: ToolOutput,
    max: usize,
    side_effect: bool,
) -> (Message, &'static str) {
    let (content, complete) = match output {
        ToolOutput::Text(text) => {
            if text.len() <= max {
                (text, true)
            } else {
                (char_prefix(&text, max).to_owned(), false)
            }
        }
        ToolOutput::Json(value) => {
            let mut sink = CappedWriter {
                bytes: Vec::new(),
                limit: max,
            };
            match serde_json::to_writer(&mut sink, &value) {
                Ok(()) => (String::from_utf8_lossy(&sink.bytes).into_owned(), true),
                Err(_) => (utf8_prefix(&sink.bytes), false),
            }
        }
    };
    if complete {
        return (Message::tool_result(id.clone(), content), "ok");
    }
    if side_effect {
        let keep = max.saturating_sub(TRUNCATION_NOTE.len());
        let cut = format!("{}{TRUNCATION_NOTE}", char_prefix(&content, keep));
        return (Message::tool_result(id.clone(), cut), "ok_truncated");
    }
    let detail = format!("the result is more than {max} bytes; the limit is {max} bytes");
    (
        error_message(id, "result_too_large", name, &detail),
        "result_too_large",
    )
}

/// A writer that keeps at most `limit` bytes and fails past that, so a large JSON value is never
/// serialized in full.
struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = self.limit.saturating_sub(self.bytes.len());
        if data.len() > room {
            self.bytes
                .extend_from_slice(data.get(..room).unwrap_or_default());
            return Err(io::Error::other("the result is over the size limit"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The longest valid UTF-8 prefix of `bytes`.
fn utf8_prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) => std::str::from_utf8(bytes.get(..error.valid_up_to()).unwrap_or_default())
            .unwrap_or_default()
            .to_owned(),
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
fn char_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default()
}

/// The trace event of one settled call (arguments are never logged). `held` is traced as its own
/// event name. The retry count is the malformed rounds the turn had spent when the call was
/// admitted.
fn trace_call(
    session: SessionId,
    epoch: TurnEpoch,
    retries: u32,
    id: &ToolCallId,
    tool: &str,
    outcome: &'static str,
    duration: Duration,
) {
    let duration_us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
    let tool = bounded(tool, MAX_TOOL_NAME_LEN);
    if outcome == "held" {
        tracing::debug!(
            target: "brisvox::tools",
            session = session.get(),
            epoch = epoch.get(),
            call = %id,
            tool = %tool,
            outcome,
            duration_us,
            retries,
            "tool call held"
        );
    } else {
        tracing::debug!(
            target: "brisvox::tools",
            session = session.get(),
            epoch = epoch.get(),
            call = %id,
            tool = %tool,
            outcome,
            duration_us,
            retries,
            "tool call settled"
        );
    }
}

/// The JSON body of an error result, as a value.
fn error_value(kind: &str, tool: &str, detail: &str) -> Value {
    json!({
        "error": kind,
        "tool": bounded(tool, MAX_TOOL_NAME_LEN),
        "detail": bounded(detail, MAX_DETAIL_BYTES),
    })
}

/// A tool message carrying an error result.
fn error_message(id: &ToolCallId, kind: &str, tool: &str, detail: &str) -> Message {
    Message::tool_error(id.clone(), error_value(kind, tool, detail).to_string())
}

/// `text` as at most `max` bytes, cut at a character boundary and ending in `...` when cut (the
/// ellipsis counts toward `max`).
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = char_prefix(text, max.saturating_sub(3)).to_owned();
    cut.push_str("...");
    cut
}

/// Locks `mutex`. Critical sections cannot panic, so a poisoned lock still guards consistent data.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_keeps_the_limit_including_the_ellipsis() {
        assert_eq!(bounded("abcdef", 6), "abcdef");
        assert_eq!(bounded("abcdefg", 6), "abc...");
        assert_eq!(bounded("ééééé", 6).len(), 5);
    }

    #[test]
    fn utf8_prefix_drops_a_split_character() {
        let bytes = "aé".as_bytes();
        assert_eq!(utf8_prefix(bytes.get(..2).unwrap_or_default()), "a");
    }
}
