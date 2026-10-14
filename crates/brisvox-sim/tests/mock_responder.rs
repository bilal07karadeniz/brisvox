//! The mocks' additions for speculation: an LLM that answers by request, and STT
//! interims that follow the speech and converge to the final.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::{
    LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, SttConfig, SttEvent, SttInput,
    SttProvider, SttSession,
};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use bytes::Bytes;

const MS: Duration = Duration::from_millis(1);

fn request(text: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, text)],
        tools: Vec::new(),
    }
}

/// The first text delta of a response ("" if it has none).
async fn first_text(mut stream: LlmStream) -> String {
    while let Some(event) = stream.next().await {
        if let LlmEvent::TextDelta(text) = event {
            return text.to_string();
        }
    }
    String::new()
}

#[tokio::test(start_paused = true)]
async fn a_responder_answers_by_request_and_leaves_the_queue_alone() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = MockLlm::new(clock);
    llm.push(LlmScript::text(["queued"]));
    // Answers the request that says "hello"; any other request falls through to the queue.
    llm.set_responder(Arc::new(|request: &LlmRequest| {
        request
            .messages
            .iter()
            .any(|m| m.content.as_ref() == "hello")
            .then(|| LlmScript::text(["from the responder"]))
    }));
    let mut texts = Vec::new();
    for text in ["hello", "hello", "other", "other"] {
        texts.push(first_text(llm.stream(request(text)).await.unwrap()).await);
    }
    // Two requests to the responder consume nothing of the queue: "queued" is still the third
    // answer; the fourth finds the queue empty and the fallback silent.
    assert_eq!(
        texts,
        ["from the responder", "from the responder", "queued", ""]
    );
    assert_eq!(llm.requests().len(), 4);
    llm.clear_script();
    let text = first_text(llm.stream(request("hello")).await.unwrap()).await;
    assert_eq!(text, "", "clear_script drops the responder");
}

fn config() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

fn mock(clock: &SharedClock) -> MockStt {
    MockStt::new(clock.clone()).with_latency(
        LatencyProfile::Fixed(MS),
        LatencyProfile::Fixed(100 * MS),
        LatencyProfile::Fixed(200 * MS),
        0,
    )
}

/// 20 ms of PCM16 at a constant level.
fn level(sample: i16) -> SttInput {
    let bytes: Vec<u8> = std::iter::repeat_n(sample.to_le_bytes(), 320)
        .flatten()
        .collect();
    SttInput::Audio(Bytes::from(bytes))
}

/// (ms since `start`, text) of the next interim.
async fn next_interim(
    clock: &SharedClock,
    start: tokio::time::Instant,
    s: &mut SttSession,
) -> (u128, String) {
    let at = |clock: &SharedClock| (clock.now() - start).as_millis();
    match s.events.recv().await.unwrap() {
        SttEvent::Transcript {
            text,
            is_final: false,
            ..
        } => (at(clock), text.to_string()),
        // Whatever else arrives shows in the failed comparison.
        other => (at(clock), format!("not an interim: {other:?}")),
    }
}

#[tokio::test(start_paused = true)]
async fn interims_can_start_with_the_speech_and_converge_to_the_final() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.set_interims_from_speech(500.0);
    stt.set_converged_interims(true);
    stt.push(SttUtterance::words("book a table"));
    let mut s = stt.connect(config()).await.unwrap();
    let start = clock.now();
    // 300 ms of quiet: no interim is scheduled yet.
    for _ in 0..15 {
        s.input.send(level(10)).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    s.input.send(level(2_000)).await.unwrap();
    let speech = (clock.now() - start).as_millis();
    assert_eq!(speech, 300);
    // The interims follow the speech at 100 ms: the mock's two word prefixes, then the whole
    // text.
    for (i, text) in ["book", "book a", "book a table"].into_iter().enumerate() {
        let (at, seen) = next_interim(&clock, start, &mut s).await;
        assert_eq!(seen, text);
        assert_eq!(at, speech + 100 * (i as u128 + 1));
    }
}

#[tokio::test(start_paused = true)]
async fn by_default_the_last_interim_is_a_word_short_and_starts_with_the_first_frame() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let stt = mock(&clock);
    stt.push(SttUtterance::words("book a table"));
    let mut s = stt.connect(config()).await.unwrap();
    let start = clock.now();
    s.input.send(level(10)).await.unwrap();
    assert_eq!(
        next_interim(&clock, start, &mut s).await,
        (100, "book".to_owned())
    );
    assert_eq!(
        next_interim(&clock, start, &mut s).await,
        (200, "book a".to_owned())
    );
    clock.sleep(500 * MS).await;
    // No third interim: the last one stays a word short of the final.
    assert!(s.events.try_recv().is_err());
}
