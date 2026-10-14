//! Tool UX through the engine: the filler policy of the tool layer drives the runner's
//! filler round (delay on the injected clock, rotating phrases, clip hints, off switch), and
//! interruption semantics hold end to end: a ReadOnly call is cancelled and counted, a SideEffect
//! call completes exactly once and its result is recorded in the context, and a response
//! regenerated after the interruption does not run it again. The LLM is the simulator's
//! `MockLlm`; time is a paused `SimClock`.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::{
    ClipKey, EngineConfig as ToolEngineConfig, FillerConfig, FillerPhrase, FillerPolicy, SessionId,
    StaticClips, ToolEngine, ToolKind, ToolRegistry, ToolSpec, ToolStatsSnapshot,
};
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use brisvox_engine::{EngineConfig, PlayoutBook, RoundKey};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use common::MS;
use common::llm::{Gate, Rig, SlowTool, ToolProbe, lookup_tools};
use serde_json::json;

const SECOND: Duration = Duration::from_secs(1);

fn tool_script(text: &str) -> LlmScript {
    LlmScript::text([text]).with_tool_call("call_1", "lookup", r#"{"q":"weather"}"#)
}

struct Setup {
    rig: Rig,
    probe: Arc<ToolProbe>,
    engine: Arc<ToolEngine>,
}

impl Setup {
    fn tool_stats(&self) -> ToolStatsSnapshot {
        self.engine.stats()
    }

    fn texts(&self) -> Vec<String> {
        self.rig
            .labels()
            .into_iter()
            .filter(|l| l.starts_with("text:"))
            .collect()
    }

    fn has_text(&self, text: &str) -> bool {
        self.texts().iter().any(|t| t == &format!("text:{text}"))
    }
}

/// A runner with the `lookup` tool of `kind` that takes `delay`; the tool's own `filler_after` is
/// `tool_filler`, and `policy` (when given) is the runner's filler policy.
async fn setup(
    kind: ToolKind,
    delay: Duration,
    tool_filler: Option<Duration>,
    policy: Option<FillerPolicy>,
    book: Option<Arc<PlayoutBook>>,
) -> Setup {
    setup_with(
        EngineConfig::DEFAULT,
        kind,
        delay,
        tool_filler,
        policy,
        book,
    )
    .await
}

/// [`setup`] over `config`.
async fn setup_with(
    config: EngineConfig,
    kind: ToolKind,
    delay: Duration,
    tool_filler: Option<Duration>,
    policy: Option<FillerPolicy>,
    book: Option<Arc<PlayoutBook>>,
) -> Setup {
    let probe = Arc::new(ToolProbe::default());
    let slot: Arc<Mutex<Option<Arc<ToolEngine>>>> = Arc::default();
    let rig = Rig::new(config, {
        let probe = Arc::clone(&probe);
        let slot = Arc::clone(&slot);
        move |builder, clock| {
            let (registry, engine) = lookup_tools(clock, &probe, kind, delay, tool_filler);
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&engine));
            let mut builder = builder.with_tools(registry, engine);
            if let Some(policy) = policy {
                builder = builder.with_filler_policy(policy);
            }
            match book {
                Some(book) => builder.with_playout(book),
                None => builder,
            }
        }
    })
    .await;
    let engine = slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .unwrap();
    Setup { rig, probe, engine }
}

// ---- the filler policy drives the runner -------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_filler_comes_after_the_policys_delay_on_the_injected_clock() {
    // The tool asks for a filler after 100 ms; the fixed policy overrides that with 1.2 s.
    let s = setup(
        ToolKind::ReadOnly,
        4 * SECOND,
        Some(100 * MS),
        Some(FillerPolicy::new()),
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig
        .llm
        .push(brisvox_sim::mock_llm::LlmScript::text(["It is sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;

    s.rig.settle(1_000 * MS).await;
    assert!(!s.has_text("One moment, please."), "{:?}", s.texts());
    assert_eq!(s.rig.stats.snapshot().fillers, 0);

    s.rig.settle(400 * MS).await;
    assert!(s.has_text("One moment, please."), "{:?}", s.texts());
    assert_eq!(s.rig.stats.snapshot().fillers, 1);
    assert_eq!(s.probe.finished(), 0, "the tool is still running");

    s.rig.until_label("llm_response_ended").await;
    assert_eq!(
        s.rig.stats.snapshot().fillers,
        1,
        "one filler per tool round"
    );
    s.rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_answers_before_the_delay_gets_no_filler() {
    let s = setup(
        ToolKind::ReadOnly,
        900 * MS,
        Some(100 * MS),
        Some(FillerPolicy::new()),
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig.until_label("llm_response_ended").await;
    assert_eq!(s.rig.stats.snapshot().fillers, 0);
    assert!(!s.has_text("One moment, please."));
}

#[tokio::test(start_paused = true)]
async fn phrases_rotate_per_filler_and_a_cached_clip_is_hinted_while_the_text_is_still_spoken() {
    let policy = FillerPolicy::new()
        .with_tool_phrases(
            "lookup",
            [
                FillerPhrase::new("Checking.").with_clip(ClipKey::new("en/checking")),
                FillerPhrase::new("Looking it up.").with_clip(ClipKey::new("en/missing")),
            ],
        )
        .with_clips(Arc::new(StaticClips::new([ClipKey::new("en/checking")])));
    let s = setup(
        ToolKind::ReadOnly,
        2 * SECOND,
        Some(100 * MS),
        Some(policy),
        None,
    )
    .await;
    for turn in 1..=2 {
        s.rig.llm.push(tool_script("Hold on."));
        s.rig.llm.push(LlmScript::text(["Sunny."]));
        s.rig.turn(turn, "weather?").await;
        s.rig
            .until("the answer", |r| {
                r.labels()
                    .iter()
                    .filter(|l| *l == "llm_response_ended")
                    .count()
                    == turn as usize
            })
            .await;
    }
    // Turn 1 spoke the first phrase (its clip is cached), turn 2 the second (its clip is not).
    assert!(s.has_text("Checking."), "{:?}", s.texts());
    assert!(s.has_text("Looking it up."), "{:?}", s.texts());
    let stats = s.rig.stats.snapshot();
    assert_eq!((stats.fillers, stats.filler_clips), (2, 1));
    s.rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_disabled_policy_speaks_no_filler() {
    let s = setup(
        ToolKind::ReadOnly,
        3 * SECOND,
        Some(100 * MS),
        Some(FillerPolicy::new().with_enabled(false)),
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig.until_label("llm_response_ended").await;
    assert_eq!(s.rig.stats.snapshot().fillers, 0);
    assert!(!s.has_text("One moment, please."));
}

#[tokio::test(start_paused = true)]
async fn the_default_runner_fills_after_the_configured_1_2_s() {
    // No policy set: the runner builds it from `LlmConfig::filler`, by default a fixed 1.2 s that
    // overrides the tool's own 700 ms.
    assert_eq!(EngineConfig::DEFAULT.llm.filler, FillerConfig::DEFAULT);
    let s = setup(ToolKind::ReadOnly, 3 * SECOND, Some(700 * MS), None, None).await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(1_100 * MS).await;
    assert!(!s.has_text("One moment, please."));
    s.rig.settle(200 * MS).await;
    assert!(s.has_text("One moment, please."));
}

#[tokio::test(start_paused = true)]
async fn the_filler_delay_comes_from_the_session_config() {
    // Per-tool delays from the config: the tool's own 700 ms.
    let mut config = EngineConfig::DEFAULT;
    config.llm.filler = FillerConfig::PER_TOOL;
    let s = setup_with(
        config,
        ToolKind::ReadOnly,
        2 * SECOND,
        Some(700 * MS),
        None,
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(600 * MS).await;
    assert!(!s.has_text("One moment, please."));
    s.rig.settle(200 * MS).await;
    assert!(s.has_text("One moment, please."));

    // Fillers switched off in the config.
    let mut config = EngineConfig::DEFAULT;
    config.llm.filler.enabled = false;
    let s = setup_with(
        config,
        ToolKind::ReadOnly,
        2 * SECOND,
        Some(100 * MS),
        None,
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(1_500 * MS).await;
    assert_eq!(s.rig.stats.snapshot().fillers, 0);
    assert!(!s.has_text("One moment, please."));
}

/// Writes the playout of one utterance into `book` the way the TTS stage and the transport do:
/// the sent text, timed words and the audio length, then a position.
fn played(book: &PlayoutBook, key: RoundKey, utterance: u64, words: &[(&str, u64)], at: u64) {
    book.register_utterance(key, utterance);
    let sent: Vec<&str> = words.iter().map(|(w, _)| *w).collect();
    assert!(book.push_sent(utterance, &sent.join(" ")));
    for (word, start) in words {
        assert!(book.push_word(utterance, *word, Duration::from_millis(*start)));
    }
    let total = words.last().map_or(0, |(_, start)| start + 400);
    assert!(book.add_audio(utterance, Duration::from_millis(total)));
    book.position(key.epoch, utterance, Duration::from_millis(at));
}

const FILLER_WORDS: [(&str, u64); 3] = [("One", 0), ("moment,", 300), ("please.", 600)];

/// Turn 1: "Let me check." (played in full), a 10 s tool, the filler after 1.2 s; the caller
/// barges in with the filler played up to `filler_at` ms; turn 2 asks again.
async fn filler_cut_by_barge_in(filler_at: u64) -> Setup {
    let book = PlayoutBook::shared(16);
    let s = setup(
        ToolKind::ReadOnly,
        10 * SECOND,
        Some(100 * MS),
        Some(FillerPolicy::new()),
        Some(Arc::clone(&book)),
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(1_300 * MS).await;
    assert!(s.has_text("One moment, please."));
    let epoch = TurnEpoch::new(1);
    played(
        &book,
        RoundKey::new(epoch, 0),
        1,
        &[("Let", 0), ("me", 200), ("check.", 400)],
        800,
    );
    let filler = RoundKey::new(epoch, 1);
    played(&book, filler, 2, &FILLER_WORDS, filler_at);
    book.note_clear(TurnEpoch::new(2));
    s.rig.interrupt(2).await;
    s.rig.until_label("interrupt").await;
    // The final position after the clear.
    book.position(epoch, 2, Duration::from_millis(filler_at));
    // The filler waits behind the closed call: right after the barge-in it follows the result.
    let conversation = s.rig.conversation();
    assert!(conversation[2].starts_with("tool!:"), "{conversation:?}");
    assert_eq!(conversation[3], "assistant:One moment, please.");
    assert_eq!(book.snapshot(filler).unwrap().msg_index, Some(3));
    s.rig.context_copy().validate().unwrap();

    s.rig.llm.push(LlmScript::text(["Still checking."]));
    s.rig.turn(2, "hello?").await;
    s.rig.until_label("llm_response_ended").await;
    s.rig.context_copy().validate().unwrap();
    s
}

#[tokio::test(start_paused = true)]
async fn a_played_filler_is_in_the_context_cut_to_what_was_heard() {
    // Barge-in 500 ms into the filler: "One moment," was heard, "please." was not.
    let s = filler_cut_by_barge_in(500).await;
    let conversation = s.rig.conversation();
    assert_eq!(conversation[3], "assistant:One moment,", "{conversation:?}");
    assert_eq!(conversation[4], "user:hello?");
    // The request of turn 2 carried the heard part of the filler, before the user message.
    let request = s.rig.llm.requests().last().cloned().unwrap();
    assert!(
        request
            .messages
            .iter()
            .any(|m| &*m.content == "One moment,"),
        "{:?}",
        request.messages
    );
    let stats = s.rig.stats.snapshot();
    assert_eq!((stats.fillers, stats.fillers_in_context), (1, 1));
    assert_eq!(stats.cut_errors, 0);
}

#[tokio::test(start_paused = true)]
async fn a_filler_that_was_never_heard_or_never_started_is_not_in_the_context() {
    // The barge-in came before the first word of the filler was played.
    let s = filler_cut_by_barge_in(0).await;
    let conversation = s.rig.conversation();
    assert!(
        conversation.iter().all(|m| !m.contains("One moment")),
        "{conversation:?}"
    );
    assert_eq!(conversation[3], "user:hello?", "{conversation:?}");
    assert_eq!(s.rig.stats.snapshot().cut_errors, 0);

    // A barge-in before the delay: the filler timer dies with the round, nothing is spoken.
    let s = setup(
        ToolKind::ReadOnly,
        10 * SECOND,
        Some(100 * MS),
        Some(FillerPolicy::new()),
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Let me check."));
    s.rig.turn(1, "weather?").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(300 * MS).await;
    s.rig.interrupt(2).await;
    s.rig.until_label("interrupt").await;
    s.rig.settle(3 * SECOND).await;
    assert!(!s.has_text("One moment, please."));
    assert_eq!(s.rig.stats.snapshot().fillers, 0);
    assert!(
        s.rig
            .conversation()
            .iter()
            .all(|m| !m.contains("One moment"))
    );
}

// ---- interruption semantics through the engine -------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_interrupted_read_only_call_is_cancelled_counted_and_not_run_to_the_end() {
    let s = setup(ToolKind::ReadOnly, 3 * SECOND, None, None, None).await;
    s.rig.llm.push(tool_script("Looking."));
    s.rig.turn(1, "look").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.interrupt(2).await;
    s.rig.until_label("interrupt").await;
    s.rig.settle(5 * SECOND).await;

    assert_eq!(s.probe.finished(), 0, "the read-only future was dropped");
    let runner = s.rig.stats.snapshot();
    assert_eq!((runner.halts, runner.interrupted_tool_calls), (1, 1));
    // The runner aborts its round task at a barge-in, which drops the engine's future: the
    // engine counts the ReadOnly call as cancelled; no result is produced, nothing is reported late.
    let tools = s.tool_stats();
    assert_eq!(tools.read_only_cancelled, 1);
    assert_eq!(tools.side_effect_late_results, 0);
    assert!(s.engine.take_late_notes().is_empty());
    assert_eq!(s.rig.context_copy().pending_tool_calls().len(), 0);
    s.rig.context_copy().validate().unwrap();

    // The next turn asks again: a ReadOnly call is never suppressed or replayed, it runs.
    s.rig.llm.push(tool_script("Looking again."));
    s.rig.llm.push(LlmScript::text(["Sunny."]));
    s.rig.turn(2, "look again").await;
    s.rig.until_label("llm_response_ended").await;
    assert_eq!((s.probe.started(), s.probe.finished()), (2, 1));
}

/// Turn 1 books, the caller barges in `after` into the call; turn 2 follows `gap` after the
/// barge-in, and the LLM regenerates the same call.
async fn book_interrupt_regenerate(after: Duration, gap: Duration) -> Setup {
    let s = setup(ToolKind::SideEffect, 3 * SECOND, None, None, None).await;
    s.rig.llm.push(tool_script("Booking it."));
    s.rig.turn(1, "book a table").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(after).await;
    s.rig.interrupt(2).await;
    s.rig.until_label("interrupt").await;
    // The context is valid right away: the open call was closed with an `interrupted` result.
    let conversation = s.rig.conversation();
    assert!(
        conversation.last().unwrap().starts_with("tool!:"),
        "{conversation:?}"
    );
    s.rig.context_copy().validate().unwrap();
    s.rig.settle(gap).await;

    // The caller speaks again and the model regenerates the identical booking.
    s.rig.llm.push(tool_script("Sure, booking."));
    s.rig.llm.push(LlmScript::text(["Your table is booked."]));
    s.rig.turn(2, "yes, book it").await;
    s
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_side_effect_completes_once_is_recorded_and_is_not_rerun_on_regeneration() {
    // The caller cuts in 0.5 s into a 3 s booking; the next turn starts 4 s later, after the
    // booking finished on its own.
    let s = book_interrupt_regenerate(500 * MS, 4 * SECOND).await;
    assert_eq!(
        s.probe.finished(),
        1,
        "the booking completed after the barge-in"
    );
    s.rig
        .until("the regenerated turn ends", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == 1
        })
        .await;

    assert_eq!(s.probe.started(), 1, "the SideEffect body ran exactly once");
    let conversation = s.rig.conversation();
    // The result is in the context once, as the late note appended when turn 2 started. The
    // regenerated call came after it, so it is answered `already_reported`, not with the result.
    let notes = conversation
        .iter()
        .filter(|m| m.contains("Late result of tool call t1.0") && m.contains("sunny"))
        .count();
    let results = conversation.iter().filter(|m| m.contains("sunny")).count();
    let repeats = conversation
        .iter()
        .filter(|m| m.starts_with("tool!:") && m.contains("already_reported"))
        .count();
    assert_eq!(notes, 1, "{conversation:?}");
    assert_eq!(results, 1, "{conversation:?}");
    assert_eq!(repeats, 1, "{conversation:?}");
    assert_eq!(
        conversation.last().map(String::as_str),
        Some("assistant:Your table is booked.")
    );
    s.rig.context_copy().validate().unwrap();

    let tools = s.tool_stats();
    assert_eq!(
        tools.side_effect_interrupted, 1,
        "the barge-in cut off one SideEffect call"
    );
    assert_eq!((tools.orphan_replays, tools.orphan_reported), (0, 1));
    assert_eq!(tools.side_effect_late_results, 1);

    // Nothing is reported or run again afterwards.
    s.rig.llm.push(LlmScript::text(["Anything else?"]));
    s.rig.turn(3, "no thanks").await;
    s.rig
        .until("third answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == 2
        })
        .await;
    assert_eq!(s.probe.started(), 1);
    assert_eq!(
        s.rig
            .conversation()
            .iter()
            .filter(|m| m.contains("Late result"))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_regeneration_while_the_booking_still_runs_is_refused_and_the_result_is_noted_once() {
    // Turn 2 starts 1 s after the barge-in, while the 3 s booking (started 0.5 s before the
    // barge-in) is still running.
    let s = book_interrupt_regenerate(500 * MS, 500 * MS).await;
    s.rig
        .until("the regenerated turn ends", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == 1
        })
        .await;
    assert_eq!(
        s.probe.started(),
        1,
        "the booking was not started a second time"
    );
    let conversation = s.rig.conversation();
    let refused = conversation
        .iter()
        .filter(|m| m.starts_with("tool!:") && m.contains("duplicate_call"))
        .count();
    assert_eq!(refused, 1, "{conversation:?}");
    s.rig.context_copy().validate().unwrap();
    assert_eq!(s.tool_stats().orphan_in_flight, 1);

    // The booking finishes; its result reaches the context exactly once, with the next turn.
    s.rig.settle(5 * SECOND).await;
    assert_eq!(s.probe.finished(), 1);
    s.rig.llm.push(LlmScript::text(["Anything else?"]));
    s.rig.turn(3, "thanks").await;
    s.rig
        .until("third answer", |r| {
            r.labels()
                .iter()
                .filter(|l| *l == "llm_response_ended")
                .count()
                == 2
        })
        .await;
    let notes = s
        .rig
        .conversation()
        .iter()
        .filter(|m| m.contains("Late result of tool call t1.0") && m.contains("sunny"))
        .count();
    assert_eq!(notes, 1);
    assert_eq!(s.probe.started(), 1);
    s.rig.context_copy().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_interrupted_during_its_filler_still_completes_once() {
    let s = setup(
        ToolKind::SideEffect,
        5 * SECOND,
        Some(100 * MS),
        Some(FillerPolicy::new()),
        None,
    )
    .await;
    s.rig.llm.push(tool_script("Booking it."));
    s.rig.turn(1, "book").await;
    s.rig
        .until("tool started", |_| s.probe.started() == 1)
        .await;
    s.rig.settle(1_500 * MS).await;
    assert!(s.has_text("One moment, please."));
    s.rig.interrupt(2).await;
    s.rig.until_label("interrupt").await;
    s.rig.settle(6 * SECOND).await;
    assert_eq!((s.probe.started(), s.probe.finished()), (1, 1));

    s.rig.llm.push(LlmScript::text(["Done?"]));
    s.rig.turn(2, "is it done").await;
    s.rig.until_label("llm_response_ended").await;
    let conversation = s.rig.conversation();
    assert_eq!(
        conversation
            .iter()
            .filter(|m| m.contains("Late result of tool call t1.0"))
            .count(),
        1,
        "{conversation:?}"
    );
    // Without a playout book every spoken line counts as heard: the filler is in the context
    // after the closed call, before the late note and the next user message.
    let filler = conversation
        .iter()
        .position(|m| m == "assistant:One moment, please.")
        .unwrap();
    assert!(
        conversation[filler - 1].starts_with("tool!:"),
        "{conversation:?}"
    );
    s.rig.context_copy().validate().unwrap();
}

// ---- executions the runner discards ----------------------------------------------

/// How many `llm_response_ended` frames the runner sent so far.
fn ended(rig: &Rig) -> usize {
    rig.labels()
        .iter()
        .filter(|l| *l == "llm_response_ended")
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_booking_that_finished_while_the_filler_was_held_is_noted_and_not_rerun_after_a_barge_in()
{
    // The booking takes 1 s; the filler is due at 0.5 s, but its frames back up behind a gate.
    // The booking finishes while the filler is still being sent; then the caller barges in, so
    // the runner drops the finished execution. Its result must still be recorded, and the
    // regenerated booking must not run again.
    let gate = Gate::default();
    gate.hold.store(true, Ordering::SeqCst);
    let probe = Arc::new(ToolProbe::default());
    let llm = MockLlm::new(Arc::new(SimClock::new()));
    let rig = Rig::gated(EngineConfig::DEFAULT, llm.clone(), &gate, {
        let probe = Arc::clone(&probe);
        move |builder, clock| {
            let (registry, engine) =
                lookup_tools(clock, &probe, ToolKind::SideEffect, SECOND, Some(SECOND));
            builder
                .with_tools(registry, engine)
                .with_filler_policy(FillerPolicy::new().with_delay(500 * MS))
        }
    })
    .await;
    llm.push(tool_script("Booking it."));
    rig.turn(1, "book a table").await;
    rig.until("the filler", |r| r.stats.snapshot().fillers == 1)
        .await;
    rig.until("the booking finished", |_| probe.finished() == 1)
        .await;
    rig.settle(100 * MS).await;
    rig.interrupt(2).await;
    gate.hold.store(false, Ordering::SeqCst);
    gate.release.notify_one();
    rig.until_label("interrupt").await;
    rig.context_copy().validate().unwrap();

    // The model regenerates the identical booking in the next turn.
    llm.push(tool_script("Sure, booking."));
    llm.push(LlmScript::text(["Your table is booked."]));
    rig.turn(2, "yes, book it").await;
    rig.until("the regenerated turn ends", |r| ended(r) >= 1)
        .await;
    assert_eq!(probe.started(), 1, "the booking ran exactly once");
    let conversation = rig.conversation();
    assert_eq!(
        conversation
            .iter()
            .filter(|m| m.contains("Late result of tool call t1.0") && m.contains("sunny"))
            .count(),
        1,
        "the result of the dropped execution is reported once: {conversation:?}"
    );
    assert_eq!(
        conversation.iter().filter(|m| m.contains("sunny")).count(),
        1,
        "the result is in the context once: {conversation:?}"
    );
    assert!(
        conversation
            .iter()
            .any(|m| m.starts_with("tool!:") && m.contains("already_reported")),
        "the regenerated call is answered without running or repeating it: {conversation:?}"
    );
    rig.context_copy().validate().unwrap();
}

/// `lookup` (SideEffect, 3 s) and `weather` (ReadOnly, 5 s), each with its own probe.
fn booking_and_weather(
    rig_clock: &SimClock,
    book: &Arc<ToolProbe>,
    weather: &Arc<ToolProbe>,
) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let clock: SharedClock = Arc::new(*rig_clock);
    let mut registry = ToolRegistry::new();
    for (name, kind, delay, probe) in [
        ("lookup", ToolKind::SideEffect, 3 * SECOND, book),
        ("weather", ToolKind::ReadOnly, 5 * SECOND, weather),
    ] {
        registry
            .register(
                ToolSpec::new(name, "A tool.", json!({"type": "object"}), kind)
                    .with_timeout(30 * SECOND)
                    .with_filler_after(None),
                SlowTool {
                    probe: Arc::clone(probe),
                    clock: clock.clone(),
                    delay,
                    result: json!({ "answer": name }),
                },
            )
            .unwrap();
    }
    let registry = Arc::new(registry);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock,
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}

#[tokio::test(start_paused = true)]
async fn a_repeat_cut_off_with_its_slow_sibling_is_answered_again_and_never_rerun() {
    let book = Arc::new(ToolProbe::default());
    let weather = Arc::new(ToolProbe::default());
    let rig = Rig::new(EngineConfig::DEFAULT, {
        let (book, weather) = (Arc::clone(&book), Arc::clone(&weather));
        move |builder, clock| {
            let (registry, engine) = booking_and_weather(clock, &book, &weather);
            builder.with_tools(registry, engine)
        }
    })
    .await;
    // Turn 1: the booking is cut off 0.5 s in; it finishes on its own at 3 s.
    rig.llm.push(tool_script("Booking it."));
    rig.turn(1, "book a table").await;
    rig.until("booking started", |_| book.started() == 1).await;
    rig.settle(500 * MS).await;
    rig.interrupt(2).await;
    rig.until_label("interrupt").await;
    rig.settle(3 * SECOND).await;
    assert_eq!(book.finished(), 1);

    // Turn 2: the model regenerates the booking next to a slow weather lookup, and the caller
    // barges in while the weather still runs.
    rig.llm.push(
        LlmScript::text(["Booking, and checking the weather."])
            .with_tool_call("c1", "lookup", r#"{"q":"weather"}"#)
            .with_tool_call("c2", "weather", "{}"),
    );
    rig.turn(2, "book it and tell me the weather").await;
    rig.until("weather started", |_| weather.started() == 1)
        .await;
    rig.settle(SECOND).await;
    rig.interrupt(3).await;
    rig.settle(10 * MS).await;
    rig.context_copy().validate().unwrap();

    // Turn 3: the booking is regenerated once more; it is answered without running again. Its
    // note reached the context when turn 2 started, so neither repeat carries the result again.
    rig.llm.push(tool_script("Booking."));
    rig.llm.push(LlmScript::text(["Booked."]));
    rig.turn(3, "just book it").await;
    rig.until("the third turn ends", |r| ended(r) >= 1).await;
    assert_eq!(book.started(), 1, "the booking ran exactly once");
    let conversation = rig.conversation();
    assert_eq!(
        conversation
            .iter()
            .filter(|m| m.contains(r#"{"answer":"lookup"}"#))
            .count(),
        1,
        "the booking's result is in the context once (its note): {conversation:?}"
    );
    assert!(
        conversation
            .last_chunk::<3>()
            .is_some_and(|[_, answer, _]| answer.starts_with("tool!:")
                && answer.contains("already_reported")),
        "turn 3's repeat is answered `already_reported`: {conversation:?}"
    );
    assert_eq!(
        conversation.last().map(String::as_str),
        Some("assistant:Booked."),
        "{conversation:?}"
    );
    rig.context_copy().validate().unwrap();
}
