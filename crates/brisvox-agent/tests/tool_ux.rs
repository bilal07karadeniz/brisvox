//! Tool UX: the filler policy of the tool layer, and what happens to tool calls when the
//! caller interrupts (ReadOnly calls are cancelled and counted; SideEffect calls complete exactly
//! once, their result is reported, and a regenerated identical call of a later turn does not run
//! them again). Paused time with `SimClock`, so every duration below is exact.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_agent::Assembled;
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::tools::{
    ClipKey, DEFAULT_FILLER_DELAY, DEFAULT_FILLER_TEXT, EngineConfig, Execution, FillerConfig,
    FillerPhrase, FillerPolicy, SessionId, StaticClips, Tool, ToolCallCtx, ToolEngine, ToolFuture,
    ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);
const SECOND: Duration = Duration::from_secs(1);

/// A tool that waits `delay` on the injected clock and answers `{"booked": args}`. Counts runs.
#[derive(Clone)]
struct Slow {
    clock: SharedClock,
    delay: Duration,
    runs: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl Slow {
    fn new(clock: &SharedClock, delay: Duration) -> Self {
        Self {
            clock: Arc::clone(clock),
            delay,
            runs: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Tool for Slow {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.clock.sleep(self.delay).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::Json(json!({ "booked": args })))
        })
    }
}

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

fn object_schema() -> Value {
    json!({ "type": "object" })
}

fn call(turn: u32, seq: u32, name: &str, arguments: Value) -> Assembled {
    Assembled::Complete(ToolCall {
        id: ToolCallId::new(epoch(turn), seq),
        provider_id: None,
        name: Arc::from(name),
        arguments,
        provider_data: None,
    })
}

/// The tool under test, `book`, with `kind` and a 30 s timeout, over a fresh engine.
struct Rig {
    clock: SharedClock,
    engine: Arc<ToolEngine>,
    tool: Slow,
}

impl Rig {
    fn new(kind: ToolKind, delay: Duration, config: EngineConfig) -> Self {
        let clock: SharedClock = Arc::new(SimClock::new());
        let tool = Slow::new(&clock, delay);
        let mut registry = ToolRegistry::new();
        registry
            .register(
                ToolSpec::new("book", "books", object_schema(), kind)
                    .with_timeout(Duration::from_secs(30)),
                tool.clone(),
            )
            .unwrap();
        let engine = Arc::new(ToolEngine::new(
            Arc::new(registry),
            SessionId::new(7),
            Arc::clone(&clock),
            config,
        ));
        Self {
            clock,
            engine,
            tool,
        }
    }

    /// Runs one `book` call of `turn` to the end; the execution counts as delivered to the
    /// context.
    async fn run(&self, turn: u32, seq: u32, args: Value) -> Execution {
        let token = CancellationToken::new();
        let mut execution = self
            .engine
            .execute(vec![call(turn, seq, "book", args)], epoch(turn), &token)
            .await;
        execution.mark_delivered();
        execution
    }

    /// Runs one `book` call of `turn` and cancels the turn `after` into it (a barge-in).
    async fn run_interrupted(
        &self,
        turn: u32,
        seq: u32,
        args: Value,
        after: Duration,
    ) -> Execution {
        let token = CancellationToken::new();
        let engine = Arc::clone(&self.engine);
        let run_token = token.clone();
        let handle = tokio::spawn(async move {
            engine
                .execute(vec![call(turn, seq, "book", args)], epoch(turn), &run_token)
                .await
        });
        self.clock.sleep(after).await;
        token.cancel();
        let mut execution = handle.await.unwrap();
        execution.mark_delivered();
        execution
    }
}

fn kind_of(execution: &Execution, index: usize) -> Option<String> {
    match &execution.results[index] {
        Message::Tool {
            content,
            is_error: true,
            ..
        } => serde_json::from_str::<Value>(content)
            .ok()
            .and_then(|body| body["error"].as_str().map(str::to_owned)),
        _ => None,
    }
}

fn content_of(execution: &Execution, index: usize) -> String {
    execution.results[index].text().to_owned()
}

// ---- the filler policy --------------------------------------------------------------------

fn registry_with(tools: &[(&str, Option<Duration>)]) -> ToolRegistry {
    let clock: SharedClock = Arc::new(SimClock::new());
    let mut registry = ToolRegistry::new();
    for (name, filler_after) in tools {
        registry
            .register(
                ToolSpec::new(*name, "t", object_schema(), ToolKind::ReadOnly)
                    .with_filler_after(*filler_after),
                Slow::new(&clock, Duration::ZERO),
            )
            .unwrap();
    }
    registry
}

#[test]
fn the_default_policy_fills_after_1200_ms_whatever_the_tools_own_delay_is() {
    assert_eq!(DEFAULT_FILLER_DELAY, 1200 * MS);
    let registry = registry_with(&[("a", Some(300 * MS)), ("b", None)]);
    let policy = FillerPolicy::new();
    let plan = policy
        .plan(&registry, &[call(1, 0, "a", json!({}))])
        .unwrap();
    assert_eq!((plan.after, &*plan.tool), (1200 * MS, "a"));
    // A tool that opted out (`filler_after: None`) never fills.
    assert_eq!(policy.plan(&registry, &[call(1, 0, "b", json!({}))]), None);
    // With a fixed delay set by the caller.
    let plan = FillerPolicy::new()
        .with_delay(2 * SECOND)
        .plan(&registry, &[call(1, 0, "a", json!({}))])
        .unwrap();
    assert_eq!(plan.after, 2 * SECOND);
}

#[test]
fn the_per_tool_policy_uses_the_smallest_tool_delay_and_the_first_call_wins_a_tie() {
    let registry = registry_with(&[
        ("slow", Some(900 * MS)),
        ("quick", Some(400 * MS)),
        ("also_quick", Some(400 * MS)),
        ("silent", None),
    ]);
    let policy = FillerPolicy::per_tool();
    let calls = [
        call(1, 0, "slow", json!({})),
        call(1, 1, "silent", json!({})),
        call(1, 2, "quick", json!({})),
        call(1, 3, "also_quick", json!({})),
    ];
    let plan = policy.plan(&registry, &calls).unwrap();
    assert_eq!((plan.after, &*plan.tool), (400 * MS, "quick"));
    // Unknown tools do not count.
    assert_eq!(
        policy.plan(&registry, &[call(1, 0, "nope", json!({}))]),
        None
    );
    assert_eq!(policy.plan(&registry, &[]), None);
}

#[test]
fn phrases_rotate_per_spoken_filler_and_a_tool_can_have_its_own() {
    let registry = registry_with(&[
        ("calendar", Some(Duration::ZERO)),
        ("other", Some(Duration::ZERO)),
    ]);
    let policy = FillerPolicy::new()
        .with_phrases([
            FillerPhrase::new("One moment."),
            FillerPhrase::new("Let me see."),
        ])
        .with_tool_phrases("calendar", [FillerPhrase::new("Checking the calendar.")]);
    let generic = policy
        .plan(&registry, &[call(1, 0, "other", json!({}))])
        .unwrap();
    let calendar = policy
        .plan(&registry, &[call(1, 0, "calendar", json!({}))])
        .unwrap();
    // Planning does not advance the rotation; speaking does.
    for _ in 0..3 {
        let _ = policy.plan(&registry, &[call(1, 0, "other", json!({}))]);
    }
    let said: Vec<String> = [&generic, &generic, &calendar, &generic]
        .into_iter()
        .map(|plan| policy.choose(plan).unwrap().text.to_string())
        .collect();
    // The rotation counter is shared, so the per-tool phrase (one entry) does not break the
    // alternation of the generic pair: 0 -> "One moment.", 1 -> "Let me see.", calendar always its
    // own, 3 -> "Let me see." (counter 3, 3 % 2 = 1).
    assert_eq!(
        said,
        [
            "One moment.",
            "Let me see.",
            "Checking the calendar.",
            "Let me see."
        ]
    );
}

#[test]
fn each_clone_of_a_policy_rotates_its_own_phrases() {
    // A deployment builds one policy from configuration and clones it into every session: one
    // session's fillers must not move another session's rotation.
    let registry = registry_with(&[("other", Some(Duration::ZERO))]);
    let template = FillerPolicy::new().with_phrases([
        FillerPhrase::new("One moment."),
        FillerPhrase::new("Let me see."),
    ]);
    let plan = template
        .plan(&registry, &[call(1, 0, "other", json!({}))])
        .unwrap();
    let session_a = template.clone();
    let session_b = template.clone();
    let say = |policy: &FillerPolicy| policy.choose(&plan).unwrap().text.to_string();
    assert_eq!(say(&session_a), "One moment.");
    assert_eq!(
        say(&session_b),
        "One moment.",
        "session B starts its own rotation"
    );
    assert_eq!(say(&session_a), "Let me see.");
    assert_eq!(say(&session_b), "Let me see.");
    assert_eq!(say(&template), "One moment.", "the template is untouched");
}

#[test]
fn a_clip_key_reaches_the_choice_only_when_the_cache_has_the_clip() {
    let registry = registry_with(&[("a", Some(Duration::ZERO))]);
    let plan = FillerPolicy::new()
        .plan(&registry, &[call(1, 0, "a", json!({}))])
        .unwrap();
    let phrase = FillerPhrase::new("One moment.").with_clip(ClipKey::new("en/one-moment"));

    // No cache: text only.
    let policy = FillerPolicy::new().with_phrases([phrase.clone()]);
    let choice = policy.choose(&plan).unwrap();
    assert_eq!((&*choice.text, choice.clip), ("One moment.", None));

    // A cache without the clip: text only.
    let policy = policy
        .clone()
        .with_clips(Arc::new(StaticClips::new([ClipKey::new("en/other")])));
    assert_eq!(policy.choose(&plan).unwrap().clip, None);

    // A cache with the clip: the key comes along with the text.
    let policy = policy.with_clips(Arc::new(StaticClips::new([ClipKey::new("en/one-moment")])));
    let choice = policy.choose(&plan).unwrap();
    assert_eq!(
        choice.clip.as_ref().map(ClipKey::as_str),
        Some("en/one-moment")
    );
    assert_eq!(&*choice.text, "One moment.");
}

#[test]
fn a_disabled_policy_or_one_without_phrases_says_nothing() {
    let registry = registry_with(&[("a", Some(Duration::ZERO))]);
    let calls = [call(1, 0, "a", json!({}))];
    assert!(FillerPolicy::new().plan(&registry, &calls).is_some());
    assert_eq!(
        FillerPolicy::new()
            .with_enabled(false)
            .plan(&registry, &calls),
        None
    );
    assert_eq!(
        FillerPolicy::new().with_phrases([]).plan(&registry, &calls),
        None
    );
    assert!(!FillerPolicy::new().with_phrases([]).is_active());
    let plan = FillerPolicy::new().plan(&registry, &calls).unwrap();
    assert_eq!(FillerPolicy::new().with_enabled(false).choose(&plan), None);
    // The default text is the documented one.
    assert_eq!(
        &*FillerPolicy::new().choose(&plan).unwrap().text,
        DEFAULT_FILLER_TEXT
    );
}

#[test]
fn a_policy_built_from_config_follows_it() {
    let registry = registry_with(&[("a", Some(300 * MS))]);
    let calls = [call(1, 0, "a", json!({}))];
    let config = FillerConfig::default();
    assert!(config.enabled);
    assert_eq!(config.delay, Some(DEFAULT_FILLER_DELAY));
    assert_eq!(
        FillerPolicy::from_config(config)
            .plan(&registry, &calls)
            .unwrap()
            .after,
        1200 * MS
    );
    let per_tool = FillerConfig {
        delay: None,
        ..config
    };
    assert_eq!(
        FillerPolicy::from_config(per_tool)
            .plan(&registry, &calls)
            .unwrap()
            .after,
        300 * MS
    );
    let off = FillerConfig {
        enabled: false,
        ..config
    };
    assert_eq!(FillerPolicy::from_config(off).plan(&registry, &calls), None);
}

// ---- interruption: ReadOnly calls are cancelled and counted -------------------------------

#[tokio::test(start_paused = true)]
async fn an_interrupted_read_only_call_is_dropped_counted_and_never_reported_late() {
    let rig = Rig::new(ToolKind::ReadOnly, 3 * SECOND, EngineConfig::default());
    let execution = rig
        .run_interrupted(1, 0, json!({"q": "weather"}), 500 * MS)
        .await;
    assert_eq!(kind_of(&execution, 0).as_deref(), Some("interrupted"));
    assert!(execution.late_notes.is_empty());
    rig.clock.sleep(10 * SECOND).await;
    assert_eq!(
        (rig.tool.runs(), rig.tool.finished()),
        (1, 0),
        "the future was dropped mid-run"
    );
    assert!(rig.engine.take_late_notes().is_empty());
    let stats = rig.engine.stats();
    assert_eq!(stats.read_only_cancelled, 1);
    assert_eq!(stats.side_effect_interrupted, 0);

    // A ReadOnly call is never replayed or suppressed: the same call in the next turn runs.
    let again = rig.run(2, 0, json!({"q": "weather"})).await;
    assert_eq!(kind_of(&again, 0), None);
    assert_eq!(rig.tool.runs(), 2);
    assert_eq!(rig.engine.stats().orphan_replays, 0);
}

#[tokio::test(start_paused = true)]
async fn a_call_that_has_not_started_when_the_turn_is_cancelled_is_counted_separately() {
    let rig = Rig::new(ToolKind::ReadOnly, SECOND, EngineConfig::default());
    let token = CancellationToken::new();
    token.cancel();
    let execution = rig
        .engine
        .execute(vec![call(1, 0, "book", json!({}))], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&execution, 0).as_deref(), Some("interrupted"));
    assert_eq!(rig.tool.runs(), 0);
    let stats = rig.engine.stats();
    assert_eq!(
        (stats.not_started_cancelled, stats.read_only_cancelled),
        (1, 0)
    );
}

// ---- interruption: SideEffect calls complete once and are reported ------------------------

#[tokio::test(start_paused = true)]
async fn an_interrupted_side_effect_completes_once_and_its_result_is_reported_once() {
    let rig = Rig::new(ToolKind::SideEffect, 3 * SECOND, EngineConfig::default());
    let mut execution = rig
        .run_interrupted(1, 0, json!({"table": "T4"}), 500 * MS)
        .await;
    assert_eq!(kind_of(&execution, 0).as_deref(), Some("interrupted"));
    assert!(
        content_of(&execution, 0).contains("its result follows as a note"),
        "{}",
        content_of(&execution, 0)
    );
    assert_eq!(rig.tool.finished(), 0, "still running at the interruption");
    assert!(rig.engine.take_late_notes().is_empty());

    // The call finishes on its own task, after the interruption.
    rig.clock.sleep(4 * SECOND).await;
    assert_eq!((rig.tool.runs(), rig.tool.finished()), (1, 1));
    let notes = rig.engine.take_late_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].text().contains("T4"), "{}", notes[0].text());
    assert!(notes[0].text().contains("t1.0"));
    // ... exactly once.
    assert!(rig.engine.take_late_notes().is_empty());
    let stats = rig.engine.stats();
    assert_eq!(stats.side_effect_interrupted, 1);
    assert_eq!(stats.side_effect_late_results, 1);
    assert_eq!(stats.read_only_cancelled, 0);

    // The note and the context stay valid together.
    let mut context = Context::new();
    context.push(Message::user("book T4"));
    context.push(Message::assistant_with_calls(
        "",
        vec![ToolCall {
            id: ToolCallId::new(epoch(1), 0),
            provider_id: None,
            name: Arc::from("book"),
            arguments: json!({"table": "T4"}),
            provider_data: None,
        }],
    ));
    execution.append_to(&mut context);
    for note in notes {
        context.push(note);
    }
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_regenerated_identical_call_gets_the_recorded_result_and_the_tool_does_not_run_again() {
    let rig = Rig::new(ToolKind::SideEffect, 3 * SECOND, EngineConfig::default());
    let args = json!({"table": "T4", "people": 2});
    rig.run_interrupted(1, 0, args.clone(), 500 * MS).await;
    rig.clock.sleep(4 * SECOND).await;
    assert_eq!(rig.tool.finished(), 1);

    // Turn 2: the LLM regenerates the same booking (new epoch, new call id, keys in another order).
    let regenerated = rig.run(2, 0, json!({"people": 2, "table": "T4"})).await;
    assert_eq!(
        kind_of(&regenerated, 0),
        None,
        "{}",
        content_of(&regenerated, 0)
    );
    assert!(content_of(&regenerated, 0).contains("T4"));
    assert!(matches!(
        &regenerated.results[0],
        Message::Tool { call_id, .. } if *call_id == ToolCallId::new(epoch(2), 0)
    ));
    assert_eq!(rig.tool.runs(), 1, "the booking ran exactly once");

    // The result reaches the LLM once: as the tool result, not also as a note.
    assert!(regenerated.late_notes.is_empty());
    assert!(rig.engine.take_late_notes().is_empty());
    let stats = rig.engine.stats();
    assert_eq!((stats.orphan_replays, stats.orphan_in_flight), (1, 0));

    // The orphan is consumed: a third identical call is a new request and runs.
    let third = rig.run(3, 0, args).await;
    assert_eq!(kind_of(&third, 0), None);
    assert_eq!(rig.tool.runs(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_regenerated_call_while_the_first_still_runs_is_refused_and_the_note_comes_once() {
    let rig = Rig::new(ToolKind::SideEffect, 5 * SECOND, EngineConfig::default());
    let args = json!({"table": "T9"});
    rig.run_interrupted(1, 0, args.clone(), 500 * MS).await;

    // 1 s in, turn 2 regenerates the booking: the first run is still going.
    rig.clock.sleep(SECOND).await;
    let regenerated = rig.run(2, 0, args.clone()).await;
    assert_eq!(kind_of(&regenerated, 0).as_deref(), Some("duplicate_call"));
    assert!(content_of(&regenerated, 0).contains("still running"));
    assert_eq!(rig.tool.runs(), 1);

    // A further regeneration is refused too, until the result is in.
    let again = rig.run(3, 0, args).await;
    assert_eq!(kind_of(&again, 0).as_deref(), Some("duplicate_call"));

    // The first run completes; its result is reported once.
    rig.clock.sleep(6 * SECOND).await;
    assert_eq!((rig.tool.runs(), rig.tool.finished()), (1, 1));
    assert_eq!(rig.engine.take_late_notes().len(), 1);
    assert!(rig.engine.take_late_notes().is_empty());
    assert_eq!(rig.engine.stats().orphan_in_flight, 2);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_execute_future_orphans_a_running_side_effect_too() {
    // The runner aborts its round task at a barge-in, which drops `execute` without letting it
    // observe the cancellation.
    let rig = Rig::new(ToolKind::SideEffect, 3 * SECOND, EngineConfig::default());
    let args = json!({"table": "T1"});
    {
        let token = CancellationToken::new();
        let run = rig
            .engine
            .execute(vec![call(1, 0, "book", args.clone())], epoch(1), &token);
        tokio::select! {
            _ = run => panic!("the tool takes 3 s"),
            () = rig.clock.sleep(500 * MS) => {}
        }
    }
    let regenerated = rig.run(2, 0, args).await;
    assert_eq!(kind_of(&regenerated, 0).as_deref(), Some("duplicate_call"));
    assert_eq!(rig.tool.runs(), 1);
    rig.clock.sleep(4 * SECOND).await;
    assert_eq!(rig.tool.finished(), 1);
    assert_eq!(rig.engine.take_late_notes().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_side_effect_is_an_orphan_as_well() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tool = Slow::new(&clock, 4 * SECOND);
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new("book", "books", object_schema(), ToolKind::SideEffect)
                .with_timeout(SECOND),
            tool.clone(),
        )
        .unwrap();
    let engine = ToolEngine::new(
        Arc::new(registry),
        SessionId::new(1),
        Arc::clone(&clock),
        EngineConfig::default(),
    );
    let token = CancellationToken::new();
    let first = engine
        .execute(vec![call(1, 0, "book", json!({"t": 1}))], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&first, 0).as_deref(), Some("timeout"));
    let second = engine
        .execute(vec![call(2, 0, "book", json!({"t": 1}))], epoch(2), &token)
        .await;
    assert_eq!(kind_of(&second, 0).as_deref(), Some("duplicate_call"));
    assert_eq!(tool.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn only_an_identical_call_of_another_response_within_the_window_is_suppressed() {
    let rig = Rig::new(ToolKind::SideEffect, SECOND, EngineConfig::default());
    rig.run_interrupted(1, 0, json!({"table": "T4"}), 100 * MS)
        .await;
    rig.clock.sleep(2 * SECOND).await;
    assert_eq!(rig.tool.finished(), 1);

    // Other arguments: a different request, it runs.
    let other = rig.run(2, 0, json!({"table": "T5"})).await;
    assert_eq!(kind_of(&other, 0), None);
    assert_eq!(rig.tool.runs(), 2);

    // The same turn, a later round: still a repeat of the cut-off call (the model was told not
    // to repeat it), so it is answered from the record and does not run.
    rig.run_interrupted(3, 0, json!({"table": "T7"}), 100 * MS)
        .await;
    rig.clock.sleep(2 * SECOND).await;
    let same_turn = rig.run(3, 1, json!({"table": "T7"})).await;
    assert_eq!(kind_of(&same_turn, 0), None);
    assert!(content_of(&same_turn, 0).contains("T7"));
    assert_eq!(rig.tool.runs(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_window_expires_and_can_be_turned_off() {
    let config = EngineConfig {
        orphan_window: 10 * SECOND,
        ..EngineConfig::default()
    };
    let rig = Rig::new(ToolKind::SideEffect, SECOND, config);
    rig.run_interrupted(1, 0, json!({"t": 1}), 100 * MS).await;
    rig.clock.sleep(11 * SECOND).await;
    let late = rig.run(2, 0, json!({"t": 1})).await;
    assert_eq!(kind_of(&late, 0), None);
    assert_eq!(rig.tool.runs(), 2, "past the window it is a new request");

    let off = EngineConfig {
        orphan_window: Duration::ZERO,
        ..EngineConfig::default()
    };
    let rig = Rig::new(ToolKind::SideEffect, SECOND, off);
    rig.run_interrupted(1, 0, json!({"t": 1}), 100 * MS).await;
    rig.clock.sleep(2 * SECOND).await;
    let again = rig.run(2, 0, json!({"t": 1})).await;
    assert_eq!(kind_of(&again, 0), None);
    assert_eq!(
        rig.tool.runs(),
        2,
        "with the window at zero nothing is suppressed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_whose_result_was_delivered_is_not_an_orphan() {
    let rig = Rig::new(ToolKind::SideEffect, SECOND, EngineConfig::default());
    let first = rig.run(1, 0, json!({"t": 1})).await;
    assert_eq!(kind_of(&first, 0), None);
    // The user asks for the same thing again in a later turn: a genuine second request.
    let second = rig.run(2, 0, json!({"t": 1})).await;
    assert_eq!(kind_of(&second, 0), None);
    assert_eq!(rig.tool.runs(), 2);
    assert_eq!(rig.engine.stats().orphan_replays, 0);
}

#[tokio::test(start_paused = true)]
async fn the_same_key_is_still_replayed_from_the_store_and_counted() {
    let rig = Rig::new(ToolKind::SideEffect, SECOND, EngineConfig::default());
    let first = rig.run(1, 0, json!({"t": 1})).await;
    let replay = rig.run(1, 0, json!({"t": 1})).await;
    assert_eq!(content_of(&first, 0), content_of(&replay, 0));
    assert_eq!(rig.tool.runs(), 1);
    assert_eq!(rig.engine.stats().side_effect_replays, 1);
}

// ---- an execution that never reaches the context ---------------------------------

/// `book` (SideEffect, `book_delay`, timeout `book_timeout`) and `weather` (ReadOnly, 5 s) over a
/// fresh engine.
fn engine_with_sibling(
    book_delay: Duration,
    book_timeout: Duration,
) -> (SharedClock, Arc<ToolEngine>, Slow) {
    let clock: SharedClock = Arc::new(SimClock::new());
    let book = Slow::new(&clock, book_delay);
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec::new("book", "books", object_schema(), ToolKind::SideEffect)
                .with_timeout(book_timeout),
            book.clone(),
        )
        .unwrap();
    registry
        .register(
            ToolSpec::new("weather", "weather", object_schema(), ToolKind::ReadOnly)
                .with_timeout(30 * SECOND),
            Slow::new(&clock, 5 * SECOND),
        )
        .unwrap();
    let engine = Arc::new(ToolEngine::new(
        Arc::new(registry),
        SessionId::new(3),
        Arc::clone(&clock),
        EngineConfig::default(),
    ));
    (clock, engine, book)
}

/// Turn 1: `book(args)` is interrupted 0.5 s in and finishes at 3 s, its note in the mailbox.
async fn orphan_a_booking(clock: &SharedClock, engine: &Arc<ToolEngine>, args: &Value) {
    let token = CancellationToken::new();
    let run_token = token.clone();
    let run_engine = Arc::clone(engine);
    let calls = vec![call(1, 0, "book", args.clone())];
    let handle = tokio::spawn(async move { run_engine.execute(calls, epoch(1), &run_token).await });
    clock.sleep(500 * MS).await;
    token.cancel();
    let mut first = handle.await.unwrap();
    assert_eq!(kind_of(&first, 0).as_deref(), Some("interrupted"));
    first.mark_delivered();
    clock.sleep(2_700 * MS).await;
}

#[tokio::test(start_paused = true)]
async fn a_replay_dropped_with_its_round_keeps_the_orphan_and_its_note() {
    let (clock, engine, book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    let args = json!({"table": "T4"});
    orphan_a_booking(&clock, &engine, &args).await;
    assert_eq!(book.finished(), 1);

    // Turn 2 regenerates the booking next to a slow ReadOnly sibling; the caller barges in while
    // the sibling runs, so the round (and the replayed result in it) is dropped.
    {
        let token = CancellationToken::new();
        let run = engine.execute(
            vec![
                call(2, 0, "book", args.clone()),
                call(2, 1, "weather", json!({})),
            ],
            epoch(2),
            &token,
        );
        tokio::select! {
            _ = run => panic!("the sibling takes 5 s"),
            () = clock.sleep(SECOND) => {}
        }
    }
    assert_eq!(book.runs(), 1);

    // Turn 3 regenerates it again: still answered from the record, the tool does not run.
    let token = CancellationToken::new();
    let mut third = engine
        .execute(vec![call(3, 0, "book", args)], epoch(3), &token)
        .await;
    assert_eq!(kind_of(&third, 0), None, "{}", content_of(&third, 0));
    assert!(content_of(&third, 0).contains("T4"));
    assert_eq!(book.runs(), 1, "the booking ran exactly once");
    // The result reaches the LLM once: the replay carries it, so its note is not repeated.
    assert!(third.late_notes.is_empty());
    third.mark_delivered();
    assert!(engine.take_late_notes().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_discarded_execution_with_a_replay_puts_the_note_back_and_keeps_the_orphan() {
    let (clock, engine, book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    let args = json!({"table": "T4"});
    orphan_a_booking(&clock, &engine, &args).await;

    // Turn 2's execution comes back but never reaches the context (the runner discards it).
    let token = CancellationToken::new();
    let discarded = engine
        .execute(vec![call(2, 0, "book", args.clone())], epoch(2), &token)
        .await;
    assert_eq!(kind_of(&discarded, 0), None);
    assert!(
        discarded.late_notes.is_empty(),
        "the replay covers the note"
    );
    drop(discarded);

    // The note of the booking is back in the mailbox, and a later regeneration is suppressed too.
    // The note was taken into the context, so the regeneration does not repeat the result.
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].text().contains("T4"));
    let mut third = engine
        .execute(vec![call(3, 0, "book", args)], epoch(3), &token)
        .await;
    assert_eq!(kind_of(&third, 0).as_deref(), Some("already_reported"));
    third.mark_delivered();
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_repeat_after_the_orphans_note_was_taken_does_not_repeat_the_result() {
    let (clock, engine, book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    let args = json!({"table": "T4"});
    orphan_a_booking(&clock, &engine, &args).await;
    assert_eq!(book.finished(), 1);

    // The runner appends the booking's note at the start of the next turn ...
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].text().contains("T4"));

    // ... and the regenerated response repeats the booking: the note told the LLM already.
    let token = CancellationToken::new();
    let mut repeat = engine
        .execute(vec![call(2, 0, "book", args.clone())], epoch(2), &token)
        .await;
    assert_eq!(kind_of(&repeat, 0).as_deref(), Some("already_reported"));
    assert!(
        !content_of(&repeat, 0).contains("T4"),
        "{}",
        content_of(&repeat, 0)
    );
    assert!(repeat.late_notes.is_empty());
    repeat.mark_delivered();
    assert_eq!(book.runs(), 1, "the booking ran exactly once");
    let stats = engine.stats();
    assert_eq!((stats.orphan_reported, stats.orphan_replays), (1, 0));

    // Any further repeat within the window is answered the same way.
    let mut again = engine
        .execute(vec![call(3, 0, "book", args)], epoch(3), &token)
        .await;
    assert_eq!(kind_of(&again, 0).as_deref(), Some("already_reported"));
    again.mark_delivered();
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_repeat_after_a_delivered_execution_carried_the_note_does_not_repeat_the_result() {
    let (clock, engine, book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    let args = json!({"table": "T4"});
    orphan_a_booking(&clock, &engine, &args).await;

    // Turn 2 runs another tool; its execution carries the booking's note into the context.
    let token = CancellationToken::new();
    let mut second = engine
        .execute(vec![call(2, 0, "weather", json!({}))], epoch(2), &token)
        .await;
    assert_eq!(second.late_notes.len(), 1);
    assert!(second.late_notes[0].text().contains("T4"));
    second.mark_delivered();

    // Turn 3 repeats the booking: answered without the result.
    let mut third = engine
        .execute(vec![call(3, 0, "book", args.clone())], epoch(3), &token)
        .await;
    assert_eq!(kind_of(&third, 0).as_deref(), Some("already_reported"));
    third.mark_delivered();
    assert_eq!(book.runs(), 1);
    assert_eq!(engine.stats().orphan_reported, 1);
}

#[tokio::test(start_paused = true)]
async fn a_discarded_execution_that_carried_the_note_leaves_the_orphan_replayable() {
    let (clock, engine, book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    let args = json!({"table": "T4"});
    orphan_a_booking(&clock, &engine, &args).await;

    // Turn 2's execution takes the booking's note but never reaches the context.
    let token = CancellationToken::new();
    let second = engine
        .execute(vec![call(2, 0, "weather", json!({}))], epoch(2), &token)
        .await;
    assert_eq!(second.late_notes.len(), 1);
    drop(second);

    // The LLM never read the result: the repeat is answered with it (and its note is withheld).
    let mut third = engine
        .execute(vec![call(3, 0, "book", args)], epoch(3), &token)
        .await;
    assert_eq!(kind_of(&third, 0), None, "{}", content_of(&third, 0));
    assert!(content_of(&third, 0).contains("T4"));
    assert!(third.late_notes.is_empty());
    third.mark_delivered();
    assert!(engine.take_late_notes().is_empty());
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_result_in_a_discarded_execution_is_noted_and_never_run_again() {
    let (_clock, engine, book) = engine_with_sibling(100 * MS, 30 * SECOND);
    let args = json!({"table": "T2"});
    let token = CancellationToken::new();
    // The booking finishes inside its execution, which the runner then drops (a barge-in while
    // the filler was still being said).
    let execution = engine
        .execute(vec![call(1, 0, "book", args.clone())], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&execution, 0), None);
    drop(execution);

    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1, "the result is reported, not lost");
    assert!(notes[0].text().contains("T2"));
    // The note was taken into the context: the regenerated call is answered without running and
    // without the result a second time.
    let mut regenerated = engine
        .execute(vec![call(2, 0, "book", args)], epoch(2), &token)
        .await;
    assert_eq!(
        kind_of(&regenerated, 0).as_deref(),
        Some("already_reported")
    );
    assert!(!content_of(&regenerated, 0).contains("T2"));
    regenerated.mark_delivered();
    assert_eq!(book.runs(), 1, "the regenerated call does not run again");
}

#[tokio::test(start_paused = true)]
async fn late_notes_taken_by_a_discarded_execution_go_back_to_the_mailbox() {
    let (clock, engine, _book) = engine_with_sibling(3 * SECOND, 30 * SECOND);
    orphan_a_booking(&clock, &engine, &json!({"table": "T8"})).await;
    let token = CancellationToken::new();
    let execution = engine
        .execute(vec![call(2, 0, "weather", json!({}))], epoch(2), &token)
        .await;
    assert_eq!(execution.late_notes.len(), 1);
    drop(execution);
    assert_eq!(engine.take_late_notes().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_retry_in_a_later_round_of_the_same_turn_after_a_timeout_does_not_run_again() {
    // An SMS-like call: it times out at 1 s and really takes 4 s.
    let (clock, engine, book) = engine_with_sibling(4 * SECOND, SECOND);
    let args = json!({"to": "+15550100"});
    let token = CancellationToken::new();
    let mut first = engine
        .execute(vec![call(1, 0, "book", args.clone())], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&first, 0).as_deref(), Some("timeout"));
    first.mark_delivered();

    // Round 2 of the same turn repeats the call although it was told not to.
    let mut retry = engine
        .execute(vec![call(1, 1, "book", args.clone())], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&retry, 0).as_deref(), Some("duplicate_call"));
    retry.mark_delivered();
    assert_eq!(book.runs(), 1);

    // After it finished, a further retry of the same turn gets the recorded result.
    clock.sleep(4 * SECOND).await;
    let mut again = engine
        .execute(vec![call(1, 2, "book", args)], epoch(1), &token)
        .await;
    assert_eq!(kind_of(&again, 0), None, "{}", content_of(&again, 0));
    again.mark_delivered();
    assert_eq!(book.runs(), 1, "the message was sent once");
}

#[tokio::test(start_paused = true)]
async fn two_identical_calls_of_one_response_are_separate_calls() {
    let (_clock, engine, book) = engine_with_sibling(100 * MS, 30 * SECOND);
    let args = json!({"table": "T3"});
    let token = CancellationToken::new();
    let mut execution = engine
        .execute(
            vec![call(1, 0, "book", args.clone()), call(1, 1, "book", args)],
            epoch(1),
            &token,
        )
        .await;
    execution.mark_delivered();
    assert_eq!(
        (kind_of(&execution, 0), kind_of(&execution, 1)),
        (None, None)
    );
    assert_eq!(book.runs(), 2);
}
