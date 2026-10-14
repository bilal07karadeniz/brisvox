//! MockTts: audio proportional to text, word timestamps, flush and priority cancel, latency and
//! faults, under paused time.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::{ContextId, Error, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_tts::MockTts;

const MS: Duration = Duration::from_millis(1);
const C1: ContextId = ContextId(1);
const C2: ContextId = ContextId(2);

fn config() -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from("test"),
    }
}

fn mock(clock: &SharedClock) -> MockTts {
    MockTts::new(clock.clone())
        .with_latency(
            LatencyProfile::Fixed(20 * MS),
            LatencyProfile::Fixed(150 * MS),
            LatencyProfile::Fixed(10 * MS),
            0,
        )
        .with_audio(60, 40 * MS)
}

fn text(context: ContextId, t: &str) -> TtsInput {
    TtsInput::Text {
        context,
        text: Arc::from(t),
    }
}

#[derive(Debug, Default)]
struct Summary {
    first_audio_ms: Option<u128>,
    audio_bytes: usize,
    words: Vec<(String, Duration)>,
    flushed_ms: Option<u128>,
    cancelled: bool,
    error: Option<String>,
    other_contexts: usize,
}

/// Reads events until `context` is flushed or cancelled, or the stream ends.
async fn drain(clock: &SharedClock, s: &mut TtsSession, context: ContextId) -> Summary {
    let start = clock.now();
    let mut out = Summary::default();
    while let Some(event) = s.events.recv().await {
        let ms = (clock.now() - start).as_millis();
        match event {
            TtsEvent::Audio { context: c, audio } if c == context => {
                out.first_audio_ms.get_or_insert(ms);
                out.audio_bytes += audio.len();
            }
            TtsEvent::Word {
                context: c,
                word,
                start,
            } if c == context => {
                out.words.push((word.to_string(), start));
            }
            TtsEvent::Flushed { context: c } if c == context => {
                out.flushed_ms = Some(ms);
                return out;
            }
            TtsEvent::Cancelled { context: c } if c == context => {
                out.cancelled = true;
                return out;
            }
            TtsEvent::Error(e) => {
                out.error = Some(e.to_string());
                return out;
            }
            _ => out.other_contexts += 1,
        }
    }
    out
}

#[tokio::test(start_paused = true)]
async fn audio_and_word_timestamps_follow_the_text() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    s.input.send(text(C1, "hello world")).await.unwrap();
    s.input.send(TtsInput::Flush { context: C1 }).await.unwrap();
    let out = drain(&clock, &mut s, C1).await;
    assert_eq!(out.first_audio_ms, Some(150));
    // 11 chars x 60 ms = 660 ms of 24 kHz PCM16.
    assert_eq!(out.audio_bytes, 660 * 24 * 2);
    assert_eq!(
        out.words,
        [("hello".into(), Duration::ZERO), ("world".into(), 360 * MS)]
    );
    // 17 chunks of 40 ms (last one 20 ms), 10 ms apart: last at 150 + 16 x 10 = 310; the
    // context is busy until one interval later, when `Flushed` follows.
    assert_eq!(out.flushed_ms, Some(320));
}

#[tokio::test(start_paused = true)]
async fn continuation_text_keeps_offsets_running() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    s.input.send(text(C1, "Hi")).await.unwrap();
    s.input.send(text(C1, " there friend")).await.unwrap();
    s.input.send(TtsInput::Flush { context: C1 }).await.unwrap();
    let out = drain(&clock, &mut s, C1).await;
    let words: Vec<(&str, u128)> = out
        .words
        .iter()
        .map(|(w, d)| (w.as_str(), d.as_millis()))
        .collect();
    assert_eq!(words, [("Hi", 0), ("there", 180), ("friend", 540)]);
    assert_eq!(tts.texts().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn cancel_overtakes_queued_text_and_silences_the_context() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    s.input
        .send(text(C1, "a very long answer that is being spoken"))
        .await
        .unwrap();
    // Barge-in right away: the cancel goes through the priority channel.
    s.cancel.send(C1).await.unwrap();
    s.input
        .send(text(C1, "late text for the cancelled turn"))
        .await
        .unwrap();
    let out = drain(&clock, &mut s, C1).await;
    assert!(out.cancelled);
    assert_eq!(out.audio_bytes, 0, "no audio of a cancelled context");
    assert!(
        tts.texts().is_empty(),
        "the cancel must overtake the queued text"
    );
    // The next turn works normally and nothing of C1 follows.
    s.input.send(text(C2, "ok")).await.unwrap();
    s.input.send(TtsInput::Flush { context: C2 }).await.unwrap();
    let out = drain(&clock, &mut s, C2).await;
    assert!(out.flushed_ms.is_some());
    assert_eq!(
        out.other_contexts, 0,
        "stale events of the cancelled context leaked"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_mid_utterance_stops_its_audio() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    s.input
        .send(text(C1, "one two three four five six"))
        .await
        .unwrap();
    let mut audio_events = 0;
    while audio_events < 3 {
        if let Some(TtsEvent::Audio { .. }) = s.events.recv().await {
            audio_events += 1;
        }
    }
    s.cancel.send(C1).await.unwrap();
    let out = drain(&clock, &mut s, C1).await;
    assert!(out.cancelled);
    assert_eq!(out.audio_bytes, 0, "audio after the cancel");
}

#[tokio::test(start_paused = true)]
async fn faults() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    tts.push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
    assert!(matches!(
        tts.connect(config()).await,
        Err(Error::Rejected { status: 503, .. })
    ));

    tts.push_session_faults(vec![
        StreamFault::Stall {
            before_event: 0,
            duration: 2_000 * MS,
        },
        StreamFault::Disconnect { after_events: 3 },
    ]);
    let mut s = tts.connect(config()).await.unwrap();
    s.input.send(text(C1, "hello world")).await.unwrap();
    let out = drain(&clock, &mut s, C1).await;
    assert_eq!(out.first_audio_ms, Some(2_150), "stalled first byte");
    // Events 0..3 are Word, Word, Audio; the drop follows right after the third.
    assert_eq!(out.words.len(), 2);
    assert_eq!(out.audio_bytes, 40 * 24 * 2);
    assert_eq!(out.error.as_deref(), Some("mock-tts: disconnected"));
    assert!(s.events.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn closing_the_session_ends_its_task() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let tts = mock(&clock);
    let s = tts.connect(config()).await.unwrap();
    s.input.send(text(C1, "goodbye")).await.unwrap();
    drop(s);
    counter
        .wait_for_baseline(clock.as_ref(), Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancel_during_a_stall_is_acknowledged_at_once() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    tts.push_session_faults(vec![StreamFault::Stall {
        before_event: 0,
        duration: 3_600_000 * MS,
    }]);
    let mut s = tts.connect(config()).await.unwrap();
    s.input.send(text(C1, "stuck in a stall")).await.unwrap();
    clock.sleep(500 * MS).await; // inside the one-hour stall
    let start = clock.now();
    s.cancel.send(C1).await.unwrap();
    let out = drain(&clock, &mut s, C1).await;
    assert!(out.cancelled);
    assert_eq!(out.audio_bytes, 0);
    assert!(clock.now() - start < 10 * MS, "cancel waited for the stall");
}

#[tokio::test(start_paused = true)]
async fn flush_for_an_idle_or_unknown_context_reports_at_once() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    s.input.send(TtsInput::Flush { context: C2 }).await.unwrap();
    let out = drain(&clock, &mut s, C2).await;
    assert_eq!(out.flushed_ms, Some(0));
}

#[tokio::test(start_paused = true)]
async fn many_cancels_stay_bounded_and_correct() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let tts = mock(&clock);
    let mut s = tts.connect(config()).await.unwrap();
    for id in 0..200 {
        let context = ContextId(id);
        s.input.send(text(context, "x")).await.unwrap();
        s.cancel.send(context).await.unwrap();
        let out = drain(&clock, &mut s, context).await;
        assert!(out.cancelled, "context {id}");
    }
    s.input.send(text(ContextId(1_000), "fine")).await.unwrap();
    s.input
        .send(TtsInput::Flush {
            context: ContextId(1_000),
        })
        .await
        .unwrap();
    assert!(
        drain(&clock, &mut s, ContextId(1_000))
            .await
            .flushed_ms
            .is_some()
    );
}
