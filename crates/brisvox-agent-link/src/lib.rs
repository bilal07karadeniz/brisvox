//! The out-of-process agent protocol: a call's conversation runs in brisvox, its
//! application logic in a separate agent process (any language) that talks to the call over one
//! WebSocket. `docs/AGENT_PROTOCOL.md` is the specification; this crate is the server side and
//! a reference agent.
//!
//! - [`protocol`]: the versioned JSON messages (events, commands, tool calls, error codes).
//! - [`attach`]: per call, the server connects out to the agent's URL ([`AgentLinkConfig`]),
//!   sends `session.start`, takes the agent's `session.accept` (tools, prompt, greeting) and
//!   wires the call: the call's [`EngineEvents`](brisvox_engine::EngineEvents) go out as events;
//!   the agent's commands come in through the engine's own inputs: `say` as a bot line
//!   ([`brisvox_engine::agent_input`]), `context.update` / `tools.set` as conversation edits,
//!   `transfer` through the call's [`TelephonyOps`](brisvox_agent::telephony::TelephonyOps),
//!   `end` through [`AgentLink::ended`]. The agent's tools become remote tools of the call's
//!   tool engine (a `tool.call` per execution, with timeouts and the engine's ReadOnly /
//!   SideEffect rules).
//! - [`client`]: the reference agent process in Rust (used by the tests).
//!
//! # Failure and fallback
//! The link never fails the call. If the agent cannot be reached, answers late, rejects the call
//! or breaks the protocol at the start, the call runs on its agent definition alone. If the link
//! fails later, the call goes on: remote tools answer with an error (a SideEffect call's outcome
//! is reported as unknown), events stop, the LLM keeps answering with the context it has. An
//! agent that declared `resume` in its `session.accept` is reconnected with bounded backoff
//! (`AgentLinkConfig::reconnect_attempts`): the new `session.start` carries
//! [`protocol::Resume`], the remote tools are offered again once the agent answers
//! (`docs/AGENT_PROTOCOL.md`, "Reconnecting").
//!
//! # Bounds
//! Every queue is bounded: events (drop-newest, reported as `events.lost`), control messages
//! (a tool call fails at once when full), the call's input queues (`queue_full`), the in-flight
//! remote calls. Every network write and the handshake have timeouts on the call's clock; a
//! silent agent is detected by the keepalive.

pub mod client;
mod link;
pub mod protocol;
pub mod security;
mod tool;

pub use link::{
    AgentLink, AgentLinkConfig, HOP_BUCKET_COUNT, HOP_BUCKETS_US, LinkCall, LinkError, LinkStats,
    attach,
};
