//! Tests of the reload hub on a fake target (no providers, no files): the triggers, the bound,
//! the single flight and the counters. The end-to-end ones, with real definitions, snapshots,
//! calls and MCP servers, are in `tests/reload.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::SimClock;

use super::*;

/// A target that counts what it is asked and answers what the test set.
struct Fake {
    applies: AtomicUsize,
    shutdowns: AtomicUsize,
    applies_at_shutdown: AtomicUsize,
    changed_calls: AtomicUsize,
    /// `changed()` answers `true` from this call on (1-based); 0: never.
    changed_from: usize,
    /// `apply()` takes this long (on the paused clock).
    apply_takes: Duration,
    result: ReloadResult,
    clock: SharedClock,
    busy: AtomicBool,
    overlapped: AtomicBool,
}

impl Fake {
    fn new(clock: &SharedClock) -> Self {
        Self {
            applies: AtomicUsize::new(0),
            shutdowns: AtomicUsize::new(0),
            applies_at_shutdown: AtomicUsize::new(0),
            changed_calls: AtomicUsize::new(0),
            changed_from: 0,
            apply_takes: Duration::ZERO,
            result: ReloadResult::Ok,
            clock: clock.clone(),
            busy: AtomicBool::new(false),
            overlapped: AtomicBool::new(false),
        }
    }
}

impl ReloadTarget for Fake {
    fn apply(&self) -> BoxFuture<'_, ReloadReport> {
        Box::pin(async move {
            if self.busy.swap(true, Ordering::SeqCst) {
                self.overlapped.store(true, Ordering::SeqCst);
            }
            if !self.apply_takes.is_zero() {
                self.clock.sleep(self.apply_takes).await;
            }
            let n = self.applies.fetch_add(1, Ordering::SeqCst) + 1;
            self.busy.store(false, Ordering::SeqCst);
            ReloadReport {
                result: self.result,
                generation: 1 + u64::try_from(n).unwrap(),
                message: "fake".to_owned(),
                slots: vec![SlotReport {
                    slot: "agent".to_owned(),
                    agent: "fake".to_owned(),
                    result: SlotResult::Applied,
                    detail: String::new(),
                }],
            }
        })
    }

    fn changed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let n = self.changed_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.changed_from != 0 && n >= self.changed_from
        })
    }

    fn generation(&self) -> u64 {
        1 + u64::try_from(self.applies.load(Ordering::SeqCst)).unwrap()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.applies_at_shutdown
                .store(self.applies.load(Ordering::SeqCst), Ordering::SeqCst);
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        })
    }
}

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn the_poll_interval_comes_from_the_environment() {
    assert_eq!(ReloadSettings::from_env(&env(&[])).unwrap().poll, None);
    for off in ["", "0", "  "] {
        let settings = ReloadSettings::from_env(&env(&[(ENV_AGENT_RELOAD_POLL_MS, off)])).unwrap();
        assert_eq!(settings.poll, None, "{off:?}");
    }
    let settings = ReloadSettings::from_env(&env(&[(ENV_AGENT_RELOAD_POLL_MS, "250")])).unwrap();
    assert_eq!(settings.poll, Some(Duration::from_millis(250)));
    for bad in ["fast", "1.5", "-3", "99", "86400001", "250ms"] {
        let err = ReloadSettings::from_env(&env(&[(ENV_AGENT_RELOAD_POLL_MS, bad)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains(ENV_AGENT_RELOAD_POLL_MS), "{bad}: {err}");
    }
}

#[test]
fn the_report_says_what_the_endpoint_answers() {
    let report = |result| ReloadReport {
        result,
        generation: 3,
        message: "m \"quoted\"".to_owned(),
        slots: vec![SlotReport {
            slot: "agent".to_owned(),
            agent: "a".to_owned(),
            result: SlotResult::Invalid,
            detail: "line 1\nline 2".to_owned(),
        }],
    };
    let statuses = [
        (ReloadResult::Ok, 200),
        (ReloadResult::Unchanged, 200),
        (ReloadResult::Invalid, 422),
        (ReloadResult::NeedsRestart, 409),
        (ReloadResult::TimedOut, 504),
        (ReloadResult::Closed, 503),
    ];
    for (result, status) in statuses {
        assert_eq!(report(result).http_status(), status, "{result:?}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&report(ReloadResult::Invalid).to_json()).unwrap();
    assert_eq!(json["result"], "invalid");
    assert_eq!(json["generation"], 3);
    assert_eq!(json["message"], "m \"quoted\"");
    assert_eq!(json["agents"][0]["slot"], "agent");
    assert_eq!(json["agents"][0]["result"], "invalid");
    assert_eq!(json["agents"][0]["detail"], "line 1\nline 2");
}

#[tokio::test(start_paused = true)]
async fn a_reload_is_counted_by_its_result_and_publishes_the_generation() {
    let clock = SimClock::shared();
    let obs = Arc::new(Observability::new());
    let mut fake = Fake::new(&clock);
    fake.result = ReloadResult::Ok;
    let fake = Arc::new(fake);
    let hub = ReloadHub::new(fake.clone(), clock).with_observability(Arc::clone(&obs));
    assert_eq!(obs.metrics().agent_generation(), 1);

    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Ok);
    assert_eq!(obs.metrics().agent_reloads("ok"), 1);
    assert_eq!(obs.metrics().agent_generation(), 2);
    let text = obs.render(
        &crate::SessionManager::new(crate::ManagerConfig::new(1).unwrap(), SimClock::shared())
            .unwrap(),
    );
    assert!(
        text.contains("brisvox_agent_reloads_total{result=\"ok\"} 1"),
        "{text}"
    );
    assert!(text.contains("brisvox_agent_generation 2"), "{text}");

    // Each result has its own series; a refused reload does not move the generation.
    for (result, label) in [
        (ReloadResult::Invalid, "invalid"),
        (ReloadResult::NeedsRestart, "needs_restart"),
        (ReloadResult::Unchanged, "unchanged"),
    ] {
        let obs = Arc::new(Observability::new());
        let mut fake = Fake::new(&SimClock::shared());
        fake.result = result;
        let hub =
            ReloadHub::new(Arc::new(fake), SimClock::shared()).with_observability(Arc::clone(&obs));
        hub.reload(ReloadTrigger::Signal).await;
        assert_eq!(obs.metrics().agent_reloads(label), 1, "{label}");
        assert_eq!(obs.metrics().agent_reloads("ok"), 0, "{label}");
        assert_eq!(obs.metrics().agent_generation(), 1, "{label}");
    }
}

#[tokio::test(start_paused = true)]
async fn reloads_run_one_at_a_time() {
    let clock = SimClock::shared();
    let mut fake = Fake::new(&clock);
    fake.apply_takes = Duration::from_millis(50);
    let fake = Arc::new(fake);
    let hub = Arc::new(ReloadHub::new(fake.clone(), clock));
    let mut runs = Vec::new();
    for trigger in [
        ReloadTrigger::Endpoint,
        ReloadTrigger::Signal,
        ReloadTrigger::Poll,
    ] {
        let hub = Arc::clone(&hub);
        runs.push(tokio::spawn(async move { hub.reload(trigger).await }));
    }
    for run in runs {
        assert_eq!(run.await.unwrap().result, ReloadResult::Ok);
    }
    assert_eq!(fake.applies.load(Ordering::SeqCst), 3);
    assert!(
        !fake.overlapped.load(Ordering::SeqCst),
        "two reloads ran at once"
    );
}

#[tokio::test(start_paused = true)]
async fn a_reload_that_takes_too_long_is_given_up_and_the_next_one_can_run() {
    let clock = SimClock::shared();
    let mut fake = Fake::new(&clock);
    fake.apply_takes = Duration::from_secs(500);
    let fake = Arc::new(fake);
    let obs = Arc::new(Observability::new());
    let hub = ReloadHub::new(fake.clone(), clock.clone())
        .with_timeout(Duration::from_secs(10))
        .with_observability(Arc::clone(&obs));
    let started = clock.now();
    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::TimedOut);
    assert_eq!(report.http_status(), 504);
    assert!(
        report.message.contains("old definition stays"),
        "{report:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(started),
        Duration::from_secs(10)
    );
    assert_eq!(
        fake.applies.load(Ordering::SeqCst),
        0,
        "dropped, not finished"
    );
    assert_eq!(obs.metrics().agent_reloads("invalid"), 1);
    // The gate was released with the dropped future: nothing is stuck.
    let again = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(again.result, ReloadResult::TimedOut);
}

#[tokio::test(start_paused = true)]
async fn the_file_poll_runs_on_the_injected_clock() {
    let clock = SimClock::shared();
    let mut fake = Fake::new(&clock);
    // Changed from the third check on.
    fake.changed_from = 3;
    let fake = Arc::new(fake);
    let hub = Arc::new(ReloadHub::new(fake.clone(), clock));
    hub.watch_files(Duration::from_secs(2));

    // Nothing happens until the clock moves: the interval is simulated time, not wall time.
    tokio::task::yield_now().await;
    assert_eq!(fake.changed_calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_millis(2_100)).await;
    tokio::task::yield_now().await;
    assert_eq!(fake.changed_calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(fake.changed_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fake.applies.load(Ordering::SeqCst), 0);
    // The third check sees the change: one reload.
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fake.changed_calls.load(Ordering::SeqCst), 3);
    assert_eq!(fake.applies.load(Ordering::SeqCst), 1);

    // Closing the hub stops the poll.
    hub.close();
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fake.changed_calls.load(Ordering::SeqCst), 3);
    hub.shutdown().await;
    assert_eq!(fake.shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_closed_hub_refuses_and_counts_nothing() {
    let clock = SimClock::shared();
    let fake = Arc::new(Fake::new(&clock));
    let obs = Arc::new(Observability::new());
    let hub = ReloadHub::new(fake.clone(), clock).with_observability(Arc::clone(&obs));
    hub.close();
    let report = hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(report.result, ReloadResult::Closed);
    assert_eq!(report.http_status(), 503);
    assert_eq!(fake.applies.load(Ordering::SeqCst), 0);
    for label in crate::observability::registry::RELOAD_RESULTS {
        assert_eq!(obs.metrics().agent_reloads(label), 0, "{label}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sighup_reloads() {
    let clock = brisvox_core::SystemClock::shared();
    let fake = Arc::new(Fake::new(&clock));
    let hub = Arc::new(ReloadHub::new(fake.clone(), clock.clone()));
    hub.watch_hangup().unwrap();
    // SAFETY-free: raise the signal through the shell, which the process handles.
    let status = std::process::Command::new("kill")
        .args(["-HUP", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    for _ in 0..200 {
        if fake.applies.load(Ordering::SeqCst) == 1 {
            break;
        }
        clock.sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fake.applies.load(Ordering::SeqCst), 1);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// The poll: a change is asked for once (the state machine of `AgentSet::changed`, on a paused clock)

fn moved(hash: u64) -> Option<Observation> {
    Some(Observation::Moved {
        stat: None,
        hash: Some(hash),
    })
}

fn state(loaded: u64) -> SlotState {
    SlotState {
        loaded,
        stat: None,
        seen: None,
        rejected: None,
    }
}

#[test]
fn a_changed_file_is_asked_for_once_when_it_stayed_the_same_for_an_interval() {
    let mut s = state(1);
    // First sight of a different text: wait one more interval (an editor may still be saving).
    assert!(!s.observe(moved(2)));
    // Unchanged since: now it is stable, ask for the reload, once.
    assert!(s.observe(Some(Observation::Same)));
    assert!(!s.observe(Some(Observation::Same)));
    assert!(!s.observe(Some(Observation::Same)));
    // The same text seen again (touched, same content) is not asked for again.
    assert!(!s.observe(moved(2)));
    assert!(!s.observe(Some(Observation::Same)));
    // A different text is.
    assert!(!s.observe(moved(3)));
    assert!(s.observe(Some(Observation::Same)));
    // Going back to the text that is live is not a change; unreadable files conclude nothing.
    let mut s = state(1);
    assert!(!s.observe(moved(2)));
    assert!(!s.observe(moved(1)));
    assert!(!s.observe(Some(Observation::Same)));
    assert!(!s.observe(Some(Observation::Gone)));
    assert!(!s.observe(None));
}

/// A target whose `changed()` is the real poll state machine fed with scripted observations,
/// and whose `apply()` refuses (or hangs) as the test wants.
struct Polled {
    state: Mutex<SlotState>,
    script: Mutex<std::collections::VecDeque<Observation>>,
    started: AtomicUsize,
    result: ReloadResult,
    takes: Duration,
    clock: SharedClock,
}

impl Polled {
    fn new(clock: &SharedClock, result: ReloadResult, takes: Duration) -> Self {
        Self {
            state: Mutex::new(state(1)),
            script: Mutex::new(std::collections::VecDeque::from([Observation::Moved {
                stat: None,
                hash: Some(2),
            }])),
            started: AtomicUsize::new(0),
            result,
            takes,
            clock: clock.clone(),
        }
    }
}

impl ReloadTarget for Polled {
    fn apply(&self) -> BoxFuture<'_, ReloadReport> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            if !self.takes.is_zero() {
                self.clock.sleep(self.takes).await;
            }
            ReloadReport {
                result: self.result,
                generation: 1,
                message: "refused".to_owned(),
                slots: Vec::new(),
            }
        })
    }

    fn changed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            // After the script the file stays as it is.
            let next = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Observation::Same);
            self.state.lock().unwrap().observe(Some(next))
        })
    }

    fn generation(&self) -> u64 {
        1
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Moves the paused clock by `by` in one-second steps, letting the tasks run after each.
async fn run_for(by: Duration) {
    for _ in 0..by.as_secs() {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_poll_does_not_retry_a_file_that_needs_a_restart() {
    let clock = SimClock::shared();
    let target = Arc::new(Polled::new(
        &clock,
        ReloadResult::NeedsRestart,
        Duration::ZERO,
    ));
    let obs = Arc::new(Observability::new());
    let hub = Arc::new(ReloadHub::new(target.clone(), clock).with_observability(Arc::clone(&obs)));
    hub.watch_files(Duration::from_secs(1));
    run_for(Duration::from_secs(20)).await;
    assert_eq!(target.started.load(Ordering::SeqCst), 1, "asked once");
    assert_eq!(obs.metrics().agent_reloads("needs_restart"), 1);
    // The endpoint and SIGHUP can always ask again.
    hub.reload(ReloadTrigger::Endpoint).await;
    assert_eq!(target.started.load(Ordering::SeqCst), 2);
    assert_eq!(obs.metrics().agent_reloads("needs_restart"), 2);
    hub.close();
}

#[tokio::test(start_paused = true)]
async fn the_poll_does_not_rerun_a_reload_that_timed_out() {
    let clock = SimClock::shared();
    // A reload that never finishes in the hub's bound (a slow MCP server).
    let target = Arc::new(Polled::new(
        &clock,
        ReloadResult::Ok,
        Duration::from_secs(500),
    ));
    let obs = Arc::new(Observability::new());
    let hub = Arc::new(
        ReloadHub::new(target.clone(), clock)
            .with_timeout(Duration::from_secs(5))
            .with_observability(Arc::clone(&obs)),
    );
    hub.watch_files(Duration::from_secs(1));
    run_for(Duration::from_secs(60)).await;
    assert_eq!(target.started.load(Ordering::SeqCst), 1, "asked once");
    assert_eq!(
        obs.metrics().agent_reloads("invalid"),
        1,
        "the timeout, once"
    );
    hub.close();
}

#[tokio::test(start_paused = true)]
async fn a_new_edit_after_a_refusal_is_asked_for_again() {
    let clock = SimClock::shared();
    let target = Arc::new(Polled::new(&clock, ReloadResult::Invalid, Duration::ZERO));
    let hub = Arc::new(ReloadHub::new(target.clone(), clock));
    hub.watch_files(Duration::from_secs(1));
    run_for(Duration::from_secs(10)).await;
    assert_eq!(target.started.load(Ordering::SeqCst), 1);
    target.script.lock().unwrap().push_back(Observation::Moved {
        stat: None,
        hash: Some(3),
    });
    run_for(Duration::from_secs(10)).await;
    assert_eq!(target.started.load(Ordering::SeqCst), 2);
    hub.close();
}

// ---------------------------------------------------------------------------------------------
// A reload racing the shutdown

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_a_reload_in_progress_before_closing_the_live_snapshots() {
    let clock = SimClock::shared();
    let mut fake = Fake::new(&clock);
    fake.apply_takes = Duration::from_millis(500);
    let fake = Arc::new(fake);
    let hub = Arc::new(ReloadHub::new(fake.clone(), clock));
    let reload = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move { hub.reload(ReloadTrigger::Endpoint).await })
    };
    tokio::task::yield_now().await;
    let shutdown = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move { hub.shutdown().await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        fake.shutdowns.load(Ordering::SeqCst),
        0,
        "the live snapshots stay open while a reload is running"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    reload.await.unwrap();
    shutdown.await.unwrap();
    assert_eq!(fake.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.applies_at_shutdown.load(Ordering::SeqCst),
        1,
        "the reload finished before the shutdown closed anything"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_does_not_wait_for_a_stuck_reload_longer_than_its_bound() {
    let clock = SimClock::shared();
    let mut fake = Fake::new(&clock);
    fake.apply_takes = Duration::from_secs(500);
    let fake = Arc::new(fake);
    let hub = Arc::new(ReloadHub::new(fake.clone(), clock.clone()));
    let reload = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move { hub.reload(ReloadTrigger::Endpoint).await })
    };
    tokio::task::yield_now().await;
    let started = clock.now();
    hub.shutdown().await;
    let waited = clock.now().saturating_duration_since(started);
    assert!(
        waited <= RELOAD_STOP_TIMEOUT + WATCHER_STOP_TIMEOUT,
        "{waited:?}"
    );
    assert_eq!(fake.shutdowns.load(Ordering::SeqCst), 1);
    reload.abort();
}

#[cfg(all(
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
mod vetoes {
    use super::*;

    fn agent(extra: &str) -> AgentConfig {
        let text = format!(
            r#"
[agent]
name = "veto-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "K"

[llm]
provider = "openai"
api_key_env = "K"

[tts]
provider = "cartesia"
api_key_env = "K"
voice = "v"

{extra}
"#
        );
        config::load_str(&text, "agent.toml", &env(&[("K", "key")])).unwrap()
    }

    #[test]
    fn a_changed_recording_table_needs_a_restart() {
        let live = agent("");
        assert_eq!(recording_veto(&live, &agent("")), None);
        let changed = agent("[recording]\naudio = true\n");
        let reason = recording_veto(&live, &changed).unwrap();
        assert!(reason.contains("[recording]"), "{reason}");
        assert_eq!(recording_veto(&changed, &changed), None);
    }

    #[test]
    fn a_provider_whose_limit_the_start_dropped_cannot_come_in_by_a_reload() {
        // The start dropped a limit for `cartesia` (no call used it then).
        let veto = provider_limit_veto(vec!["cartesia".to_owned()]);
        let reason = veto(&agent(""), &agent("")).unwrap();
        assert!(reason.contains("cartesia") && reason.contains("BRISVOX_PROVIDER_LIMITS"));
        // A limit for something the definition does not use does not matter.
        let veto = provider_limit_veto(vec!["gladia".to_owned()]);
        assert_eq!(veto(&agent(""), &agent("")), None);
        assert_eq!(
            provider_limit_veto(Vec::new())(&agent(""), &agent("")),
            None
        );
    }

    #[test]
    fn an_adaptive_turn_policy_needs_the_model_the_start_loaded() {
        // The default policy is adaptive.
        let adaptive = agent("");
        let fixed = agent(
            "[turn]
policy = \"fixed_silence\"
silence_ms = 800
",
        );
        let vars: Arc<dyn Env + Send + Sync> = Arc::new(env(&[]));
        // With a model nothing is held back.
        assert_eq!(
            smart_turn_veto(Arc::clone(&vars), true)(&fixed, &adaptive),
            None
        );
        // Without one, an adaptive policy is held back where a model could be loaded.
        let veto = smart_turn_veto(vars, false);
        assert_eq!(veto(&fixed, &fixed), None);
        let held = veto(&fixed, &adaptive);
        assert_eq!(held.is_some(), cfg!(feature = "smart-turn-ort"), "{held:?}");
        // Switched off in the environment: nothing to load.
        let off: Arc<dyn Env + Send + Sync> = Arc::new(env(&[("BRISVOX_SMART_TURN_MODEL", "off")]));
        assert_eq!(smart_turn_veto(off, false)(&fixed, &adaptive), None);
    }
}
