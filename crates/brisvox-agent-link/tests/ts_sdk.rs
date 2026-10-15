//! The TypeScript SDK (`sdk/typescript`) against the real agent link: a call (`run_call`
//! over the loopback transport, the simulator's mock providers, typed user turns) linked to
//! `sdk/typescript/dist/examples/restaurant.js`, a Node process, over a real WebSocket on
//! 127.0.0.1. It is the twin of `python_sdk.rs` and runs the same script.
//!
//! The test needs Node 20 or newer and the built SDK (`npm ci && npm run build` in
//! `sdk/typescript`). It runs only when `BRISVOX_TS_SDK` names the `node` executable (otherwise
//! it prints a note and passes), so a plain local test run needs no Node. `scripts/verify.sh`
//! builds the SDK, runs its own tests and sets the variable; with `BRISVOX_REQUIRE_TS_SDK=1` (CI
//! sets it) a missing Node FAILS the test instead of skipping it, so the gate cannot go green
//! without exercising the SDK. With the variable set, a missing build fails the test as well:
//!
//! ```text
//! (cd sdk/typescript && npm ci && npm run build)
//! BRISVOX_TS_SDK=node cargo nextest run -p brisvox-agent-link --test ts_sdk
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use brisvox_agent_link::{AgentLinkConfig, LinkCall, attach};
use brisvox_core::{SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::sim::{LoopbackCall, open_loopback};
use brisvox_engine::{
    EngineConfig, InterruptionConfig, Offload, SessionBuilder, TextTurn, run_call, text_turns,
};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::session::SimSession;

mod support;

const WAIT: Duration = Duration::from_secs(20);
/// How long the Node process may take to start and print its port: process start-up is not what
/// these tests check, and a busy shared runner (Windows, thousands of tests in parallel) has
/// needed more than `WAIT` for it.
const STARTUP_WAIT: Duration = Duration::from_secs(90);

/// The Node agent process; killed when dropped so a failing test leaves nothing behind.
struct TsAgent {
    child: Child,
    url: String,
}

impl Drop for TsAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_agent(node: &str, example: &str) -> TsAgent {
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/typescript");
    let example = sdk.join(format!("dist/examples/{example}.js"));
    assert!(
        example.exists(),
        "{} is missing: build the SDK first (`npm ci && npm run build` in sdk/typescript)",
        example.display()
    );
    let mut child = Command::new(node)
        .arg(&example)
        .args(["--host", "127.0.0.1", "--port", "0"])
        .env_remove("BRISVOX_AGENT_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the executable named by BRISVOX_TS_SDK starts");
    let stdout = child.stdout.take().expect("piped stdout");
    // The agent prints `LISTENING ws://127.0.0.1:<port>` once it accepts connections.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let _ = tx.send(lines.next().and_then(Result::ok));
        // Keep draining so the child never blocks on a full pipe.
        for _ in lines {}
    });
    let line = rx
        .recv_timeout(STARTUP_WAIT)
        .expect("the TypeScript agent reports its port in time")
        .expect("the TypeScript agent printed a line");
    let url = line
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("unexpected first line of the agent: {line}"))
        .trim()
        .to_owned();
    TsAgent { child, url }
}

async fn until(what: &str, cond: impl Fn() -> bool) {
    let clock = SystemClock::shared();
    for _ in 0..2_000 {
        if cond() {
            return;
        }
        clock.sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typescript_agent_greets_runs_its_tools_and_ends_the_call() {
    let Some(node) = std::env::var_os("BRISVOX_TS_SDK") else {
        let required = std::env::var("BRISVOX_REQUIRE_TS_SDK")
            .is_ok_and(|value| !value.is_empty() && value != "0");
        assert!(
            !required,
            "BRISVOX_REQUIRE_TS_SDK is set but BRISVOX_TS_SDK names no Node 20+ executable"
        );
        eprintln!(
            "skipped: set BRISVOX_TS_SDK to a Node 20+ executable (and build sdk/typescript)"
        );
        return;
    };
    let agent = spawn_agent(&node.to_string_lossy(), "restaurant");

    let clock: SharedClock = SystemClock::shared();
    let mut config = AgentLinkConfig::new(agent.url.clone());
    // A TypeScript process answers well inside the defaults; the margin keeps a busy CI box green.
    config.connect_timeout = Duration::from_secs(5);
    config.handshake_timeout = Duration::from_secs(5);
    let sim = SimSession::new(clock.clone());
    // Turn 1: the LLM books a table (a `side_effect` remote tool with a nested, typed argument).
    sim.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "c1",
        "book_table",
        r#"{"time":"19:30","party_size":4,"contact":{"name":"Ann"}}"#,
    ));
    sim.llm.push(LlmScript::text(["You are booked."]));
    // Turn 2: a `read_only` tool whose argument is an enum.
    sim.llm.push(LlmScript::text(["Checking."]).with_tool_call(
        "c2",
        "opening_hours",
        r#"{"day":"mon"}"#,
    ));
    sim.llm.push(LlmScript::text(["We are closed on Monday."]));
    // Turn 3: "goodbye" also reaches the LLM; the TypeScript handler ends the call itself.
    sim.llm.push(LlmScript::text(["Bye."]));

    let LoopbackCall {
        session,
        client: _client,
        cancel,
    } = open_loopback(&clock, 65_536).await.unwrap();
    let (text, input) = text_turns(4);
    let mut engine = EngineConfig::DEFAULT;
    engine.lead_margin = Duration::ZERO;
    engine.llm.retries = 0;
    engine.interruption = InterruptionConfig::OFF;
    let builder = SessionBuilder::new(
        engine,
        clock.clone(),
        TimelineRecorder::new(),
        Offload::Inline(clock.clone()),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_system_prompt("Local prompt.")
    .with_text_input(input);
    let call = LinkCall {
        session_id: 7,
        agent: "luigi".to_owned(),
        clock: clock.clone(),
        cancel: cancel.clone(),
        ops: None,
        identity: None,
    };
    let (builder, link) = attach(&Arc::new(config), builder, call).await;
    let link = link.expect("the TypeScript agent accepted the call");
    let call = run_call(session, builder, &cancel).await.unwrap();

    // The greeting is a bot line of the TypeScript agent (`greeting=` of its `session.accept`).
    until("the greeting", || {
        call.handles().driver.snapshot().lines_started == 1
    })
    .await;

    text.try_send(TextTurn::now("a table for four at half past seven"))
        .unwrap();
    until("the booking answer", || sim.llm.requests().len() == 2).await;
    let first = &sim.llm.requests()[0];
    let mut names: Vec<String> = first.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        ["book_table", "opening_hours"],
        "the TypeScript tools"
    );
    let second = format!("{:?}", sim.llm.requests()[1].messages);
    assert!(
        second.contains("booked") && second.contains("Ann") && second.contains("19:30"),
        "the tool result reached the LLM: {second}"
    );
    until("the bot is idle", || {
        call.handles().driver.snapshot().turns_ended == 1
    })
    .await;

    text.try_send(TextTurn::now("and Monday?")).unwrap();
    until("the hours answer", || sim.llm.requests().len() == 4).await;
    let fourth = format!("{:?}", sim.llm.requests()[3].messages);
    assert!(fourth.contains("Closed on Monday."), "{fourth}");
    until("the bot is idle again", || {
        call.handles().driver.snapshot().turns_ended == 2
    })
    .await;

    // "goodbye": the TypeScript handler sends `end` with a farewell; the call speaks it and ends.
    text.try_send(TextTurn::now("ok goodbye")).unwrap();
    tokio::time::timeout(WAIT, link.ended())
        .await
        .expect("the TypeScript agent ended the call");
    let stats = link.stats();
    assert_eq!(
        (stats.tool_calls, stats.tool_results),
        (2, 2),
        "both tool calls were answered by TypeScript: {stats:?}"
    );
    assert!(
        call.handles().driver.snapshot().lines_started >= 2,
        "the greeting and the farewell were bot lines"
    );
    link.close("hangup").await;
    cancel.cancel();
}

/// The link of a call is cut in the middle (a proxy drops the sockets); the TypeScript
/// agent (`resume: true`, `examples/resumable.ts`) keeps the call parked, the server reconnects
/// with `session.start.resume`, and the agent takes its session up again: state kept, `seq`
/// continuing, tools still working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typescript_agent_resumes_a_call_whose_link_was_dropped() {
    let Some(node) = std::env::var_os("BRISVOX_TS_SDK") else {
        let required = std::env::var("BRISVOX_REQUIRE_TS_SDK")
            .is_ok_and(|value| !value.is_empty() && value != "0");
        assert!(
            !required,
            "BRISVOX_REQUIRE_TS_SDK is set but BRISVOX_TS_SDK names no Node 20+ executable"
        );
        eprintln!(
            "skipped: set BRISVOX_TS_SDK to a Node 20+ executable (and build sdk/typescript)"
        );
        return;
    };
    let agent = spawn_agent(&node.to_string_lossy(), "resumable");
    support::a_dropped_link_is_resumed_by_the_sdk_agent(&agent.url).await;
}
