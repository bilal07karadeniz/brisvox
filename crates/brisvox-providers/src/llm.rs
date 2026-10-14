//! LLM provider interface (minimal; refined with the first real adapter in M3).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Who authored a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// System prompt.
    System,
    /// The caller.
    User,
    /// The agent.
    Assistant,
    /// A tool result.
    Tool,
}

/// A tool call the assistant made, as part of an assistant [`Message`]: what the adapter needs to
/// replay the turn to a stateless provider (OpenAI Responses with `store: false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The provider's call id (the `id` of the [`LlmEvent::ToolCallDelta`] that opened it); the
    /// matching [`Role::Tool`] message carries it as `tool_call_id`.
    pub id: Arc<str>,
    /// Tool name.
    pub name: Arc<str>,
    /// Arguments as JSON text (an object).
    pub arguments: Arc<str>,
}

/// One message of the conversation context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Text content.
    pub content: Arc<str>,
    /// For [`Role::Tool`]: the call this result answers.
    pub tool_call_id: Option<Arc<str>>,
    /// For [`Role::Assistant`]: the tools the assistant called in this turn (empty otherwise).
    /// Without them a tool result in the context answers no call, which providers reject.
    pub tool_calls: Vec<ToolCall>,
    /// For [`Role::Assistant`]: the opaque `provider_data` of the [`LlmEvent::Done`] that ended
    /// the turn, echoed back verbatim (for OpenAI: the reasoning items the next request must
    /// replay). Adapters ignore data that is not theirs.
    pub provider_data: Option<Arc<str>>,
}

impl Message {
    /// A message without tool calls, tool call id or provider data.
    pub fn new(role: Role, content: impl Into<Arc<str>>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            provider_data: None,
        }
    }

    /// A [`Role::Tool`] message: the result of the call `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<Arc<str>>, content: impl Into<Arc<str>>) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            ..Self::new(Role::Tool, content)
        }
    }

    /// A [`Role::Assistant`] message with the tool calls it made and the provider data of its
    /// response.
    pub fn assistant(
        content: impl Into<Arc<str>>,
        tool_calls: Vec<ToolCall>,
        provider_data: Option<Arc<str>>,
    ) -> Self {
        Self {
            tool_calls,
            provider_data,
            ..Self::new(Role::Assistant, content)
        }
    }
}

/// A tool the LLM may call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Tool name.
    pub name: Arc<str>,
    /// What it does (shown to the LLM).
    pub description: Arc<str>,
    /// JSON Schema of the arguments, as JSON text.
    pub parameters_schema: Arc<str>,
}

/// One completion request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmRequest {
    /// Conversation so far.
    pub messages: Vec<Message>,
    /// Tools available for this request.
    pub tools: Vec<ToolSpec>,
}

/// Why a response ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishReason {
    /// Natural end of the answer.
    Stop,
    /// The model wants tool calls executed.
    ToolCalls,
    /// Output token limit reached.
    Length,
}

/// Token accounting of one response (when the provider reports it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u32,
    /// Of `input_tokens`, served from the provider's prompt cache.
    pub cached_input_tokens: u32,
    /// Generated tokens.
    pub output_tokens: u32,
}

/// A streamed piece of an LLM response.
#[derive(Debug)]
pub enum LlmEvent {
    /// Text delta.
    TextDelta(Arc<str>),
    /// Fragment of a tool call; fragments with the same `index` concatenate. `id` and `name`
    /// arrive on the first fragment of a call. Fragments of different calls may interleave.
    ToolCallDelta {
        /// Position of the call in the response.
        index: u32,
        /// Call id (first fragment).
        id: Option<Arc<str>>,
        /// Tool name (first fragment).
        name: Option<Arc<str>>,
        /// Raw JSON argument fragment.
        arguments: Arc<str>,
    },
    /// The response is complete. The last event of a successful stream.
    Done {
        /// Why it ended.
        finish: FinishReason,
        /// Token accounting, if reported.
        usage: Option<Usage>,
        /// Opaque, adapter-defined JSON text to store on the assistant [`Message`] of this turn
        /// and pass back with it ([`Message::provider_data`]); `None` when the provider needs
        /// nothing replayed.
        provider_data: Option<Arc<str>>,
    },
    /// The stream failed; no further events follow.
    Error(Error),
}

/// Capacity of a response stream's event channel (backpressure: the provider waits).
pub const LLM_EVENT_CAPACITY: usize = 64;

/// A response being streamed. Dropping it cancels the request.
#[derive(Debug)]
pub struct LlmStream {
    events: mpsc::Receiver<LlmEvent>,
}

impl LlmStream {
    /// A stream fed through `events` (adapters create the channel with
    /// [`LLM_EVENT_CAPACITY`]).
    pub fn new(events: mpsc::Receiver<LlmEvent>) -> Self {
        Self { events }
    }

    /// The next event; `None` once the provider closed the stream. A stream that ends without
    /// [`LlmEvent::Done`] or [`LlmEvent::Error`] was cut off.
    ///
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<LlmEvent> {
        self.events.recv().await
    }
}

/// A streaming LLM.
pub trait LlmProvider: Send + Sync + 'static {
    /// Provider name, e.g. `"openai"`.
    fn name(&self) -> &'static str;

    /// Starts streaming a response to `request`.
    ///
    /// # Errors
    /// Errors before any event is produced (e.g. [`Error::Rejected`]); failures after that are
    /// reported as [`LlmEvent::Error`].
    fn stream(&self, request: LlmRequest) -> impl Future<Output = Result<LlmStream>> + Send;

    /// Pre-establishes whatever makes the next request fast (DNS, TLS, pooled connection).
    ///
    /// # Errors
    /// Warm-up failures (callers may ignore them; `stream` retries).
    fn warm(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}
