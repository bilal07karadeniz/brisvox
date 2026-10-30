//! The tools of an agent snapshot across a reload (`docs/OPERATIONS.md`, "Reloading the agent"),
//! with real MCP servers (the stdio test server of `tests/mcp_tools.rs`): the new definition's
//! servers are connected with the start's bounds before anything changes, the old definition's
//! servers stay up for the calls that run on it and are shut down when the last of those calls
//! ends, and a new definition whose server cannot be reached changes nothing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

#[path = "support/mcp_harness.rs"]
mod harness;
#[path = "support/reload_rig.rs"]
mod rig;

use std::path::Path;
use std::sync::{Arc, Weak};

use brisvox_server::mcp::{McpHost, McpLinkState};
use brisvox_server::reload::{ReloadResult, ReloadTrigger, SlotResult};

use harness::{agent_text, state_dir, stdio_entry};
use rig::{Rig, eventually};

/// A definition with one stdio MCP server whose state goes to `dir`.
fn with_server(name: &str, dir: &Path, extra_env: &str) -> String {
    let servers = stdio_entry(name, dir, "\"read_echo\"", "\"read_echo\"", "", extra_env);
    agent_text(&servers, "")
}

/// Whether the MCP connections behind `host` still exist and are up. They exist as long as a
/// call (or the live snapshot) owns their snapshot, and only closing them (or dropping the last
/// owner) makes them go: no waiting for a negative is needed.
fn is_up(host: &Weak<McpHost>) -> bool {
    host.upgrade()
        .is_some_and(|host| host.status().iter().all(|s| s.state == McpLinkState::Up))
}

fn lines(dir: &Path, file: &str) -> Vec<String> {
    rig::read(&dir.join(file))
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_definitions_mcp_server_closes_when_its_last_call_ends() {
    let old_dir = state_dir("reload-old");
    let new_dir = state_dir("reload-new");
    let rig = Rig::start("mcp", &with_server("old", &old_dir, "")).await;
    eventually("the first server to start", 20, || {
        lines(&old_dir, "starts.log").len() == 1
    })
    .await;

    let old_host = Arc::downgrade(&rig.swap.current().mcp_host().unwrap());

    // Two calls run on the old definition.
    let mut first = rig.ws_call().await;
    let second = rig.ws_call().await;
    rig.until("both calls run", |m| m.stats().active == 2).await;

    // The new definition names another server: it is connected (and its tools checked) before
    // the swap; the old one is not touched.
    rig.file.write(&with_server("new", &new_dir, ""));
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    assert_eq!(
        lines(&new_dir, "starts.log").len(),
        1,
        "the new server runs"
    );
    assert!(lines(&old_dir, "exits.log").is_empty());
    assert!(
        is_up(&old_host),
        "the old server must stay while calls run on its definition"
    );

    // One of the two calls ends: the other still holds the old definition.
    first.close(None).await.unwrap();
    rig.until("one call ended", |m| m.stats().active == 1).await;
    assert!(
        is_up(&old_host) && lines(&old_dir, "exits.log").is_empty(),
        "the old server must stay until the last call on it ends"
    );

    // A call started now runs the new definition; ending it does not touch the old server.
    let third = rig.ws_call().await;
    rig.until("the new call runs", |m| m.stats().active == 2)
        .await;
    drop(third);
    rig.until("the new call ended", |m| m.stats().active == 1)
        .await;
    assert!(lines(&old_dir, "exits.log").is_empty());
    assert!(lines(&new_dir, "exits.log").is_empty());

    // The last call on the old definition ends: now its server is shut down (stdin closed, the
    // process leaves by itself), and the new one is untouched.
    drop(second);
    rig.until("the last old call ended", |m| m.stats().active == 0)
        .await;
    eventually("the old snapshot to be released", 20, || {
        old_host.upgrade().is_none()
    })
    .await;
    eventually("the old server to be closed", 20, || {
        lines(&old_dir, "exits.log") == ["stdin closed"]
    })
    .await;
    assert!(lines(&new_dir, "exits.log").is_empty());
    assert_eq!(lines(&new_dir, "starts.log").len(), 1, "never restarted");

    // At the end of the process the live definition's server is closed as well.
    rig.hub.shutdown().await;
    eventually("the live server to be closed", 20, || {
        lines(&new_dir, "exits.log") == ["stdin closed"]
    })
    .await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_definition_whose_mcp_server_cannot_be_reached_changes_nothing() {
    let old_dir = state_dir("reload-keep-old");
    let bad_dir = state_dir("reload-keep-bad");
    let rig = Rig::start("mcp-bad", &with_server("old", &old_dir, "")).await;
    eventually("the server to start", 20, || {
        lines(&old_dir, "starts.log").len() == 1
    })
    .await;
    let old_host = Arc::downgrade(&rig.swap.current().mcp_host().unwrap());
    let call = rig.ws_call().await;
    rig.until("the call runs", |m| m.stats().active == 1).await;

    // The new definition's server exits at once: the handshake never succeeds.
    rig.file
        .write(&with_server("new", &bad_dir, ", MCP_TEST_MODE = \"exit\""));
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");
    assert_eq!(report.slots[0].result, SlotResult::Invalid);
    assert!(
        report.message.contains("cannot use the MCP server `new`"),
        "{}",
        report.message
    );
    // The position in the file is in the error, like at the start.
    assert!(report.message.contains("agent.toml"), "{}", report.message);

    // Nothing changed: same snapshot, the old server still up and still serving.
    assert_eq!(report.generation, 1);
    assert_eq!(rig.swap.current().agent().name(), "mcp-test");
    assert!(is_up(&old_host));
    assert!(lines(&old_dir, "exits.log").is_empty());
    assert_eq!(lines(&old_dir, "starts.log").len(), 1);

    // A tool the new server does not offer is the same kind of error (the allowlist is checked
    // against the listing).
    let servers = stdio_entry("new", &bad_dir, "\"no_such_tool\"", "", "", "");
    rig.file.write(&agent_text(&servers, ""));
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");
    assert!(
        report
            .message
            .contains("does not offer a tool `no_such_tool`"),
        "{}",
        report.message
    );
    // Its server was started for the check and closed again: nothing is left running.
    eventually("the rejected definition's server to be closed", 20, || {
        lines(&bad_dir, "exits.log")
            .iter()
            .any(|l| l == "stdin closed")
    })
    .await;
    assert!(lines(&old_dir, "exits.log").is_empty());

    drop(call);
    rig.until("the call ended", |m| m.stats().active == 0).await;
    rig.hub.shutdown().await;
    rig.finish().await;
}
