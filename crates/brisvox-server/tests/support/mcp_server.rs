//! A small MCP server for the MCP tool tests, on rmcp's server side. Shared by the stdio
//! example (`examples/mcp_test_server.rs`, which the tests spawn) and the in-process streamable
//! HTTP server of `tests/mcp_tools.rs`.
//!
//! Its tools (what each one is for in the tests):
//! - `read_echo`: answers `echo: <text>` (a read-only tool).
//! - `files.read`: answers `contents of <path>` (a name with a dot).
//! - `meta_probe`: answers the idempotency key it found in the request's `_meta`.
//! - `book`: a tool with side effects; counts its runs in `effects.log` of the state directory,
//!   waits `delay_ms` first, and answers `booked #<n>`.
//! - `slow`: waits `ms` (default 30 s); records in `events.log` when it is cancelled.
//! - `fail`: answers an error result (`isError`).
//! - `protocol_error`: answers a JSON-RPC error.
//! - `crash`: exits the process without answering.
//! - `env_names`: answers the names of the environment variables the process has.
//! - `structured`: answers structured content.
//! - `hidden`: a tool the tests leave out of the allowlist.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Clock, SystemClock};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelledNotificationParam,
    ContentBlock, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

/// The test server's handler.
#[derive(Clone, Debug, Default)]
pub struct TestServer {
    /// Where `effects.log` and `events.log` go (none: nowhere).
    pub dir: Option<PathBuf>,
    /// List the tools this many per page (none: all at once).
    pub page_size: Option<usize>,
    /// Also list `bloat`, a tool whose input schema is far too large.
    pub bloat: bool,
}

fn schema(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => Arc::new(JsonObject::new()),
    }
}

fn text(s: impl Into<String>) -> Vec<ContentBlock> {
    vec![ContentBlock::text(s)]
}

/// Appends a line to `dir/file`.
pub fn append(dir: &Path, file: &str, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file))
        .unwrap();
    // One write, so two tasks appending at once cannot interleave their lines.
    f.write_all(format!("{line}\n").as_bytes()).unwrap();
}

/// The lines of `dir/file` (none if it does not exist).
pub fn lines(dir: &Path, file: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(file))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

impl TestServer {
    fn log(&self, file: &str, line: &str) {
        if let Some(dir) = &self.dir {
            append(dir, file, line);
        }
    }

    /// Every tool, in a fixed order.
    pub fn tools() -> Vec<Tool> {
        let no_args = json!({"type": "object", "properties": {}, "additionalProperties": false});
        vec![
            Tool::new(
                "read_echo",
                "Echoes the text back.",
                schema(json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                })),
            ),
            Tool::new(
                "files.read",
                "Reads a file.",
                schema(json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                })),
            ),
            Tool::new(
                "meta_probe",
                "Reports the idempotency key it was called with.",
                schema(no_args.clone()),
            ),
            Tool::new(
                "book",
                "Books something (a side effect).",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "what": {"type": "string"},
                        "delay_ms": {"type": "integer", "minimum": 0}
                    },
                    "required": ["what"]
                })),
            ),
            Tool::new(
                "slow",
                "Waits.",
                schema(json!({
                    "type": "object",
                    "properties": {"ms": {"type": "integer", "minimum": 0}}
                })),
            ),
            Tool::new("fail", "Always reports an error.", schema(no_args.clone())),
            Tool::new(
                "protocol_error",
                "Always answers a JSON-RPC error.",
                schema(no_args.clone()),
            ),
            Tool::new("crash", "Exits the process.", schema(no_args.clone())),
            Tool::new(
                "env_names",
                "Lists the environment variable names.",
                schema(no_args.clone()),
            ),
            Tool::new(
                "structured",
                "Answers structured content.",
                schema(no_args.clone()),
            ),
            Tool::new("hidden", "Not in the test allowlist.", schema(no_args)),
        ]
    }

    fn number_of(args: &JsonObject, key: &str, default: u64) -> u64 {
        args.get(key).and_then(Value::as_u64).unwrap_or(default)
    }
}

impl ServerHandler for TestServer {
    #[allow(deprecated)]
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("brisvox-mcp-test-server", "1.0.0"))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut all = Self::tools();
        if self.bloat {
            let properties: serde_json::Map<String, Value> = (0..400)
                .map(|i| {
                    (
                        format!("property_{i}"),
                        json!({"type": "string", "description": "x".repeat(60)}),
                    )
                })
                .collect();
            all.push(Tool::new(
                "bloat",
                "A schema nobody should send to a model.",
                schema(json!({"type": "object", "properties": properties})),
            ));
        }
        let Some(size) = self.page_size else {
            return Ok(ListToolsResult::with_all_items(all));
        };
        let start: usize = request
            .and_then(|r| r.cursor)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let end = (start + size).min(all.len());
        let mut result = ListToolsResult::with_all_items(all[start..end].to_vec());
        if end < all.len() {
            result.next_cursor = Some(end.to_string());
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.clone().unwrap_or_default();
        let result = match &*request.name {
            "read_echo" => {
                let t = args.get("text").and_then(Value::as_str).unwrap_or("");
                CallToolResult::success(text(format!("echo: {t}")))
            }
            "files.read" => {
                let p = args.get("path").and_then(Value::as_str).unwrap_or("");
                CallToolResult::success(text(format!("contents of {p}")))
            }
            "meta_probe" => {
                let key = request
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("io.brisvox/idempotency_key"))
                    .or_else(|| context.meta.get("io.brisvox/idempotency_key"))
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    .to_owned();
                CallToolResult::success(text(key))
            }
            "book" => {
                self.log("effects.log", &Value::Object(args.clone()).to_string());
                let n = self
                    .dir
                    .as_ref()
                    .map_or(0, |d| lines(d, "effects.log").len());
                let delay = Self::number_of(&args, "delay_ms", 0);
                if delay > 0 {
                    SystemClock.sleep(Duration::from_millis(delay)).await;
                }
                CallToolResult::success(text(format!("booked #{n}")))
            }
            "slow" => {
                let ms = Self::number_of(&args, "ms", 30_000);
                tokio::select! {
                    () = SystemClock.sleep(Duration::from_millis(ms)) => {
                        self.log("events.log", "slow finished");
                        CallToolResult::success(text("slow done"))
                    }
                    () = context.ct.cancelled() => {
                        self.log("events.log", "slow cancelled");
                        CallToolResult::error(text("cancelled"))
                    }
                }
            }
            "fail" => CallToolResult::error(text("boom: the order id is unknown")),
            "protocol_error" => {
                return Err(ErrorData::invalid_params(
                    "the server does not like this",
                    None,
                ));
            }
            "crash" => {
                self.log("events.log", "crashing");
                std::process::exit(3);
            }
            "env_names" => {
                let mut names: Vec<String> = std::env::vars_os()
                    .map(|(k, _)| k.to_string_lossy().into_owned())
                    .collect();
                names.sort();
                CallToolResult::success(text(names.join(",")))
            }
            "structured" => {
                let mut r = CallToolResult::success(text("{\"n\":3}"));
                r.structured_content = Some(json!({"n": 3, "items": ["a", "b"]}));
                r
            }
            "hidden" => {
                self.log("events.log", "hidden ran");
                CallToolResult::success(text("hidden ran"))
            }
            other => {
                return Err(ErrorData::invalid_params(
                    Cow::Owned(format!("unknown tool {other}")),
                    None,
                ));
            }
        };
        Ok(result.into())
    }

    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        self.log(
            "events.log",
            &format!("notifications/cancelled {:?}", notification.request_id),
        );
    }
}

/// The answer of a minimal server of the session era (protocol 2025-11-25), written by hand
/// because the SDK's own server always answers `server/discover`: no `server/discover` (a
/// "method not found" error, as old servers answer), `initialize`, `tools/list` and
/// `tools/call` of `read_echo`. `None` for a notification (nothing to answer).
pub fn legacy_reply(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let error = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}});
    let result = |result: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": result});
    Some(match method {
        "initialize" => result(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "legacy-test-server", "version": "0.1.0"}
        })),
        "tools/list" => {
            let tools: Vec<Value> = TestServer::tools()
                .iter()
                .filter(|t| t.name == "read_echo")
                .map(|t| serde_json::to_value(t).unwrap())
                .collect();
            result(json!({"tools": tools}))
        }
        "tools/call" => {
            let params = request.get("params");
            let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
            let text = params
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if name == Some("read_echo") {
                result(json!({"content": [{"type": "text", "text": format!("echo: {text}")}]}))
            } else {
                error(-32602, "unknown tool")
            }
        }
        _ => error(-32601, "Method not found"),
    })
}
