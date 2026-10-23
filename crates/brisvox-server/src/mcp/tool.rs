//! An MCP tool as a [`Tool`] of the server's registry: `Tool::call` is a `tools/call` request.
//!
//! The engine's rules hold unchanged (`docs/TOOLS.md`): arguments are validated against the
//! tool's schema before the call is made, a ReadOnly call is dropped at its timeout or at a
//! barge-in (dropping the request sends `notifications/cancelled`), a SideEffect call is never
//! dropped by the engine and its outcome after the request left is reported as unknown when the
//! connection fails or the wait runs out, never retried.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::{Tool, ToolCallCtx, ToolError, ToolFuture, ToolOutput};
use brisvox_core::SharedClock;
use rmcp::ServiceError;
use rmcp::model::{CallToolRequestParams, CallToolResponse, RequestMetaObject};
use serde_json::Value;

use super::convert;
use super::server::Server;

/// The `_meta` key that carries the engine's idempotency key of the call, so a server can
/// deduplicate a redelivered call.
pub const META_IDEMPOTENCY_KEY: &str = "io.brisvox/idempotency_key";

/// How long a SideEffect call is waited for after its slot timed out (the engine answers the
/// caller at the tool's timeout and keeps the future; its result then becomes a late note). Under
/// the engine's own backstop (120 s).
pub(crate) const SIDE_EFFECT_WAIT: Duration = Duration::from_secs(60);

/// How much longer than the tool's timeout a ReadOnly call waits by itself (see `run`).
const READ_ONLY_GRACE: Duration = Duration::from_millis(500);

/// One allowlisted tool of an MCP server.
pub(crate) struct McpTool {
    server: Arc<Server>,
    /// The server's own name of the tool.
    mcp_name: String,
    side_effect: bool,
    timeout: Duration,
    clock: SharedClock,
}

impl McpTool {
    pub(crate) fn new(
        server: Arc<Server>,
        mcp_name: String,
        side_effect: bool,
        timeout: Duration,
        clock: SharedClock,
    ) -> Self {
        Self {
            server,
            mcp_name,
            side_effect,
            timeout,
            clock,
        }
    }

    async fn run(&self, args: Value, ctx: ToolCallCtx) -> Result<ToolOutput, ToolError> {
        let server = &self.server;
        let Value::Object(arguments) = args else {
            return Err(ToolError::new("the arguments must be a JSON object"));
        };
        let Some(peer) = server.peer() else {
            // Nothing was sent: not an unknown outcome, even for a SideEffect tool.
            return Err(ToolError::new(format!(
                "the MCP server `{}` is not connected right now (it is restarting or down); \
                 the call was not made",
                server.name
            )));
        };
        // At most `max_in_flight` calls to this server at once, over every call of the process.
        // A call over the limit is refused before anything is sent, so it is safe to say it did
        // not run, for a SideEffect tool too. The permit lives as long as the request does.
        let (permits, limit) = server.in_flight();
        let Ok(_permit) = Arc::clone(permits).try_acquire_owned() else {
            tracing::warn!(server = %server.name, limit, "an MCP call was refused: too many in flight");
            return Err(ToolError::new(format!(
                "the MCP server `{}` is busy ({limit} calls in flight); the call was not made, \
                 try again in a moment",
                server.name
            )));
        };
        let mut params =
            CallToolRequestParams::new(Cow::Owned(self.mcp_name.clone())).with_arguments(arguments);
        let mut meta = RequestMetaObject::new();
        meta.insert(
            META_IDEMPOTENCY_KEY.to_owned(),
            Value::String(format!("{:016x}", ctx.idempotency_key().digest())),
        );
        params.meta = Some(meta);
        // A ReadOnly call is dropped by the engine at its timeout (answering `timeout`); the own
        // wait, a little longer, is the backstop for a caller that does not drop it.
        let wait = if self.side_effect {
            SIDE_EFFECT_WAIT.max(self.timeout)
        } else {
            self.timeout + READ_ONLY_GRACE
        };
        // Dropping the request future (a timeout, or the engine dropping a ReadOnly call)
        // sends `notifications/cancelled`; a late answer is ignored.
        let outcome = tokio::select! {
            biased;
            outcome = peer.call_tool_once(params) => outcome,
            () = self.clock.sleep(wait) => {
                tracing::warn!(server = %server.name, tool = %self.mcp_name, "an MCP call timed out");
                return Err(ToolError::new(self.unknown_or(format!(
                    "the MCP server `{}` did not answer in time",
                    server.name
                ))));
            }
        };
        match outcome {
            Ok(CallToolResponse::Complete(result)) => convert::tool_output(result),
            Ok(CallToolResponse::InputRequired(_)) => Err(ToolError::new(format!(
                "the MCP server `{}` asked for more input (elicitation or sampling), which this \
                 agent cannot give",
                server.name
            ))),
            Ok(_) => Err(ToolError::new(format!(
                "the MCP server `{}` answered with a task, which this agent does not support",
                server.name
            ))),
            Err(error) => Err(self.service_error(&error)),
        }
    }

    /// `message`, with the note that the outcome is unknown for a tool that has side effects.
    fn unknown_or(&self, message: String) -> String {
        if self.side_effect {
            format!("{message}; the outcome is unknown")
        } else {
            message
        }
    }

    /// The tool error for a failed request.
    fn service_error(&self, error: &ServiceError) -> ToolError {
        let server = &self.server.name;
        match error {
            // The server answered with a protocol error: it did not run the tool.
            ServiceError::McpError(data) => {
                let message: String = data.message.chars().take(300).collect();
                ToolError::new(format!(
                    "the MCP server `{server}` rejected the call ({}): {message}",
                    data.code.0
                ))
            }
            // The request may or may not have reached the server before the connection ended.
            _ => ToolError::new(self.unknown_or(format!(
                "the connection to the MCP server `{server}` failed during the call"
            ))),
        }
    }
}

impl Tool for McpTool {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(self.run(args, ctx))
    }
}
