//! The wire format of the agent protocol, version 1 (`docs/AGENT_PROTOCOL.md` is the normative
//! description; this module is its implementation).
//!
//! Every message is one WebSocket text frame holding one JSON object with a `"type"` field.
//! Messages from the server ([`ServerMessage`]) carry an envelope: `seq`, a number that grows by
//! one with every message the server writes on the link, and `t_ms`, the milliseconds since the
//! link opened. Messages from the agent ([`AgentMessage`]) are commands, tool results and the
//! handshake answer; a command may carry an `id` (echoed in its [`ServerBody::CommandResult`])
//! and `re`, the `seq` of the event it reacts to (the server measures the hop latency with it).
//!
//! Unknown fields are ignored on both sides (forward compatibility within a version); an unknown
//! `type` from the agent is answered with [`ErrorCode::UnknownType`].

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol's name, sent in `session.start`.
pub const PROTOCOL: &str = "brisvox.agent";

/// The protocol version this build speaks.
pub const VERSION: u32 = 1;

/// The versions this build speaks, offered in `session.start` (the agent picks one).
pub const SUPPORTED_VERSIONS: &[u32] = &[VERSION];

/// Largest message either side may send, in bytes (the server closes the link on a larger one).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Most tools an agent may declare.
pub const MAX_TOOLS: usize = 64;

/// Most messages of an agent's initial context (`session.accept.context`) and of one
/// `context.update.append`.
pub const MAX_CONTEXT_MESSAGES: usize = 32;

/// Longest command `id`, in bytes.
pub const MAX_COMMAND_ID_BYTES: usize = 64;

/// Longest free text of a transfer (`summary`, `reason`) and of an `end.reason`, in bytes.
pub const MAX_NOTE_BYTES: usize = 512;

/// One message from the server: the envelope and the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerMessage {
    /// The message's number on the link: 1 for `session.start`, then one more per message (a
    /// resumed link continues the count of the call).
    pub seq: u64,
    /// Milliseconds since the link opened (the server's clock).
    pub t_ms: u64,
    /// What the message says.
    #[serde(flatten)]
    pub body: ServerBody,
}

/// The bodies of the server's messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ServerBody {
    /// The first message: a call started and wants an agent.
    #[serde(rename = "session.start")]
    SessionStart {
        /// [`PROTOCOL`].
        protocol: String,
        /// The versions the server speaks ([`SUPPORTED_VERSIONS`]).
        versions: Vec<u32>,
        /// The server's id of the call (unique in the server process).
        session_id: String,
        /// The name of the agent definition the call runs.
        agent: String,
        /// What the server can do on this call.
        capabilities: Capabilities,
        /// Present when this connection continues a call whose earlier connection was lost
        /// (only offered to an agent that declared `resume` in its `session.accept`). Absent
        /// on the first connection of a call, and in version 1 as it was first specified.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<Resume>,
        /// Who is on the line, for a telephony call (Twilio, Telnyx, SIP) when the agent
        /// definition opted in (`[agent_link] caller_identity`). Absent otherwise, and for
        /// every transport without a carrier. Repeated by a resuming `session.start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<CallIdentity>,
    },
    /// The last message: the call ended. The server closes the link after it.
    #[serde(rename = "session.end")]
    SessionEnd {
        /// Why (`hangup`, `ended_by_agent`, `shutdown`, ...).
        reason: String,
    },
    /// A user turn started (speech; also a confirmed barge-in).
    #[serde(rename = "turn.user_started")]
    TurnUserStarted {
        /// The turn's epoch.
        epoch: u32,
    },
    /// The user turn ended: the engine answers it now.
    #[serde(rename = "turn.user_ended")]
    TurnUserEnded {
        /// The turn's epoch.
        epoch: u32,
    },
    /// The bot's audio of `epoch` started playing.
    #[serde(rename = "turn.bot_started")]
    TurnBotStarted {
        /// The response's epoch.
        epoch: u32,
    },
    /// The bot's audio of `epoch` was played to its end.
    #[serde(rename = "turn.bot_ended")]
    TurnBotEnded {
        /// The response's epoch.
        epoch: u32,
    },
    /// A barge-in: the call moved to `epoch`, everything older was dropped.
    #[serde(rename = "turn.interrupted")]
    TurnInterrupted {
        /// The new epoch.
        epoch: u32,
    },
    /// The latencies of a turn, when its first audio went out.
    #[serde(rename = "turn.metrics")]
    TurnMetrics {
        /// The turn's epoch.
        epoch: u32,
        /// The latencies known then.
        metrics: Metrics,
    },
    /// A transcript segment from the STT.
    #[serde(rename = "user.transcript")]
    UserTranscript {
        /// The text.
        text: Arc<str>,
        /// `false` for an interim hypothesis.
        #[serde(rename = "final")]
        is_final: bool,
    },
    /// The whole text of the user turn the LLM answers.
    #[serde(rename = "user.text")]
    UserText {
        /// The turn's epoch.
        epoch: u32,
        /// The text.
        text: Arc<str>,
    },
    /// The LLM started a response round.
    #[serde(rename = "bot.llm_started")]
    BotLlmStarted {
        /// The response's epoch.
        epoch: u32,
    },
    /// A streamed LLM text delta (a bot line arrives as one delta).
    #[serde(rename = "bot.text")]
    BotText {
        /// The response's epoch.
        epoch: u32,
        /// The delta.
        text: Arc<str>,
    },
    /// The LLM finished a response round.
    #[serde(rename = "bot.llm_stopped")]
    BotLlmStopped {
        /// The response's epoch.
        epoch: u32,
    },
    /// A chunk of text sent to the TTS.
    #[serde(rename = "bot.tts_text")]
    BotTtsText {
        /// The response's epoch.
        epoch: u32,
        /// The chunk.
        text: Arc<str>,
    },
    /// The TTS produced the response's last audio.
    #[serde(rename = "bot.tts_stopped")]
    BotTtsStopped {
        /// The response's epoch.
        epoch: u32,
    },
    /// A stage of the call reported an error.
    #[serde(rename = "error")]
    Error {
        /// The stage (`stt`, `llm`, `tts`, `transport`, `agent_link`).
        stage: String,
        /// What happened (never credentials).
        message: String,
        /// Whether the call cannot continue.
        fatal: bool,
    },
    /// Events the agent did not get: the link's event queue was full, or the server fell behind
    /// the call. Sent before the next event that fits.
    #[serde(rename = "events.lost")]
    EventsLost {
        /// Events dropped since the previous `events.lost`.
        count: u64,
    },
    /// The LLM called a tool the agent declared: answer with `tool.result`.
    #[serde(rename = "tool.call")]
    ToolCall {
        /// The request's id on this link: echo it in `tool.result`.
        id: u64,
        /// The engine's id of the call (stable for its life, unique in the call).
        call_id: String,
        /// The tool.
        name: String,
        /// The arguments, already valid against the declared schema.
        arguments: Value,
        /// How the engine runs the tool.
        kind: WireToolKind,
        /// The turn the call belongs to.
        epoch: u32,
        /// 16 hex digits, stable for this call: pass it to APIs that deduplicate requests.
        idempotency_key: String,
        /// How long the server waits for the result.
        timeout_ms: u64,
    },
    /// The server stopped waiting for a `tool.call`: a late `tool.result` is ignored. For a
    /// `side_effect` tool this is advice only (the outcome is recorded as unknown).
    #[serde(rename = "tool.cancel")]
    ToolCancel {
        /// The `tool.call` id.
        id: u64,
        /// Why.
        reason: CancelReason,
    },
    /// The outcome of a command that carried an `id`.
    #[serde(rename = "command.result")]
    CommandResult {
        /// The command's `id`.
        id: String,
        /// Whether it was applied (queued, for `say` and `context.update`).
        ok: bool,
        /// Why not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
    },
}

/// Why a `session.start` is a resumption (`session.start.resume`, docs/AGENT_PROTOCOL.md,
/// "Reconnecting").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// Which reconnection attempt of this outage this is (1 for the first).
    pub attempt: u32,
    /// The `seq` of the last message the server wrote on the previous connection. The `seq`
    /// of a resumed link continues from here, so it never repeats within a call.
    pub previous_seq: u64,
    /// Milliseconds since the previous connection was lost, on the server's clock.
    pub down_ms: u64,
}

/// Who is on the line (`session.start.call`): an addition within version 1, off unless the
/// agent definition opts in (`docs/AGENT_PROTOCOL.md`, "Caller identity").
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallIdentity {
    /// The carrier or transport: `twilio`, `telnyx` or `sip`.
    pub carrier: String,
    /// The carrier's id of the call: Twilio `CallSid`, Telnyx `call_control_id`, SIP `Call-ID`.
    pub carrier_call_id: String,
    /// The caller's number or SIP user part. Absent when the carrier did not say, and when
    /// the server's PII redaction covers phone numbers ([`Self::pii_redacted`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The called number or SIP user part, with the same absence rules as [`Self::from`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// `true`: `from` and `to` were withheld because the server redacts phone numbers
    /// (recording or log redaction). Absent means `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pii_redacted: bool,
}

/// The numbers are personal data and must not reach a log through `{:?}`: `from` and `to` show
/// only whether they are present.
impl std::fmt::Debug for CallIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let party = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("CallIdentity")
            .field("carrier", &self.carrier)
            .field("carrier_call_id", &self.carrier_call_id)
            .field("from", &party(&self.from))
            .field("to", &party(&self.to))
            .field("pii_redacted", &self.pii_redacted)
            .finish()
    }
}

/// What the server can do on a call (`session.start.capabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `transfer` is available (a carrier call-control is bound to the call).
    pub transfer: bool,
    /// Remote tools can be declared.
    pub tools: bool,
}

/// Turn latencies in milliseconds (`turn.metrics`); absent when not known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// The end-of-turn decision's wait after the user stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_of_turn_ms: Option<f64>,
    /// The STT final after the user stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_final_ms: Option<f64>,
    /// The LLM's first token after its request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_ttft_ms: Option<f64>,
    /// The TTS's first byte after its request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_ttfb_ms: Option<f64>,
    /// The first audio out after the user stopped (server-side voice-to-voice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_ms: Option<f64>,
}

/// How the engine runs a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireToolKind {
    /// No side effects: may run speculatively; cancelled on a barge-in or its timeout.
    ReadOnly,
    /// Changes something: never speculative, never run twice; not cancelled by a barge-in.
    SideEffect,
}

/// Why the server stopped waiting for a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The caller barged in.
    Interrupted,
    /// The result did not come within the wait.
    Timeout,
    /// The engine stopped waiting (its own timeout, or the end of the call).
    Abandoned,
}

/// An error of a command (`command.result.error`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    /// The machine-readable kind.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
}

/// The error codes of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The message is not JSON, or misses a field, or a field has the wrong type.
    BadMessage,
    /// The `type` is not one this version knows.
    UnknownType,
    /// A value breaks a rule (an empty or too long text, a bad phone number, ...).
    InvalidArgument,
    /// A bound was hit (too many tools, messages, bytes).
    TooLarge,
    /// The call's input queue is full: retry later.
    QueueFull,
    /// The call cannot do this (no call-control for `transfer`).
    Unsupported,
    /// The call is over.
    CallEnded,
    /// The command came before `session.accept`, or a second handshake answer.
    NotReady,
    /// The carrier refused the request; nothing changed.
    Rejected,
    /// The carrier could not be reached; nothing changed.
    Unavailable,
    /// The request may or may not have been applied (a timeout): do not repeat it blindly.
    OutcomeUnknown,
    /// None of the offered versions is spoken by the agent (sent by the agent in
    /// `session.reject`).
    UnsupportedVersion,
}

impl ErrorCode {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadMessage => "bad_message",
            Self::UnknownType => "unknown_type",
            Self::InvalidArgument => "invalid_argument",
            Self::TooLarge => "too_large",
            Self::QueueFull => "queue_full",
            Self::Unsupported => "unsupported",
            Self::CallEnded => "call_ended",
            Self::NotReady => "not_ready",
            Self::Rejected => "rejected",
            Self::Unavailable => "unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::UnsupportedVersion => "unsupported_version",
        }
    }
}

/// One message from the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentMessage {
    /// The answer to `session.start`: the agent takes the call.
    #[serde(rename = "session.accept")]
    SessionAccept(SessionAccept),
    /// The answer to `session.start`: the agent does not take the call; it runs on the agent
    /// definition alone.
    #[serde(rename = "session.reject")]
    SessionReject {
        /// Why ([`ErrorCode`] wire name or the agent's own).
        #[serde(default)]
        code: Option<String>,
        /// An explanation.
        #[serde(default)]
        message: Option<String>,
    },
    /// Speak a line verbatim (a bot turn; no LLM request).
    #[serde(rename = "say")]
    Say {
        /// Echoed in `command.result`.
        #[serde(default)]
        id: Option<String>,
        /// The `seq` of the event this reacts to.
        #[serde(default)]
        re: Option<u64>,
        /// The line.
        text: String,
        /// Cut a running response (and the lines waiting) for it.
        #[serde(default)]
        interrupt: bool,
    },
    /// End the call, after the bot finished speaking (and after `say`, when given).
    #[serde(rename = "end")]
    End {
        /// Echoed in `command.result`.
        #[serde(default)]
        id: Option<String>,
        /// The `seq` of the event this reacts to.
        #[serde(default)]
        re: Option<u64>,
        /// A farewell spoken first (a plain line).
        #[serde(default)]
        say: Option<String>,
        /// Why (for the logs).
        #[serde(default)]
        reason: Option<String>,
    },
    /// Transfer the caller through the carrier's call-control.
    #[serde(rename = "transfer")]
    Transfer {
        /// Echoed in `command.result`.
        #[serde(default)]
        id: Option<String>,
        /// The `seq` of the event this reacts to.
        #[serde(default)]
        re: Option<u64>,
        /// The E.164 number (`+` and 8 to 15 digits).
        destination: String,
        /// `cold` (default) or `warm`.
        #[serde(default)]
        mode: Option<WireTransferMode>,
        /// What the receiving party is told first (warm only).
        #[serde(default)]
        summary: Option<String>,
        /// Why (for the logs and the carrier's records).
        #[serde(default)]
        reason: Option<String>,
    },
    /// Change the conversation from the next response on.
    #[serde(rename = "context.update")]
    ContextUpdate {
        /// Echoed in `command.result`.
        #[serde(default)]
        id: Option<String>,
        /// The `seq` of the event this reacts to.
        #[serde(default)]
        re: Option<u64>,
        /// Replaces the system prompt.
        #[serde(default)]
        system_prompt: Option<String>,
        /// Messages appended to the conversation.
        #[serde(default)]
        append: Vec<WireMessage>,
    },
    /// Offer only these tools to the LLM from the next response on (`null`: all again).
    #[serde(rename = "tools.set")]
    ToolsSet {
        /// Echoed in `command.result`.
        #[serde(default)]
        id: Option<String>,
        /// The `seq` of the event this reacts to.
        #[serde(default)]
        re: Option<u64>,
        /// The tool names (local and remote); `null` or absent: every tool.
        #[serde(default)]
        enabled: Option<Vec<String>>,
    },
    /// The answer to a `tool.call`.
    #[serde(rename = "tool.result")]
    ToolResult {
        /// The `tool.call` id.
        id: u64,
        /// The result (any JSON; a string is given to the LLM as it is).
        #[serde(default)]
        result: Option<Value>,
        /// A failure, given to the LLM as a `tool_failed` error. Wins over `result`.
        #[serde(default)]
        error: Option<String>,
    },
}

/// The body of `session.accept`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionAccept {
    /// The version the agent picked from `session.start.versions`.
    pub version: u32,
    /// Tools the LLM may call; the server sends `tool.call` for them.
    #[serde(default)]
    pub tools: Vec<ToolDecl>,
    /// Replaces the agent definition's system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Messages the conversation starts with (after the system prompt).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<WireMessage>,
    /// A first line the bot speaks at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
    /// The event types the agent wants (absent: all). `session.*`, `tool.*`, `command.result`,
    /// `events.lost` and `error` are always sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    /// The agent keeps the call's state after its connection is lost and can take the call up
    /// again: after a failure the server reconnects with bounded backoff and sends a
    /// `session.start` carrying [`Resume`]. Absent or `false` (an agent written against
    /// version 1 as first specified): the link is not reopened.
    ///
    /// In the `session.accept` answering a resuming `session.start` this field, `tools`,
    /// `system_prompt`, `context` and `greeting` are ignored (the call keeps what it has); only
    /// `version` is required and `events` re-selects the subscription.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's signature
fn is_false(value: &bool) -> bool {
    !*value
}

/// A tool the agent declares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDecl {
    /// 1 to 64 ASCII letters, digits, `_` or `-`.
    pub name: String,
    /// What the tool does, for the LLM.
    pub description: String,
    /// The arguments: a JSON Schema object (`"type": "object"`).
    pub parameters: Value,
    /// `read_only` or `side_effect`.
    pub kind: WireToolKind,
    /// How long the server waits for the result (default 5000; clamped to the server's bounds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// After this long without a result the bot says a filler phrase (default 700; 0: never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filler_after_ms: Option<u64>,
    /// Hold the tool for the user's confirmation (`side_effect` tools only; default `false`):
    /// when the model calls it, it does not run; the model is told to ask the user, and the tool
    /// is called (a `tool.call`) only after the user's next reply confirms
    /// (`docs/TOOLS.md`, "Confirmation gate").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// An identical call of this `side_effect` tool (same arguments) may repeat in a later
    /// response of the same turn (default `false`): by default the engine refuses such a repeat
    /// as `already_done` once the first one succeeded. For a tool whose repetition is
    /// legitimate, such as pressing the same IVR digit twice (`docs/TOOLS.md`, "What never
    /// twice covers"). Declaring it on a `read_only` tool refuses the declaration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeatable: bool,
}

/// A conversation message of the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMessage {
    /// Who.
    pub role: WireRole,
    /// The text.
    pub text: String,
}

/// The roles an agent may add to the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireRole {
    /// Instructions or notes for the model.
    System,
    /// Words of the caller.
    User,
    /// Words of the bot (as if it had said them).
    Assistant,
}

/// Transfer modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTransferMode {
    /// Hand the call over at once.
    Cold,
    /// Brief the receiving party first.
    Warm,
}

/// The `type` and `id` of a message that did not parse, to answer it precisely.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Probe {
    #[serde(rename = "type", default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) id: Option<Value>,
}

/// The `type` names of [`AgentMessage`].
pub(crate) const AGENT_TYPES: &[&str] = &[
    "session.accept",
    "session.reject",
    "say",
    "end",
    "transfer",
    "context.update",
    "tools.set",
    "tool.result",
];

/// The event `type` names an agent may subscribe to (`session.accept.events`).
pub const EVENT_TYPES: &[&str] = &[
    "turn.user_started",
    "turn.user_ended",
    "turn.bot_started",
    "turn.bot_ended",
    "turn.interrupted",
    "turn.metrics",
    "user.transcript",
    "user.text",
    "bot.llm_started",
    "bot.text",
    "bot.llm_stopped",
    "bot.tts_text",
    "bot.tts_stopped",
];

impl ServerBody {
    /// The wire `type` of this body.
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "session.start",
            Self::SessionEnd { .. } => "session.end",
            Self::TurnUserStarted { .. } => "turn.user_started",
            Self::TurnUserEnded { .. } => "turn.user_ended",
            Self::TurnBotStarted { .. } => "turn.bot_started",
            Self::TurnBotEnded { .. } => "turn.bot_ended",
            Self::TurnInterrupted { .. } => "turn.interrupted",
            Self::TurnMetrics { .. } => "turn.metrics",
            Self::UserTranscript { .. } => "user.transcript",
            Self::UserText { .. } => "user.text",
            Self::BotLlmStarted { .. } => "bot.llm_started",
            Self::BotText { .. } => "bot.text",
            Self::BotLlmStopped { .. } => "bot.llm_stopped",
            Self::BotTtsText { .. } => "bot.tts_text",
            Self::BotTtsStopped { .. } => "bot.tts_stopped",
            Self::Error { .. } => "error",
            Self::EventsLost { .. } => "events.lost",
            Self::ToolCall { .. } => "tool.call",
            Self::ToolCancel { .. } => "tool.cancel",
            Self::CommandResult { .. } => "command.result",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn call_identity_is_an_optional_addition_to_session_start() {
        let start = |call: Option<CallIdentity>| ServerBody::SessionStart {
            protocol: PROTOCOL.to_owned(),
            versions: vec![1],
            session_id: "9".to_owned(),
            agent: "a".to_owned(),
            capabilities: Capabilities {
                transfer: true,
                tools: true,
            },
            resume: None,
            call,
        };
        // Absent unless given: byte for byte what version 1 specified.
        assert!(
            serde_json::to_value(start(None))
                .unwrap()
                .get("call")
                .is_none()
        );
        let identity = CallIdentity {
            carrier: "twilio".to_owned(),
            carrier_call_id: "CA1".to_owned(),
            from: Some("+15551230001".to_owned()),
            to: Some("+15551230002".to_owned()),
            pii_redacted: false,
        };
        // The numbers never show in `{:?}`, not even through the message that carries them.
        for text in [
            format!("{identity:?}"),
            format!("{:?}", start(Some(identity.clone()))),
        ] {
            assert!(!text.contains("15551230"), "{text}");
            assert!(
                text.contains("CA1") && text.contains("<redacted>"),
                "{text}"
            );
        }
        let value = serde_json::to_value(start(Some(identity.clone()))).unwrap();
        assert_eq!(
            value["call"],
            json!({"carrier": "twilio", "carrier_call_id": "CA1",
                   "from": "+15551230001", "to": "+15551230002"})
        );
        assert_eq!(
            serde_json::from_value::<ServerBody>(value).unwrap(),
            start(Some(identity))
        );
        // Withheld numbers: no from/to, the flag says why.
        let withheld = CallIdentity {
            carrier: "sip".to_owned(),
            carrier_call_id: "abc@host".to_owned(),
            pii_redacted: true,
            ..CallIdentity::default()
        };
        assert_eq!(
            serde_json::to_value(start(Some(withheld))).unwrap()["call"],
            json!({"carrier": "sip", "carrier_call_id": "abc@host", "pii_redacted": true})
        );
    }

    #[test]
    fn resumption_is_an_optional_addition_to_version_one() {
        let caps = Capabilities {
            transfer: false,
            tools: true,
        };
        let start = |resume: Option<Resume>| ServerBody::SessionStart {
            protocol: PROTOCOL.to_owned(),
            versions: vec![1],
            session_id: "9".to_owned(),
            agent: "a".to_owned(),
            capabilities: caps,
            resume,
            call: None,
        };
        let first = start(None);
        // The first start is byte for byte what version 1 specified: no `resume` key.
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!({"type": "session.start", "protocol": "brisvox.agent", "versions": [1],
                   "session_id": "9", "agent": "a",
                   "capabilities": {"transfer": false, "tools": true}})
        );
        // A resumption carries it; an agent that does not know the field ignores it.
        let again = start(Some(Resume {
            attempt: 2,
            previous_seq: 40,
            down_ms: 350,
        }));
        let value = serde_json::to_value(&again).unwrap();
        assert_eq!(
            value["resume"],
            json!({"attempt": 2, "previous_seq": 40, "down_ms": 350})
        );
        assert_eq!(serde_json::from_value::<ServerBody>(value).unwrap(), again);
        // A session.start of an older server parses (no `resume`).
        let old = json!({"type": "session.start", "protocol": "brisvox.agent", "versions": [1],
            "session_id": "9", "agent": "a", "capabilities": {"transfer": true, "tools": true}});
        let parsed: ServerBody = serde_json::from_value(old).unwrap();
        assert!(matches!(
            parsed,
            ServerBody::SessionStart { resume: None, .. }
        ));
        // The agent's side: `resume` is absent unless declared, and absent means false.
        let plain = serde_json::to_value(AgentMessage::SessionAccept(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        }))
        .unwrap();
        assert_eq!(
            plain,
            json!({"type": "session.accept", "version": 1, "tools": []})
        );
        let declared: AgentMessage =
            serde_json::from_value(json!({"type": "session.accept", "version": 1, "resume": true}))
                .unwrap();
        assert!(matches!(declared, AgentMessage::SessionAccept(a) if a.resume));
        let legacy: AgentMessage =
            serde_json::from_value(json!({"type": "session.accept", "version": 1})).unwrap();
        assert!(matches!(legacy, AgentMessage::SessionAccept(a) if !a.resume));
    }

    #[test]
    fn server_messages_carry_the_envelope_and_a_dotted_type() {
        let message = ServerMessage {
            seq: 7,
            t_ms: 1234,
            body: ServerBody::UserTranscript {
                text: Arc::from("hi"),
                is_final: true,
            },
        };
        let text = serde_json::to_string(&message).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            json!({"seq": 7, "t_ms": 1234, "type": "user.transcript", "text": "hi", "final": true})
        );
        let back: ServerMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(back, message);
        let result = ServerBody::CommandResult {
            id: "a".to_owned(),
            ok: true,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({"type": "command.result", "id": "a", "ok": true})
        );
    }

    #[test]
    fn agent_messages_parse_with_defaults_and_ignore_unknown_fields() {
        let say: AgentMessage =
            serde_json::from_str(r#"{"type":"say","text":"hello","future_field":1}"#).unwrap();
        assert_eq!(
            say,
            AgentMessage::Say {
                id: None,
                re: None,
                text: "hello".to_owned(),
                interrupt: false
            }
        );
        let accept: AgentMessage = serde_json::from_str(
            r#"{"type":"session.accept","version":1,"tools":[{"name":"lookup","description":"d",
            "parameters":{"type":"object"},"kind":"read_only","timeout_ms":2000}]}"#,
        )
        .unwrap();
        let AgentMessage::SessionAccept(accept) = accept else {
            panic!("not an accept");
        };
        assert_eq!(accept.tools.len(), 1);
        assert_eq!(accept.tools[0].kind, WireToolKind::ReadOnly);
        assert!(serde_json::from_str::<AgentMessage>(r#"{"type":"fly"}"#).is_err());
        assert!(serde_json::from_str::<AgentMessage>(r#"{"type":"say"}"#).is_err());
    }

    #[test]
    fn the_type_lists_match_the_enums() {
        for name in AGENT_TYPES {
            assert!(
                serde_json::from_str::<Probe>(&format!(r#"{{"type":"{name}"}}"#)).is_ok(),
                "{name}"
            );
        }
        let bodies = [
            ServerBody::TurnUserStarted { epoch: 0 },
            ServerBody::TurnUserEnded { epoch: 0 },
            ServerBody::TurnBotStarted { epoch: 0 },
            ServerBody::TurnBotEnded { epoch: 0 },
            ServerBody::TurnInterrupted { epoch: 0 },
            ServerBody::TurnMetrics {
                epoch: 0,
                metrics: Metrics::default(),
            },
            ServerBody::UserTranscript {
                text: Arc::from(""),
                is_final: false,
            },
            ServerBody::UserText {
                epoch: 0,
                text: Arc::from(""),
            },
            ServerBody::BotLlmStarted { epoch: 0 },
            ServerBody::BotText {
                epoch: 0,
                text: Arc::from(""),
            },
            ServerBody::BotLlmStopped { epoch: 0 },
            ServerBody::BotTtsText {
                epoch: 0,
                text: Arc::from(""),
            },
            ServerBody::BotTtsStopped { epoch: 0 },
        ];
        assert_eq!(bodies.len(), EVENT_TYPES.len());
        for (body, name) in bodies.iter().zip(EVENT_TYPES) {
            assert_eq!(body.type_name(), *name);
            assert_eq!(serde_json::to_value(body).unwrap()["type"], json!(name));
        }
        assert_eq!(
            serde_json::to_value(ErrorCode::OutcomeUnknown).unwrap(),
            json!(ErrorCode::OutcomeUnknown.as_str())
        );
    }
}
