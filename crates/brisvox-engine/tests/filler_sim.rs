//! Filler clips and SideEffect regeneration through the whole production pipeline:
//! a `SimSession` with a scripted caller (audio in, energy VAD, loopback transport) on
//! [`EngineFactory`]. A slow SideEffect booking is announced with a pre-rendered filler clip that
//! the TTS stage plays (the provider never renders the phrase); the caller barges in while the
//! booking runs; the response regenerated after the barge-in repeats the identical call, which
//! does not run again; the booking's result reaches the context exactly once; the filler is in
//! the context cut to what was heard.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::context::Context;
use brisvox_agent::tools::{
    ClipKey, EngineConfig as ToolEngineConfig, FillerPhrase, FillerPolicy, SessionId, ToolEngine,
    ToolKind, ToolRegistry, ToolSpec, ToolStatsSnapshot,
};
use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_engine::sim::EngineFactory;
use brisvox_engine::{ClipWord, FillerClip, FillerClipStore, SessionHandles, TtsStatsSnapshot};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::scenario::Scenario;
use brisvox_sim::session::{SessionRun, SimSession};
use bytes::Bytes;
use common::MS;
use common::llm::{SlowTool, ToolProbe, summarize};
use serde_json::json;

const SECOND: Duration = Duration::from_secs(1);
const FILLER: &str = "One moment, please.";
const CLIP: &str = "en/one-moment";
const BOOKING: &str = r#"{"party":2}"#;

/// 1.2 s of a square wave at 24 kHz (the mock TTS's rate) with timed words.
fn store() -> Arc<FillerClipStore> {
    let samples = 24 * 1200;
    let mut pcm = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let v: i16 = if (i / 8) % 2 == 0 { 3000 } else { -3000 };
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    let clip = FillerClip::new(
        SampleRate::Hz24000,
        Bytes::from(pcm),
        vec![
            ClipWord::new("One", Duration::ZERO),
            ClipWord::new("moment,", 400 * MS),
            ClipWord::new("please.", 800 * MS),
        ],
    )
    .unwrap();
    Arc::new(FillerClipStore::new().with_clip(ClipKey::new(CLIP), clip))
}

struct Fixture {
    factory: EngineFactory,
    probe: Arc<ToolProbe>,
    engine: Arc<Mutex<Option<Arc<ToolEngine>>>>,
}

impl Fixture {
    /// A call with the SideEffect tool `book` taking `delay`, the filler policy of the session
    /// default (1.2 s for every tool with a filler) speaking [`FILLER`] from its clip.
    fn new(delay: Duration) -> Self {
        let probe = Arc::new(ToolProbe::default());
        let engine: Arc<Mutex<Option<Arc<ToolEngine>>>> = Arc::default();
        let (tool_probe, slot) = (Arc::clone(&probe), Arc::clone(&engine));
        let tools = Arc::new(move |clock: &SharedClock| {
            let mut registry = ToolRegistry::new();
            registry
                .register(
                    ToolSpec::new(
                        "book",
                        "Books a table.",
                        json!({"type": "object", "properties": {"party": {"type": "integer"}}}),
                        ToolKind::SideEffect,
                    )
                    .with_timeout(30 * SECOND),
                    SlowTool {
                        probe: Arc::clone(&tool_probe),
                        clock: clock.clone(),
                        delay,
                        result: json!({"booking": "confirmed-42"}),
                    },
                )
                .unwrap();
            let registry = Arc::new(registry);
            let engine = Arc::new(ToolEngine::new(
                Arc::clone(&registry),
                SessionId::new(1),
                clock.clone(),
                ToolEngineConfig::default(),
            ));
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&engine));
            (registry, engine)
        });
        let policy = FillerPolicy::new()
            .with_phrases([FillerPhrase::new(FILLER).with_clip(ClipKey::new(CLIP))]);
        let factory = EngineFactory::new()
            .with_guards_off()
            .with_tool_factory(tools)
            .with_filler_policy(policy)
            .with_filler_clips(store());
        Self {
            factory,
            probe,
            engine,
        }
    }

    fn handles(&self) -> SessionHandles {
        self.factory.handles().pop().unwrap()
    }

    fn tts(&self) -> TtsStatsSnapshot {
        self.handles().tts.snapshot()
    }

    fn tools(&self) -> ToolStatsSnapshot {
        self.engine
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .unwrap()
            .stats()
    }

    fn context(&self) -> Context {
        self.handles()
            .context
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn conversation(&self) -> Vec<String> {
        summarize(&self.context())
    }
}

/// Wire arrival (call time) of the first and the last audio of `epoch`.
fn audio_span(run: &SessionRun, epoch: u32) -> Option<(Duration, Duration)> {
    let audio: Vec<Duration> = run
        .output
        .iter()
        .filter(|r| r.kind == "audio" && r.epoch == Some(epoch))
        .map(|r| r.at)
        .collect();
    Some((*audio.first()?, *audio.last()?))
}

fn is_word_prefix(text: &str, full: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    let all: Vec<&str> = full.split_whitespace().collect();
    all.starts_with(&words)
}

#[tokio::test(start_paused = true)]
async fn a_played_filler_clip_is_heard_whole_and_kept_in_the_context() {
    // No barge-in: the booking takes 3 s, the clip plays to the end while it runs.
    let f = Fixture::new(3 * SECOND);
    let session = SimSession::new(Arc::new(SimClock::new()));
    session
        .llm
        .push(LlmScript::text(["Let me book that."]).with_tool_call("c1", "book", BOOKING));
    session.llm.push(LlmScript::text(["Your table is booked."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("book a table for two", 800 * MS)
        .with_tail(8 * SECOND);
    let run = session
        .run_with(&f.factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));

    let tts = f.tts();
    assert_eq!(
        (tts.clips, tts.clips_played, tts.clip_fallbacks),
        (1, 1, 0),
        "{tts:?}"
    );
    let spoken: Vec<String> = session.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert!(
        !spoken.iter().any(|t| t.contains("moment")),
        "the provider never rendered the filler: {spoken:?}"
    );
    let conversation = f.conversation();
    assert_eq!(f.probe.started(), 1);
    // The filler was heard whole: it is in the context in full, after the closed call and
    // before the answer.
    let filler = conversation
        .iter()
        .position(|m| m == &format!("assistant:{FILLER}"))
        .unwrap_or_else(|| panic!("{conversation:?}"));
    let result = conversation
        .iter()
        .position(|m| m.starts_with("tool:") && m.contains("confirmed-42"))
        .unwrap();
    assert!(result < filler, "{conversation:?}");
    assert_eq!(
        conversation.last().map(String::as_str),
        Some("assistant:Your table is booked.")
    );
    f.context().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_during_a_side_effect_does_not_run_it_again_and_its_result_is_in_the_context_once()
 {
    // The 5 s booking starts at about 1.5 s; the filler clip plays from about 2.7 s; the caller
    // barges in at 3.0 s, in the middle of the clip, and the regenerated response (about 4.2 s)
    // repeats the identical booking while it still runs. The caller thanks the bot at 9 s.
    let f = Fixture::new(5 * SECOND);
    let session = SimSession::new(Arc::new(SimClock::new()));
    session
        .llm
        .push(LlmScript::text(["Let me book that."]).with_tool_call("c1", "book", BOOKING));
    session
        .llm
        .push(LlmScript::text(["Sure."]).with_tool_call("c1", "book", BOOKING));
    session.llm.push(LlmScript::text(["It is on its way."]));
    session.llm.push(LlmScript::text(["You are welcome."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("book a table for two", 800 * MS)
        .silence(2_100 * MS)
        .speak("sorry go on", 600 * MS)
        .silence(5_400 * MS)
        .speak("thanks", 400 * MS)
        .with_tail(3 * SECOND);
    let run = session
        .run_with(&f.factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    run.check_no_stale_audio_after_interrupt().unwrap();

    // The clip started playing before the barge-in and was cut by it.
    let (clip_from, _) = audio_span(&run, 0).unwrap();
    let barge_in = 3_000 * MS;
    assert!(
        clip_from < barge_in,
        "first audio of turn 0 at {clip_from:?}"
    );
    let tts = f.tts();
    assert_eq!((tts.clips, tts.clip_fallbacks), (1, 0), "{tts:?}");
    let spoken: Vec<String> = session.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert!(!spoken.iter().any(|t| t.contains("moment")), "{spoken:?}");

    // The booking ran once; the regenerated identical call was answered without running it.
    assert_eq!((f.probe.started(), f.probe.finished()), (1, 1));
    let tools = f.tools();
    assert_eq!(tools.side_effect_interrupted, 1, "{tools:?}");
    assert_eq!(tools.orphan_in_flight, 1, "{tools:?}");
    assert_eq!(tools.side_effect_late_results, 1, "{tools:?}");

    let conversation = f.conversation();
    let duplicate = conversation
        .iter()
        .filter(|m| m.starts_with("tool!:") && m.contains("duplicate_call"))
        .count();
    assert_eq!(duplicate, 1, "{conversation:?}");
    // The booking's result is in the context exactly once (the late note of the one run).
    let results = conversation
        .iter()
        .filter(|m| m.contains("confirmed-42"))
        .count();
    assert_eq!(results, 1, "{conversation:?}");

    // The filler is in the context only as far as it was heard: the barge-in came about half a
    // second into the clip, so a strict, non-empty word prefix of the phrase.
    let fillers: Vec<&String> = conversation
        .iter()
        .filter(|m| {
            m.strip_prefix("assistant:")
                .is_some_and(|t| !t.is_empty() && is_word_prefix(t, FILLER))
        })
        .collect();
    assert_eq!(fillers.len(), 1, "{conversation:?}");
    assert!(
        !conversation
            .iter()
            .any(|m| m == &format!("assistant:{FILLER}")),
        "the cut filler is not kept whole: {conversation:?}"
    );
    assert_eq!(
        conversation.last().map(String::as_str),
        Some("assistant:You are welcome.")
    );
    f.context().validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_that_finishes_before_the_regenerated_call_is_reported_once() {
    // The 2 s booking starts at about 1.5 s; the filler clip plays from about 2.7 s; the caller
    // barges in at 3.0 s and the booking finishes at about 3.5 s, while the caller still speaks.
    // Its late note reaches the context when the regenerated turn starts (about 4.2 s), before
    // the regenerated response repeats the identical booking: the repeat must not report the
    // result a second time.
    let f = Fixture::new(2 * SECOND);
    let session = SimSession::new(Arc::new(SimClock::new()));
    session
        .llm
        .push(LlmScript::text(["Let me book that."]).with_tool_call("c1", "book", BOOKING));
    session
        .llm
        .push(LlmScript::text(["Sure."]).with_tool_call("c1", "book", BOOKING));
    session.llm.push(LlmScript::text(["It is booked."]));
    session.llm.push(LlmScript::text(["You are welcome."]));
    let scenario = Scenario::new()
        .silence(100 * MS)
        .speak("book a table for two", 800 * MS)
        .silence(2_100 * MS)
        .speak("sorry go on", 600 * MS)
        .silence(5_400 * MS)
        .speak("thanks", 400 * MS)
        .with_tail(3 * SECOND);
    let run = session
        .run_with(&f.factory, &scenario, 2 * SECOND)
        .await
        .unwrap();
    assert_eq!(run.outcome, Ok(()));
    run.check_no_stale_audio_after_interrupt().unwrap();

    // The booking's result is in the context exactly once (the late note of the one run), and
    // the regenerated identical call, which came after the note, was answered without it.
    let conversation = f.conversation();
    let results = conversation
        .iter()
        .filter(|m| m.contains("confirmed-42"))
        .count();
    assert_eq!(results, 1, "{conversation:?}");
    let note = conversation
        .iter()
        .position(|m| m.contains("confirmed-42"))
        .unwrap_or_else(|| panic!("{conversation:?}"));
    let repeat = conversation
        .iter()
        .position(|m| m.starts_with("tool!:") && m.contains("already_reported"))
        .unwrap_or_else(|| panic!("{conversation:?}"));
    assert!(note < repeat, "{conversation:?}");

    // The booking ran once and finished as a late note; the regenerated identical call came
    // after the note was in the context.
    assert_eq!((f.probe.started(), f.probe.finished()), (1, 1));
    let tools = f.tools();
    assert_eq!(tools.side_effect_interrupted, 1, "{tools:?}");
    assert_eq!(tools.side_effect_late_results, 1, "{tools:?}");
    assert_eq!(tools.orphan_in_flight, 0, "{tools:?}");
    assert_eq!(tools.orphan_replays, 0, "{tools:?}");
    assert_eq!(tools.orphan_reported, 1, "{tools:?}");
    assert_eq!(
        conversation.last().map(String::as_str),
        Some("assistant:You are welcome.")
    );
    f.context().validate().unwrap();
}
