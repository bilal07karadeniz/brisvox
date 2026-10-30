//! Agent definition reload without a restart (`docs/OPERATIONS.md`, "Reloading the agent"), end
//! to end on real sockets: new calls run the new definition, calls in progress keep theirs, an
//! invalid file keeps the old definition and says where it is wrong, a change that needs a restart
//! is refused, the admin endpoint is authenticated, the file poll reloads on change, and a reload
//! applies completely or not at all. The MCP side (the old definition's servers close when its
//! last call ends) is in `tests/reload_mcp.rs`.
#![cfg(all(
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

#[path = "support/reload_rig.rs"]
mod rig;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_server::reload::{AgentSet, ReloadHub, ReloadResult, ReloadTrigger, SlotResult};
use futures::SinkExt;
use serde_json::Value;

use rig::{Rig, agent_text, asked, calls_of, eventually};

fn one() -> String {
    agent_text("one", "You are agent ONE.", "")
}

fn two() -> String {
    agent_text("two", "You are agent TWO.", "")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_gives_new_calls_the_new_definition_and_calls_in_progress_keep_theirs() {
    let rig = Rig::start("swap", &one()).await;
    let mut old = rig.rtvi_call().await;
    rig.until("the first call runs", |m| m.stats().active == 1)
        .await;
    assert_eq!(calls_of(&rig.seen, "one"), 1);

    // The operator edits the file and reloads.
    rig.file.write(&two());
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    assert_eq!(report.generation, 2);
    assert_eq!(report.slots[0].result, SlotResult::Applied);
    assert!(
        report.slots[0].detail.contains("one -> two")
            && report.slots[0].detail.contains("1 call(s)"),
        "{report:?}"
    );
    assert_eq!(rig.swap.current().agent().name(), "two");

    // The call that was in progress goes on with its own definition...
    rig.say(&mut old, "hello old").await;
    let ran_one = asked(&rig.seen, "one");
    assert!(ran_one.contains("You are agent ONE."), "{ran_one}");
    assert!(ran_one.contains("hello old"), "{ran_one}");
    assert_eq!(calls_of(&rig.seen, "two"), 0);

    // ...a call that starts now gets the new one.
    let mut new = rig.rtvi_call().await;
    rig.say(&mut new, "hello new").await;
    let ran_two = asked(&rig.seen, "two");
    assert!(ran_two.contains("You are agent TWO."), "{ran_two}");
    assert!(ran_two.contains("hello new"), "{ran_two}");
    assert!(!ran_two.contains("ONE") && !ran_two.contains("hello old"));
    assert_eq!(
        calls_of(&rig.seen, "one"),
        1,
        "the old call did not restart"
    );

    // The reload was counted and the generation published.
    assert_eq!(rig.obs.metrics().agent_reloads("ok"), 1);
    assert_eq!(rig.obs.metrics().agent_generation(), 2);

    // Both calls end normally; the server drains clean.
    old.send(rig::rtvi_message("disconnect-bot", "bye-1", &Value::Null))
        .await
        .unwrap();
    new.send(rig::rtvi_message("disconnect-bot", "bye-2", &Value::Null))
        .await
        .unwrap();
    rig.until("both calls ended", |m| m.stats().active == 0)
        .await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_file_keeps_the_old_definition_and_reports_where_it_is_wrong() {
    let rig = Rig::start("invalid", &one()).await;

    // A syntax error.
    rig.file
        .write(&format!("{}\n[agent\nname = broken\n", one()));
    let report = rig.hub.reload(ReloadTrigger::Signal).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");
    assert_eq!(report.http_status(), 422);
    assert_eq!(report.generation, 1);
    assert_eq!(report.slots[0].result, SlotResult::Invalid);
    // The error names the file and a line.
    let file = rig.file.path.display().to_string();
    assert!(report.message.contains(&file), "{}", report.message);
    assert!(
        report.slots[0].detail.contains("agent.toml:"),
        "{}",
        report.slots[0].detail
    );

    // A file that parses but is wrong (an unknown provider setting).
    rig.file.write(&agent_text(
        "two",
        "You are agent TWO.",
        "[llm.nonsense]\nx = 1\n",
    ));
    let report = rig.hub.reload(ReloadTrigger::Signal).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");

    // A secret the new file names that the environment lacks is an error too, like at the start.
    rig.file.write(
        &one()
            .replace("MCP_TEST_LLM_KEY", "NO_SUCH_KEY")
            .replace("ONE", "TWO"),
    );
    let report = rig.hub.reload(ReloadTrigger::Signal).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");
    assert!(report.message.contains("NO_SUCH_KEY"), "{}", report.message);

    // The old definition still runs new calls.
    assert_eq!(rig.swap.current().agent().name(), "one");
    let mut call = rig.rtvi_call().await;
    rig.say(&mut call, "still there").await;
    assert!(asked(&rig.seen, "one").contains("You are agent ONE."));
    assert_eq!(rig.obs.metrics().agent_reloads("invalid"), 3);
    assert_eq!(rig.obs.metrics().agent_reloads("ok"), 0);
    assert_eq!(rig.obs.metrics().agent_generation(), 1);

    // Fixing the file is enough: no restart, no stuck state.
    rig.file.write(&two());
    let report = rig.hub.reload(ReloadTrigger::Signal).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    assert_eq!(report.generation, 2);
    call.send(rig::rtvi_message("disconnect-bot", "bye", &Value::Null))
        .await
        .unwrap();
    rig.until("the call ended", |m| m.stats().active == 0).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_did_not_change_is_not_rebuilt() {
    let rig = Rig::start("same", &one()).await;
    let before = rig.swap.current();
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Unchanged, "{report:?}");
    assert_eq!(report.http_status(), 200);
    assert_eq!(report.generation, 1);
    assert!(
        Arc::ptr_eq(&before, &rig.swap.current()),
        "an unchanged file must not build a new snapshot"
    );
    assert_eq!(rig.obs.metrics().agent_reloads("unchanged"), 1);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_that_needs_a_restart_is_refused_and_changes_nothing() {
    let rig = Rig::start("restart", &one()).await;
    // Recording is set up when the server starts (directory, sweeper): the table cannot change.
    rig.file.write(&agent_text(
        "two",
        "You are agent TWO.",
        "[recording]\naudio = true\n",
    ));
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::NeedsRestart, "{report:?}");
    assert_eq!(report.http_status(), 409);
    assert_eq!(report.slots[0].result, SlotResult::NeedsRestart);
    assert!(report.message.contains("[recording]"), "{}", report.message);
    assert_eq!(rig.swap.current().agent().name(), "one");
    assert_eq!(report.generation, 1);
    assert_eq!(rig.obs.metrics().agent_reloads("needs_restart"), 1);
    // Taking the table out again is a plain unchanged file relative to the live one? No: the
    // text differs from what was loaded, and the definition is equal to the live one except
    // for the name, so it applies.
    rig.file.write(&two());
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_endpoint_reloads_with_its_token_only() {
    let rig = Rig::start_with("endpoint", &one(), Some("admin-secret")).await;
    rig.file.write(&two());

    let (status, _) = rig.post_reload("").await;
    assert!(status.contains("401"), "{status}");
    let (status, _) = rig
        .post_reload("Authorization: Bearer not-the-token\r\n")
        .await;
    assert!(status.contains("401"), "{status}");
    assert_eq!(
        rig.swap.current().agent().name(),
        "one",
        "no token, no reload"
    );

    let (status, body) = rig
        .post_reload("Authorization: Bearer admin-secret\r\n")
        .await;
    assert!(status.contains("200"), "{status}: {body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["result"], "ok", "{body}");
    assert_eq!(json["generation"], 2);
    assert_eq!(json["agents"][0]["slot"], "agent");
    assert_eq!(json["agents"][0]["agent"], "two");
    assert_eq!(rig.swap.current().agent().name(), "two");

    // An invalid file through the endpoint: 422 with the error in the body.
    rig.file.write("not [valid toml");
    let (status, body) = rig
        .post_reload("Authorization: Bearer admin-secret\r\n")
        .await;
    assert!(status.contains("422"), "{status}: {body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["result"], "invalid");
    assert!(json["message"].as_str().unwrap().contains("agent.toml"));
    assert_eq!(rig.swap.current().agent().name(), "two");

    // The scrape endpoint shows the reloads (open on loopback).
    let (status, body) = rig
        .http("GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(status.contains("200"), "{status}");
    assert!(
        body.contains("brisvox_agent_reloads_total{result=\"ok\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("brisvox_agent_reloads_total{result=\"invalid\"} 1"),
        "{body}"
    );
    assert!(body.contains("brisvox_agent_generation 2"), "{body}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_loopback_without_a_token_the_endpoint_works_for_local_processes_not_browsers() {
    let rig = Rig::start("local", &one()).await;
    rig.file.write(&two());
    let (status, _) = rig.post_reload("Origin: http://evil.example\r\n").await;
    assert!(status.contains("403"), "{status}");
    assert_eq!(rig.swap.current().agent().name(), "one");
    let (status, body) = rig.post_reload("").await;
    assert!(status.contains("200"), "{status}: {body}");
    assert_eq!(rig.swap.current().agent().name(), "two");
    let (status, _) = rig
        .http("GET /admin/reload HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert!(status.contains("405"), "{status}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_poll_picks_up_an_edit_reports_a_bad_one_and_picks_up_the_fix() {
    // What the poll does when nothing changes, or when a refused file stays refused, is tested
    // on a paused clock in `src/reload/tests.rs`; here only what has to happen does, with
    // generous bounds.
    let rig = Rig::start("poll", &one()).await;
    rig.hub.watch_files(Duration::from_millis(100));

    rig.file.write(&two());
    eventually("the poll to reload the edited file", 20, || {
        rig.swap.current().agent().name() == "two"
    })
    .await;
    assert_eq!(rig.hub.generation(), 2);

    // An invalid edit is reported (counted) ...
    rig.file
        .write("this = = is not toml, and it is long enough to differ");
    eventually("the invalid file to be reported", 20, || {
        rig.obs.metrics().agent_reloads("invalid") >= 1
    })
    .await;
    assert_eq!(rig.swap.current().agent().name(), "two");

    // ... and fixing the file is enough.
    rig.file
        .write(&agent_text("three", "You are agent THREE.", ""));
    eventually("the poll to reload the fixed file", 20, || {
        rig.swap.current().agent().name() == "three"
    })
    .await;
    assert_eq!(rig.hub.generation(), 3);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_that_was_shut_down_swaps_nothing_in() {
    use brisvox_server::reload::ReloadTarget;
    let clock = SystemClock::shared();
    let env = Arc::new(rig::keys());
    let seen: rig::Seen = Arc::default();
    let obs = Arc::new(brisvox_server::Observability::new());
    let pool = rig::inference_pool(&clock);
    let file = rig::AgentFile::new("closed", &one());
    let (slot, swap) = rig::slot("agent", &file, &env, &seen, &clock, &pool, &obs).await;
    let set = AgentSet::new(
        vec![slot],
        env as Arc<dyn brisvox_server::config::Env + Send + Sync>,
    );
    file.write(&two());
    // The process is shutting down (the live snapshots were closed): a reload that gets here
    // anyway builds nothing that stays and swaps nothing in.
    set.shutdown().await;
    let report = set.apply().await;
    assert_eq!(report.result, ReloadResult::Closed, "{report:?}");
    assert_eq!(swap.current().agent().name(), "one");
    assert_eq!(set.generation(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_definitions_reload_completely_or_not_at_all() {
    let clock = SystemClock::shared();
    let env = Arc::new(rig::keys());
    let seen: rig::Seen = Arc::default();
    let obs = Arc::new(brisvox_server::Observability::new());
    let pool = rig::inference_pool(&clock);
    let main = rig::AgentFile::new("two-main", &one());
    let ws = rig::AgentFile::new("two-ws", &agent_text("ws-one", "You are WS ONE.", ""));
    let (main_slot, main_swap) = rig::slot("agent", &main, &env, &seen, &clock, &pool, &obs).await;
    let (ws_slot, ws_swap) =
        rig::slot("websocket_agent", &ws, &env, &seen, &clock, &pool, &obs).await;
    let set = AgentSet::new(
        vec![main_slot, ws_slot],
        env as Arc<dyn brisvox_server::config::Env + Send + Sync>,
    );
    let hub = ReloadHub::new(Arc::new(set), clock).with_observability(Arc::clone(&obs));

    // The main definition is fine, the websocket one is not: neither changes.
    main.write(&two());
    ws.write("broken = = file");
    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Invalid, "{report:?}");
    assert_eq!(report.slots[0].result, SlotResult::Held, "{report:?}");
    assert_eq!(report.slots[1].result, SlotResult::Invalid, "{report:?}");
    assert_eq!(main_swap.current().agent().name(), "one");
    assert_eq!(ws_swap.current().agent().name(), "ws-one");
    assert_eq!(report.generation, 1);

    // Both fixed: both change, in one generation.
    ws.write(&agent_text("ws-two", "You are WS TWO.", ""));
    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    assert_eq!(report.generation, 2);
    assert_eq!(main_swap.current().agent().name(), "two");
    assert_eq!(ws_swap.current().agent().name(), "ws-two");

    // Only one file changes: only that one is rebuilt.
    let main_before = main_swap.current();
    ws.write(&agent_text("ws-three", "You are WS THREE.", ""));
    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok, "{report:?}");
    assert_eq!(report.slots[0].result, SlotResult::Unchanged);
    assert_eq!(report.slots[1].result, SlotResult::Applied);
    assert!(Arc::ptr_eq(&main_before, &main_swap.current()));
    assert_eq!(ws_swap.current().agent().name(), "ws-three");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_during_the_drain_is_refused() {
    let mut rig = Rig::start("drain", &one()).await;
    let mut call = rig.rtvi_call().await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.file.write(&two());

    let (trigger, mut signals) = brisvox_server::ShutdownSignals::manual();
    let server = rig.server.take().unwrap();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(brisvox_server::ShutdownSignal::Requested));
    rig.until("the drain to begin", |m| !m.is_accepting()).await;

    // New calls are refused, so a new definition has no use: no reload, and it says why.
    let report = rig.hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Closed, "{report:?}");
    assert_eq!(report.http_status(), 503);
    let (status, body) = rig.post_reload("").await;
    assert!(status.contains("503"), "{status}: {body}");
    assert_eq!(rig.swap.current().agent().name(), "one");

    call.send(rig::rtvi_message("disconnect-bot", "bye", &Value::Null))
        .await
        .unwrap();
    let report = tokio::time::timeout(rig::WAIT, run).await.unwrap().unwrap();
    assert_eq!(
        report.outcome(),
        brisvox_server::DrainOutcome::Drained,
        "{report:?}"
    );
}
