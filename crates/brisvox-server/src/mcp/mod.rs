//! MCP tools for agents (`docs/TOOLS.md`, "MCP tools"; cargo feature `mcp`).
//!
//! The agent file's `[[mcp.servers]]` name MCP (Model Context Protocol) servers, reached over
//! stdio (a program the server starts) or streamable HTTP. At startup [`McpHost::connect`]
//! connects to each, lists its tools, checks the file's allowlist against them and registers
//! each allowed tool as an ordinary [`Tool`](brisvox_agent::tools::Tool) of the server's registry
//! ([`ToolSetup::with_mcp`](crate::ToolSetup::with_mcp)). From there on a tool is like any
//! other: schema-checked arguments, the engine's timeout, the idempotency key, speculation for
//! ReadOnly tools only, the confirmation gate.
//!
//! # What is trusted
//! Nothing a server says about itself. The operator's file decides which tools exist (the
//! allowlist), which of them only read (`read_only`; every other tool is treated as having side
//! effects, whatever its `readOnlyHint` says, which is only logged) and which need the user's
//! confirmation. A server's descriptions and results are untrusted text for the LLM, as any
//! tool's result is; descriptions are cut at 1000 characters and results at 60 KiB.
//!
//! # Lifecycle
//! The connections are made once, shared by every call. A stdio server that exits is restarted
//! with a growing delay (at most 10 attempts in a row); calls made meanwhile fail at once with a
//! `tool_failed` result instead of waiting. [`McpHost::shutdown`] closes every connection: a
//! stdio server's standard input is closed, the process waited for, then killed.
//!
//! The child process gets the environment its entry sets plus `PATH` (and, on Windows, what the
//! system needs to start a program), not the server's own: the provider keys stay out of it.

mod convert;
mod server;
#[cfg(test)]
mod tests;
mod tool;

use std::sync::{Arc, Mutex, PoisonError};

use brisvox_agent::tools::{ToolKind, ToolRegistry, ToolSpec};
use brisvox_core::SharedClock;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{AgentConfig, McpServerSettings};
use crate::{Error, Result};

pub use server::McpLinkState;
pub use tool::META_IDEMPOTENCY_KEY;

use server::{CLOSE_TIMEOUT, Opened, Server};
use tool::McpTool;

/// Largest input schema of a tool the agent accepts, as compact JSON.
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024;

/// The MCP connections of a server: one per `[[mcp.servers]]` entry.
pub struct McpHost {
    servers: Vec<Arc<Server>>,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    clock: SharedClock,
}

impl std::fmt::Debug for McpHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHost")
            .field(
                "servers",
                &self.servers.iter().map(|s| &*s.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The state of one MCP server (see [`McpHost::status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpServerStatus {
    /// The server's name in the file.
    pub name: String,
    /// Whether it is connected.
    pub state: McpLinkState,
    /// How many times it was reconnected after losing the connection.
    pub restarts: u64,
}

impl McpHost {
    /// Connects to every `[[mcp.servers]]` of `agent` (concurrently, each within its startup
    /// timeout), checks each allowlist against the tools the server lists, and registers the
    /// allowed tools into `registry` under the names the LLM will see.
    ///
    /// Nothing is left running when this fails: the connections made so far are closed.
    ///
    /// # Errors
    /// [`Error::Config`] with the position in the file of the server that cannot be reached, of
    /// a tool its server does not offer or whose schema cannot be used, or of a name another tool
    /// already has.
    pub async fn connect(
        agent: &AgentConfig,
        clock: &SharedClock,
        registry: &mut ToolRegistry,
    ) -> Result<Self> {
        let settings = &agent.mcp().servers;
        let servers: Vec<Arc<Server>> = settings
            .iter()
            .map(|s| Arc::new(Server::new(s, clock.clone())))
            .collect();
        let opened = futures::future::join_all(servers.iter().map(|s| s.open())).await;
        let mut ready: Vec<(Arc<Server>, &McpServerSettings, Opened)> = Vec::new();
        let mut failure: Option<Error> = None;
        for ((server, settings), opened) in servers.iter().zip(settings).zip(opened) {
            match opened {
                Ok(opened) => ready.push((Arc::clone(server), settings, opened)),
                Err(reason) if failure.is_none() => {
                    failure = Some(
                        agent
                            .error_at(
                                &settings.key,
                                settings.span.clone(),
                                format!("cannot use the MCP server `{}`: {reason}", settings.name),
                            )
                            .into(),
                    );
                }
                Err(_) => {}
            }
        }
        // Register the tools of the servers that came up; the first problem stops the start.
        let mut registered: Vec<(ToolSpec, McpTool)> = Vec::new();
        if failure.is_none() {
            for (server, settings, opened) in &ready {
                match tools_of(agent, server, settings, &opened.tools, clock) {
                    Ok(mut tools) => registered.append(&mut tools),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        if failure.is_none() {
            for (spec, tool) in registered {
                let name = spec.name.to_string();
                if let Err(error) = registry.register(spec, tool) {
                    failure = Some(match find_tool(agent, &name) {
                        Some((key, span)) => agent.error_at(&key, span, error.to_string()).into(),
                        None => Error::Config(format!("MCP tool `{name}`: {error}")),
                    });
                    break;
                }
            }
        }
        if let Some(error) = failure {
            // Dropping a service closes its connection (stdin, then the child).
            drop(ready);
            return Err(error);
        }
        let cancel = CancellationToken::new();
        let mut tasks = Vec::with_capacity(ready.len());
        for (server, settings, opened) in ready {
            tracing::info!(
                server = %settings.name,
                tools = ?settings.tools.iter().map(|t| t.exposed.as_str()).collect::<Vec<_>>(),
                offered = opened.tools.len(),
                "MCP server connected"
            );
            server.mark_up(&opened.service);
            tasks.push(tokio::spawn(
                server.supervise(opened.service, cancel.child_token()),
            ));
        }
        Ok(Self {
            servers,
            cancel,
            tasks: Mutex::new(tasks),
            clock: clock.clone(),
        })
    }

    /// The state of each server, in file order.
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.servers
            .iter()
            .map(|s| McpServerStatus {
                name: s.name.to_string(),
                state: s.state(),
                restarts: s.restarts(),
            })
            .collect()
    }

    /// Closes every connection: a stdio server's standard input is closed, the process waited
    /// for (a few seconds), then killed. Calls fail at once afterwards. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            let abort = task.abort_handle();
            tokio::select! {
                _ = task => {}
                () = self.clock.sleep(CLOSE_TIMEOUT * 2) => {
                    tracing::warn!("an MCP connection did not close in time; aborting it");
                    abort.abort();
                }
            }
        }
    }
}

impl Drop for McpHost {
    fn drop(&mut self) {
        // The supervisors close their connections when the token fires; without a runtime to
        // run them, the services drop and the children are killed.
        self.cancel.cancel();
    }
}

/// The key path and position of the allowlist entry whose exposed name is `exposed`.
fn find_tool(agent: &AgentConfig, exposed: &str) -> Option<(String, std::ops::Range<usize>)> {
    agent
        .mcp()
        .servers
        .iter()
        .flat_map(|s| s.tools.iter())
        .find(|t| t.exposed == exposed)
        .map(|t| (t.key.clone(), t.span.clone()))
}

/// The specs and tools of one server's allowlist, built from what it lists.
fn tools_of(
    agent: &AgentConfig,
    server: &Arc<Server>,
    settings: &McpServerSettings,
    listed: &[rmcp::model::Tool],
    clock: &SharedClock,
) -> Result<Vec<(ToolSpec, McpTool)>> {
    let mut out = Vec::with_capacity(settings.tools.len());
    for entry in &settings.tools {
        let Some(tool) = listed.iter().find(|t| t.name == entry.name.as_str()) else {
            let mut offered: Vec<&str> = listed.iter().map(|t| &*t.name).collect();
            offered.sort_unstable();
            offered.truncate(30);
            return Err(agent
                .error_at(
                    &entry.key,
                    entry.span.clone(),
                    format!(
                        "the MCP server `{}` does not offer a tool `{}` (it offers: {})",
                        settings.name,
                        entry.name,
                        if offered.is_empty() {
                            "none".to_owned()
                        } else {
                            offered.join(", ")
                        }
                    ),
                )
                .into());
        };
        // The schema goes to the LLM with every request: an untrusted party must not be able to
        // make every round of every call carry a megabyte.
        let schema_bytes =
            serde_json::to_string(&*tool.input_schema).map_or(usize::MAX, |s| s.len());
        if schema_bytes > MAX_SCHEMA_BYTES {
            return Err(agent
                .error_at(
                    &entry.key,
                    entry.span.clone(),
                    format!(
                        "the input schema of the tool `{}` of the MCP server `{}` is {schema_bytes} bytes, more than the {MAX_SCHEMA_BYTES} the agent accepts; leave the tool out of the allowlist",
                        entry.name, settings.name
                    ),
                )
                .into());
        }
        // The server's annotations are hints from an untrusted party: for the record only.
        tracing::debug!(
            server = %settings.name,
            tool = %entry.name,
            read_only_hint = ?tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            destructive_hint = ?tool.annotations.as_ref().and_then(|a| a.destructive_hint),
            idempotent_hint = ?tool.annotations.as_ref().and_then(|a| a.idempotent_hint),
            configured_read_only = entry.read_only,
            "MCP tool annotations (not trusted)"
        );
        let kind = if entry.read_only {
            ToolKind::ReadOnly
        } else {
            ToolKind::SideEffect
        };
        let description = tool
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .or(tool.title.as_deref())
            .unwrap_or(&entry.name);
        let mut spec = ToolSpec::new(
            entry.exposed.as_str(),
            convert::description(description),
            Value::Object((*tool.input_schema).clone()),
            kind,
        )
        .with_timeout(settings.timeout)
        .with_confirm(entry.confirm);
        if let Some(filler) = settings.filler_after {
            spec = spec.with_filler_after(Some(filler));
        }
        let mcp = McpTool::new(
            Arc::clone(server),
            entry.name.clone(),
            kind == ToolKind::SideEffect,
            settings.timeout,
            clock.clone(),
        );
        out.push((spec, mcp));
    }
    Ok(out)
}
