//! Provider-neutral conversation context.
//!
//! One internal representation of the conversation that every LLM adapter translates from
//! (OpenAI, Anthropic, Gemini, OpenAI-compatible; `docs/TOOLS.md` item 2):
//!
//! - [`Message`]: system / user / assistant (text and/or tool calls) / tool result.
//! - [`ToolCallId`]: **stable internal ids** `t<epoch>.<seq>`, independent of the provider.
//!   `seq` counts every tool call of the turn across tool-loop rounds and restarts, so an id is
//!   never reused within a session: mint ids with [`Context::next_tool_call_id`] (the tool-call
//!   assembler owns this). Idempotency keys (session, epoch, call id; TOOLS.md item 6)
//!   therefore never confuse two calls. The provider's own id is kept next to it for
//!   round-trips to the same provider; switching providers mid-call only needs the internal
//!   id.
//! - [`Context::validate`]: the ordering every provider requires — each tool call is answered
//!   exactly once by a tool result, after the assistant message that made it and before any
//!   later message; no empty assistant messages.
//! - Interruptions with calls in flight: [`Context::interrupt_pending_tool_calls`] closes them
//!   with error results so the context stays valid; a SideEffect call that later completes is
//!   reported on the next turn as a system note ([`Context::push_late_tool_result`],
//!   TOOLS.md item 8).
//! - [`TokenEstimator`]: a hook for context-size decisions, with a cheap default.
//!
//! All types are serde-serializable (session snapshots, logs, tests). Opaque per-provider data
//! that must be echoed back (e.g. reasoning signatures) rides in `provider_data` fields; the
//! adapters define and verify their contents.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use brisvox_core::TurnEpoch;
use serde::{Deserialize, Serialize};

/// Stable internal id of a tool call: `t<epoch>.<seq>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(Arc<str>);

impl ToolCallId {
    /// The id of the `seq`-th tool call of turn `epoch` (counting across tool-loop rounds;
    /// prefer [`Context::next_tool_call_id`], which counts for you).
    pub fn new(epoch: TurnEpoch, seq: u32) -> Self {
        Self(Arc::from(format!("t{}.{seq}", epoch.get())))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this id belongs to turn `epoch`.
    pub fn is_of(&self, epoch: TurnEpoch) -> bool {
        self.0
            .strip_prefix('t')
            .and_then(|rest| rest.split_once('.'))
            .is_some_and(|(e, _)| e == epoch.get().to_string())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A complete tool call (arguments fully assembled and parsed; partial JSON never gets here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Stable internal id.
    pub id: ToolCallId,
    /// The id the provider gave the call, if any (sent back when talking to that provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<Arc<str>>,
    /// Tool name.
    pub name: Arc<str>,
    /// Arguments: a JSON object (providers require an object; adapters reject anything else).
    pub arguments: serde_json::Value,
    /// Opaque provider data to echo back with this call (adapter-defined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_data: Option<serde_json::Value>,
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Instructions for the model (also used for late notes, e.g. a tool result that arrived
    /// after an interruption).
    System {
        /// Text.
        text: Arc<str>,
    },
    /// What the caller said.
    User {
        /// Text (transcript).
        text: Arc<str>,
    },
    /// What the agent said (only what was actually played, after interruptions) and/or
    /// the tools it called. At least one of the two must be non-empty.
    Assistant {
        /// Spoken text (may be empty when the turn only called tools).
        #[serde(default, skip_serializing_if = "str_is_empty")]
        text: Arc<str>,
        /// Tool calls, in the order the model issued them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        /// Opaque provider data to echo back with this message (adapter-defined).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<serde_json::Value>,
    },
    /// The result of one tool call.
    Tool {
        /// The call it answers.
        call_id: ToolCallId,
        /// Result content (usually JSON text) given to the model.
        content: Arc<str>,
        /// Whether the call failed (the content then describes the error).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// The system note that reports a late tool result: a result that arrived after its call's slot
/// was closed, by an interruption or by a timeout (see [`Context::push_late_tool_result`]).
pub fn late_tool_result_note(call: &ToolCallId, name: &str, content: &str) -> Message {
    Message::system(format!(
        "Late result of tool call {call} ({name}), which completed after its slot was closed \
         (the user interrupted, or the tool timed out): {content}"
    ))
}

/// `skip_serializing_if` helper (serde passes a reference).
fn str_is_empty(text: &Arc<str>) -> bool {
    text.is_empty()
}

impl Message {
    /// A system message.
    pub fn system(text: impl Into<Arc<str>>) -> Self {
        Self::System { text: text.into() }
    }

    /// A user message.
    pub fn user(text: impl Into<Arc<str>>) -> Self {
        Self::User { text: text.into() }
    }

    /// An assistant message with text only.
    pub fn assistant(text: impl Into<Arc<str>>) -> Self {
        Self::Assistant {
            text: text.into(),
            tool_calls: Vec::new(),
            provider_data: None,
        }
    }

    /// An assistant message with (optional) text and tool calls.
    pub fn assistant_with_calls(text: impl Into<Arc<str>>, tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            text: text.into(),
            tool_calls,
            provider_data: None,
        }
    }

    /// A successful tool result.
    pub fn tool_result(call_id: ToolCallId, content: impl Into<Arc<str>>) -> Self {
        Self::Tool {
            call_id,
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed tool result (the content explains the error to the model).
    pub fn tool_error(call_id: ToolCallId, content: impl Into<Arc<str>>) -> Self {
        Self::Tool {
            call_id,
            content: content.into(),
            is_error: true,
        }
    }

    /// The message's text (assistant text, tool content), empty if none.
    pub fn text(&self) -> &str {
        match self {
            Self::System { text } | Self::User { text } | Self::Assistant { text, .. } => text,
            Self::Tool { content, .. } => content,
        }
    }
}

/// Why a context is not valid for an LLM request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// A tool result answers no earlier, still-open tool call.
    #[error("message {index}: tool result for unknown or already answered call {call_id}")]
    UnexpectedToolResult {
        /// Position in the context.
        index: usize,
        /// The call id it named.
        call_id: ToolCallId,
    },
    /// A message other than a tool result arrived while tool calls were unanswered.
    #[error("message {index}: tool calls still unanswered: {pending:?}")]
    UnansweredToolCalls {
        /// Position of the message that came too early.
        index: usize,
        /// The open calls.
        pending: Vec<ToolCallId>,
    },
    /// Two tool calls share an id.
    #[error("message {index}: duplicate tool call id {call_id}")]
    DuplicateToolCallId {
        /// Position in the context.
        index: usize,
        /// The repeated id.
        call_id: ToolCallId,
    },
    /// An assistant message with neither text nor tool calls (providers reject it; drop it,
    /// e.g. after truncation to nothing played).
    #[error("message {index}: empty assistant message")]
    EmptyAssistant {
        /// Position in the context.
        index: usize,
    },
}

/// Estimates how many tokens a message costs (for context-size decisions; exact counts are
/// provider-specific).
///
/// `Send + Sync`: an estimator is shared by the async tasks of a session (the window refresh holds
/// a `&dyn TokenEstimator` across the summarizer's await, which must keep that future `Send`).
pub trait TokenEstimator: Send + Sync {
    /// Estimated tokens of `message`, including per-message overhead.
    fn estimate(&self, message: &Message) -> usize;
}

/// Default estimate: one token per 4 bytes of text, tool names and serialized tool arguments,
/// plus 4 tokens of per-message overhead. Ids and provider data are not counted. A rough,
/// cheap figure (about right for English, low for Turkish); good enough for trimming decisions.
#[derive(Debug, Clone, Copy, Default)]
pub struct CharsPerToken;

impl TokenEstimator for CharsPerToken {
    fn estimate(&self, message: &Message) -> usize {
        let mut bytes = message.text().len();
        if let Message::Assistant { tool_calls, .. } = message {
            for call in tool_calls {
                bytes += call.name.len() + call.arguments.to_string().len();
            }
        }
        bytes.div_ceil(4) + 4
    }
}

/// The conversation so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Context {
    messages: Vec<Message>,
}

impl Context {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// A context starting with a system prompt.
    pub fn with_system(prompt: impl Into<Arc<str>>) -> Self {
        Self {
            messages: vec![Message::system(prompt)],
        }
    }

    /// Appends a message (validity is checked by [`Self::validate`], typically before a
    /// request).
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// The messages, oldest first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Mutable access (e.g. truncating the last assistant message to played text).
    /// Callers keep the invariants [`Self::validate`] checks.
    pub fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }

    /// Number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether there are no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.messages.iter().flat_map(|m| match m {
            Message::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
    }

    /// The id for the next tool call of turn `epoch`: one past the calls of that turn already
    /// in the context, so ids never repeat across tool-loop rounds. Mint all ids of one LLM
    /// response with [`Self::next_tool_call_ids`] before pushing it.
    pub fn next_tool_call_id(&self, epoch: TurnEpoch) -> ToolCallId {
        let used = self.tool_calls().filter(|c| c.id.is_of(epoch)).count();
        ToolCallId::new(epoch, u32::try_from(used).unwrap_or(u32::MAX))
    }

    /// `count` consecutive fresh ids for turn `epoch` (see [`Self::next_tool_call_id`]).
    pub fn next_tool_call_ids(&self, epoch: TurnEpoch, count: usize) -> Vec<ToolCallId> {
        let used = self.tool_calls().filter(|c| c.id.is_of(epoch)).count();
        (used..used.saturating_add(count))
            .map(|seq| ToolCallId::new(epoch, u32::try_from(seq).unwrap_or(u32::MAX)))
            .collect()
    }

    /// Tool calls of the latest assistant message that have no result yet, in issue order.
    pub fn pending_tool_calls(&self) -> Vec<&ToolCall> {
        let Some(at) = self
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::Assistant { .. }))
        else {
            return Vec::new();
        };
        let answered: HashSet<&ToolCallId> = self
            .messages
            .iter()
            .skip(at)
            .filter_map(|m| match m {
                Message::Tool { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect();
        match self.messages.get(at) {
            Some(Message::Assistant { tool_calls, .. }) => tool_calls
                .iter()
                .filter(|c| !answered.contains(&c.id))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Closes every pending tool call with an error result saying it was interrupted (call it
    /// when the user barges in while tools run), so the next turn's context is valid. Returns
    /// the closed ids: ReadOnly ones are cancelled; SideEffect ones keep running and report
    /// via [`Self::push_late_tool_result`].
    pub fn interrupt_pending_tool_calls(&mut self) -> Vec<ToolCallId> {
        let closed: Vec<ToolCallId> = self
            .pending_tool_calls()
            .into_iter()
            .map(|c| c.id.clone())
            .collect();
        for id in &closed {
            self.messages.push(Message::tool_error(
                id.clone(),
                "interrupted by the user before a result was available",
            ));
        }
        closed
    }

    /// Reports a tool result that arrived after its call was closed by an interruption, as a
    /// system note (the tool message slot is already used).
    pub fn push_late_tool_result(&mut self, call: &ToolCallId, name: &str, content: &str) {
        self.messages
            .push(late_tool_result_note(call, name, content));
    }

    /// Checks tool-call ordering: every result answers an open call of the latest assistant
    /// message, every call is answered before any other message, ids are unique, and no
    /// assistant message is empty. Calls still open at the end are allowed (the tools are
    /// running).
    ///
    /// # Errors
    /// The first violation found.
    pub fn validate(&self) -> Result<(), ContextError> {
        let mut seen: HashSet<&ToolCallId> = HashSet::new();
        let mut open: Vec<&ToolCallId> = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            if let Message::Tool { call_id, .. } = message {
                let Some(position) = open.iter().position(|id| *id == call_id) else {
                    return Err(ContextError::UnexpectedToolResult {
                        index,
                        call_id: call_id.clone(),
                    });
                };
                open.remove(position);
                continue;
            }
            if !open.is_empty() {
                return Err(ContextError::UnansweredToolCalls {
                    index,
                    pending: open.into_iter().cloned().collect(),
                });
            }
            if let Message::Assistant {
                text, tool_calls, ..
            } = message
            {
                if text.is_empty() && tool_calls.is_empty() {
                    return Err(ContextError::EmptyAssistant { index });
                }
                for call in tool_calls {
                    if !seen.insert(&call.id) {
                        return Err(ContextError::DuplicateToolCallId {
                            index,
                            call_id: call.id.clone(),
                        });
                    }
                    open.push(&call.id);
                }
            }
        }
        Ok(())
    }

    /// Estimated tokens of the whole context.
    pub fn estimate_tokens(&self, estimator: &dyn TokenEstimator) -> usize {
        self.messages.iter().map(|m| estimator.estimate(m)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: ToolCallId, name: &str) -> ToolCall {
        ToolCall {
            provider_id: Some(Arc::from(format!("call_{id}"))),
            id,
            name: Arc::from(name),
            arguments: json!({ "city": "Ankara" }),
            provider_data: None,
        }
    }

    const T1: TurnEpoch = TurnEpoch::new(1);

    fn tool_turn() -> Context {
        let mut c = Context::with_system("You are a helpful agent.");
        c.push(Message::user("Weather in Ankara and Izmir?"));
        let ids = c.next_tool_call_ids(T1, 2);
        c.push(Message::assistant_with_calls(
            "Let me check.",
            vec![
                call(ids[0].clone(), "weather"),
                call(ids[1].clone(), "weather"),
            ],
        ));
        c.push(Message::tool_result(ids[1].clone(), r#"{"temp":24}"#));
        c.push(Message::tool_error(ids[0].clone(), "timeout"));
        c.push(Message::assistant("Ankara timed out; Izmir is 24 degrees."));
        c
    }

    #[test]
    fn ids_are_stable_and_provider_independent() {
        let id = ToolCallId::new(TurnEpoch::new(7), 2);
        assert_eq!(id.as_str(), "t7.2");
        assert_eq!(id, ToolCallId::new(TurnEpoch::new(7), 2));
        assert_ne!(id, ToolCallId::new(TurnEpoch::new(7), 3));
        assert_eq!(id.to_string(), "t7.2");
        assert!(id.is_of(TurnEpoch::new(7)));
        assert!(!id.is_of(TurnEpoch::new(70)) && !id.is_of(TurnEpoch::new(0)));
        assert!(!ToolCallId::new(TurnEpoch::new(70), 2).is_of(TurnEpoch::new(7)));
    }

    #[test]
    fn a_complete_tool_turn_is_valid_in_any_result_order() {
        let c = tool_turn();
        assert_eq!(c.validate(), Ok(()));
        assert!(c.pending_tool_calls().is_empty());
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn tool_loop_rounds_in_one_turn_get_fresh_ids() {
        let mut c = tool_turn();
        // Second round of the same turn: the model calls another tool after the first results.
        c.messages_mut().pop();
        let next = c.next_tool_call_id(T1);
        assert_eq!(next.as_str(), "t1.2");
        c.push(Message::assistant_with_calls(
            "",
            vec![call(next.clone(), "forecast")],
        ));
        c.push(Message::tool_result(next, "{}"));
        c.push(Message::assistant("Done."));
        assert_eq!(c.validate(), Ok(()));
        // A new turn starts at 0 again; other turns' calls do not count.
        assert_eq!(c.next_tool_call_id(TurnEpoch::new(2)).as_str(), "t2.0");
        assert_eq!(
            c.next_tool_call_ids(TurnEpoch::new(10), 2),
            [
                ToolCallId::new(TurnEpoch::new(10), 0),
                ToolCallId::new(TurnEpoch::new(10), 1)
            ]
        );
    }

    #[test]
    fn ordering_violations_are_reported() {
        let id = ToolCallId::new(TurnEpoch::ZERO, 0);
        // Any non-tool message while calls are open, including a system message.
        for early in [Message::user("hello?"), Message::system("note")] {
            let mut c = Context::new();
            c.push(Message::assistant_with_calls(
                "",
                vec![call(id.clone(), "lookup")],
            ));
            assert_eq!(c.pending_tool_calls().len(), 1);
            assert_eq!(c.validate(), Ok(()), "open calls at the end are allowed");
            c.push(early);
            assert!(matches!(
                c.validate(),
                Err(ContextError::UnansweredToolCalls { index: 1, .. })
            ));
        }
        // A result for a call nobody made, or placed before its call.
        let mut stray = Context::new();
        stray.push(Message::tool_result(id.clone(), "{}"));
        stray.push(Message::assistant_with_calls(
            "",
            vec![call(id.clone(), "lookup")],
        ));
        assert!(matches!(
            stray.validate(),
            Err(ContextError::UnexpectedToolResult { index: 0, .. })
        ));
        // A second result for the same call, and a result for an earlier, completed turn.
        let t1_0 = ToolCallId::new(T1, 0);
        let mut twice = tool_turn();
        twice
            .messages_mut()
            .insert(5, Message::tool_result(t1_0.clone(), "{}"));
        assert!(matches!(
            twice.validate(),
            Err(ContextError::UnexpectedToolResult { index: 5, .. })
        ));
        let mut stale = tool_turn();
        stale.push(Message::user("thanks"));
        stale.push(Message::tool_result(t1_0.clone(), "{}"));
        assert!(matches!(
            stale.validate(),
            Err(ContextError::UnexpectedToolResult { index: 7, .. })
        ));
        // A genuine duplicate id (minted by hand).
        let mut dup = tool_turn();
        dup.push(Message::user("again"));
        dup.push(Message::assistant_with_calls(
            "",
            vec![call(t1_0, "weather")],
        ));
        assert!(matches!(
            dup.validate(),
            Err(ContextError::DuplicateToolCallId { index: 7, .. })
        ));
        // An empty assistant message.
        let mut empty = Context::new();
        empty.push(Message::user("hi"));
        empty.push(Message::assistant(""));
        assert_eq!(
            empty.validate(),
            Err(ContextError::EmptyAssistant { index: 1 })
        );
    }

    #[test]
    fn an_interruption_closes_pending_calls_and_late_results_become_notes() {
        let mut c = Context::new();
        c.push(Message::user("Book a table and check the weather"));
        let ids = c.next_tool_call_ids(TurnEpoch::ZERO, 2);
        c.push(Message::assistant_with_calls(
            "",
            vec![
                call(ids[0].clone(), "book_table"),
                call(ids[1].clone(), "weather"),
            ],
        ));
        c.push(Message::tool_result(ids[1].clone(), "{}"));
        // The user barges in while book_table (SideEffect) still runs.
        assert_eq!(c.interrupt_pending_tool_calls(), [ids[0].clone()]);
        assert!(c.pending_tool_calls().is_empty());
        c.push(Message::user("Actually, make it 8 pm."));
        assert_eq!(c.validate(), Ok(()));
        // The booking completes later and is reported on the next turn.
        c.push_late_tool_result(&ids[0], "book_table", r#"{"booked":"19:00"}"#);
        assert_eq!(c.validate(), Ok(()));
        assert!(c.messages().last().unwrap().text().contains("t0.0"));
        assert!(c.interrupt_pending_tool_calls().is_empty(), "nothing open");
    }

    #[test]
    fn serde_round_trip_and_wire_shape() {
        let mut c = tool_turn();
        if let Some(Message::Assistant { provider_data, .. }) = c.messages_mut().get_mut(2) {
            *provider_data = Some(json!({ "signature": "abc" }));
        }
        let text = serde_json::to_string(&c).unwrap();
        let back: Context = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c);
        let value = serde_json::to_value(&c).unwrap();
        assert_eq!(
            value[1],
            json!({ "role": "user", "text": "Weather in Ankara and Izmir?" })
        );
        assert_eq!(
            value[2],
            json!({
                "role": "assistant",
                "text": "Let me check.",
                "tool_calls": [
                    { "id": "t1.0", "provider_id": "call_t1.0", "name": "weather",
                      "arguments": { "city": "Ankara" } },
                    { "id": "t1.1", "provider_id": "call_t1.1", "name": "weather",
                      "arguments": { "city": "Ankara" } }
                ],
                "provider_data": { "signature": "abc" }
            })
        );
        assert_eq!(
            value[4],
            json!({ "role": "tool", "call_id": "t1.0", "content": "timeout", "is_error": true })
        );
        assert_eq!(value[3].get("is_error"), None, "false is omitted");
        // Optional fields may be absent on input; unknown roles are rejected.
        let minimal: Message =
            serde_json::from_value(json!({ "role": "assistant", "text": "hi" })).unwrap();
        assert_eq!(minimal, Message::assistant("hi"));
        assert!(
            serde_json::from_value::<Message>(json!({ "role": "robot", "text": "x" })).is_err()
        );
    }

    #[test]
    fn token_estimate_counts_text_and_tool_arguments() {
        let est = CharsPerToken;
        assert_eq!(est.estimate(&Message::user("")), 4);
        assert_eq!(est.estimate(&Message::user("12345678")), 4 + 2);
        let with_call =
            Message::assistant_with_calls("", vec![call(ToolCallId::new(T1, 0), "weather")]);
        // "weather" (7) + {"city":"Ankara"} (17) = 24 bytes -> 6 tokens + 4.
        assert_eq!(est.estimate(&with_call), 10);
        let c = tool_turn();
        let total: usize = c.messages().iter().map(|m| est.estimate(m)).sum();
        assert_eq!(c.estimate_tokens(&est), total);
    }
}
