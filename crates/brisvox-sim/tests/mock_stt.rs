//! MockStt: interim/final scripting, finalize semantics, latency and faults, under paused time.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::{Error, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use bytes::Bytes;

const MS: Duration = Duration::from_millis(1);

fn config() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

fn mock(clock: &SharedClock) -> MockStt {
    MockStt::new(clock.clone()).with_latency(
        LatencyProfile::Fixed(30 * MS),
        LatencyProfile::Fixed(100 * MS),
        LatencyProfile::Fixed(200 * MS),
        0,
    )
}

/// (ms since `start`, text, is_final, speech_final, from_finalize)
type Seen = (u128, String, bool, bool, bool);

async fn next(
    clock: &SharedClock,
    start: tokio::time::Instant,
    s: &mut SttSession,
) -> Option<Seen> {
    match s.events.recv().await? {
        SttEvent::Transcript {
            text,
            is_final,
            speech_final,
            from_finalize,
            ..
        } => Some((
            (clock.now() - start).as_millis(),
            text.to_string(),
            is_final,
            speech_final,
            from_finalize,
        )),
        SttEvent::Error(e) => Some((
            (clock.now() - start).as_millis(),
            format!("error: {e}"),
            false,
            false,
            false,
        )),
    }
}

fn audio() -> SttInput {
    SttInput::Audio(Bytes::from(vec![0u8; 640]))
}

#[tokio::test(start_paused = true)]
async fn interims_then_finalize_produces_flagged_finals() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::words("book a table for two"));
    let start = clock.now();
    let mut s = stt.connect(config()).await.unwrap();
    assert_eq!((clock.now() - start).as_millis(), 30, "connect latency");
    let start = clock.now();
    s.input.send(audio()).await.unwrap();
    assert_eq!(
        next(&clock, start, &mut s).await.unwrap(),
        (100, "book".into(), false, false, false)
    );
    assert_eq!(
        next(&clock, start, &mut s).await.unwrap(),
        (200, "book a".into(), false, false, false)
    );
    clock.sleep(50 * MS).await; // t = 250: the user stops; VAD finalizes
    s.input.send(SttInput::Finalize).await.unwrap();
    // Pending interims (300, 400) are dropped; the final follows 200 ms after Finalize.
    assert_eq!(
        next(&clock, start, &mut s).await.unwrap(),
        (450, "book a table for two".into(), true, true, true)
    );
    assert_eq!(stt.finalizes(), 1);
    assert_eq!(stt.audio_bytes(), 640);
}

#[tokio::test(start_paused = true)]
async fn several_final_segments_end_with_speech_final_and_the_next_utterance_follows() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance {
        interims: Vec::new(),
        finals: vec!["I'd like".into(), "a coffee".into()],
        language: None,
    });
    stt.push(SttUtterance::words("thanks"));
    let start = clock.now();
    let mut s = stt.connect(config()).await.unwrap();
    s.input.send(audio()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap();
    let a = next(&clock, start, &mut s).await.unwrap();
    let b = next(&clock, start, &mut s).await.unwrap();
    assert_eq!((a.2, a.3, a.4), (true, false, true));
    assert_eq!(
        (b.1.as_str(), b.2, b.3, b.4),
        ("a coffee", true, true, true)
    );
    s.input.send(audio()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(next(&clock, start, &mut s).await.unwrap().1, "thanks");
}

#[tokio::test(start_paused = true)]
async fn connect_error_disconnect_and_stall() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push_session_faults(vec![StreamFault::ConnectError { status: 401 }]);
    assert!(matches!(
        stt.connect(config()).await,
        Err(Error::Rejected { status: 401, .. })
    ));

    stt.push(SttUtterance::words("one two three"));
    stt.push_session_faults(vec![
        StreamFault::Stall {
            before_event: 1,
            duration: 1_000 * MS,
        },
        StreamFault::Disconnect { after_events: 2 },
    ]);
    let start = clock.now();
    let mut s = stt.connect(config()).await.unwrap();
    let start_audio = clock.now();
    s.input.send(audio()).await.unwrap();
    assert_eq!(next(&clock, start_audio, &mut s).await.unwrap().0, 100);
    // Second interim (due at 200) stalls 1 s.
    assert_eq!(next(&clock, start_audio, &mut s).await.unwrap().0, 1_200);
    // The connection drops right after the second event.
    let (_, text, ..) = next(&clock, start, &mut s).await.unwrap();
    assert!(text.starts_with("error: mock-stt: disconnected"), "{text}");
    assert!(
        s.events.recv().await.is_none(),
        "stream must close after the drop"
    );
}

#[tokio::test(start_paused = true)]
async fn closing_the_session_ends_its_task() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let stt = mock(&clock);
    stt.push(SttUtterance::words("hello there"));
    let s = stt.connect(config()).await.unwrap();
    s.input.send(audio()).await.unwrap();
    drop(s);
    counter
        .wait_for_baseline(clock.as_ref(), Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn finalize_without_audio_is_ignored_and_empty_finals_still_end_the_utterance() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::default()); // the user said nothing recognisable
    stt.push(SttUtterance::words("next"));
    let start = clock.now();
    let mut s = stt.connect(config()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap(); // no audio yet: ignored
    s.input.send(audio()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap(); // double finalize: ignored
    let (_, text, is_final, speech_final, from_finalize) =
        next(&clock, start, &mut s).await.unwrap();
    assert_eq!(
        (text.as_str(), is_final, speech_final, from_finalize),
        ("", true, true, true)
    );
    s.input.send(audio()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(next(&clock, start, &mut s).await.unwrap().1, "next");
    assert_eq!(stt.finalizes(), 4);
}

#[tokio::test(start_paused = true)]
async fn input_closed_with_pending_events_still_delivers_them() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::words("all of it"));
    let start = clock.now();
    let s = stt.connect(config()).await.unwrap();
    s.input.send(audio()).await.unwrap();
    s.input.send(SttInput::Finalize).await.unwrap();
    let (input, mut events) = (s.input, s.events);
    drop(input);
    let mut texts = Vec::new();
    while let Some(SttEvent::Transcript { text, .. }) = events.recv().await {
        texts.push(text.to_string());
    }
    assert_eq!(texts, ["all of it"]);
    assert!((clock.now() - start) >= 200 * MS);
}

#[tokio::test(start_paused = true)]
async fn dropping_during_a_stall_ends_the_task_promptly() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let stt = mock(&clock);
    stt.push(SttUtterance::words("stalled words here"));
    stt.push_session_faults(vec![StreamFault::Stall {
        before_event: 0,
        duration: 3_600_000 * MS,
    }]);
    let s = stt.connect(config()).await.unwrap();
    s.input.send(audio()).await.unwrap();
    clock.sleep(500 * MS).await; // inside the one-hour stall
    drop(s);
    counter
        .wait_for_baseline(clock.as_ref(), Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn hanging_up_discards_the_interims_still_owed() {
    // Interims come every 100 ms (100, 200, 300, 400 ms after the first audio). The caller hangs
    // up at 150 ms: the one interim already due was delivered, the three owed ones are dropped,
    // and the events close without a final transcript.
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::words("book a table for two"));
    let s = stt.connect(config()).await.unwrap();
    let start = clock.now();
    s.input.send(audio()).await.unwrap();
    let (input, mut events) = (s.input, s.events);
    clock.sleep(150 * MS).await;
    drop(input);
    let mut seen = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            SttEvent::Transcript { text, is_final, .. } => seen.push((text.to_string(), is_final)),
            SttEvent::Error(e) => panic!("unexpected error {e}"),
        }
    }
    assert_eq!(seen, [("book".to_owned(), false)]);
    assert!(
        (clock.now() - start) < 250 * MS,
        "the session must end at the hang-up, not play out the owed interims"
    );
}

#[tokio::test(start_paused = true)]
async fn hanging_up_before_the_first_interim_yields_no_transcript_at_all() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::words("book a table for two"));
    let s = stt.connect(config()).await.unwrap();
    s.input.send(audio()).await.unwrap();
    let (input, mut events) = (s.input, s.events);
    drop(input);
    assert!(events.recv().await.is_none());
}
