//! Shared by `python_sdk.rs` and `ts_sdk.rs`: the dropped-and-resumed link scenario
//! (the SDKs' `resume` support), run against a real SDK agent process through a TCP proxy whose
//! connections the test can cut.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent_link::{AgentLinkConfig, LinkCall, attach};
use brisvox_core::{SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::sim::{LoopbackCall, open_loopback};
use brisvox_engine::{
    EngineConfig, InterruptionConfig, Offload, SessionBuilder, TextTurn, run_call, text_turns,
};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::session::SimSession;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The longest the scenario waits for the SDK agent or the link.
pub const WAIT: Duration = Duration::from_secs(20);

/// A TCP forwarder in front of an agent: [`DropProxy::drop_links`] cuts every connection that goes
/// through it (the agent and the link both see the socket close), later connections work again.
pub struct DropProxy {
    /// The `ws://` URL to hand to the link.
    pub url: String,
    drops: watch::Sender<u64>,
    accepting: JoinHandle<()>,
}

impl Drop for DropProxy {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

impl DropProxy {
    /// Listen on a free loopback port and forward to `agent_url` (`ws://host:port`).
    pub async fn start(agent_url: &str) -> DropProxy {
        let upstream = agent_url
            .strip_prefix("ws://")
            .unwrap_or_else(|| panic!("a ws:// agent URL, got {agent_url}"))
            .trim_end_matches('/')
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (drops, _) = watch::channel(0u64);
        let tx = drops.clone();
        let accepting = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let mut cut = tx.subscribe();
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(&upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = cut.changed() => {}
                    }
                    // Both sockets are dropped here: the peers see the close.
                });
            }
        });
        DropProxy {
            url: format!("ws://127.0.0.1:{port}"),
            drops,
            accepting,
        }
    }

    /// Cut every connection that is open now.
    pub fn drop_links(&self) {
        self.drops.send_modify(|generation| *generation += 1);
    }
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

/// A tool result as the LLM saw it, with the Debug escaping of the quotes undone.
fn unescaped(text: String) -> String {
    text.replace("\\\"", "\"")
}

/// The scenario, against `examples/resumable` of an SDK listening on `agent_url`.
///
/// A call with the SDK agent: the greeting, a `remember` tool call, then the link is cut. The SDK
/// agent keeps the call parked, the server reconnects with `session.start.resume`, the agent
/// re-attaches its session (state kept, no second start, `seq` continuing, `onResumed` run), and
/// its tools work on the resumed link: `recall` reports the note from before the cut. The call
/// then ends normally (the agent's `end`).
pub async fn a_dropped_link_is_resumed_by_the_sdk_agent(agent_url: &str) {
    let proxy = DropProxy::start(agent_url).await;
    let clock: SharedClock = SystemClock::shared();
    let mut config = AgentLinkConfig::new(proxy.url.clone());
    // A Node or Python process answers well inside the defaults; the margin keeps a busy box green.
    config.connect_timeout = Duration::from_secs(5);
    config.handshake_timeout = Duration::from_secs(5);
    config.reconnect_backoff = Duration::from_millis(20);
    config.reconnect_backoff_max = Duration::from_millis(60);
    let sim = SimSession::new(clock.clone());
    // Turn 1: the LLM has the agent keep a note (a `side_effect` remote tool).
    sim.llm.push(LlmScript::text(["Noting."]).with_tool_call(
        "c1",
        "remember",
        r#"{"note":"window seat"}"#,
    ));
    sim.llm.push(LlmScript::text(["Noted."]));
    // Turn 2, after the cut: a `read_only` remote tool reports what the agent kept.
    sim.llm
        .push(LlmScript::text(["Checking."]).with_tool_call("c2", "recall", "{}"));
    sim.llm.push(LlmScript::text(["A window seat."]));
    // Turn 3: "goodbye" also reaches the LLM; the agent's handler ends the call itself.
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
    let link = link.expect("the SDK agent accepted the call");
    let call = run_call(session, builder, &cancel).await.unwrap();

    until("the greeting", || {
        call.handles().driver.snapshot().lines_started == 1
    })
    .await;
    text.try_send(TextTurn::now("a window seat please"))
        .unwrap();
    until("the answer after the first tool", || {
        sim.llm.requests().len() == 2
    })
    .await;
    let after_tool = unescaped(format!("{:?}", sim.llm.requests()[1].messages));
    assert!(after_tool.contains("noted (1)"), "{after_tool}");
    until("the bot is idle", || {
        call.handles().driver.snapshot().turns_ended == 1
    })
    .await;
    assert_eq!(link.stats().reconnects, 0);

    // The link drops in the middle of the call.
    proxy.drop_links();
    until("the link was resumed", || {
        link.stats().reconnects == 1 && link.is_up()
    })
    .await;

    // The resumed agent kept the call: its note is there, it started once, the remote tools are
    // offered and answered again, and the `seq` count continued AT THE SDK: `seq_continues` is
    // true only if the last message the SDK itself read on the lost link equals the server's
    // `previous_seq` and the resuming `session.start` is the next number (the link was idle when
    // it dropped, so nothing was in flight).
    text.try_send(TextTurn::now("what did I ask for?")).unwrap();
    until("the answer after recall", || sim.llm.requests().len() == 4).await;
    let mut names: Vec<String> = sim.llm.requests()[2]
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["recall", "remember"], "remote tools are back");
    let recalled = unescaped(format!("{:?}", sim.llm.requests()[3].messages));
    for expected in [
        "window seat",
        r#""starts":1"#,
        r#""resumes":1"#,
        r#""seq_continues":true"#,
        r#""attempt":1"#,
    ] {
        assert!(
            recalled.contains(expected),
            "recall result lacks {expected}: {recalled}"
        );
    }
    until("the bot is idle again", || {
        call.handles().driver.snapshot().turns_ended == 2
    })
    .await;

    // The resumed link ends the call like a first one: the agent's `end` command.
    text.try_send(TextTurn::now("ok goodbye")).unwrap();
    tokio::time::timeout(WAIT, link.ended())
        .await
        .expect("the resumed agent ended the call");
    let stats = link.stats();
    assert_eq!(stats.reconnects, 1, "{stats:?}");
    assert_eq!(
        (stats.tool_calls, stats.tool_results),
        (2, 2),
        "both tool calls were answered, one on each link: {stats:?}"
    );
    link.close("hangup").await;
    cancel.cancel();
}
