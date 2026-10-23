//! MCP tools (`[[mcp.servers]]`, `docs/TOOLS.md` "MCP tools") against real MCP servers: a stdio
//! server (`examples/mcp_test_server.rs`, a child process) and an in-process streamable HTTP
//! server, both on the SDK's server side. The calls go through the real `ToolEngine`, so the
//! engine's guarantees are the ones tested: kinds, schema checks, speculation, "never twice",
//! timeouts, cancellation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

#[path = "support/mcp_harness.rs"]
mod harness;
#[path = "support/mcp_server.rs"]
mod mcp_server;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use brisvox_agent::Assembled;
use brisvox_agent::context::{Message, ToolCall, ToolCallId};
use brisvox_agent::tools::{
    EngineConfig, Execution, IdempotencyKey, SessionId, ToolEngine, ToolKind, ToolRegistry,
};
use brisvox_core::{Clock, SharedClock, SystemClock, TurnEpoch};
use brisvox_server::ToolSetup;
use brisvox_server::config::AgentConfig;
use brisvox_server::mcp::{McpHost, McpLinkState};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use harness::{agent_text, eventually, load, state_dir, stdio_entry};
use mcp_server::{TestServer, lines};

/// Real time elapsed since `started`.
fn since(started: Instant) -> Duration {
    SystemClock.now().saturating_duration_since(started)
}

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

/// A complete tool call `seq` of turn `turn`.
fn call(turn: u32, seq: u32, name: &str, arguments: Value) -> Assembled {
    Assembled::Complete(ToolCall {
        id: ToolCallId::new(epoch(turn), seq),
        provider_id: Some(Arc::from(format!("provider_{turn}_{seq}"))),
        name: Arc::from(name),
        arguments,
        provider_data: None,
    })
}

/// The agent's MCP tools, connected, and an engine over them.
struct Rig {
    host: Arc<McpHost>,
    registry: Arc<ToolRegistry>,
    engine: ToolEngine,
}

impl Rig {
    async fn start(agent: &AgentConfig) -> Self {
        Self::try_start(agent).await.unwrap()
    }

    async fn try_start(agent: &AgentConfig) -> Result<Self, brisvox_server::Error> {
        let clock = clock();
        let setup = ToolSetup::none().with_mcp(agent, &clock).await?;
        let host = setup.mcp_host().expect("the agent has MCP servers");
        let registry = Arc::new(setup.app().filtered(|_| true));
        let engine = ToolEngine::new(
            Arc::clone(&registry),
            SessionId::new(7),
            clock,
            EngineConfig::default(),
        );
        Ok(Self {
            host,
            registry,
            engine,
        })
    }

    /// Runs one call of turn `turn` to its result.
    async fn run(&self, turn: u32, name: &str, arguments: Value) -> Execution {
        let cancel = CancellationToken::new();
        let mut execution = tokio::time::timeout(
            Duration::from_secs(20),
            self.engine
                .execute(vec![call(turn, 0, name, arguments)], epoch(turn), &cancel),
        )
        .await
        .expect("the engine must not hang");
        execution.mark_delivered();
        execution
    }
}

/// (is_error, JSON body or text) of the `index`-th result.
fn result(execution: &Execution, index: usize) -> (bool, Value) {
    match &execution.results[index] {
        Message::Tool {
            content, is_error, ..
        } => (
            *is_error,
            serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.to_string())),
        ),
        other => panic!("not a tool result: {other:?}"),
    }
}

/// The text of the first (only) result.
fn text(execution: &Execution) -> String {
    match &execution.results[0] {
        Message::Tool { content, .. } => content.to_string(),
        other => panic!("not a tool result: {other:?}"),
    }
}

fn error_kind(execution: &Execution) -> String {
    let (is_error, body) = result(execution, 0);
    assert!(is_error, "an error was expected: {body}");
    body["error"].as_str().unwrap().to_owned()
}

fn error_message(execution: &Execution) -> String {
    let (_, body) = result(execution, 0);
    body["detail"].as_str().unwrap_or_default().to_owned()
}

const ALL_TOOLS: &str = r#""read_echo", "files.read", "meta_probe", "book", "slow", "fail", "protocol_error", "crash", "env_names", "structured""#;
const READ_ONLY: &str =
    r#""read_echo", "files.read", "meta_probe", "slow", "fail", "env_names", "structured""#;

/// A rig over the stdio test server with every tool except `hidden` allowed.
async fn stdio_rig(tag: &str, extra: &str, env_extra: &str) -> (Rig, std::path::PathBuf) {
    let dir = state_dir(tag);
    let servers = stdio_entry("test", &dir, ALL_TOOLS, READ_ONLY, extra, env_extra);
    let agent = load(&agent_text(&servers, ""), &[]).unwrap();
    (Rig::start(&agent).await, dir)
}

// ---------------------------------------------------------------------------------------------
// The allowlist and the kinds

#[tokio::test]
async fn the_allowlist_decides_which_tools_exist_and_the_file_decides_their_kind() {
    let (rig, dir) = stdio_rig(
        "allowlist",
        "confirm = [\"book\"]\ntimeout_ms = 4000\nfiller_after_ms = 250\nprefix = \"t_\"",
        "",
    )
    .await;
    let spec = |name: &str| rig.registry.spec(name).unwrap().clone();
    let names: Vec<String> = rig.registry.specs().map(|s| s.name.to_string()).collect();
    assert!(names.contains(&"t_read_echo".to_owned()), "{names:?}");
    // A dot becomes an underscore; the tool outside the allowlist is not registered.
    assert!(names.contains(&"t_files_read".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("hidden")), "{names:?}");
    assert_eq!(names.len(), 11, "ten tools and the gate's own: {names:?}");

    // Kinds come from the file, not from the server.
    assert_eq!(spec("t_read_echo").kind, ToolKind::ReadOnly);
    assert_eq!(spec("t_files_read").kind, ToolKind::ReadOnly);
    assert_eq!(spec("t_book").kind, ToolKind::SideEffect);
    assert_eq!(spec("t_crash").kind, ToolKind::SideEffect);
    assert!(spec("t_book").confirm && !spec("t_read_echo").confirm);
    assert_eq!(spec("t_book").timeout, Duration::from_secs(4));
    assert_eq!(
        spec("t_book").filler_after,
        Some(Duration::from_millis(250))
    );
    assert!(spec("t_read_echo").description.contains("Echoes"));
    assert_eq!(spec("t_read_echo").parameters["required"], json!(["text"]));
    // The gate's own tool is there because a tool is held for confirmation.
    assert!(rig.registry.spec("confirm_pending").is_some());

    // A call works under the name the model sees, and the dotted tool under its own name.
    let ran = rig.run(1, "t_read_echo", json!({"text": "hi"})).await;
    assert_eq!(text(&ran), "echo: hi");
    let ran = rig.run(2, "t_files_read", json!({"path": "/a"})).await;
    assert_eq!(text(&ran), "contents of /a");
    // The tool outside the allowlist cannot be reached by name.
    let ran = rig.run(3, "hidden", json!({})).await;
    assert_eq!(error_kind(&ran), "unknown_tool");
    assert!(lines(&dir, "events.log").iter().all(|l| l != "hidden ran"));
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_tool_the_server_does_not_offer_stops_the_start_at_its_position() {
    let dir = state_dir("missing");
    let servers = stdio_entry("test", &dir, "\"read_echo\", \"no_such_tool\"", "", "", "");
    let text = agent_text(&servers, "");
    let agent = load(&text, &[]).unwrap();
    let error = Rig::try_start(&agent).await.err().unwrap().to_string();
    assert!(error.contains("mcp.servers[0].tools[1]"), "{error}");
    assert!(error.contains("no_such_tool"), "{error}");
    assert!(
        error.contains("read_echo"),
        "the error lists what the server offers: {error}"
    );
    let line = text
        .lines()
        .position(|l| l.contains("no_such_tool"))
        .unwrap()
        + 1;
    assert!(error.contains(&format!("agent.toml:{line}:")), "{error}");
    // The connection made before the failure was closed again: the child saw stdin close.
    eventually("the child to see stdin close", 10, || {
        lines(&dir, "exits.log") == ["stdin closed"]
    })
    .await;
}

#[tokio::test]
async fn a_tool_with_an_oversize_schema_stops_the_start_at_its_entry() {
    let dir = state_dir("bloat");
    let servers = stdio_entry(
        "test",
        &dir,
        "\"read_echo\", \"bloat\"",
        "",
        "",
        ", MCP_TEST_BLOAT = \"1\"",
    );
    let agent = load(&agent_text(&servers, ""), &[]).unwrap();
    let error = Rig::try_start(&agent).await.err().unwrap().to_string();
    assert!(error.contains("mcp.servers[0].tools[1]"), "{error}");
    assert!(
        error.contains("input schema") && error.contains("16384"),
        "{error}"
    );
    // Left out of the allowlist, the same server starts.
    let servers = stdio_entry(
        "test",
        &dir,
        "\"read_echo\"",
        "",
        "",
        ", MCP_TEST_BLOAT = \"1\"",
    );
    let rig = Rig::start(&load(&agent_text(&servers, ""), &[]).unwrap()).await;
    assert_eq!(rig.registry.len(), 1);
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_server_that_cannot_start_is_a_start_error_naming_it() {
    // A program that does not exist.
    let text = agent_text(
        "[[mcp.servers]]\nname = \"ghost\"\ncommand = \"definitely-not-a-program-0451\"\ntools = [\"a\"]\n",
        "",
    );
    let error = Rig::try_start(&load(&text, &[]).unwrap())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("`ghost`") && error.contains("cannot start"),
        "{error}"
    );
    assert!(error.contains("mcp.servers[0]"), "{error}");

    // A program that exits at once.
    let dir = state_dir("exits");
    let servers = stdio_entry(
        "quitter",
        &dir,
        "\"read_echo\"",
        "",
        "",
        ", MCP_TEST_MODE = \"exit\"",
    );
    let error = Rig::try_start(&load(&agent_text(&servers, ""), &[]).unwrap())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("`quitter`") && error.contains("handshake failed"),
        "{error}"
    );

    // A program that never answers: bounded by the startup timeout.
    let servers = stdio_entry(
        "mute",
        &dir,
        "\"read_echo\"",
        "",
        "startup_timeout_ms = 1000",
        ", MCP_TEST_MODE = \"silent\"",
    );
    let started = SystemClock.now();
    let error = Rig::try_start(&load(&agent_text(&servers, ""), &[]).unwrap())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        since(started) < Duration::from_secs(8),
        "{:?}",
        since(started)
    );
    assert!(error.contains("startup timeout"), "{error}");
}

#[tokio::test]
async fn a_name_another_tool_has_stops_the_start_at_the_allowlist_entry() {
    use brisvox_agent::tools::{
        RegistryError, Tool, ToolCallCtx, ToolFuture, ToolOutput, ToolSpec,
    };
    use brisvox_server::AppTools;

    struct Taken;
    impl Tool for Taken {
        fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
            Box::pin(async { Ok(ToolOutput::Text("mine".into())) })
        }
    }
    struct App;
    impl AppTools for App {
        fn register(
            &self,
            registry: &mut ToolRegistry,
            _clock: &SharedClock,
        ) -> Result<(), RegistryError> {
            registry.register(
                ToolSpec::new(
                    "read_echo",
                    "mine",
                    json!({"type": "object"}),
                    ToolKind::ReadOnly,
                ),
                Taken,
            )
        }
    }
    let dir = state_dir("taken");
    let servers = stdio_entry("test", &dir, "\"read_echo\"", "\"read_echo\"", "", "");
    let agent = load(&agent_text(&servers, ""), &[]).unwrap();
    let clock = clock();
    let setup = ToolSetup::none().with_app(&App, &clock).unwrap();
    let error = setup
        .with_mcp(&agent, &clock)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("mcp.servers[0].tools[0]"), "{error}");
    assert!(error.contains("read_echo"), "{error}");
}

#[tokio::test]
async fn a_server_of_the_session_era_works_too() {
    // No `server/discover`, protocol versions up to 2025-11-25: the client falls back to
    // the `initialize` handshake.
    let dir = state_dir("legacy");
    let servers = stdio_entry(
        "old",
        &dir,
        "\"read_echo\"",
        "\"read_echo\"",
        "",
        ", MCP_TEST_MODE = \"legacy\"",
    );
    let rig = Rig::start(&load(&agent_text(&servers, ""), &[]).unwrap()).await;
    let ran = rig.run(1, "read_echo", json!({"text": "old school"})).await;
    assert_eq!(text(&ran), "echo: old school");
    rig.host.shutdown().await;
}

#[tokio::test]
async fn the_listing_is_read_page_by_page() {
    let (rig, _dir) = stdio_rig("pages", "", ", MCP_TEST_PAGE_SIZE = \"3\"").await;
    assert_eq!(
        rig.registry.len(),
        10,
        "all allowlisted tools came through 4 pages"
    );
    let ran = rig.run(1, "read_echo", json!({"text": "paged"})).await;
    assert_eq!(text(&ran), "echo: paged");
    rig.host.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// What the engine guarantees

#[tokio::test]
async fn arguments_are_checked_against_the_servers_schema_before_the_call_leaves() {
    let (rig, dir) = stdio_rig("schema", "", "").await;
    // `book` needs `what`.
    let ran = rig.run(1, "book", json!({"delay_ms": 5})).await;
    assert_eq!(error_kind(&ran), "invalid_arguments");
    assert!(
        lines(&dir, "effects.log").is_empty(),
        "nothing reached the server"
    );
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_side_effect_tool_never_runs_speculatively_and_never_twice_for_one_call() {
    let (rig, dir) = stdio_rig("effects", "", "").await;
    let cancel = CancellationToken::new();
    let book = || call(1, 0, "book", json!({"what": "table for two"}));

    // A speculative response holds the call: it is not made.
    let speculation = rig.engine.speculate(vec![book()], epoch(1), &cancel).await;
    assert_eq!(speculation.held_calls().count(), 1);
    // A ReadOnly call of a speculative response does run.
    let speculative_read = rig
        .engine
        .speculate(
            vec![call(1, 1, "read_echo", json!({"text": "early"}))],
            epoch(1),
            &cancel,
        )
        .await;
    assert_eq!(speculative_read.held_calls().count(), 0);
    assert_eq!(speculative_read.answered().count(), 1);
    SystemClock.sleep(Duration::from_millis(300)).await;
    assert!(
        lines(&dir, "effects.log").is_empty(),
        "a held call must not reach the server"
    );

    // The turn commits: it runs once.
    let mut committed = rig.engine.commit(speculation, &cancel).await;
    committed.mark_delivered();
    assert_eq!(text(&committed), "booked #1");
    assert_eq!(lines(&dir, "effects.log").len(), 1);

    // The same call again (a replay of call t1.0): the recorded result, no second booking.
    let replay = rig.run(1, "book", json!({"what": "table for two"})).await;
    assert_eq!(text(&replay), "booked #1");
    assert_eq!(
        lines(&dir, "effects.log").len(),
        1,
        "never twice for one call id"
    );

    // A discarded speculation never runs its SideEffect call.
    let dropped = rig
        .engine
        .speculate(
            vec![call(3, 0, "book", json!({"what": "never"}))],
            epoch(3),
            &cancel,
        )
        .await;
    drop(dropped);
    SystemClock.sleep(Duration::from_millis(300)).await;
    assert_eq!(lines(&dir, "effects.log").len(), 1);
    rig.host.shutdown().await;
}

#[tokio::test]
async fn the_idempotency_key_of_the_call_reaches_the_server_in_meta() {
    let (rig, _dir) = stdio_rig("meta", "", "").await;
    let ran = rig.run(4, "meta_probe", json!({})).await;
    let key = IdempotencyKey::new(SessionId::new(7), epoch(4), ToolCallId::new(epoch(4), 0));
    assert_eq!(text(&ran), format!("{:016x}", key.digest()));
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_read_only_call_is_dropped_at_its_timeout_and_the_server_is_told() {
    let (rig, dir) = stdio_rig("timeout", "timeout_ms = 300", "").await;
    let started = SystemClock.now();
    let ran = rig.run(1, "slow", json!({"ms": 20_000})).await;
    assert_eq!(error_kind(&ran), "timeout");
    assert!(
        since(started) < Duration::from_secs(5),
        "{:?}",
        since(started)
    );
    // Dropping the request cancelled it at the server (`notifications/cancelled`).
    eventually("the server to see the cancellation", 10, || {
        lines(&dir, "events.log")
            .iter()
            .any(|l| l.starts_with("notifications/cancelled") || l == "slow cancelled")
    })
    .await;
    assert!(!lines(&dir, "events.log").contains(&"slow finished".to_owned()));
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_barge_in_drops_a_read_only_call_and_cancels_it_at_the_server() {
    let (rig, dir) = stdio_rig("bargein", "timeout_ms = 20000", "").await;
    let cancel = CancellationToken::new();
    let interrupt = cancel.clone();
    tokio::spawn(async move {
        SystemClock.sleep(Duration::from_millis(300)).await;
        interrupt.cancel();
    });
    let mut ran = rig
        .engine
        .execute(
            vec![call(1, 0, "slow", json!({"ms": 20_000}))],
            epoch(1),
            &cancel,
        )
        .await;
    ran.mark_delivered();
    assert_eq!(error_kind(&ran), "interrupted");
    eventually("the server to see the cancellation", 10, || {
        lines(&dir, "events.log")
            .iter()
            .any(|l| l.starts_with("notifications/cancelled") || l == "slow cancelled")
    })
    .await;
    rig.host.shutdown().await;
}

#[tokio::test]
async fn a_side_effect_call_outlives_its_timeout_and_its_result_follows_as_a_note() {
    let (rig, dir) = stdio_rig("late", "timeout_ms = 200", "").await;
    let ran = rig
        .run(1, "book", json!({"what": "slow booking", "delay_ms": 900}))
        .await;
    assert_eq!(error_kind(&ran), "timeout");
    assert!(
        error_message(&ran).contains("keeps running"),
        "{}",
        error_message(&ran)
    );
    // The booking was made once, and its result arrives as a late note.
    let mut notes = Vec::new();
    eventually("the late note", 10, || {
        notes.extend(rig.engine.take_late_notes());
        !notes.is_empty()
    })
    .await;
    let note = notes[0].text().to_owned();
    assert!(note.contains("booked #1"), "{note}");
    assert_eq!(lines(&dir, "effects.log").len(), 1);
    rig.host.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Results and errors

#[tokio::test]
async fn results_and_errors_map_to_the_engines_results() {
    let (rig, _dir) = stdio_rig("results", "", "").await;
    // isError: true is the tool's error, with its text.
    let ran = rig.run(1, "fail", json!({})).await;
    assert_eq!(error_kind(&ran), "tool_failed");
    assert!(error_message(&ran).contains("boom: the order id is unknown"));
    // A JSON-RPC error: the server rejected the call (nothing ran).
    let ran = rig.run(2, "protocol_error", json!({})).await;
    assert_eq!(error_kind(&ran), "tool_failed");
    let message = error_message(&ran);
    assert!(
        message.contains("rejected the call") && !message.contains("outcome is unknown"),
        "{message}"
    );
    // Structured content is the result as JSON.
    let ran = rig.run(3, "structured", json!({})).await;
    let (is_error, body) = result(&ran, 0);
    assert!(!is_error);
    assert_eq!(body, json!({"n": 3, "items": ["a", "b"]}));
    rig.host.shutdown().await;
}

#[tokio::test]
async fn the_child_gets_its_own_environment_and_not_the_servers() {
    let dir = state_dir("env");
    let servers = stdio_entry(
        "test",
        &dir,
        "\"env_names\"",
        "\"env_names\"",
        "env_from = { FROM_SERVER = \"MCP_SECRET_SOURCE\" }",
        ", FROM_ENTRY = \"1\"",
    );
    let agent = load(
        &agent_text(&servers, ""),
        &[("MCP_SECRET_SOURCE", "s3cret")],
    )
    .unwrap();
    // The test process has cargo's variables; the child must not.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let rig = Rig::start(&agent).await;
    let ran = rig.run(1, "env_names", json!({})).await;
    let names: Vec<String> = text(&ran).split(',').map(str::to_owned).collect();
    assert!(names.contains(&"FROM_ENTRY".to_owned()), "{names:?}");
    assert!(names.contains(&"FROM_SERVER".to_owned()), "{names:?}");
    assert!(
        names.contains(&"MCP_TEST_STATE_DIR".to_owned()),
        "{names:?}"
    );
    for leaked in [
        "CARGO_MANIFEST_DIR",
        "CARGO_PKG_NAME",
        "CARGO",
        "MCP_SECRET_SOURCE",
        "MCP_TEST_LLM_KEY",
    ] {
        assert!(
            !names.contains(&leaked.to_owned()),
            "{leaked} leaked: {names:?}"
        );
    }
    rig.host.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// The lifecycle

#[tokio::test]
async fn a_server_that_exits_is_restarted_and_calls_meanwhile_fail_at_once() {
    // The restart waits 1.5 s before the server serves again.
    let (rig, dir) = stdio_rig("crash", "", ", MCP_TEST_RESTART_DELAY_MS = \"1500\"").await;
    assert_eq!(rig.host.status()[0].state, McpLinkState::Up);

    // `crash` is a SideEffect tool in this file: the process dies during the call, so the
    // outcome is unknown, and that is said.
    let ran = rig.run(1, "crash", json!({})).await;
    assert_eq!(error_kind(&ran), "tool_failed", "{}", text(&ran));
    assert!(
        error_message(&ran).contains("the outcome is unknown"),
        "{}",
        text(&ran)
    );

    // While it restarts, a call fails at once (it does not wait for the server).
    eventually("the restart to be noticed", 10, || {
        rig.host.status()[0].state == McpLinkState::Restarting
    })
    .await;
    let started = SystemClock.now();
    let ran = rig.run(2, "read_echo", json!({"text": "x"})).await;
    assert!(
        since(started) < Duration::from_millis(500),
        "{:?}",
        since(started)
    );
    assert_eq!(error_kind(&ran), "tool_failed");
    assert!(
        error_message(&ran).contains("not connected"),
        "{}",
        error_message(&ran)
    );
    assert!(error_message(&ran).contains("was not made"));

    // Then it is back: the tools work again, on a new process.
    eventually("the server to be connected again", 20, || {
        rig.host.status()[0].state == McpLinkState::Up
    })
    .await;
    let ran = rig.run(3, "read_echo", json!({"text": "again"})).await;
    assert_eq!(text(&ran), "echo: again");
    assert_eq!(rig.host.status()[0].restarts, 1);
    assert_eq!(lines(&dir, "starts.log").len(), 2);

    // A ReadOnly call that loses the connection is a plain failure, not an unknown outcome.
    let read_only_crash = {
        let servers = stdio_entry(
            "test",
            &dir,
            "\"crash\"",
            "\"crash\"",
            "",
            ", MCP_TEST_RESTART_DELAY_MS = \"1500\"",
        );
        Rig::start(&load(&agent_text(&servers, ""), &[]).unwrap()).await
    };
    let ran = read_only_crash.run(1, "crash", json!({})).await;
    assert_eq!(error_kind(&ran), "tool_failed");
    assert!(!text(&ran).contains("outcome is unknown"), "{}", text(&ran));
    read_only_crash.host.shutdown().await;
    rig.host.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_stdin_so_the_server_exits_by_itself() {
    let (rig, dir) = stdio_rig("shutdown", "", "").await;
    let started = SystemClock.now();
    rig.host.shutdown().await;
    assert!(
        since(started) < Duration::from_secs(8),
        "{:?}",
        since(started)
    );
    assert_eq!(rig.host.status()[0].state, McpLinkState::Down);
    // The server saw its stdin close and left (not killed).
    eventually("the child to exit on stdin EOF", 10, || {
        lines(&dir, "exits.log") == ["stdin closed"]
    })
    .await;
    // Calls after the shutdown fail at once.
    let ran = rig.run(1, "read_echo", json!({"text": "late"})).await;
    assert_eq!(error_kind(&ran), "tool_failed");
    // A second shutdown is harmless.
    rig.host.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Streamable HTTP

const TOKEN: &str = "http-test-bearer-0042";

struct HttpServer {
    addr: SocketAddr,
    authorized: Arc<AtomicUsize>,
    unauthorized: Arc<AtomicUsize>,
    stop: CancellationToken,
}

#[derive(Clone)]
struct BearerState {
    authorized: Arc<AtomicUsize>,
    unauthorized: Arc<AtomicUsize>,
}

async fn require_bearer(
    State(state): State<BearerState>,
    request: Request,
    next: Next,
) -> Response {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if header.as_deref() == Some(&format!("Bearer {TOKEN}")) {
        state.authorized.fetch_add(1, Ordering::SeqCst);
        next.run(request).await
    } else {
        state.unauthorized.fetch_add(1, Ordering::SeqCst);
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// An in-process streamable HTTP MCP server that wants `Authorization: Bearer TOKEN`.
async fn http_server(page_size: Option<usize>) -> HttpServer {
    let handler = TestServer {
        dir: None,
        page_size,
        bloat: false,
    };
    let stop = CancellationToken::new();
    let config = StreamableHttpServerConfig::default().with_cancellation_token(stop.child_token());
    let service: StreamableHttpService<TestServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
    let state = BearerState {
        authorized: Arc::new(AtomicUsize::new(0)),
        unauthorized: Arc::new(AtomicUsize::new(0)),
    };
    let router =
        axum::Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_bearer,
            ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ct = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct.cancelled_owned().await })
            .await;
    });
    HttpServer {
        addr,
        authorized: state.authorized,
        unauthorized: state.unauthorized,
        stop,
    }
}

fn http_entry(addr: SocketAddr, bearer: bool) -> String {
    format!(
        "[[mcp.servers]]\nname = \"crm\"\nurl = \"http://{addr}/mcp\"\n{}tools = [\"read_echo\", \"files.read\", \"book\", \"fail\"]\nread_only = [\"read_echo\", \"files.read\", \"fail\"]\nprefix = true\n",
        if bearer {
            "bearer_env = \"CRM_TOKEN\"\n"
        } else {
            ""
        }
    )
}

#[tokio::test]
async fn streamable_http_lists_calls_and_sends_the_bearer_token() {
    // A server of the newest revision (stateless), listing two tools per page.
    let server = http_server(Some(2)).await;
    let agent = load(
        &agent_text(&http_entry(server.addr, true), ""),
        &[("CRM_TOKEN", TOKEN)],
    )
    .unwrap();
    let rig = Rig::start(&agent).await;
    let mut names: Vec<String> = rig.registry.specs().map(|s| s.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        ["crm_book", "crm_fail", "crm_files_read", "crm_read_echo"]
    );
    assert_eq!(
        rig.registry.spec("crm_book").unwrap().kind,
        ToolKind::SideEffect
    );
    assert_eq!(
        rig.registry.spec("crm_read_echo").unwrap().kind,
        ToolKind::ReadOnly
    );

    let ran = rig
        .run(1, "crm_read_echo", json!({"text": "over http"}))
        .await;
    assert_eq!(text(&ran), "echo: over http");
    let ran = rig.run(2, "crm_fail", json!({})).await;
    assert_eq!(error_kind(&ran), "tool_failed");
    let ran = rig.run(3, "crm_book", json!({"what": "x"})).await;
    assert_eq!(text(&ran), "booked #0");
    // Every request carried the token; none without it was made.
    assert!(server.authorized.load(Ordering::SeqCst) >= 5);
    assert_eq!(server.unauthorized.load(Ordering::SeqCst), 0);
    rig.host.shutdown().await;
    server.stop.cancel();
}

/// A streamable HTTP server of the session era, written by hand: `initialize` hands out a
/// session id (header `Mcp-Session-Id`), every other request must carry a live one or is
/// answered 404, and there is no `server/discover`. `sessions` is what the server remembers.
#[derive(Clone)]
struct SessionServer {
    sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    next: Arc<AtomicUsize>,
}

async fn session_post(
    State(server): State<SessionServer>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    let id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let is_initialize = request["method"] == "initialize";
    if !is_initialize
        && !id
            .as_ref()
            .is_some_and(|id| server.sessions.lock().unwrap().contains(id))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(reply) = mcp_server::legacy_reply(&request) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let mut response = (
        StatusCode::OK,
        [("content-type", "application/json")],
        reply.to_string(),
    )
        .into_response();
    if is_initialize {
        let session = format!("session-{}", server.next.fetch_add(1, Ordering::SeqCst));
        server.sessions.lock().unwrap().insert(session.clone());
        response
            .headers_mut()
            .insert("mcp-session-id", session.parse().unwrap());
    }
    response
}

async fn session_server() -> (HttpServer, SessionServer) {
    let server = SessionServer {
        sessions: Arc::default(),
        next: Arc::new(AtomicUsize::new(0)),
    };
    let state = BearerState {
        authorized: Arc::new(AtomicUsize::new(0)),
        unauthorized: Arc::new(AtomicUsize::new(0)),
    };
    let router = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(session_post)
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                .delete(|| async { StatusCode::OK }),
        )
        .with_state(server.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let ct = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct.cancelled_owned().await })
            .await;
    });
    (
        HttpServer {
            addr,
            authorized: state.authorized,
            unauthorized: state.unauthorized,
            stop,
        },
        server,
    )
}

#[tokio::test]
async fn a_session_era_http_server_works_and_a_restart_of_it_is_survived() {
    let (http, server) = session_server().await;
    let entry = format!(
        "[[mcp.servers]]\nname = \"crm\"\nurl = \"http://{}/mcp\"\nbearer_env = \"CRM_TOKEN\"\ntools = [\"read_echo\"]\nread_only = [\"read_echo\"]\nprefix = true\n",
        http.addr
    );
    let agent = load(&agent_text(&entry, ""), &[("CRM_TOKEN", TOKEN)]).unwrap();
    let rig = Rig::start(&agent).await;
    let ran = rig.run(1, "crm_read_echo", json!({"text": "before"})).await;
    assert_eq!(text(&ran), "echo: before");
    assert_eq!(server.sessions.lock().unwrap().len(), 1, "one session");
    // The server restarts: it forgets its sessions and answers 404 to the old one. The client
    // starts a new session and the call goes through.
    server.sessions.lock().unwrap().clear();
    let ran = rig.run(2, "crm_read_echo", json!({"text": "after"})).await;
    assert_eq!(text(&ran), "echo: after");
    assert_eq!(server.sessions.lock().unwrap().len(), 1, "a new session");
    assert_eq!(http.unauthorized.load(Ordering::SeqCst), 0);
    rig.host.shutdown().await;
    http.stop.cancel();
}

#[tokio::test]
async fn a_server_that_refuses_the_token_stops_the_start() {
    let server = http_server(None).await;
    // Without a token, and with a wrong one.
    let without = load(&agent_text(&http_entry(server.addr, false), ""), &[]).unwrap();
    let error = Rig::try_start(&without).await.err().unwrap().to_string();
    assert!(
        error.contains("`crm`") && error.contains("mcp.servers[0]"),
        "{error}"
    );
    let wrong = load(
        &agent_text(&http_entry(server.addr, true), ""),
        &[("CRM_TOKEN", "not-the-token")],
    )
    .unwrap();
    let error = Rig::try_start(&wrong).await.err().unwrap().to_string();
    assert!(error.contains("`crm`"), "{error}");
    assert!(
        !error.contains("not-the-token"),
        "the token must not appear: {error}"
    );
    assert!(server.unauthorized.load(Ordering::SeqCst) >= 2);
    assert_eq!(server.authorized.load(Ordering::SeqCst), 0);
    server.stop.cancel();
}

#[tokio::test]
async fn an_http_server_that_is_gone_fails_calls_without_hanging() {
    let server = http_server(None).await;
    let agent = load(
        &agent_text(&http_entry(server.addr, true), ""),
        &[("CRM_TOKEN", TOKEN)],
    )
    .unwrap();
    let rig = Rig::start(&agent).await;
    server.stop.cancel();
    SystemClock.sleep(Duration::from_millis(300)).await;
    let started = SystemClock.now();
    let ran = rig
        .run(1, "crm_read_echo", json!({"text": "anyone?"}))
        .await;
    assert!(
        since(started) < Duration::from_secs(10),
        "{:?}",
        since(started)
    );
    assert_eq!(error_kind(&ran), "tool_failed");
    rig.host.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// A public server

/// Interop with DeepWiki's public MCP server, through the shipped example agent (needs the
/// network, no key): runs only with `BRISVOX_LIVE_TESTS=1`.
#[tokio::test]
async fn the_example_agent_talks_to_the_public_deepwiki_server() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/agents/mcp/docs-en.toml");
    let keys = [
        ("DEEPGRAM_API_KEY", "x"),
        ("OPENAI_API_KEY", "x"),
        ("CARTESIA_API_KEY", "x"),
    ];
    let agent = load(&std::fs::read_to_string(path).unwrap(), &keys).unwrap();
    let rig = Rig::start(&agent).await;
    let mut names: Vec<String> = rig.registry.specs().map(|s| s.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        ["wiki_ask_wiki_question", "wiki_read_wiki_structure"]
    );
    let ran = rig
        .run(
            1,
            "wiki_read_wiki_structure",
            json!({"repoName": "modelcontextprotocol/rust-sdk"}),
        )
        .await;
    let (is_error, body) = result(&ran, 0);
    assert!(!is_error, "{body}");
    assert!(body.to_string().len() > 50, "{body}");
    rig.host.shutdown().await;
}
