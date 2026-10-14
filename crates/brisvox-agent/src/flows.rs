//! Node-based conversation flows, the equivalent of Pipecat Flows' basics.
//!
//! A [`Flow`] is a set of named [`Node`]s. A node is one step of the conversation: its own prompt
//! (role and task messages), its own set of tools, a [`ContextStrategy`] for what happens to the
//! history when the node is entered, and optional [`Action`]s to run on entering (`pre`) and after
//! the node's first response was played (`post`). A tool's result moves the conversation from node
//! to node through the [`Transition`] of its [`ToolBinding`].
//!
//! The module is data plus a state machine; it does no IO and never touches the clock except to
//! bound a summarizer. It sits on the existing pieces: [`Context`](crate::Context) for the
//! history, [`ToolRegistry`](crate::tools::ToolRegistry) and
//! [`ToolEngine`](crate::tools::ToolEngine) for validation, execution, timeouts and idempotency
//! (a flow never runs a tool itself), and [`window`](crate::window)'s
//! [`Summarizer`](crate::window::Summarizer) for `ResetWithSummary`.
//!
//! # The protocol (what the LLM runner does per round)
//!
//! ```text
//! flow.start(&mut ctx)                            // once: installs node 0's prompt, returns pre actions
//! loop {
//!     request = (ctx, flow.tool_specs())          // the CURRENT node's tools (name order)
//!     response = llm(request); ctx.push(assistant message of the response)
//!     let mut screened = flow.screen(response.calls());      // tools outside the node are refused
//!     let exec = engine.execute(screened.take_admitted(), epoch, &cancel).await;
//!     let applied = flow.apply(&mut ctx, screened, exec, Some(&summarizer)).await?;
//!     // results appended; maybe a transition ran: applied.transition has the entered node's
//!     // pre/post actions; applied.respond says whether to run another round
//! }
//! ```
//!
//! # Rules
//!
//! - **Message 0 is the node prompt.** The manager owns it: [`FlowManager::start`] installs it and
//!   every transition replaces it (`Append`) or rebuilds the context around it (`Reset`,
//!   `ResetWithSummary`). It is a leading system message, so the long-call window
//!   ([`build_window`](crate::window::build_window)) always keeps it. One deliberate difference
//!   from Pipecat: `Append` swaps the prompt instead of adding the new task messages at the end
//!   of the history, because an appended system message would be dropped by the window after
//!   enough turns and the model would lose its instructions.
//! - **Only the current node's tools can run.** [`FlowManager::screen`] answers calls to other
//!   tools with a `tool_not_available` error before they reach the engine. This covers a model
//!   that calls a tool it saw earlier in the history.
//! - **Transitions come from results.** `Goto` fires when the call succeeded; `Rules` match on the
//!   result JSON, the arguments or success/failure; `Custom` is a function. A failed call (a
//!   timeout, `unknown_outcome`, a tool error) never fires a `Goto`, so the model can retry. The
//!   first transition of a response that names a node of the flow wins; the others are counted.
//!   Explicit transition functions are tools with a trivial body ([`example::Ack`]) and a
//!   `Goto`; application code can also call [`FlowManager::transition_to`].
//! - **SideEffect tools run once.** The engine's idempotency key guarantees it for a call id. The
//!   flow adds: a [`Screened`] is consumed by `apply` (a round cannot be applied twice), a round
//!   screened before a transition is stale afterwards ([`Applied::stale`]: results appended, no
//!   second transition), and the tool of a left node is no longer offered.
//! - **State.** [`ToolBinding::capture_args`] and [`ToolBinding::capture_result`] copy values of
//!   successful calls into a small key/value state ([`MAX_STATE_ENTRIES`], [`MAX_STATE_VALUE_BYTES`]
//!   bound it). Task messages use `{key}` or `{key.sub.0}` placeholders; the values are bounded
//!   to 256 characters and stripped of control characters, because they originate in caller
//!   speech and end up in a system message. Constrain the tool schemas (enums, lengths) for
//!   fields that feed prompts.
//! - **Cancellation.** The only await of [`FlowManager::apply`] is the summarizer (bounded by a
//!   timeout on the injected clock). The round's results, captures and *decision* are recorded
//!   before it: if the future is dropped there (barge-in, session cancel),
//!   [`FlowManager::pending_transition`] names the node, no tool is offered meanwhile (so a
//!   SideEffect tool cannot be called again from the node being left), and
//!   [`FlowManager::resume_pending`] (or the next `apply`) finishes the transition. The node,
//!   generation and context strategy change only when the whole transition succeeded.
//!   [`FlowManager::apply_results`] + `resume_pending` is the same thing in two steps, so the
//!   runner can play [`FlowManager::pending_pre_actions`] while the summary is made.
//! - **Reset invalidates indices.** [`Entered::context_replaced`] says the history was replaced:
//!   drop the window `Summary`, the round to message index map and the play-out truncation
//!   indices, which refer to messages that no longer exist.
//! - **Not tool results.** Only `tool_failed`, `timeout`, `unknown_outcome` and
//!   `result_too_large` errors are results of a tool that ran. Every other error answer
//!   (`budget_exhausted`, `stale_turn`, `interrupted`, `duplicate_call`, `invalid_arguments`,
//!   `arguments_too_large`, `too_many_calls`, `unknown_tool`, `store_full`, a malformed call) and
//!   the flow's own refusals never reach a rule or custom function and never transition: the
//!   model simply retries.
//! - **Unsettled SideEffect calls.** A SideEffect call that ended with `timeout`,
//!   `unknown_outcome` or an interruption while running may still be running. Its tool is neither
//!   offered nor admitted ([`FlowManager::unsettled_tool`]) until its real result reaches
//!   [`FlowManager::apply_recorded_result`] or the node changes, because a retry would get a new
//!   call id and the engine's idempotency key would not stop a second run.
//!
//! The reference flow is in [`example`] (a restaurant reservation in English and Turkish).

mod def;
pub mod example;
mod manager;
mod render;

pub use def::{
    Action, Condition, ContextStrategy, Flow, FlowBuilder, FlowConfig, FlowDefError, Node, NodeId,
    Rule, ToolBinding, ToolOutcome, Transition,
};
pub use manager::{
    Applied, Entered, FLOW_SUMMARY_PREFIX, FlowError, FlowManager, MAX_STATE_ENTRIES,
    MAX_STATE_KEY_BYTES, MAX_STATE_VALUE_BYTES, RejectedTransition, SUMMARY_MESSAGE_MAX_TOKENS,
    Screened,
};
