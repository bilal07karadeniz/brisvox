//! A stdio MCP server for the MCP tool tests (`tests/mcp_tools.rs` starts it): the tools of
//! `tests/support/mcp_server.rs`. Environment:
//! - `MCP_TEST_STATE_DIR`: where it records its starts (`starts.log`), exits (`exits.log`), the
//!   runs of its side-effect tool (`effects.log`) and other events (`events.log`).
//! - `MCP_TEST_RESTART_DELAY_MS`: how long every start after the first waits before serving (to
//!   make a restart observable).
//! - `MCP_TEST_PAGE_SIZE`: list the tools this many per page.
//! - `MCP_TEST_BLOAT`: also list `bloat`, a tool with a far too large input schema.
//! - `MCP_TEST_MODE`: `exit` (exit at once), `silent` (never answer) or `legacy` (a server of the
//!   session era: no `server/discover`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::indexing_slicing
)]
// a test fixture

#[path = "../tests/support/mcp_server.rs"]
mod mcp_server;

use std::path::PathBuf;
use std::time::Duration;

use brisvox_core::{Clock, SystemClock};
use rmcp::ServiceExt as _;

#[tokio::main]
async fn main() {
    let dir = std::env::var_os("MCP_TEST_STATE_DIR").map(PathBuf::from);
    if let Some(dir) = &dir {
        mcp_server::append(dir, "starts.log", "start");
        let starts = mcp_server::lines(dir, "starts.log").len();
        let delay = std::env::var("MCP_TEST_RESTART_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        if starts > 1
            && let Some(ms) = delay
        {
            SystemClock.sleep(Duration::from_millis(ms)).await;
        }
    }
    match std::env::var("MCP_TEST_MODE").as_deref() {
        // Exits at once: the handshake can never succeed.
        Ok("exit") => std::process::exit(1),
        // Never answers: the handshake runs into the startup timeout.
        Ok("silent") => std::future::pending::<()>().await,
        _ => {}
    }
    if std::env::var("MCP_TEST_MODE").as_deref() == Ok("legacy") {
        return legacy_stdio().await;
    }
    let server = mcp_server::TestServer {
        dir: dir.clone(),
        page_size: std::env::var("MCP_TEST_PAGE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok()),
        bloat: std::env::var("MCP_TEST_BLOAT").is_ok(),
    };
    match server.serve(rmcp::transport::stdio()).await {
        Ok(service) => {
            let _ = service.waiting().await;
            if let Some(dir) = &dir {
                mcp_server::append(dir, "exits.log", "stdin closed");
            }
        }
        Err(e) => eprintln!("mcp_test_server: {e}"),
    }
}

/// A server of the session era over stdio, by hand (see `mcp_server::legacy_reply`).
async fn legacy_stdio() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(reply) = mcp_server::legacy_reply(&request) {
            let mut text = reply.to_string();
            text.push('\n');
            if out.write_all(text.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    }
}
