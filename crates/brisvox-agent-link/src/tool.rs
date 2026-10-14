//! Remote tools: a [`Tool`] whose execution is a `tool.call` to the agent process, answered by
//! its `tool.result`.
//!
//! The engine's rules apply unchanged (TOOLS.md): a `read_only` call is dropped when the
//! caller barges in or its timeout passes, and the link then sends `tool.cancel` (a late result
//! is ignored); a `side_effect` call is never dropped by the engine on a timeout or a barge-in:
//! the link keeps waiting up to `side_effect_wait`, the result then becomes a late note, and
//! after that wait (or when the link goes down) the outcome is reported as unknown. The
//! idempotency key of the call travels in `tool.call`, so the agent can deduplicate.

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::{
    Tool, ToolCallCtx, ToolError, ToolFuture, ToolKind, ToolOutput, ToolSpec,
};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::link::LinkShared;
use crate::protocol::{CancelReason, ServerBody, WireToolKind};

/// One declared tool of the agent process.
pub(crate) struct RemoteTool {
    shared: Arc<LinkShared>,
    name: Arc<str>,
    kind: ToolKind,
    timeout: Duration,
}

impl RemoteTool {
    pub(crate) fn new(shared: Arc<LinkShared>, spec: &ToolSpec) -> Self {
        Self {
            shared,
            name: Arc::clone(&spec.name),
            kind: spec.kind,
            timeout: spec.timeout,
        }
    }

    async fn run(&self, args: Value, ctx: ToolCallCtx) -> Result<ToolOutput, ToolError> {
        let shared = &self.shared;
        let side_effect = self.kind == ToolKind::SideEffect;
        if !shared.is_up() {
            shared.count_tool_refused();
            return Err(ToolError::new("the agent process is not connected"));
        }
        let id = shared.next_tool_id();
        let (reply, answer) = oneshot::channel();
        if !shared.add_pending(id, reply) {
            shared.count_tool_refused();
            return Err(ToolError::new(
                "too many tool calls are waiting for the agent",
            ));
        }
        let mut waiting = Waiting {
            shared,
            id,
            interrupted: ctx.cancel.clone(),
            sent: false,
            settled: false,
            timed_out: false,
        };
        let call = ServerBody::ToolCall {
            id,
            call_id: ctx.call.to_string(),
            name: self.name.to_string(),
            arguments: args,
            kind: if side_effect {
                WireToolKind::SideEffect
            } else {
                WireToolKind::ReadOnly
            },
            epoch: ctx.epoch.get(),
            idempotency_key: format!("{:016x}", ctx.idempotency_key().digest()),
            timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
        };
        if !shared.send_control(call) {
            shared.count_tool_refused();
            return Err(ToolError::new("the agent link is busy or down"));
        }
        waiting.sent = true;
        shared.count_tool_call();
        // A read-only call is also bounded by the engine (which drops this future); the own wait
        // is the backstop. A side-effect call outlives its slot (see the module docs).
        let wait = if side_effect {
            shared.config().side_effect_wait.max(self.timeout)
        } else {
            self.timeout
        };
        let outcome = tokio::select! {
            biased;
            answer = answer => answer.ok(),
            () = shared.not_up() => None,
            () = shared.clock.sleep(wait) => {
                waiting.timed_out = true;
                shared.count_tool_timeout();
                return Err(ToolError::new(if side_effect {
                    "the agent did not answer in time; the outcome is unknown"
                } else {
                    "the agent did not answer in time"
                }));
            }
        };
        waiting.settled = true;
        match outcome {
            Some(Ok(Value::String(text))) => Ok(ToolOutput::Text(text)),
            Some(Ok(value)) => Ok(ToolOutput::Json(value)),
            Some(Err(message)) => Err(ToolError::new(message)),
            None => Err(ToolError::new(if side_effect {
                "the agent link closed before the result; the outcome is unknown"
            } else {
                "the agent link closed before the result"
            })),
        }
    }
}

impl Tool for RemoteTool {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(self.run(args, ctx))
    }
}

/// A call waiting for its result. When the wait ends without one (the engine dropped the future,
/// or the own wait ran out) the call is forgotten and the agent is told (`tool.cancel`).
struct Waiting<'a> {
    shared: &'a LinkShared,
    id: u64,
    interrupted: CancellationToken,
    sent: bool,
    settled: bool,
    timed_out: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.shared.remove_pending(self.id);
        if !self.sent || self.settled || !self.shared.is_up() {
            return;
        }
        let reason = if self.timed_out {
            CancelReason::Timeout
        } else if self.interrupted.is_cancelled() {
            CancelReason::Interrupted
        } else {
            CancelReason::Abandoned
        };
        if self.shared.send_control(ServerBody::ToolCancel {
            id: self.id,
            reason,
        }) {
            self.shared.count_tool_cancel();
        }
    }
}
