//! The per-call flow state machine: current node, collected state, and the round-level API the
//! LLM runner calls. See the [module docs](super) for the protocol.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::pin::pin;
use std::sync::Arc;

use brisvox_core::{SharedClock, TurnEpoch};
use futures::future::{Either, select};
use serde_json::{Map, Value, json};

use super::def::{Action, ContextStrategy, Flow, Node, NodeId, ToolBinding, ToolOutcome};
use super::render::render;
use crate::assemble::Assembled;
use crate::context::{
    CharsPerToken, Context, ContextError, Message, TokenEstimator, ToolCall, ToolCallId,
};
use crate::tools::{Execution, ToolKind, ToolSpec};
use crate::window::{CLIP_MARKER, Summarizer, Summary as WindowSummary, clipped};

/// Most keys the flow state keeps. A new key beyond this is dropped (counted in
/// [`FlowManager::state_dropped`]); existing keys can still be overwritten.
pub const MAX_STATE_ENTRIES: usize = 64;
/// Largest value the flow state keeps, serialized. A larger value is dropped (counted in
/// [`FlowManager::state_dropped`]) and the key's earlier value is removed (it would be stale).
pub const MAX_STATE_VALUE_BYTES: usize = 4096;
/// Longest state key, in bytes. A longer key is dropped (counted in
/// [`FlowManager::state_dropped`]): keys chosen by the model (`capture_args`) must not be a way
/// around the size bound of the state.
pub const MAX_STATE_KEY_BYTES: usize = 64;
/// Largest message, in estimated tokens, given to the summarizer: a longer one is clipped the way
/// the window clips it (see [`crate::window::CLIP_MARKER`]).
pub const SUMMARY_MESSAGE_MAX_TOKENS: usize = 1000;
/// The only error kinds that are tool results: the tool ran (or may have run) and the engine
/// reports how it ended (`ToolEngine` produces them from a tool's own run).
///
/// Every other error result is an answer made up without the tool having run (`budget_exhausted`,
/// `stale_turn`, `interrupted`, `duplicate_call`, `invalid_arguments`, `arguments_too_large`,
/// `too_many_calls`, `unknown_tool`, `store_full`, `invalid tool call`, and the flow's own
/// `missing_result` and `tool_not_available`) or something this list does not know; no transition
/// looks at it. The list is an allow-list on purpose: a refusal kind the engine gains later is
/// not routed until it is added here.
const TOOL_RESULT_ERRORS: [&str; 4] = [
    "tool_failed",
    "timeout",
    "unknown_outcome",
    "result_too_large",
];
/// Error kinds of a SideEffect call whose effect may be running, or may have happened, without the
/// flow knowing the result (see [`FlowManager::allows_tool`]): the tool is not offered again in
/// the node until a recorded late result arrives or the node changes.
const OUTCOME_UNKNOWN_ERRORS: [&str; 3] = ["timeout", "unknown_outcome", "interrupted"];
/// Start of the system message that carries the summary of a `ResetWithSummary` transition.
pub const FLOW_SUMMARY_PREFIX: &str = "Summary of the conversation before this step of the call:\n";
/// Tool names listed in a `tool_not_available` refusal.
const MAX_LISTED_TOOLS: usize = 16;
/// Longest tool name echoed in a refusal, in characters (the registry limit).
const MAX_SHOWN_NAME: usize = 64;

/// Why a [`FlowManager`] call failed. The context is unchanged unless stated otherwise.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FlowError {
    /// [`FlowManager::start`] has not run.
    #[error("the flow was not started")]
    NotStarted,
    /// [`FlowManager::start`] ran already.
    #[error("the flow was already started")]
    AlreadyStarted,
    /// The target of an explicit transition is not a node of the flow.
    #[error("node {0} is not in the flow")]
    UnknownNode(NodeId),
    /// A transition needs every tool call answered first.
    #[error("tool calls are still unanswered: {0:?}")]
    PendingToolCalls(Vec<ToolCallId>),
    /// Message 0 of the context is not a system message, so it is not the node prompt: the context
    /// was not built by this manager.
    #[error("message 0 of the context is not the node prompt (a system message)")]
    ContextMismatch,
    /// The context broke [`Context::validate`]. From [`FlowManager::apply`] this means the
    /// response's assistant message was not pushed before the round (the results were already
    /// appended).
    #[error(transparent)]
    Context(#[from] ContextError),
}

/// What entering a node asks of the runner.
#[derive(Debug, Clone, PartialEq)]
pub struct Entered {
    /// The node entered.
    pub node: NodeId,
    /// The strategy the node asks for.
    pub requested: ContextStrategy,
    /// The strategy that was applied: differs from `requested` only when a summary was asked for
    /// and `Append` was used instead (no summarizer, a failure, a timeout, or an empty reply), or
    /// when there was nothing to summarize and `Reset` was used.
    pub applied: ContextStrategy,
    /// Actions to perform now, before the model answers.
    pub pre_actions: Vec<Action>,
    /// Actions to perform after the node's first response was played.
    pub post_actions: Vec<Action>,
    /// Whether the model answers right away (otherwise wait for the caller).
    pub respond_immediately: bool,
    /// The history was replaced (`Reset`, or `ResetWithSummary` that summarized): message indices
    /// of the old context mean nothing any more. The runner must drop everything keyed by them:
    /// the window [`Summary`](crate::window::Summary) (its `covers_until` is an index; the next
    /// `build_window` would fail with `InvalidSummary`), the round to message index map, and the
    /// play-out book's truncation indices. False for `Append` (indices are kept), for a summary
    /// that fell back to `Append`, and for [`FlowManager::start`].
    pub context_replaced: bool,
}

impl Entered {
    /// Whether a summary was requested but the history was kept instead.
    pub fn fell_back(&self) -> bool {
        self.requested == ContextStrategy::ResetWithSummary
            && self.applied == ContextStrategy::Append
    }
}

/// A transition a tool asked for that the flow refused: the target is not a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedTransition {
    /// The call whose result asked for it.
    pub call: ToolCallId,
    /// The unknown target.
    pub target: NodeId,
}

/// The result of [`FlowManager::apply`].
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The node changed between [`FlowManager::screen`] and this call (a transition happened in
    /// between): the results of calls still in the context were appended, but nothing was
    /// captured and no transition ran. Results whose calls a reset removed are dropped
    /// ([`Applied::dropped_results`]) so the context stays valid.
    pub stale: bool,
    /// The node entered, if a transition ran.
    pub transition: Option<Entered>,
    /// The node the round decided to enter, when the transition has not run yet: set by
    /// [`FlowManager::apply_results`] (and by [`FlowManager::apply`] if the transition could not
    /// complete, which is an error). Finish it with [`FlowManager::resume_pending`]; the node's
    /// `Say` actions are available meanwhile in [`FlowManager::pending_pre_actions`].
    pub pending: Option<NodeId>,
    /// The transition replaced the history (see [`Entered::context_replaced`]).
    pub context_replaced: bool,
    /// Results the flow did not append because their calls are no longer in the context (only
    /// possible for a stale round, see [`Applied::stale`]).
    pub dropped_results: usize,
    /// Transitions that named a node outside the flow (ignored; also logged).
    pub rejected: Vec<RejectedTransition>,
    /// Transitions that fired after the winning one in the same response (ignored).
    pub ignored_transitions: usize,
    /// The engine's retry budget for the turn is spent (see
    /// [`Execution::budget_exhausted`]).
    pub budget_exhausted: bool,
    /// Whether the runner should start another LLM round now: always, except when a transition
    /// entered a node with `respond_immediately` off.
    pub respond: bool,
}

/// One slot of a screened response, in issue order.
#[derive(Debug)]
enum Slot {
    /// Goes to the engine; the id is the call it answers.
    Admitted(ToolCallId),
    /// Refused by the flow; the message is its result.
    Refused(Message),
}

/// A response's tool calls after the current node's tool set was applied (see
/// [`FlowManager::screen`]). Consumed by [`FlowManager::apply`], so a round cannot be applied
/// twice.
#[derive(Debug)]
pub struct Screened {
    generation: u64,
    admitted: Vec<Assembled>,
    slots: Vec<Slot>,
    calls: Vec<ToolCall>,
}

impl Screened {
    /// The calls for the tool engine: the response's calls minus the refused ones, in issue order.
    pub fn admitted(&self) -> &[Assembled] {
        &self.admitted
    }

    /// The same calls, moved out for [`ToolEngine::execute`](crate::tools::ToolEngine::execute).
    /// Leaves nothing behind; the screening itself stays valid for [`FlowManager::apply`].
    pub fn take_admitted(&mut self) -> Vec<Assembled> {
        std::mem::take(&mut self.admitted)
    }

    /// Number of calls the flow refused.
    pub fn refused_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, Slot::Refused(_)))
            .count()
    }

    /// Whether the response had no call at all.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The engine's execution of the admitted calls, completed with the refusals into one result
    /// per call of the response, in issue order. A result the engine did not deliver is replaced
    /// by a `missing_result` error, and extra ones are dropped, so the result is always complete.
    /// [`FlowManager::apply`] does this itself; call it only to look at the merged results.
    pub fn merge(&self, execution: Execution) -> Execution {
        let Execution {
            results,
            late_notes,
            budget_exhausted,
            delivery,
        } = execution;
        let mut engine = results.into_iter();
        let results = self
            .slots
            .iter()
            .map(|slot| match slot {
                Slot::Refused(message) => message.clone(),
                Slot::Admitted(id) => engine.next().unwrap_or_else(|| {
                    Message::tool_error(
                        id.clone(),
                        json!({
                            "error": "missing_result",
                            "detail": "the tool engine returned no result for this call",
                        })
                        .to_string(),
                    )
                }),
            })
            .collect();
        Execution {
            results,
            late_notes,
            budget_exhausted,
            delivery,
        }
    }
}

/// What summarizing the history gave.
enum Summarized {
    /// Use this text.
    Text(String),
    /// There was no history to summarize.
    Nothing,
    /// No summarizer, a failure, a timeout or an empty reply.
    Failed,
}

/// The state machine of one call's flow: which node it is in and what was collected.
///
/// One manager per call, driven from the runner's single round task (`&mut self`). It never
/// blocks; the only await is the summarizer of a `ResetWithSummary` transition, bounded by
/// [`FlowConfig::summary_timeout`](super::FlowConfig::summary_timeout) on the injected clock.
pub struct FlowManager {
    flow: Arc<Flow>,
    clock: SharedClock,
    current: Arc<Node>,
    started: bool,
    generation: u64,
    transitions: u64,
    state: Map<String, Value>,
    state_dropped: u64,
    /// Tool calls of the latest epoch that a reset removed from the context (see
    /// [`FlowManager::next_tool_call_ids`]).
    id_offset: Option<(TurnEpoch, u32)>,
    /// A transition decided but not yet entered (see [`FlowManager::apply_results`]).
    pending: Option<NodeId>,
    /// State keys the flow itself writes (`capture_result`): the model's arguments cannot
    /// overwrite them.
    reserved_keys: BTreeSet<Arc<str>>,
    /// SideEffect tools of the current node whose latest call ended with an unknown outcome
    /// (timeout, lost, interrupted while running). Not offered and refused until a recorded late
    /// result settles them or the node changes. Holds names of registered tools only, so it is
    /// bounded by the registry.
    unsettled_effects: BTreeSet<Arc<str>>,
}

impl FlowManager {
    /// A manager positioned on the flow's initial node. Call [`Self::start`] before the first
    /// round.
    pub fn new(flow: Arc<Flow>, clock: SharedClock) -> Self {
        let current = Arc::clone(&flow.initial);
        let reserved_keys = flow
            .nodes()
            .flat_map(Node::tools)
            .chain(&flow.global_tools)
            .flat_map(|binding| binding.capture_results.iter().cloned())
            .collect();
        Self {
            flow,
            clock,
            current,
            started: false,
            generation: 0,
            transitions: 0,
            state: Map::new(),
            state_dropped: 0,
            id_offset: None,
            pending: None,
            reserved_keys,
            unsettled_effects: BTreeSet::new(),
        }
    }

    /// The flow.
    pub fn flow(&self) -> &Arc<Flow> {
        &self.flow
    }

    /// The current node.
    pub fn current(&self) -> &NodeId {
        &self.current.id
    }

    /// How many transitions ran.
    pub fn transitions(&self) -> u64 {
        self.transitions
    }

    /// Changes whenever the node changes. A [`Screened`] remembers the value it was made at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The values collected so far (tool captures and [`Self::set_state`]).
    pub fn state(&self) -> &Map<String, Value> {
        &self.state
    }

    /// Values that were not kept because of [`MAX_STATE_ENTRIES`], [`MAX_STATE_VALUE_BYTES`],
    /// [`MAX_STATE_KEY_BYTES`], or because a model argument named a key the flow writes itself.
    pub fn state_dropped(&self) -> u64 {
        self.state_dropped
    }

    /// Stores `value` under `key`. Returns false (and counts a drop) when the value is larger
    /// than [`MAX_STATE_VALUE_BYTES`], the key is longer than [`MAX_STATE_KEY_BYTES`], or a new
    /// key would exceed [`MAX_STATE_ENTRIES`].
    ///
    /// A value dropped for its size also removes the key's earlier value: keeping it would show
    /// data of an older call as current in the prompt of the node the new value led to. The
    /// placeholder stays unresolved instead.
    pub fn set_state(&mut self, key: impl Into<String>, value: Value) -> bool {
        let key = key.into();
        let too_big = value.to_string().len() > MAX_STATE_VALUE_BYTES;
        let too_long = key.len() > MAX_STATE_KEY_BYTES;
        let too_many = !self.state.contains_key(&key) && self.state.len() >= MAX_STATE_ENTRIES;
        if too_big || too_long || too_many {
            self.state_dropped = self.state_dropped.saturating_add(1);
            tracing::warn!(flow = %self.flow.name, too_big, too_long, too_many, "flow state value dropped");
            if too_big {
                self.state.remove(&key);
            }
            return false;
        }
        self.state.insert(key, value);
        true
    }

    /// The current node's prompt as text (role and task messages, placeholders filled from the
    /// state), exactly what message 0 of the context holds after entering the node.
    pub fn system_prompt(&self) -> String {
        self.prompt_of(&self.current)
    }

    fn prompt_of(&self, node: &Node) -> String {
        let role = node.role.as_deref().unwrap_or(&self.flow.role);
        role.iter()
            .chain(&node.task)
            .map(|message| render(message, &self.state))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Names of the tools the current node offers (its own and the global ones), in name order.
    fn tool_names(&self) -> BTreeSet<&str> {
        if self.pending.is_some() {
            return BTreeSet::new();
        }
        self.current
            .tools
            .iter()
            .chain(&self.flow.global_tools)
            .map(ToolBinding::name)
            .filter(|name| !self.unsettled_effects.contains(*name))
            .collect()
    }

    /// Whether the current node offers the tool `name`. Nothing is offered while a transition is
    /// pending ([`Self::pending_transition`]): the step is over, so a tool of the node being left
    /// (a SideEffect one in particular) cannot be called again.
    ///
    /// A SideEffect tool whose latest call ended without a known outcome (`timeout`,
    /// `unknown_outcome`, or interrupted while running) is not offered either
    /// ([`Self::unsettled_tool`]): a retry carries a fresh call id, so the engine's idempotency
    /// key would not stop a second run of an effect that may still be running.
    pub fn allows_tool(&self, name: &str) -> bool {
        self.pending.is_none()
            && !self.unsettled_effects.contains(name)
            && self.flow.binding(&self.current, name).is_some()
    }

    /// Whether `name` is a SideEffect tool of the current node that is withheld because its latest
    /// call has an unknown outcome. It is offered again when [`Self::apply_recorded_result`]
    /// receives the call's real result, or when the node changes.
    pub fn unsettled_tool(&self, name: &str) -> bool {
        self.unsettled_effects.contains(name)
    }

    /// The node a finished round decided to enter that has not been entered yet. Set by
    /// [`Self::apply_results`] and [`Self::apply`]; cleared when a transition completes
    /// ([`Self::resume_pending`], [`Self::transition_to`]) or is dropped
    /// ([`Self::abandon_pending`]). While it is set, no tool is offered
    /// ([`Self::tool_specs`] is empty) and the runner must finish the transition before the next
    /// LLM request, for example after a barge-in cancelled [`Self::apply`] during the summary.
    pub fn pending_transition(&self) -> Option<&NodeId> {
        self.pending.as_ref()
    }

    /// The `pre_actions` of the pending node (empty when none is pending): what the runner can
    /// play while the summarizer of a `ResetWithSummary` transition runs.
    pub fn pending_pre_actions(&self) -> &[Action] {
        self.pending
            .as_ref()
            .and_then(|node| self.flow.node(node))
            .map_or(&[], |node| &node.pre_actions)
    }

    /// Forgets the pending transition; the flow stays in the current node and offers its tools
    /// again. Returns the dropped target. Use when completing it keeps failing.
    pub fn abandon_pending(&mut self) -> Option<NodeId> {
        self.pending.take()
    }

    /// The specs for the next LLM request: the tools of the current node, in name order (so
    /// requests are reproducible and prompt-cache friendly).
    pub fn tool_specs(&self) -> Vec<&ToolSpec> {
        self.tool_names()
            .into_iter()
            .filter_map(|name| self.flow.registry.spec(name))
            .collect()
    }

    /// Fresh ids for the next `count` tool calls of turn `epoch`: use this instead of
    /// [`Context::next_tool_call_ids`] while a flow runs.
    ///
    /// A `Reset` or `ResetWithSummary` transition removes tool calls from the context, so the
    /// context alone would hand out `t<epoch>.0` again in the same turn, and the engine's
    /// idempotency store (keyed by session, epoch and call id) would take a new SideEffect call
    /// for the earlier one and not run it. The manager remembers how many calls of the epoch
    /// were removed and continues after them.
    pub fn next_tool_call_ids(
        &self,
        ctx: &Context,
        epoch: TurnEpoch,
        count: usize,
    ) -> Vec<ToolCallId> {
        let offset = self
            .id_offset
            .filter(|(known, _)| *known == epoch)
            .map_or(0, |(_, removed)| removed);
        let used = ctx
            .next_tool_call_id(epoch)
            .as_str()
            .rsplit_once('.')
            .and_then(|(_, seq)| seq.parse::<u32>().ok())
            .unwrap_or(0);
        let start = used.saturating_add(offset);
        (0..count)
            .map(|index| {
                let step = u32::try_from(index).unwrap_or(u32::MAX);
                ToolCallId::new(epoch, start.saturating_add(step))
            })
            .collect()
    }

    /// Actions to perform after the current node's first response was played.
    pub fn post_actions(&self) -> &[Action] {
        &self.current.post_actions
    }

    /// Whether the model answers right after entering the current node.
    pub fn respond_immediately(&self) -> bool {
        self.current.respond_immediately
    }

    /// Installs the initial node's prompt as message 0 of `ctx` (replacing a leading system
    /// message, otherwise inserting one) and returns what the runner must do. The history, if
    /// any, is kept: the initial node's strategy does not apply.
    ///
    /// # Errors
    /// [`FlowError::AlreadyStarted`].
    pub fn start(&mut self, ctx: &mut Context) -> Result<Entered, FlowError> {
        if self.started {
            return Err(FlowError::AlreadyStarted);
        }
        let message = Message::system(self.prompt_of(&self.current));
        let messages = ctx.messages_mut();
        match messages.first_mut() {
            Some(first @ Message::System { .. }) => *first = message,
            _ => messages.insert(0, message),
        }
        self.started = true;
        Ok(self.entered(
            &self.current,
            self.current.strategy,
            self.current.strategy,
            false,
        ))
    }

    /// Re-renders message 0 from the current state (after [`Self::set_state`], for example).
    ///
    /// # Errors
    /// [`FlowError::NotStarted`], [`FlowError::ContextMismatch`].
    pub fn refresh_prompt(&self, ctx: &mut Context) -> Result<(), FlowError> {
        if !self.started {
            return Err(FlowError::NotStarted);
        }
        let first = ctx.messages_mut().first_mut();
        match first {
            Some(first @ Message::System { .. }) => {
                *first = Message::system(self.system_prompt());
                Ok(())
            }
            _ => Err(FlowError::ContextMismatch),
        }
    }

    /// Splits a response's calls into those the current node offers and those it does not.
    ///
    /// Unavailable tools are answered here with a `tool_not_available` error result (same shape
    /// as the engine's errors) that lists the tools that are available. They never reach the
    /// engine, so they cannot run, and they spend none of its retry budget. Calls the assembler
    /// could not parse go to the engine, which answers them.
    ///
    /// Pass [`Screened::take_admitted`] to the engine, then the engine's [`Execution`] to
    /// [`Self::apply`].
    pub fn screen(&self, calls: Vec<Assembled>) -> Screened {
        let mut screened = Screened {
            generation: self.generation,
            admitted: Vec::with_capacity(calls.len()),
            slots: Vec::with_capacity(calls.len()),
            calls: Vec::new(),
        };
        for call in calls {
            let refusal = match &call {
                Assembled::Complete(complete) if !self.allows_tool(&complete.name) => {
                    Some(self.refusal(complete))
                }
                _ => None,
            };
            if let Some(message) = refusal {
                screened.slots.push(Slot::Refused(message));
                continue;
            }
            screened.slots.push(Slot::Admitted(call.id().clone()));
            if let Assembled::Complete(complete) = &call {
                screened.calls.push(complete.clone());
            }
            screened.admitted.push(call);
        }
        screened
    }

    fn refusal(&self, call: &ToolCall) -> Message {
        let shown: String = call.name.chars().take(MAX_SHOWN_NAME).collect();
        let names: Vec<&str> = self
            .tool_names()
            .into_iter()
            .take(MAX_LISTED_TOOLS)
            .collect();
        let available = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        let detail = if self.unsettled_effects.contains(&*call.name) {
            format!(
                "{shown} was not run: an earlier call of it ended without a known outcome \
                 (it may still be running or may have taken effect), so it is not repeated. \
                 Do not call it again. Available tools: {available}."
            )
        } else if self.pending.is_some() {
            format!(
                "{shown} was not run: the conversation is moving to its next step and no tool \
                 is available until it has started."
            )
        } else {
            format!(
                "{shown} is not available in the current step of the conversation. \
                 Available tools: {available}."
            )
        };
        Message::tool_error(
            call.id.clone(),
            json!({
                "error": "tool_not_available",
                "tool": shown,
                "detail": detail,
            })
            .to_string(),
        )
    }

    /// Finishes a round: appends the results of the response to `ctx` (one per call, in issue
    /// order, then the engine's late notes), remembers what the bindings capture, and runs the
    /// transition the results ask for. It is [`Self::apply_results`] followed by
    /// [`Self::resume_pending`]; use the two directly to play the entered node's `Say` actions
    /// while a `ResetWithSummary` summary is being made.
    ///
    /// `execution` is the engine's result for [`Screened::admitted`]; the flow's refusals are
    /// merged in (see [`Screened::merge`]). The assistant message with the response's calls must
    /// already be in `ctx`, as for [`Execution::append_to`].
    ///
    /// Transitions, by call in issue order:
    /// - A binding's transition is evaluated on its call's result (see [`Transition`](super::Transition)):
    ///   `Goto` fires on success, `Rules` by condition, `Custom` by function. Answers the engine
    ///   made up without running the tool (`budget_exhausted`, `stale_turn`, `interrupted`,
    ///   `duplicate_call`, `invalid_arguments`, `arguments_too_large`, `too_many_calls`,
    ///   `unknown_tool`, `store_full`) and the flow's own refusals are not tool results: no rule or
    ///   function sees them (only `tool_failed`, `timeout`, `unknown_outcome` and
    ///   `result_too_large` are routable failures), and when [`Execution::budget_exhausted`] is set no transition is decided at
    ///   all ([`Applied::budget_exhausted`]: speak a fallback).
    /// - The first transition that fires and names a node of the flow wins. Later ones are counted
    ///   in [`Applied::ignored_transitions`]; an unknown target is reported in
    ///   [`Applied::rejected`].
    /// - Captures run for every successful call of the round before the transition, so the new
    ///   node's prompt sees them.
    /// - If the node changed since [`Self::screen`] (a stale round), the results of calls still in
    ///   the context are appended and that is all ([`Applied::stale`]).
    /// - A transition applies the entered node's [`ContextStrategy`] (see [`Self::transition_to`]).
    ///
    /// A SideEffect call runs once whatever the flow does: transitions are decided from results
    /// the engine recorded, and the call's tool is no longer offered once the flow left its node
    /// or decided to (see [`Self::pending_transition`]). A SideEffect call that ended with a
    /// `timeout`, `unknown_outcome` or an interruption while running may still be running: its
    /// tool is withheld ([`Self::unsettled_tool`]) until its real result is passed to
    /// [`Self::apply_recorded_result`] or the node changes.
    ///
    /// # Cancellation
    /// The future may be dropped at its only await (the summarizer). The results, the captures and
    /// the decision are kept: [`Self::pending_transition`] names the node, no tool is offered, and
    /// the runner finishes with [`Self::resume_pending`] (or [`Self::abandon_pending`]). The next
    /// `apply` also finishes it.
    ///
    /// # Errors
    /// [`FlowError::NotStarted`] (nothing was done); [`FlowError::Context`] when the context is
    /// invalid after appending (the results are in; fix the caller); errors of
    /// [`Self::transition_to`] (the results, captures and pending decision stay, the node does not
    /// change).
    pub async fn apply(
        &mut self,
        ctx: &mut Context,
        screened: Screened,
        execution: Execution,
        summarizer: Option<&dyn Summarizer>,
    ) -> Result<Applied, FlowError> {
        let mut applied = self.apply_results(ctx, screened, execution)?;
        if applied.pending.is_some()
            && let Some(entered) = self.resume_pending(ctx, summarizer).await?
        {
            applied.respond = entered.respond_immediately;
            applied.context_replaced = entered.context_replaced;
            applied.pending = None;
            applied.transition = Some(entered);
        }
        Ok(applied)
    }

    /// The synchronous half of [`Self::apply`]: appends the results, captures state and decides
    /// the transition, but does not enter the node. A decided transition is returned in
    /// [`Applied::pending`] (and [`Self::pending_transition`]); the runner can play
    /// [`Self::pending_pre_actions`] now and finish with [`Self::resume_pending`]. No await, so
    /// nothing here can be cancelled halfway.
    ///
    /// # Errors
    /// [`FlowError::NotStarted`] (nothing was done); [`FlowError::Context`] when the context is
    /// invalid after appending.
    pub fn apply_results(
        &mut self,
        ctx: &mut Context,
        screened: Screened,
        execution: Execution,
    ) -> Result<Applied, FlowError> {
        if !self.started {
            return Err(FlowError::NotStarted);
        }
        let mut execution = screened.merge(execution);
        let stale = screened.generation != self.generation;
        let mut applied = Applied {
            stale,
            transition: None,
            pending: None,
            context_replaced: false,
            dropped_results: 0,
            rejected: Vec::new(),
            ignored_transitions: 0,
            budget_exhausted: execution.budget_exhausted,
            respond: true,
        };
        if stale {
            // A reset since `screen` may have removed the calls these results answer; a result
            // without its call would make the context invalid.
            let open: HashSet<ToolCallId> = ctx
                .pending_tool_calls()
                .into_iter()
                .map(|call| call.id.clone())
                .collect();
            let answers_open = |message: &Message| matches!(message, Message::Tool { call_id, .. } if open.contains(call_id));
            let kept = execution.results.iter().filter(|m| answers_open(m)).count();
            applied.dropped_results = execution.results.len().saturating_sub(kept);
            for message in execution.results.iter().filter(|m| answers_open(m)) {
                ctx.push(message.clone());
            }
            if applied.dropped_results == 0 {
                for note in &execution.late_notes {
                    ctx.push(note.clone());
                }
                execution.mark_delivered();
            }
            // Otherwise results were dropped with the calls a reset removed: dropping the
            // undelivered execution reports its SideEffect results as late notes and puts its
            // notes back (see `Delivery`), so nothing that happened is lost.
            ctx.validate()?;
            return Ok(applied);
        }
        execution.append_to(ctx);
        ctx.validate()?;
        let target = if self.pending.is_some() {
            // Decided earlier and not entered yet: it still wins. (`screen` refuses every call
            // while it is set, so this round has no result to decide on.)
            self.pending.clone()
        } else if execution.budget_exhausted {
            None
        } else {
            self.decide(&screened.calls, &execution.results, &mut applied)
        };
        if let Some(target) = target {
            applied.respond = self
                .flow
                .node(&target)
                .is_none_or(|node| node.respond_immediately);
            applied.pending = Some(target.clone());
            self.pending = Some(target);
        }
        Ok(applied)
    }

    /// Enters the pending node (see [`Self::pending_transition`]) with its context strategy, as
    /// [`Self::transition_to`] does. `Ok(None)` when nothing is pending. Dropping the future
    /// leaves the decision pending.
    ///
    /// # Errors
    /// As [`Self::transition_to`]; the decision stays pending.
    pub async fn resume_pending(
        &mut self,
        ctx: &mut Context,
        summarizer: Option<&dyn Summarizer>,
    ) -> Result<Option<Entered>, FlowError> {
        if !self.started {
            return Err(FlowError::NotStarted);
        }
        let Some(target) = self.pending.clone() else {
            return Ok(None);
        };
        self.enter(ctx, &target, summarizer).await.map(Some)
    }

    /// Decides a transition from a tool result the round did not deliver in time: a SideEffect
    /// call that timed out (`unknown_outcome`) whose real result arrived later (an
    /// [`Execution::late_notes`] note, the idempotency store). Captures the state and, if the
    /// current node binds the tool and its transition fires, makes the target pending (finish with
    /// [`Self::resume_pending`]). Returns the pending node. `Ok(None)` when the tool is not
    /// bound in the current node, the transition does not fire, the result is not a tool result
    /// (an engine refusal), or a transition is pending already. The result is not appended to the
    /// context: the late note already is.
    ///
    /// # Errors
    /// [`FlowError::NotStarted`].
    pub fn apply_recorded_result(
        &mut self,
        call: &ToolCall,
        content: &str,
        is_error: bool,
    ) -> Result<Option<NodeId>, FlowError> {
        if !self.started {
            return Err(FlowError::NotStarted);
        }
        if self.pending.is_some() {
            return Ok(None);
        }
        let mut winner = None;
        let mut rejected = Vec::new();
        let mut ignored = 0;
        self.consider(
            call,
            content,
            is_error,
            &mut winner,
            &mut rejected,
            &mut ignored,
        );
        self.pending = winner.clone();
        Ok(winner)
    }

    /// Captures the round's state and picks the winning transition. Synchronous and pure apart
    /// from the state.
    fn decide(
        &mut self,
        calls: &[ToolCall],
        results: &[Message],
        applied: &mut Applied,
    ) -> Option<NodeId> {
        let mut winner: Option<NodeId> = None;
        for call in calls {
            let Some((content, is_error)) = results.iter().find_map(|message| match message {
                Message::Tool {
                    call_id,
                    content,
                    is_error,
                } if *call_id == call.id => Some((&**content, *is_error)),
                _ => None,
            }) else {
                continue;
            };
            self.consider(
                call,
                content,
                is_error,
                &mut winner,
                &mut applied.rejected,
                &mut applied.ignored_transitions,
            );
        }
        winner
    }

    /// One call's result: capture, then its binding's transition.
    fn consider(
        &mut self,
        call: &ToolCall,
        content: &str,
        is_error: bool,
        winner: &mut Option<NodeId>,
        rejected: &mut Vec<RejectedTransition>,
        ignored: &mut usize,
    ) {
        let flow = Arc::clone(&self.flow);
        let node = Arc::clone(&self.current);
        let Some(binding) = flow.binding(&node, &call.name) else {
            return;
        };
        self.note_effect_outcome(&call.name, content, is_error);
        if is_error && is_not_a_result(content) {
            return;
        }
        if !is_error {
            self.capture(binding, call, content);
        }
        let Some(transition) = &binding.transition else {
            return;
        };
        let outcome = ToolOutcome::new(call, content, is_error);
        let Some(target) = transition.evaluate(&outcome) else {
            return;
        };
        if winner.is_some() {
            *ignored = ignored.saturating_add(1);
        } else if flow.node(&target).is_none() {
            tracing::warn!(flow = %flow.name, node = %node.id, target = %target, "transition to an unknown node ignored");
            rejected.push(RejectedTransition {
                call: call.id.clone(),
                target,
            });
        } else {
            *winner = Some(target);
        }
    }

    /// Tracks whether a SideEffect tool's latest call has an unknown outcome (see
    /// [`Self::unsettled_tool`]). A real result settles it; an answer the engine made up without
    /// running the tool changes nothing.
    fn note_effect_outcome(&mut self, tool: &Arc<str>, content: &str, is_error: bool) {
        let is_effect = self
            .flow
            .registry
            .spec(tool)
            .is_some_and(|spec| spec.kind == ToolKind::SideEffect);
        if !is_effect {
            return;
        }
        let unknown = is_error
            && error_kind(content)
                .is_some_and(|kind| OUTCOME_UNKNOWN_ERRORS.contains(&kind.as_str()));
        if unknown {
            self.unsettled_effects.insert(Arc::clone(tool));
        } else if !is_error || !is_not_a_result(content) {
            self.unsettled_effects.remove(&**tool);
        }
    }

    fn capture(&mut self, binding: &ToolBinding, call: &ToolCall, content: &str) {
        if binding.capture_args
            && let Value::Object(arguments) = &call.arguments
        {
            for (key, value) in arguments {
                if self.reserved_keys.contains(key.as_str()) {
                    // A model argument must not shadow a result the flow captured.
                    self.state_dropped = self.state_dropped.saturating_add(1);
                    continue;
                }
                self.set_state(key.clone(), value.clone());
            }
        }
        for key in &binding.capture_results {
            let value =
                serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.to_owned()));
            self.set_state(key.to_string(), value);
        }
    }

    /// Enters `target` as an explicit transition (not triggered by a tool): the application's
    /// own decision, a DTMF key, a timer. Applies the target's [`ContextStrategy`]:
    ///
    /// - `Append`: message 0 becomes the new prompt; the history stays.
    /// - `Reset`: the context becomes the new prompt alone.
    /// - `ResetWithSummary`: the summarizer (bounded by the flow's `summary_timeout` on the
    ///   injected clock) gets the newest part of the history (whole turns, at most
    ///   `summary_input_tokens`, each message clipped to [`SUMMARY_MESSAGE_MAX_TOKENS`]); a
    ///   summary of an earlier `ResetWithSummary` at message 1 is passed as `previous` and its
    ///   text is carried forward; the context becomes the prompt plus a [`FLOW_SUMMARY_PREFIX`]
    ///   system message with the summary, clipped to `summary_max_tokens`. If there is no
    ///   summarizer, or it fails, times out or returns nothing, the strategy falls back to
    ///   `Append` ([`Entered::fell_back`]). With no history, it is a plain `Reset`.
    ///
    /// With `Reset` or a summary, the node's [`Node::keep_last_user`] copies the newest user message
    /// behind the prompt (and the summary).
    ///
    /// Nothing changes unless the whole transition succeeds. A transition never runs a tool. A
    /// transition that was pending ([`Self::pending_transition`]) is replaced by this one.
    ///
    /// # Errors
    /// [`FlowError::NotStarted`], [`FlowError::UnknownNode`], [`FlowError::PendingToolCalls`],
    /// [`FlowError::ContextMismatch`] (message 0 is not a system message).
    pub async fn transition_to(
        &mut self,
        ctx: &mut Context,
        target: &NodeId,
        summarizer: Option<&dyn Summarizer>,
    ) -> Result<Entered, FlowError> {
        if !self.started {
            return Err(FlowError::NotStarted);
        }
        self.enter(ctx, target, summarizer).await
    }

    async fn enter(
        &mut self,
        ctx: &mut Context,
        target: &NodeId,
        summarizer: Option<&dyn Summarizer>,
    ) -> Result<Entered, FlowError> {
        let node = self
            .flow
            .node(target)
            .cloned()
            .ok_or_else(|| FlowError::UnknownNode(target.clone()))?;
        let pending: Vec<ToolCallId> = ctx
            .pending_tool_calls()
            .into_iter()
            .map(|call| call.id.clone())
            .collect();
        if !pending.is_empty() {
            return Err(FlowError::PendingToolCalls(pending));
        }
        if !matches!(ctx.messages().first(), Some(Message::System { .. })) {
            return Err(FlowError::ContextMismatch);
        }
        let prompt = Message::system(self.prompt_of(&node));
        let requested = node.strategy;
        let (applied, summary) = match requested {
            ContextStrategy::ResetWithSummary => match self.summarize(ctx, summarizer).await {
                Summarized::Text(text) => (ContextStrategy::ResetWithSummary, Some(text)),
                Summarized::Nothing => (ContextStrategy::Reset, None),
                Summarized::Failed => (ContextStrategy::Append, None),
            },
            other => (other, None),
        };
        let id_offset = if applied == ContextStrategy::Append {
            self.id_offset
        } else {
            id_offset_after_reset(ctx, self.id_offset)
        };
        match applied {
            ContextStrategy::Append => {
                let previous = ctx
                    .messages_mut()
                    .first_mut()
                    .map(|first| std::mem::replace(first, prompt));
                if let Err(error) = ctx.validate() {
                    if let (Some(previous), Some(first)) =
                        (previous, ctx.messages_mut().first_mut())
                    {
                        *first = previous;
                    }
                    return Err(error.into());
                }
            }
            ContextStrategy::Reset | ContextStrategy::ResetWithSummary => {
                let mut fresh = Context::new();
                fresh.push(prompt);
                if let Some(text) = summary {
                    fresh.push(Message::system(format!("{FLOW_SUMMARY_PREFIX}{text}")));
                }
                if node.keep_last_user
                    && let Some(last) = ctx
                        .messages()
                        .iter()
                        .rev()
                        .find(|message| matches!(message, Message::User { .. }))
                {
                    fresh.push(last.clone());
                }
                fresh.validate()?;
                *ctx = fresh;
            }
        }
        tracing::info!(
            flow = %self.flow.name,
            from = %self.current.id,
            to = %node.id,
            requested = ?requested,
            applied = ?applied,
            "flow transition"
        );
        self.id_offset = id_offset;
        self.pending = None;
        self.unsettled_effects.clear();
        self.current = Arc::clone(&node);
        self.generation = self.generation.wrapping_add(1);
        self.transitions = self.transitions.saturating_add(1);
        Ok(self.entered(
            &node,
            requested,
            applied,
            applied != ContextStrategy::Append,
        ))
    }

    fn entered(
        &self,
        node: &Node,
        requested: ContextStrategy,
        applied: ContextStrategy,
        context_replaced: bool,
    ) -> Entered {
        Entered {
            context_replaced,
            node: node.id.clone(),
            requested,
            applied,
            pre_actions: node.pre_actions.clone(),
            post_actions: node.post_actions.clone(),
            respond_immediately: node.respond_immediately,
        }
    }

    /// Summarizes the newest history of `ctx` (everything after message 0), extending the summary
    /// of an earlier `ResetWithSummary` when message 1 holds one.
    async fn summarize(&self, ctx: &Context, summarizer: Option<&dyn Summarizer>) -> Summarized {
        let Some(summarizer) = summarizer else {
            return Summarized::Failed;
        };
        let config = &self.flow.config;
        let (earlier, _) = split_history(ctx);
        let input = summary_input(ctx, config.summary_input_tokens);
        if input.is_empty() {
            // Nothing new since the last summary: carry it over unchanged.
            return match earlier {
                Some(text) => Summarized::Text(text.to_owned()),
                None => Summarized::Nothing,
            };
        }
        let previous = earlier.map(|text| WindowSummary {
            text: Arc::from(text),
            covers_until: 2,
        });
        let messages: Vec<Message> = input
            .iter()
            .map(|message| {
                clipped(message, SUMMARY_MESSAGE_MAX_TOKENS, &CharsPerToken).into_owned()
            })
            .collect();
        let request =
            pin!(summarizer.summarize(previous.as_ref(), &messages, config.summary_max_tokens));
        let timer = pin!(self.clock.sleep(config.summary_timeout));
        match select(request, timer).await {
            Either::Left((Ok(text), _)) => {
                let text = text.trim();
                if text.is_empty() {
                    Summarized::Failed
                } else {
                    Summarized::Text(clip(text, config.summary_max_tokens))
                }
            }
            Either::Left((Err(error), _)) => {
                tracing::warn!(flow = %self.flow.name, %error, "flow summary failed; keeping the history");
                Summarized::Failed
            }
            Either::Right(_) => {
                tracing::warn!(flow = %self.flow.name, "flow summary timed out; keeping the history");
                Summarized::Failed
            }
        }
    }
}

impl fmt::Debug for FlowManager {
    /// Prints the node and the state keys, never the state values (they come from the caller's
    /// speech).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlowManager")
            .field("flow", &self.flow.name)
            .field("node", &self.current.id)
            .field("started", &self.started)
            .field("generation", &self.generation)
            .field("transitions", &self.transitions)
            .field("state_keys", &self.state.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The epoch a tool call id belongs to (`t<epoch>.<seq>`).
fn epoch_of(id: &ToolCallId) -> Option<TurnEpoch> {
    let (epoch, _) = id.as_str().strip_prefix('t')?.split_once('.')?;
    epoch.parse::<u32>().ok().map(TurnEpoch::new)
}

/// The id offset after a reset wipes `ctx`: the calls of the newest epoch in it join those
/// removed by earlier resets of the same epoch. Unchanged when `ctx` holds no call.
fn id_offset_after_reset(
    ctx: &Context,
    previous: Option<(TurnEpoch, u32)>,
) -> Option<(TurnEpoch, u32)> {
    let mut latest: Option<(TurnEpoch, u32)> = None;
    for message in ctx.messages() {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for epoch in tool_calls.iter().filter_map(|call| epoch_of(&call.id)) {
            match &mut latest {
                Some((newest, count)) if *newest == epoch => *count = count.saturating_add(1),
                Some((newest, _)) if *newest > epoch => {}
                _ => latest = Some((epoch, 1)),
            }
        }
    }
    let Some((epoch, count)) = latest else {
        return previous;
    };
    let carried = previous
        .filter(|(known, _)| *known == epoch)
        .map_or(0, |(_, removed)| removed);
    Some((epoch, carried.saturating_add(count)))
}

/// The `error` kind of an engine error result, if `content` is one.
fn error_kind(content: &str) -> Option<String> {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
}

/// Whether an error result is not a tool result: an answer made up without the tool having run
/// (see [`TOOL_RESULT_ERRORS`]), or one this module does not recognise.
fn is_not_a_result(content: &str) -> bool {
    error_kind(content).is_none_or(|kind| !TOOL_RESULT_ERRORS.contains(&kind.as_str()))
}

/// The history after message 0, split into the summary of an earlier `ResetWithSummary` (the
/// system message `FLOW_SUMMARY_PREFIX` + text right behind the prompt, prefix removed) and the
/// messages after it.
fn split_history(ctx: &Context) -> (Option<&str>, &[Message]) {
    let history = ctx.messages().get(1..).unwrap_or_default();
    if let Some(Message::System { text }) = history.first()
        && let Some(summary) = text.strip_prefix(FLOW_SUMMARY_PREFIX)
    {
        return (Some(summary), history.get(1..).unwrap_or_default());
    }
    (None, history)
}

/// The part of the history given to the summarizer: whole turns (a turn starts at a user
/// message; at an assistant message when the history has no user message at all), newest first,
/// while they fit `budget` estimated tokens; the latest turn is always included. Message 0 (the
/// prompt) and an earlier flow summary (given as `previous` instead) are never included.
/// Everything else, if it all fits.
fn summary_input(ctx: &Context, budget: usize) -> &[Message] {
    let (_, history) = split_history(ctx);
    let has_user = history
        .iter()
        .any(|message| matches!(message, Message::User { .. }));
    let mut total = 0usize;
    let mut start: Option<usize> = None;
    let mut cut = false;
    for (index, message) in history.iter().enumerate().rev() {
        total = total.saturating_add(CharsPerToken.estimate(message));
        let starts_turn = if has_user {
            matches!(message, Message::User { .. })
        } else {
            matches!(message, Message::Assistant { .. })
        };
        if starts_turn {
            if total <= budget || start.is_none() {
                start = Some(index);
            } else {
                cut = true;
                break;
            }
        }
    }
    let from = match (cut, total <= budget, start) {
        (false, true, _) => 0,
        (_, _, Some(index)) => index,
        // Over budget and no turn start at all: the newest message alone.
        (_, _, None) => history.len().saturating_sub(1),
    };
    history.get(from..).unwrap_or_default()
}

/// Cuts `text` to about `max_tokens` estimated tokens (4 bytes per token), at a character
/// boundary, and marks the cut.
fn clip(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = 0;
    for (index, c) in text.char_indices() {
        let next = index + c.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push_str(CLIP_MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(turns: usize, text_len: usize) -> Context {
        let mut ctx = Context::with_system("prompt");
        for turn in 0..turns {
            ctx.push(Message::user(format!("{turn}{}", "u".repeat(text_len))));
            ctx.push(Message::assistant(format!(
                "{turn}{}",
                "a".repeat(text_len)
            )));
        }
        ctx
    }

    #[test]
    fn summary_input_takes_everything_that_fits() {
        let ctx = history(3, 8);
        assert_eq!(summary_input(&ctx, 10_000).len(), 6);
    }

    #[test]
    fn summary_input_keeps_whole_newest_turns_within_the_budget() {
        // Each message estimates to ceil(9 / 4) + 4 = 7 tokens; a turn is 14.
        let ctx = history(4, 8);
        let input = summary_input(&ctx, 30);
        assert_eq!(input.len(), 4, "two whole turns fit in 30 tokens");
        assert!(matches!(input.first(), Some(Message::User { text }) if text.starts_with('2')));
    }

    #[test]
    fn summary_input_always_includes_the_latest_turn() {
        let ctx = history(3, 400);
        let input = summary_input(&ctx, 5);
        assert_eq!(input.len(), 2);
        assert!(matches!(input.first(), Some(Message::User { text }) if text.starts_with('2')));
    }

    #[test]
    fn summary_input_without_a_user_message_still_respects_the_budget() {
        // After a Reset the history may hold assistant messages only; each is 7 tokens.
        let mut ctx = Context::with_system("p");
        for turn in 0..5 {
            ctx.push(Message::assistant(format!("{turn}{}", "a".repeat(8))));
        }
        let input = summary_input(&ctx, 15);
        assert_eq!(input.len(), 2, "the newest two fit in 15 tokens");
        assert!(
            matches!(input.first(), Some(Message::Assistant { text, .. }) if text.starts_with('3'))
        );
        // Nothing but system messages and an orphan result: the newest message alone.
        let mut odd = Context::with_system("p");
        odd.push(Message::system("x".repeat(400)));
        odd.push(Message::system("y".repeat(400)));
        assert_eq!(summary_input(&odd, 10).len(), 1);
    }

    #[test]
    fn summary_input_leaves_out_the_earlier_flow_summary() {
        let mut ctx = Context::with_system("p");
        ctx.push(Message::system(format!("{FLOW_SUMMARY_PREFIX}old")));
        ctx.push(Message::user("hi"));
        let input = summary_input(&ctx, 10_000);
        assert_eq!(input.len(), 1);
        assert_eq!(split_history(&ctx).0, Some("old"));
    }

    #[test]
    fn engine_refusals_are_not_results_but_tool_failures_are() {
        // Every answer the engine or the flow makes up without running a tool.
        for kind in [
            "budget_exhausted",
            "stale_turn",
            "interrupted",
            "duplicate_call",
            "missing_result",
            "tool_not_available",
            "invalid_arguments",
            "arguments_too_large",
            "too_many_calls",
            "unknown_tool",
            "store_full",
            "invalid tool call",
            "a_kind_the_engine_gains_later",
        ] {
            assert!(
                is_not_a_result(&json!({ "error": kind }).to_string()),
                "{kind}"
            );
        }
        // Only a tool that ran gives a routable failure.
        for kind in [
            "tool_failed",
            "timeout",
            "unknown_outcome",
            "result_too_large",
        ] {
            assert!(
                !is_not_a_result(&json!({ "error": kind }).to_string()),
                "{kind}"
            );
        }
        // An error result of an unrecognised shape is not routed either.
        assert!(is_not_a_result("plain text"));
        assert!(is_not_a_result(r#"{"error":7}"#));
    }

    #[test]
    fn summary_input_of_an_empty_history_is_empty() {
        assert!(summary_input(&Context::with_system("p"), 100).is_empty());
        assert!(summary_input(&Context::new(), 100).is_empty());
    }

    #[test]
    fn clip_cuts_at_a_character_boundary_and_marks_it() {
        assert_eq!(clip("short", 10), "short");
        let out = clip(&"é".repeat(50), 5); // 20 bytes: ten two-byte characters
        assert_eq!(out, format!("{}{CLIP_MARKER}", "é".repeat(10)));
    }
}
