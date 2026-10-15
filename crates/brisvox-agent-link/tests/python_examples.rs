//! The booking agent example (`examples/booking-agent`) against the real agent link: a
//! call (`run_call` over the loopback transport, the simulator's mock providers, typed user turns)
//! linked to `booking_agent.py`, a Python process, over a real WebSocket on 127.0.0.1. It proves
//! what the Python tests cannot: the server side accepts the example's five tool declarations
//! (JSON schemas, kinds), delivers arguments in the shape the typed tools expect, and gives the
//! LLM their results and their refusals.
//!
//! Needs a Python 3.11+ with `websockets`, like `python_sdk.rs`: it runs only when
//! `BRISVOX_PYTHON_SDK` names that interpreter (`scripts/verify.sh` sets it), and with
//! `BRISVOX_REQUIRE_PYTHON_SDK=1` a missing interpreter FAILS the test instead of skipping it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use brisvox_agent_link::{AgentLinkConfig, LinkCall, attach};
use brisvox_core::{Clock, SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::sim::{LoopbackCall, open_loopback};
use brisvox_engine::{
    EngineConfig, InterruptionConfig, Offload, SessionBuilder, TextTurn, run_call, text_turns,
};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::session::SimSession;

const WAIT: Duration = Duration::from_secs(20);
/// How long the Python process may take to start and print its port: process start-up is not
/// what these tests check, and a busy shared runner has needed more than `WAIT` for it.
const STARTUP_WAIT: Duration = Duration::from_secs(90);

/// The Python agent process; killed when dropped so a failing test leaves nothing behind.
struct PythonAgent {
    child: Child,
    url: String,
}

impl Drop for PythonAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_agent(python: &str) -> PythonAgent {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg(root.join("examples/booking-agent/booking_agent.py"))
        .args(["--host", "127.0.0.1", "--port", "0"])
        .env("PYTHONPATH", root.join("sdk/python"))
        .env_remove("BRISVOX_AGENT_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the Python interpreter named by BRISVOX_PYTHON_SDK starts");
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
        .expect("the booking agent reports its port in time")
        .expect("the booking agent printed a line");
    let url = line
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("unexpected first line of the agent: {line}"))
        .trim()
        .to_owned();
    PythonAgent { child, url }
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

/// A date ten days ahead that is not a Sunday (the salon is closed), as `YYYY-MM-DD`. Civil date
/// from days since the epoch (Howard Hinnant's algorithm); the Python agent runs on the same
/// machine, so its "today" is the same day except in the minutes around midnight UTC.
fn bookable_day() -> String {
    let mut days = SystemClock
        .wall()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86_400
        + 10;
    // 1970-01-01 was a Thursday; with Monday = 0 that is 3.
    if (days + 3) % 7 == 6 {
        days += 1;
    }
    let z = i64::try_from(days).unwrap() + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_booking_agent_checks_books_and_refuses_through_the_real_link() {
    let Some(python) = std::env::var_os("BRISVOX_PYTHON_SDK") else {
        let required = std::env::var("BRISVOX_REQUIRE_PYTHON_SDK")
            .is_ok_and(|value| !value.is_empty() && value != "0");
        assert!(
            !required,
            "BRISVOX_REQUIRE_PYTHON_SDK is set but BRISVOX_PYTHON_SDK names no Python 3.11+ with `websockets`"
        );
        eprintln!("skipped: set BRISVOX_PYTHON_SDK to a Python 3.11+ with `websockets`");
        return;
    };
    let agent = spawn_agent(&python.to_string_lossy());
    let day = bookable_day();

    let clock: SharedClock = SystemClock::shared();
    let mut config = AgentLinkConfig::new(agent.url.clone());
    // A Python process answers well inside the defaults; the margin keeps a busy CI box green.
    config.connect_timeout = Duration::from_secs(5);
    config.handshake_timeout = Duration::from_secs(5);
    let sim = SimSession::new(clock.clone());
    let book = |name: &str| {
        format!(r#"{{"service":"haircut","day":"{day}","start":"10:00","customer_name":"{name}"}}"#)
    };
    // Turn 1: the LLM asks for the free times (a `read_only` tool: enum and date arguments).
    sim.llm
        .push(LlmScript::text(["One moment."]).with_tool_call(
            "c1",
            "check_availability",
            &format!(r#"{{"service":"haircut","day":"{day}"}}"#),
        ));
    sim.llm.push(LlmScript::text(["Ten o'clock is free."]));
    // Turn 2: it books ten o'clock (a `side_effect` tool).
    sim.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "c2",
        "book_appointment",
        &book("Ann"),
    ));
    sim.llm.push(LlmScript::text(["You are booked."]));
    // Turn 3: another customer wants the same slot; the agent refuses in words the LLM can say.
    sim.llm.push(LlmScript::text(["Booking."]).with_tool_call(
        "c3",
        "book_appointment",
        &book("Bob"),
    ));
    sim.llm
        .push(LlmScript::text(["Sorry, that time is taken."]));

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
    .with_system_prompt("Fallback prompt.")
    .with_text_input(input);
    let call = LinkCall {
        session_id: 7,
        agent: "maple-hair-studio".to_owned(),
        clock: clock.clone(),
        cancel: cancel.clone(),
        ops: None,
        identity: None,
    };
    let (builder, link) = attach(&Arc::new(config), builder, call).await;
    let link = link.expect("the booking agent accepted the call");
    let call = run_call(session, builder, &cancel).await.unwrap();

    // The greeting is a bot line of the Python agent (`greeting=` of its `session.accept`).
    until("the greeting", || {
        call.handles().driver.snapshot().lines_started == 1
    })
    .await;

    text.try_send(TextTurn::now("what is free on that day for a haircut"))
        .unwrap();
    until("the availability answer", || sim.llm.requests().len() == 2).await;
    let first = &sim.llm.requests()[0];
    let mut names: Vec<String> = first.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "book_appointment",
            "cancel_booking",
            "check_availability",
            "find_booking",
            "list_services"
        ],
        "the example's five tools were accepted by the link"
    );
    let prompt = format!("{first:?}");
    assert!(
        prompt.contains("Today is 20") && prompt.contains("Maple Hair Studio"),
        "the agent's per-call prompt (with today's date) replaced the fallback: {prompt}"
    );
    let free = format!("{:?}", sim.llm.requests()[1].messages);
    assert!(
        free.contains("free_starts") && free.contains("09:00") && free.contains("more"),
        "the free times reached the LLM: {free}"
    );
    until("the bot is idle", || {
        call.handles().driver.snapshot().turns_ended == 1
    })
    .await;

    text.try_send(TextTurn::now("ten o'clock, my name is Ann"))
        .unwrap();
    until("the booking answer", || sim.llm.requests().len() == 4).await;
    let booked = format!("{:?}", sim.llm.requests()[3].messages);
    assert!(
        booked.contains("MH") && booked.contains("Ann") && booked.contains("10:30"),
        "the confirmation reached the LLM: {booked}"
    );
    until("the bot is idle again", || {
        call.handles().driver.snapshot().turns_ended == 2
    })
    .await;

    text.try_send(TextTurn::now("book the same time for Bob"))
        .unwrap();
    until("the refusal", || sim.llm.requests().len() == 6).await;
    let refused = format!("{:?}", sim.llm.requests()[5].messages);
    assert!(
        refused.contains("not available"),
        "the refusal reached the LLM verbatim: {refused}"
    );
    let stats = link.stats();
    assert_eq!(
        (stats.tool_calls, stats.tool_results),
        (3, 3),
        "all three tool calls were answered by Python: {stats:?}"
    );
    link.close("hangup").await;
    cancel.cancel();
}
