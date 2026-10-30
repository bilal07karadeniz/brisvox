//! ElevenLabs adapter tests: the provider contract suite and protocol-level checks
//! against a local mock of the ElevenLabs multi-context TTS WebSocket, plus a live test gated on
//! `BRISVOX_LIVE_TESTS=1`.
//!
//! The sockets are real, so these tests use the real system clock and short deadlines instead
//! of paused time (the `disallowed_methods` lint guards engine logic, not test pacing).
#![cfg(feature = "provider-elevenlabs")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    missing_docs
)]

#[path = "elevenlabs/mock.rs"]
mod mock;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::elevenlabs::{
    ElevenLabsConfig, ElevenLabsTts, OutputEncoding, TextNormalization, VoiceSettings,
};
use brisvox_providers::{ContextId, Error, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use brisvox_sim::contract::{CONTRACT_TIMEOUT, Fault, TtsHarness, tts_contract};
use mock::{AlignMode, CancelMode, FinalMode, Mock, audio_msg, final_msg};
use serde_json::{Value, json};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn config(mock: &Mock) -> ElevenLabsConfig {
    let mut config = ElevenLabsConfig::new(Secret::new(mock.api_key.clone()), clock());
    config.url = mock.url.clone();
    config.io_timeout = Duration::from_millis(1500);
    config
}

fn provider(mock: &Mock) -> ElevenLabsTts {
    ElevenLabsTts::new(config(mock)).unwrap()
}

fn tts_config() -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz16000,
        voice: Arc::from("voice-1"),
        language: None,
        model: None,
    }
}

fn text(context: u64, text: &str) -> TtsInput {
    TtsInput::Text {
        context: ContextId(context),
        text: Arc::from(text),
    }
}

fn flush(context: u64) -> TtsInput {
    TtsInput::Flush {
        context: ContextId(context),
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The next event within `millis` milliseconds.
async fn next(events: &mut mpsc::Receiver<TtsEvent>, millis: u64) -> Option<TtsEvent> {
    tokio::time::timeout(ms(millis), events.recv())
        .await
        .unwrap_or_else(|_| panic!("no event within {millis} ms"))
}

#[derive(Debug, Default)]
struct Utterance {
    audio: Vec<u8>,
    audio_events: usize,
    words: Vec<(String, Duration)>,
}

/// Reads events up to `Flushed` of `context`.
async fn until_flushed(events: &mut mpsc::Receiver<TtsEvent>, context: u64) -> Utterance {
    until_flushed_within(events, context, 3000).await
}

/// Like [`until_flushed`], with `millis` milliseconds allowed between two events.
async fn until_flushed_within(
    events: &mut mpsc::Receiver<TtsEvent>,
    context: u64,
    millis: u64,
) -> Utterance {
    let mut utterance = Utterance::default();
    loop {
        match next(events, millis).await.expect("stream ended early") {
            TtsEvent::Audio { context: c, audio } if c == ContextId(context) => {
                utterance.audio.extend_from_slice(&audio);
                utterance.audio_events += 1;
            }
            TtsEvent::Word {
                context: c,
                word,
                start,
            } if c == ContextId(context) => utterance.words.push((word.to_string(), start)),
            TtsEvent::Flushed { context: c } if c == ContextId(context) => return utterance,
            other => panic!("unexpected event {other:?}"),
        }
    }
}

async fn expect_silence(events: &mut mpsc::Receiver<TtsEvent>, millis: u64) {
    if let Ok(event) = tokio::time::timeout(ms(millis), events.recv()).await {
        panic!("unexpected {event:?}");
    }
}

async fn open(mock: &Mock) -> TtsSession {
    provider(mock).connect(tts_config()).await.unwrap()
}

fn word_texts(words: &[(String, Duration)]) -> Vec<&str> {
    words.iter().map(|(w, _)| w.as_str()).collect()
}

fn word_ms(words: &[(String, Duration)]) -> Vec<u64> {
    words.iter().map(|(_, d)| d.as_millis() as u64).collect()
}

/// PCM16 mono 16 kHz audio of `millis` milliseconds.
fn pcm(millis: u64) -> Vec<u8> {
    vec![1; (millis * 32) as usize]
}

/// An alignment object for `chars` with `step` ms between the characters.
fn align(chars: &str, step: u64) -> Value {
    let list: Vec<String> = chars.chars().map(String::from).collect();
    let starts: Vec<u64> = (0..list.len() as u64).map(|i| i * step).collect();
    let durations: Vec<u64> = list.iter().map(|_| step).collect();
    json!({"chars": list, "charStartTimesMs": starts, "charDurationsMs": durations})
}

fn context_of(request: &Value) -> String {
    request["context_id"].as_str().unwrap_or("").to_owned()
}

/// Makes the mock answer the text frame `expected` with the frames `reply` builds (the rest of
/// the protocol keeps its default behaviour).
fn reply_to(
    mock: &Mock,
    expected: &'static str,
    reply: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
) {
    mock.configure(|c| {
        c.responder = Some(Arc::new(move |request: &Value| {
            (request["text"] == expected && request["flush"] == true)
                .then(|| reply(&context_of(request)))
        }));
    });
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite
// ---------------------------------------------------------------------------------------------

struct Harness {
    mock: Mock,
    provider: Arc<ElevenLabsTts>,
    clock: SharedClock,
}

impl Harness {
    fn new() -> Self {
        let mock = Mock::start();
        // The alignment of a word arrives in its own message, ahead of the audio. With both in
        // one message (the default mock) a single frame is two events (Audio, Word), and the
        // suite's "idle gap after the first event" check, which stalls the second frame, would
        // find the second event already delivered.
        mock.configure(|c| {
            c.interval = ms(20);
            c.align = AlignMode::AlignmentFirst;
        });
        let mut cfg = config(&mock);
        cfg.io_timeout = CONTRACT_TIMEOUT;
        // The contract suite's sessions use the voice "contract" at 16 kHz.
        cfg.warm_target = Some((Arc::from("contract"), SampleRate::Hz16000));
        let provider = Arc::new(ElevenLabsTts::new(cfg).unwrap());
        Self {
            mock,
            provider,
            clock: clock(),
        }
    }
}

impl TtsHarness for Harness {
    type P = ElevenLabsTts;

    fn provider(&self) -> Arc<ElevenLabsTts> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn reset(&self) {
        self.mock.reset();
    }

    fn inject(&self, fault: Fault) {
        self.mock.inject(fault);
    }
}

/// Runs the whole suite (about 15 s: it contains several real-time deadline checks).
///
/// On a thread with a big stack: in debug builds the suite's futures are large and overflow the
/// default 2 MiB test-thread stack.
#[test]
fn passes_the_provider_contract_suite() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_contract_suite());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_contract_suite() {
    let harness = Harness::new();
    let report = tts_contract(&harness).await;
    let failures: Vec<String> = report
        .failures()
        .map(|c| format!("{}: {}", c.name, c.failure.as_deref().unwrap_or("")))
        .collect();
    assert!(failures.is_empty(), "contract failures: {failures:#?}");
    for check in [
        "audio_and_words",
        "flush",
        "cancel",
        "reconnect",
        "error_mapping",
        "timeout",
        "warm",
        "no_task_leak",
    ] {
        assert!(report.check(check).is_some(), "check {check} did not run");
    }
}

// ---------------------------------------------------------------------------------------------
// Handshake, auth, configuration
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn handshake_names_voice_model_and_format_and_sends_the_key_only_in_a_header() {
    let mock = Mock::start();
    let session = open(&mock).await;
    let handshakes = mock.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert!(handshakes[0].key_ok);
    assert!(!handshakes[0].key_in_query);
    assert_eq!(
        handshakes[0].uri,
        "/v1/text-to-speech/voice-1/multi-stream-input?model_id=eleven_flash_v2_5\
         &output_format=pcm_16000&inactivity_timeout=180&sync_alignment=false"
    );
    drop(session);
}

#[tokio::test]
async fn optional_settings_reach_the_url() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.model_id = "eleven_multilingual_v2".to_owned();
    cfg.language_code = Some("tr".to_owned());
    cfg.apply_text_normalization = Some(TextNormalization::On);
    cfg.enable_logging = Some(false);
    cfg.inactivity_timeout_secs = 60;
    cfg.sync_alignment = false;
    cfg.url = format!("{}/", mock.url); // a trailing slash is fine
    let _session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz24000,
            voice: Arc::from("tr_voice-2"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    assert_eq!(
        mock.handshakes()[0].uri,
        "/v1/text-to-speech/tr_voice-2/multi-stream-input?model_id=eleven_multilingual_v2\
         &output_format=pcm_24000&inactivity_timeout=60&sync_alignment=false&language_code=tr\
         &apply_text_normalization=on&enable_logging=false"
    );
}

#[tokio::test]
async fn the_session_language_and_model_reach_the_url() {
    let mock = Mock::start();
    let provider = ElevenLabsTts::new(config(&mock)).unwrap();
    let open = |language: Option<&'static str>, model: Option<&'static str>| {
        provider.connect(TtsConfig {
            sample_rate: SampleRate::Hz16000,
            voice: Arc::from("tr_voice"),
            language: language.map(Arc::from),
            model: model.map(Arc::from),
        })
    };
    let _a = open(Some("tr"), None).await.unwrap();
    let _b = open(Some("tr"), Some("eleven_v3")).await.unwrap();
    // The multilingual_v2 models do not take `language_code`: it is left out.
    let _c = open(Some("tr"), Some("eleven_multilingual_v2"))
        .await
        .unwrap();
    let uris: Vec<String> = mock.handshakes().into_iter().map(|h| h.uri).collect();
    assert_eq!(
        uris[0],
        "/v1/text-to-speech/tr_voice/multi-stream-input?model_id=eleven_flash_v2_5\
         &output_format=pcm_16000&inactivity_timeout=180&sync_alignment=false&language_code=tr"
    );
    assert!(
        uris[1].contains("model_id=eleven_v3&") && uris[1].ends_with("&language_code=tr"),
        "{}",
        uris[1]
    );
    assert!(
        uris[2].contains("model_id=eleven_multilingual_v2&") && !uris[2].contains("language_code"),
        "{}",
        uris[2]
    );
}

#[tokio::test]
async fn a_wrong_key_is_rejected_with_401() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("wrong".to_owned());
    let provider = ElevenLabsTts::new(cfg).unwrap();
    let err = provider.connect(tts_config()).await.unwrap_err();
    assert!(matches!(err, Error::Rejected { status: 401, .. }), "{err}");
}

#[tokio::test]
async fn handshake_refusals_keep_their_status() {
    let mock = Mock::start();
    for status in [401_u16, 403, 429, 500] {
        mock.inject(Fault::Reject { status });
        let err = provider(&mock).connect(tts_config()).await.unwrap_err();
        assert!(
            matches!(err, Error::Rejected { status: s, .. } if s == status),
            "{status}: {err}"
        );
    }
}

#[tokio::test]
async fn the_key_never_shows_up_in_debug_or_errors() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let debug = format!("{provider:?}");
    assert!(!debug.contains(&mock.api_key), "{debug}");
    assert!(debug.contains("REDACTED"));
    // An unusable key is rejected without echoing it.
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("bad\nkey-with-newline".to_owned());
    let err = ElevenLabsTts::new(cfg).unwrap_err().to_string();
    assert!(!err.contains("bad"), "{err}");
}

#[tokio::test]
async fn invalid_configuration_is_refused_up_front() {
    let mock = Mock::start();
    let refused = |change: &dyn Fn(&mut ElevenLabsConfig)| {
        let mut cfg = config(&mock);
        change(&mut cfg);
        assert!(ElevenLabsTts::new(cfg).is_err());
    };
    refused(&|c| c.io_timeout = Duration::ZERO);
    refused(&|c| c.keepalive_interval = Duration::ZERO);
    refused(&|c| c.final_grace = Duration::ZERO);
    refused(&|c| c.inactivity_timeout_secs = 0);
    refused(&|c| c.inactivity_timeout_secs = 181);
    refused(&|c| c.model_id = "a/b".to_owned());
    refused(&|c| c.model_id = String::new());
    refused(&|c| c.language_code = Some("t r".to_owned()));
    refused(&|c| c.chunk_length_schedule = Some(vec![10]));
    refused(&|c| c.chunk_length_schedule = Some(vec![]));
    refused(&|c| c.chunk_length_schedule = Some(vec![501]));
    refused(&|c| {
        c.voice_settings = Some(VoiceSettings {
            speed: Some(2.0),
            ..VoiceSettings::default()
        });
    });
    refused(&|c| c.url = "ws://example.com".to_owned());
    refused(&|c| c.url = "wss://api.elevenlabs.io/v1".to_owned());
    refused(&|c| c.warm_target = Some((Arc::from("a/b"), SampleRate::Hz16000)));

    // The session config is checked at connect: voice ids are plain tokens, mu-law needs 8 kHz.
    for voice in ["", "a/b", "a?b=1", "../x"] {
        let bad = TtsConfig {
            voice: Arc::from(voice),
            ..tts_config()
        };
        assert!(
            matches!(
                provider(&mock).connect(bad).await,
                Err(Error::Protocol { .. })
            ),
            "{voice:?}"
        );
    }
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    assert!(matches!(
        ElevenLabsTts::new(cfg).unwrap().connect(tts_config()).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(mock.connections(), 0, "no connection for a bad config");
}

#[tokio::test]
async fn mulaw_telephony_output_is_requested_and_counted_per_byte() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz8000,
            voice: Arc::from("tel"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    session.input.send(text(1, "Merhaba dünya.")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    // One mu-law byte per sample: 2 words x 0.2 s x 8000 Hz.
    assert_eq!(spoken.audio.len(), 3200);
    assert!(mock.handshakes()[0].uri.contains("output_format=ulaw_8000"));
    assert_eq!(word_texts(&spoken.words), ["Merhaba", "dünya."]);
    assert_eq!(word_ms(&spoken.words), [0, 200]);
}

// ---------------------------------------------------------------------------------------------
// Frames: contexts, text, flush, close
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_context_is_initialised_flushed_per_text_and_closed_on_flush() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.voice_settings = Some(VoiceSettings {
        stability: Some(0.5),
        speed: Some(1.0),
        ..VoiceSettings::default()
    });
    cfg.chunk_length_schedule = Some(vec![50, 120]);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(7, "Hello, ")).await.unwrap();
    session
        .input
        .send(text(7, "brave new world."))
        .await
        .unwrap();
    session.input.send(flush(7)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 7).await;
    assert_eq!(
        word_texts(&spoken.words),
        ["Hello,", "brave", "new", "world."]
    );
    expect_silence(&mut session.events, 100).await;

    let frames = mock.received();
    assert_eq!(frames.len(), 5, "{frames:#?}");
    let sid = context_of(&frames[0]);
    assert!(sid.starts_with("bv"), "{sid}");
    assert_eq!(
        frames[0],
        json!({
            "text": " ", "context_id": sid,
            "voice_settings": {"stability": 0.5, "speed": 1.0},
            "generation_config": {"chunk_length_schedule": [50, 120]}
        })
    );
    // Voice settings go on the first message only; every text is flushed and ends in a space.
    assert_eq!(
        frames[1],
        json!({"text": "Hello, ", "context_id": sid, "flush": true})
    );
    assert_eq!(
        frames[2],
        json!({"text": "brave new world. ", "context_id": sid, "flush": true})
    );
    assert_eq!(frames[3], json!({"context_id": sid, "flush": true}));
    assert_eq!(frames[4], json!({"context_id": sid, "close_context": true}));
}

#[tokio::test]
async fn every_engine_context_gets_its_own_server_context() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    for context in 1..=2 {
        session.input.send(text(context, "one two")).await.unwrap();
        session.input.send(flush(context)).await.unwrap();
        until_flushed(&mut session.events, context).await;
    }
    let ids: Vec<String> = mock
        .received()
        .iter()
        .filter(|f| f["text"] == " ")
        .map(context_of)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "context ids are never reused");
}

#[tokio::test]
async fn whitespace_only_text_sends_nothing_and_an_unknown_flush_is_answered_at_once() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "  \n")).await.unwrap();
    session.input.send(text(1, "")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(1)
    ));
    expect_silence(&mut session.events, 100).await;
    assert!(mock.received().is_empty());
}

#[tokio::test]
async fn text_with_nothing_to_speak_is_dropped_so_a_silent_server_cannot_kill_the_session() {
    let mock = Mock::start();
    // A server that answers text it cannot speak with nothing at all.
    reply_to(&mock, "... ", |_| Vec::new());
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(300);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    for piece in ["...", "-", "\u{1F600}", " \u{2014} ", "?!"] {
        session.input.send(text(1, piece)).await.unwrap();
    }
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(1)
    ));
    // Well past io_timeout: nothing is owed, so no Timeout.
    expect_silence(&mut session.events, 700).await;
    assert!(mock.received().is_empty(), "{:#?}", mock.received());
    // Punctuation inside speakable text still goes out.
    session.input.send(text(2, "Hi there!")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
}

#[tokio::test]
async fn text_after_the_flush_of_a_context_is_ignored_and_a_second_flush_too() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(1, "late words")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["alpha", "beta"]);
    expect_silence(&mut session.events, 200).await;
    assert!(
        mock.received().iter().all(|f| f["text"] != "late words "),
        "nothing was sent for the late text"
    );
}

#[tokio::test]
async fn text_and_a_second_flush_after_flushed_was_delivered_are_ignored_too() {
    // The other side of the race above: here the server's isFinal has long been handled (the
    // engine context is gone from the adapter) when the late input arrives.
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["alpha", "beta"]);
    tokio::time::sleep(ms(100)).await;
    session.input.send(text(1, "late words")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    expect_silence(&mut session.events, 300).await;
    assert!(
        mock.received().iter().all(|f| f["text"] != "late words "),
        "nothing was sent for the late text"
    );
    // A flush for a context nothing was sent for ends it the same way.
    session.input.send(flush(2)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(2)
    ));
    session.input.send(flush(2)).await.unwrap();
    session.input.send(text(2, "also late")).await.unwrap();
    expect_silence(&mut session.events, 300).await;
    assert!(mock.received().iter().all(|f| f["text"] != "also late "));
}

// ---------------------------------------------------------------------------------------------
// Completion: isFinal, the grace fallback, no alignment
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn flushed_waits_for_the_final_message_of_the_context() {
    let mock = Mock::start();
    mock.configure(|c| c.final_mode = FinalMode::Delayed(ms(400)));
    let mut session = open(&mock).await;
    session.input.send(text(1, "slow ending")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // All audio and words arrive; Flushed does not before isFinal.
    let mut events = 0;
    let started = tokio::time::Instant::now();
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => events += 1,
            TtsEvent::Flushed { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(events >= 4);
    assert!(
        started.elapsed() >= ms(350),
        "Flushed came before isFinal: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_snake_case_spelling_of_the_final_message_works_too() {
    let mock = Mock::start();
    mock.configure(|c| c.snake_case = true);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
}

#[tokio::test]
async fn a_server_that_sends_no_final_message_is_finished_after_the_grace() {
    let mock = Mock::start();
    mock.configure(|c| c.final_mode = FinalMode::Never);
    let mut cfg = config(&mock);
    cfg.final_grace = ms(300);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let started = tokio::time::Instant::now();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["alpha", "beta", "gamma"]);
    assert!(started.elapsed() >= ms(300), "{:?}", started.elapsed());
    expect_silence(&mut session.events, 200).await;
    // A second utterance works on the same session.
    session.input.send(text(2, "again")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
}

#[tokio::test]
async fn audio_without_any_alignment_still_completes() {
    let mock = Mock::start();
    mock.configure(|c| c.align = AlignMode::Absent);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(spoken.audio.len(), 12_800, "2 words x 0.2 s");
    assert!(spoken.words.is_empty());
}

#[tokio::test]
async fn only_the_normalized_alignment_still_gives_words_and_completion() {
    let mock = Mock::start();
    mock.configure(|c| c.align = AlignMode::NormalizedOnly);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    // The mock's normalized text is the input with "xx" appended to every word.
    assert_eq!(word_texts(&spoken.words), ["alphaxx", "betaxx"]);
}

#[tokio::test]
async fn normalized_words_can_be_requested_and_default_to_the_input_text() {
    let mock = Mock::start();
    mock.configure(|c| c.align = AlignMode::WithAudio);
    reply_to(&mock, "alpha ", |ctx| {
        vec![audio_msg(
            ctx,
            &pcm(200),
            Some(&align("alpha ", 10)),
            Some(&align("Alpha ", 10)),
        )]
    });
    for (normalized, expected) in [(false, "alpha"), (true, "Alpha")] {
        let mut cfg = config(&mock);
        cfg.use_normalized_alignment = normalized;
        let mut session = ElevenLabsTts::new(cfg)
            .unwrap()
            .connect(tts_config())
            .await
            .unwrap();
        session.input.send(text(1, "alpha")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let spoken = until_flushed(&mut session.events, 1).await;
        assert_eq!(word_texts(&spoken.words), [expected]);
    }
}

// ---------------------------------------------------------------------------------------------
// Words from characters
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn word_offsets_are_the_audio_before_the_message_plus_the_character_time() {
    let mock = Mock::start();
    // Two messages of 200 ms; the alignment of each starts at its own zero.
    reply_to(&mock, "Hello world ", |ctx| {
        vec![
            audio_msg(ctx, &pcm(200), Some(&align("Hello ", 30)), None),
            audio_msg(ctx, &pcm(200), Some(&align("world ", 35)), None),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello world")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["Hello", "world"]);
    assert_eq!(word_ms(&spoken.words), [0, 200]);
    assert_eq!(spoken.audio.len(), 12_800);
}

/// An alignment object for `chars` with the recorded start times `starts` (ms).
fn recorded(chars: &str, starts: &[u64]) -> Value {
    let list: Vec<String> = chars.chars().map(String::from).collect();
    assert_eq!(list.len(), starts.len(), "fixture: one time per character");
    json!({"chars": list, "charStartTimesMs": starts})
}

/// Sends "Hi" and answers it with three recorded messages, `(audio ms, chars, start times)`.
async fn replay(
    sync_alignment: bool,
    messages: &'static [(u64, &'static str, &'static [u64])],
) -> Utterance {
    let mock = Mock::start();
    reply_to(&mock, "Hi ", |ctx| {
        messages
            .iter()
            .map(|(audio_ms, chars, starts)| {
                audio_msg(ctx, &pcm(*audio_ms), Some(&recorded(chars, starts)), None)
            })
            .collect()
    });
    let mut cfg = config(&mock);
    cfg.sync_alignment = sync_alignment;
    let provider = ElevenLabsTts::new(cfg).unwrap();
    let mut session = provider.connect(tts_config()).await.unwrap();
    session.input.send(text(1, "Hi")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await
}

// The two fixtures below are the alignment timings of a live session (eleven_flash_v2_5,
// 2026-10-13, "Hello there, this is a short test. It has two chunks."), three messages each
// with its audio; only chars, start times and audio lengths are kept. The first run is
// `sync_alignment=true`: the times of every message continue those of the previous one, so
// adding the audio delivered so far put the last word 1.4 s past the end of the audio.
const SYNC_RECORDED: &[(u64, &str, &[u64])] = &[
    (
        997,
        "Hello there, this is ",
        &[
            0, 70, 116, 151, 197, 255, 313, 337, 360, 406, 453, 488, 511, 534, 557, 592, 627, 662,
            720, 755, 789,
        ],
    ),
    (
        685,
        "a short test. ",
        &[
            836, 871, 906, 940, 975, 1022, 1080, 1126, 1173, 1219, 1335, 1405, 1521, 1579,
        ],
    ),
    (
        1277,
        "It has two chunks. ",
        &[
            1718, 1834, 1881, 1927, 1950, 1997, 2043, 2101, 2148, 2206, 2252, 2322, 2357, 2426,
            2473, 2531, 2612, 2705, 2798,
        ],
    ),
];

// The same sentence without `sync_alignment`: every message's times start at its own zero.
const CHUNK_RECORDED: &[(u64, &str, &[u64])] = &[
    (
        997,
        "Hello there, this is ",
        &[
            0, 46, 104, 139, 186, 244, 302, 325, 348, 395, 441, 488, 522, 546, 569, 615, 650, 685,
            743, 789, 824,
        ],
    ),
    (
        748,
        "a short test. ",
        &[
            0, 23, 70, 93, 139, 174, 232, 279, 313, 372, 488, 557, 673, 731,
        ],
    ),
    (
        1153,
        "It has two chunks. ",
        &[
            0, 93, 139, 174, 197, 232, 267, 325, 360, 418, 476, 534, 569, 627, 685, 743, 813, 906,
            1022,
        ],
    ),
];

const RECORDED_WORDS: [&str; 11] = [
    "Hello", "there,", "this", "is", "a", "short", "test.", "It", "has", "two", "chunks.",
];

#[tokio::test]
async fn recorded_sync_alignment_times_run_from_the_start_of_the_context() {
    let spoken = replay(true, SYNC_RECORDED).await;
    assert_eq!(word_texts(&spoken.words), RECORDED_WORDS);
    assert_eq!(
        word_ms(&spoken.words),
        [0, 313, 534, 720, 836, 906, 1173, 1718, 1927, 2101, 2322]
    );
    let audio_ms = spoken.audio.len() as u64 / 32;
    assert!(
        word_ms(&spoken.words)
            .last()
            .is_some_and(|&last| last <= audio_ms),
        "the words must not run past the {audio_ms} ms of audio"
    );
}

#[tokio::test]
async fn recorded_chunk_relative_times_add_the_audio_delivered_before_each_message() {
    let spoken = replay(false, CHUNK_RECORDED).await;
    assert_eq!(word_texts(&spoken.words), RECORDED_WORDS);
    assert_eq!(
        word_ms(&spoken.words),
        [0, 302, 546, 743, 997, 1067, 1310, 1745, 1919, 2070, 2279]
    );
    let audio_ms = spoken.audio.len() as u64 / 32;
    assert!(
        word_ms(&spoken.words)
            .last()
            .is_some_and(|&last| last <= audio_ms),
        "the words must not run past the {audio_ms} ms of audio"
    );
}

#[tokio::test]
async fn a_word_spanning_messages_starts_at_its_first_character() {
    let mock = Mock::start();
    reply_to(&mock, "Merhaba dünya ", |ctx| {
        vec![
            audio_msg(ctx, &pcm(100), Some(&align("Mer", 20)), None),
            audio_msg(ctx, &pcm(100), Some(&align("haba dü", 20)), None),
            audio_msg(ctx, &pcm(100), Some(&align("nya ", 20)), None),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Merhaba dünya")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["Merhaba", "dünya"]);
    // "Merhaba" at the 'M' of the first message; "dünya" at the 'd' (index 5 of the second).
    assert_eq!(word_ms(&spoken.words), [0, 100 + 5 * 20]);
}

#[tokio::test]
async fn an_alignment_without_a_trailing_space_still_completes_the_last_word() {
    let mock = Mock::start();
    // The server aligns only the characters, not the space the adapter appended.
    reply_to(&mock, "one two ", |ctx| {
        vec![audio_msg(ctx, &pcm(300), Some(&align("one two", 40)), None)]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "one two")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["one", "two"]);
    assert_eq!(word_ms(&spoken.words), [0, 160]);
}

#[tokio::test]
async fn offsets_continue_across_text_pieces_and_never_go_backwards() {
    let mock = Mock::start();
    // Bad times in the second message: negative, missing.
    reply_to(&mock, "first ", |ctx| {
        vec![audio_msg(ctx, &pcm(200), Some(&align("first ", 20)), None)]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "first")).await.unwrap();
    let bad = json!({"chars": ["s", "e", "c", " ", "t", "h"], "charStartTimesMs": [-50, 0, 0, 0]});
    mock.configure(|c| {
        c.responder = Some(Arc::new(move |request: &Value| {
            let ctx = context_of(request);
            if request["text"] == "first " && request["flush"] == true {
                Some(vec![audio_msg(
                    &ctx,
                    &pcm(200),
                    Some(&align("first ", 20)),
                    None,
                )])
            } else if request["text"] == "sec th " && request["flush"] == true {
                Some(vec![audio_msg(&ctx, &pcm(200), Some(&bad), None)])
            } else {
                None
            }
        }));
    });
    session.input.send(text(1, "sec th")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    let starts = word_ms(&spoken.words);
    assert_eq!(word_texts(&spoken.words), ["first", "sec", "th"]);
    assert!(starts.windows(2).all(|w| w[0] <= w[1]), "{starts:?}");
    assert!(
        starts[1] >= 200,
        "continues after the first piece: {starts:?}"
    );
}

#[tokio::test]
async fn an_alignment_in_its_own_message_is_taken_relative_to_the_audio_so_far() {
    let mock = Mock::start();
    mock.configure(|c| c.align = AlignMode::Separate);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["alpha", "beta"]);
    // UNVERIFIED upstream: the adapter anchors an alignment-only message at the end of the audio
    // delivered so far (200 ms per word here), as documented. The audio message came first, so
    // each word starts where its own audio ends.
    assert_eq!(word_ms(&spoken.words), [200, 400]);
}

#[tokio::test]
async fn an_alignment_ahead_of_its_audio_is_anchored_at_the_audio_delivered_before_it() {
    let mock = Mock::start();
    mock.configure(|c| c.align = AlignMode::AlignmentFirst);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["alpha", "beta"]);
    assert_eq!(word_ms(&spoken.words), [0, 200]);
}

// ---------------------------------------------------------------------------------------------
// Owed output, stall detection, keepalive
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pause_between_text_pieces_neither_times_out_nor_loses_the_context() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(400);
    cfg.keepalive_interval = ms(120);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "first piece.")).await.unwrap();
    // Everything of the first piece arrives; the LLM then pauses for 2.5 x the deadline.
    for _ in 0..4 {
        assert!(matches!(
            next(&mut session.events, 1000).await,
            Some(TtsEvent::Audio { .. } | TtsEvent::Word { .. })
        ));
    }
    expect_silence(&mut session.events, 1000).await;
    session.input.send(text(1, " second piece.")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["second", "piece."]);

    // Keepalives kept the context alive during the pause: its id, empty text, about every 120 ms.
    let frames = mock.received();
    let sid = context_of(&frames[0]);
    let keepalives = frames
        .iter()
        .filter(|f| *f == &json!({"context_id": sid, "text": ""}))
        .count();
    assert!((5..=9).contains(&keepalives), "{keepalives} keepalives");
    // None after the context was closed.
    let close = frames
        .iter()
        .position(|f| f["close_context"] == true)
        .unwrap();
    assert!(
        frames[close..].iter().all(|f| f["text"] != ""),
        "keepalive after close_context"
    );
}

/// A responder that answers `"Hello "` completely and every other text frame with nothing.
fn answer_only_hello(mock: &Mock) {
    mock.configure(|c| {
        c.responder = Some(Arc::new(|request: &Value| {
            if request["text"] == "Hello " && request["flush"] == true {
                Some(vec![audio_msg(
                    &context_of(request),
                    &pcm(100),
                    Some(&align("Hello ", 20)),
                    None,
                )])
            } else if request["flush"] == true && request["text"] != "" {
                Some(vec![])
            } else {
                None
            }
        }));
    });
}

#[tokio::test]
async fn a_server_that_goes_quiet_with_text_outstanding_times_out() {
    let mock = Mock::start();
    answer_only_hello(&mock);
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(400);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "Hello")).await.unwrap();
    // The first piece is answered; the second never is.
    for _ in 0..2 {
        assert!(matches!(
            next(&mut session.events, 1000).await,
            Some(TtsEvent::Audio { .. } | TtsEvent::Word { .. })
        ));
    }
    session.input.send(text(1, "world")).await.unwrap();
    let started = tokio::time::Instant::now();
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. })) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(started.elapsed() >= ms(350), "{:?}", started.elapsed());
    assert!(
        next(&mut session.events, 1000).await.is_none(),
        "the stream ends"
    );
}

#[tokio::test]
async fn an_alignment_that_omits_characters_degrades_instead_of_failing_the_session() {
    let mock = Mock::start();
    // A healthy server whose alignment covers 8 of the 10 characters sent (it left out the
    // "ld"), and no isFinal.
    reply_to(&mock, "Hello world ", |ctx| {
        vec![audio_msg(
            ctx,
            &pcm(100),
            Some(&align("Hello wor", 10)),
            None,
        )]
    });
    mock.configure(|c| c.final_mode = FinalMode::Never);
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(400);
    cfg.final_grace = ms(200);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "Hello world")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // No Timeout error: after one quiet deadline the shortfall is tolerated, and the grace
    // ends the context. The word the alignment left open is delivered with it.
    let started = tokio::time::Instant::now();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["Hello", "wor"]);
    assert_eq!(spoken.audio.len(), 3_200);
    assert!(started.elapsed() >= ms(350), "{:?}", started.elapsed());
    // The session is still usable.
    session.input.send(text(2, "again")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
}

#[tokio::test]
async fn coverage_by_alignment_can_be_switched_off() {
    let mock = Mock::start();
    reply_to(&mock, "Hello world ", |ctx| {
        vec![audio_msg(ctx, &pcm(100), Some(&align("Hel", 20)), None)]
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(400);
    cfg.completion_by_alignment = false;
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "Hello world")).await.unwrap();
    // The first response completes the text, so the silence that follows is not a stall.
    let words = only_audio_and_words(&mut session.events, 900).await;
    // The word the alignment left open is delivered because nothing more is owed.
    assert_eq!(words, ["Hel"]);
    session.input.send(flush(1)).await.unwrap();
    // isFinal ends it.
    until_flushed(&mut session.events, 1).await;
}

/// Reads events for `millis`, failing on anything but audio and words; returns the words.
async fn only_audio_and_words(events: &mut mpsc::Receiver<TtsEvent>, millis: u64) -> Vec<String> {
    let end = tokio::time::Instant::now() + ms(millis);
    let mut words = Vec::new();
    while let Ok(event) = tokio::time::timeout_at(end, events.recv()).await {
        match event {
            Some(TtsEvent::Audio { .. }) => {}
            Some(TtsEvent::Word { word, .. }) => words.push(word.to_string()),
            other => panic!("unexpected {other:?}"),
        }
    }
    words
}

// ---------------------------------------------------------------------------------------------
// Cancel (barge-in)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_is_acknowledged_at_once_and_closes_the_context_without_a_flush() {
    for mode in [CancelMode::KeepSending, CancelMode::Honor] {
        let mock = Mock::start();
        mock.configure(|c| {
            c.interval = ms(20);
            c.cancel = mode;
        });
        let mut session = open(&mock).await;
        session
            .input
            .send(text(
                5,
                "one two three four five six seven eight nine ten eleven twelve",
            ))
            .await
            .unwrap();
        assert!(matches!(
            next(&mut session.events, 2000).await,
            Some(TtsEvent::Audio { .. })
        ));
        session.cancel.send(ContextId(5)).await.unwrap();
        // Skip what was already on its way, up to the acknowledgement.
        let mut stale = 0;
        loop {
            match next(&mut session.events, 500).await.unwrap() {
                TtsEvent::Cancelled { context } => {
                    assert_eq!(context, ContextId(5));
                    break;
                }
                TtsEvent::Audio { .. } | TtsEvent::Word { .. } => stale += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(stale <= 8, "{mode:?}: {stale} events before the ack");
        // The late messages of the cancelled context are dropped, even though the mock goes on
        // sending them.
        expect_silence(&mut session.events, 400).await;
        // Text and flushes for the cancelled id are ignored; the next context works.
        session.input.send(text(5, "ignored")).await.unwrap();
        session.input.send(flush(5)).await.unwrap();
        session.input.send(text(6, "next one")).await.unwrap();
        session.input.send(flush(6)).await.unwrap();
        let spoken = until_flushed(&mut session.events, 6).await;
        assert_eq!(word_texts(&spoken.words), ["next", "one"]);

        let frames = mock.received();
        let sid = context_of(&frames[0]);
        let cancel_at = frames
            .iter()
            .position(|f| f == &json!({"context_id": sid, "close_context": true}))
            .unwrap_or_else(|| panic!("{mode:?}: no close_context: {frames:#?}"));
        assert!(
            frames[..cancel_at]
                .iter()
                .all(|f| f["flush"] != true || f.get("text").is_some()),
            "{mode:?}: the cancel must not be preceded by a flush frame"
        );
        assert!(
            frames.iter().all(|f| f["text"] != "ignored "),
            "{mode:?}: text after the cancel was sent"
        );
        assert_eq!(
            frames.iter().filter(|f| f["close_context"] == true).count(),
            2,
            "{mode:?}: one close for the cancel, one for the flushed next context"
        );
    }
}

#[tokio::test]
async fn a_cancel_discards_what_the_slow_consumer_has_not_received() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = Duration::ZERO);
    let mut session = open(&mock).await;
    let long = "word ".repeat(400);
    session.input.send(text(1, &long)).await.unwrap();
    // Do not read: the channel and the internal queue fill up and the socket is no longer read.
    tokio::time::sleep(ms(400)).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    let mut stale = 0;
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Cancelled { .. } => break,
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => stale += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    // Only what was already in the channel (64) was delivered; the queue behind it was purged.
    assert!(stale <= 64, "{stale} stale events were delivered");
    expect_silence(&mut session.events, 300).await;
}

// ---------------------------------------------------------------------------------------------
// Errors and connection loss
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_error_message_without_a_context_ends_the_session_with_a_mapped_error() {
    let mock = Mock::start();
    reply_to(&mock, "bad ", |_| {
        vec![
            json!({"detail": {"type": "authentication_error", "code": "invalid_api_key", "message": "no"}})
                .to_string(),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "bad")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Rejected { status: 401, .. }))
    ));
    assert!(next(&mut session.events, 2000).await.is_none());
}

#[tokio::test]
async fn an_error_for_a_live_context_maps_by_code_and_text_stays_out_of_rejections() {
    let mock = Mock::start();
    reply_to(&mock, "busy ", |ctx| {
        vec![
            json!({"error": "too many", "code": "concurrent_limit_exceeded", "contextId": ctx})
                .to_string(),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "busy")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Rejected { status: 429, .. }))
    ));

    reply_to(&mock, "weird ", |ctx| {
        vec![json!({"error": "The voice does not exist", "code": "voice_not_found", "contextId": ctx}).to_string()]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "weird")).await.unwrap();
    let Some(TtsEvent::Error(Error::Protocol { detail, .. })) =
        next(&mut session.events, 2000).await
    else {
        panic!("expected Protocol");
    };
    assert!(detail.contains("voice_not_found") && detail.contains("does not exist"));
}

#[tokio::test]
async fn messages_for_unknown_contexts_are_dropped_even_when_malformed_or_errors() {
    let mock = Mock::start();
    reply_to(&mock, "Hello ", |ctx| {
        vec![
            json!({"error": "ghost", "contextId": "someone-else"}).to_string(),
            json!({"audio": 5, "contextId": "someone-else"}).to_string(),
            audio_msg("bv-cancelled", &pcm(100), Some(&align("zz ", 10)), None),
            audio_msg(ctx, &pcm(200), Some(&align("Hello ", 10)), None),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["Hello"]);
    assert_eq!(spoken.audio.len(), 6400);
}

#[tokio::test]
async fn a_message_that_is_not_json_or_has_bad_audio_is_a_protocol_error() {
    let mock = Mock::start();
    reply_to(&mock, "bad ", |ctx| {
        vec![json!({"audio": "!!!!", "contextId": ctx}).to_string()]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "bad")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Protocol { .. }))
    ));
    assert!(next(&mut session.events, 2000).await.is_none());
}

#[tokio::test]
async fn an_abnormal_close_while_output_is_owed_is_a_rejection_a_normal_one_a_disconnect() {
    for (code, expect_rejected) in [
        (Some(1008_u16), true),
        (Some(1011), true),
        (None, false),
        (Some(1000), false),
    ] {
        let mock = Mock::start();
        // The server accepts the text and says nothing: output is owed.
        reply_to(&mock, "Hello ", |_| Vec::new());
        let mut session = open(&mock).await;
        session.input.send(text(1, "Hello")).await.unwrap();
        tokio::time::sleep(ms(150)).await;
        mock.kick(code);
        let event = next(&mut session.events, 2000).await.unwrap();
        match (event, expect_rejected) {
            (TtsEvent::Error(Error::Rejected { status, .. }), true) => {
                assert_eq!(Some(status), code);
            }
            (TtsEvent::Error(Error::Disconnected { .. }), false) => {}
            (other, _) => panic!("close {code:?}: unexpected {other:?}"),
        }
        assert!(next(&mut session.events, 2000).await.is_none());
    }
}

#[tokio::test]
async fn an_idle_socket_the_server_closed_is_replaced_by_the_next_text() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "one two")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    assert_eq!(mock.connections(), 1);
    mock.kick(None);
    tokio::time::sleep(ms(150)).await;
    // No error: nothing was owed. The next utterance reconnects.
    expect_silence(&mut session.events, 100).await;
    session.input.send(text(2, "three four")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 2).await;
    assert_eq!(word_texts(&spoken.words), ["three", "four"]);
    assert_eq!(mock.connections(), 2);
}

#[tokio::test]
async fn a_context_the_server_closed_while_nothing_was_owed_continues_in_a_new_server_context() {
    let mock = Mock::start();
    // The first text is answered completely, followed by an unsolicited isFinal (the server's
    // inactivity timeout, say).
    reply_to(&mock, "Hello there ", |ctx| {
        vec![
            audio_msg(ctx, &pcm(200), Some(&align("Hello there ", 10)), None),
            final_msg(ctx, false),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello there")).await.unwrap();
    for _ in 0..3 {
        assert!(matches!(
            next(&mut session.events, 1000).await,
            Some(TtsEvent::Audio { .. } | TtsEvent::Word { .. })
        ));
    }
    // Let the unsolicited isFinal that follows the answer arrive before the next text.
    tokio::time::sleep(ms(150)).await;
    session.input.send(text(1, "Again")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["Again"]);
    // The word offset continues after the 200 ms already delivered.
    assert_eq!(word_ms(&spoken.words), [200]);
    let inits: Vec<String> = mock
        .received()
        .iter()
        .filter(|f| f["text"] == " ")
        .map(context_of)
        .collect();
    assert_eq!(inits.len(), 2);
    assert_ne!(inits[0], inits[1]);
    // The server context that ended with the unsolicited isFinal is closed on the wire, once,
    // so that it cannot count against the server's limit; the second one by the flush.
    for init in &inits {
        let closes = mock
            .received()
            .iter()
            .filter(|f| f["close_context"] == true && context_of(f) == *init)
            .count();
        assert_eq!(closes, 1, "close_context frames for {init}");
    }
}

#[tokio::test]
async fn an_isfinal_that_arrives_before_the_output_of_outstanding_text_is_not_fatal() {
    let mock = Mock::start();
    // The server may send isFinal per flushed piece: here it comes ahead of the audio.
    reply_to(&mock, "Hello ", |ctx| {
        vec![
            final_msg(ctx, false),
            audio_msg(ctx, &pcm(100), Some(&align("Hello ", 10)), None),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello")).await.unwrap();
    // The answer follows the isFinal; the session lives on and the context can still be
    // flushed after it.
    let mut spoken = Vec::new();
    for _ in 0..2 {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Audio { audio, .. } => spoken.extend_from_slice(&audio),
            TtsEvent::Word { word, .. } => assert_eq!(&*word, "Hello"),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(spoken.len(), 3_200);
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
}

#[tokio::test]
async fn a_context_the_server_closed_with_text_outstanding_ends_in_a_timeout_not_a_protocol_error()
{
    let mock = Mock::start();
    reply_to(&mock, "Oops ", |ctx| vec![final_msg(ctx, false)]);
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(300);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "Oops")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Core(
            brisvox_core::Error::Timeout { .. }
        )))
    ));
}

#[tokio::test]
async fn odd_sized_pcm_chunks_are_realigned_to_whole_samples() {
    let mock = Mock::start();
    reply_to(&mock, "ab ", |ctx| {
        vec![
            audio_msg(ctx, &[1, 2, 3], Some(&align("a", 10)), None),
            audio_msg(ctx, &[4, 5, 6], Some(&align("b ", 10)), None),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "ab")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let mut sizes = Vec::new();
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Audio { audio, .. } => sizes.push(audio.len()),
            TtsEvent::Flushed { .. } => break,
            _ => {}
        }
    }
    assert!(sizes.iter().all(|s| s % 2 == 0), "{sizes:?}");
    assert_eq!(sizes.iter().sum::<usize>(), 6);
}

// ---------------------------------------------------------------------------------------------
// Session end, limits, shutdown
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn dropping_the_input_closes_the_contexts_delivers_the_rest_and_ends() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(10));
    let session = open(&mock).await;
    let TtsSession {
        input,
        cancel: _cancel,
        mut events,
    } = session;
    input.send(text(1, "one two three")).await.unwrap();
    drop(input);
    let mut words = 0;
    while let Some(event) = next(&mut events, 2000).await {
        match event {
            TtsEvent::Word { .. } => words += 1,
            TtsEvent::Audio { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(words, 3, "what was said before the end is still delivered");
    assert!(mock.received().iter().any(|f| f["close_context"] == true));
}

#[tokio::test]
async fn the_oldest_context_is_evicted_when_four_are_open_and_its_server_context_closed() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    for context in 1..=5 {
        session.input.send(text(context, "a")).await.unwrap();
    }
    // The five single-word answers (the evicted context's may be dropped).
    let words = only_audio_and_words(&mut session.events, 600).await;
    assert!(words.len() >= 4, "{words:?}");
    let frames_after = mock.received();
    let first = context_of(&frames_after[0]);
    assert!(
        frames_after
            .iter()
            .any(|f| f == &json!({"context_id": first, "close_context": true})),
        "{frames_after:#?}"
    );
    // The evicted context answers a flush at once (nothing is known about it).
    session.input.send(flush(1)).await.unwrap();
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Flushed { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_context_without_an_open_server_context_is_evicted_before_a_live_one() {
    let mock = Mock::start();
    // Context 2's server context ends by an unsolicited isFinal once everything is spoken.
    reply_to(&mock, "dead ", |ctx| {
        vec![
            audio_msg(ctx, &pcm(100), Some(&align("dead ", 10)), None),
            final_msg(ctx, false),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "one")).await.unwrap();
    session.input.send(text(2, "dead")).await.unwrap();
    let mut words = only_audio_and_words(&mut session.events, 400).await;
    session.input.send(text(3, "three")).await.unwrap();
    session.input.send(text(4, "four")).await.unwrap();
    words.extend(only_audio_and_words(&mut session.events, 300).await);
    // The fifth context finds the table full.
    session.input.send(text(5, "five")).await.unwrap();
    words.extend(only_audio_and_words(&mut session.events, 400).await);
    words.sort();
    assert_eq!(words, ["dead", "five", "four", "one", "three"]);

    let frames = mock.received();
    let inits: Vec<String> = frames
        .iter()
        .filter(|f| f["text"] == " ")
        .map(context_of)
        .collect();
    let closed: Vec<String> = frames
        .iter()
        .filter(|f| f["close_context"] == true)
        .map(context_of)
        .collect();
    assert_eq!(
        closed,
        [inits[1].clone()],
        "only the context the server ended is closed (once); the live ones were not evicted: \
         {frames:#?}"
    );
    // The live ones still finish normally.
    for context in [1, 3, 4, 5] {
        session.input.send(flush(context)).await.unwrap();
    }
    let mut flushed = Vec::new();
    while flushed.len() < 4 {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Flushed { context } => flushed.push(context.0),
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    flushed.sort_unstable();
    assert_eq!(flushed, [1, 3, 4, 5]);
}

#[tokio::test]
async fn when_every_context_awaits_its_final_the_oldest_ends_at_once_with_its_flushed() {
    let mock = Mock::start();
    mock.configure(|c| c.final_mode = FinalMode::Delayed(ms(500)));
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(5000);
    cfg.final_grace = ms(4000);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    for context in 1..=4 {
        session.input.send(text(context, "a")).await.unwrap();
        session.input.send(flush(context)).await.unwrap();
    }
    // All four are flushed and wait for their (delayed) isFinal; a fifth needs a slot.
    session.input.send(text(5, "b")).await.unwrap();
    let sent = tokio::time::Instant::now();
    let first = loop {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Flushed { context } => break context,
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(first, ContextId(1), "the oldest is the one ended early");
    assert!(
        sent.elapsed() < ms(350),
        "Flushed({first:?}) came {:?} after the fifth context, not at once",
        sent.elapsed()
    );
    // The others are flushed when their isFinal arrives, one after the other.
    let mut rest = Vec::new();
    while rest.len() < 3 {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Flushed { context } => rest.push(context.0),
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(rest, [2, 3, 4]);
}

#[tokio::test]
async fn shutdown_ends_sessions_and_stops_warming() {
    let mock = Mock::start();
    let cfg = config(&mock);
    let shutdown = cfg.shutdown.clone();
    let provider = ElevenLabsTts::new(cfg).unwrap();
    let mut session = provider.connect(tts_config()).await.unwrap();
    session
        .input
        .send(text(1, "alpha beta gamma delta"))
        .await
        .unwrap();
    shutdown.cancel();
    let end = tokio::time::timeout(ms(2000), async {
        while session.events.recv().await.is_some() {}
    })
    .await;
    assert!(end.is_ok(), "the session did not end after shutdown");
    provider.warm().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Review fixes: completion, eviction, cancel and shutdown under I/O stalls
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn normalized_text_that_expands_neither_ends_the_wait_nor_splits_a_word() {
    let mock = Mock::start();
    // "15 apples" (8 characters) is answered in two messages. The first covers 5 plain
    // characters ("15 app") but 10 normalized ones ("fifteen app"): 15 became "fifteen".
    mock.configure(|c| {
        c.interval = ms(120);
        c.final_mode = FinalMode::Never;
    });
    reply_to(&mock, "15 apples ", |ctx| {
        vec![
            audio_msg(
                ctx,
                &pcm(100),
                Some(&align("15 app", 10)),
                Some(&align("fifteen app", 10)),
            ),
            audio_msg(
                ctx,
                &pcm(100),
                Some(&align("les ", 10)),
                Some(&align("les ", 10)),
            ),
        ]
    });
    let mut cfg = config(&mock);
    cfg.final_grace = ms(60);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "15 apples")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // The 120 ms between the two messages is longer than the grace: Flushed must still wait
    // for the second message, and "apples" must stay one word.
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(word_texts(&spoken.words), ["15", "apples"]);
    assert_eq!(spoken.audio.len(), 6_400, "both messages were delivered");
}

#[tokio::test]
async fn without_any_alignment_the_grace_is_longer_than_between_audio_chunks() {
    let mock = Mock::start();
    // Three chunks 250 ms apart, no alignment, no isFinal. The grace is 100 ms, but with
    // nothing aligned it is stretched, so the gap between chunks does not end the context.
    mock.configure(|c| {
        c.interval = ms(250);
        c.align = AlignMode::Absent;
        c.final_mode = FinalMode::Never;
    });
    let mut cfg = config(&mock);
    cfg.final_grace = ms(100);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(
        spoken.audio.len(),
        3 * 6_400,
        "all three words were delivered"
    );
}

#[tokio::test]
async fn a_flushed_context_is_not_evicted_before_its_flushed_is_delivered() {
    let mock = Mock::start();
    mock.configure(|c| c.final_mode = FinalMode::Delayed(ms(600)));
    let mut session = open(&mock).await;
    session.input.send(text(1, "a")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // Four more contexts while context 1 still waits for its isFinal: one must be evicted, and
    // it must not be the flushed one.
    for context in 2..=5 {
        session.input.send(text(context, "b")).await.unwrap();
    }
    let started = tokio::time::Instant::now();
    loop {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Flushed { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(started.elapsed() >= ms(500), "Flushed before isFinal");
}

#[tokio::test]
async fn cancel_is_acknowledged_at_once_while_the_socket_is_still_being_opened() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(3000);
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one two")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // The server closes the idle socket; the reconnect for the next text then stalls.
    mock.kick(None);
    tokio::time::sleep(ms(150)).await;
    mock.inject(Fault::ConnectStall { duration: ms(2000) });
    session.input.send(text(2, "three four")).await.unwrap();
    tokio::time::sleep(ms(150)).await;
    let started = tokio::time::Instant::now();
    session.cancel.send(ContextId(2)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(2)
    ));
    assert!(
        started.elapsed() < ms(500),
        "the acknowledgement waited for the connect: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn cancel_of_a_flushed_context_waiting_for_its_final_message_sends_no_second_close() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.final_mode = FinalMode::Delayed(ms(600));
        c.interval = ms(20);
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "one two three")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. } | TtsEvent::Word { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Cancelled { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    // The isFinal that arrives later neither produces a Flushed nor anything else.
    expect_silence(&mut session.events, 900).await;
    let closes = mock
        .received()
        .iter()
        .filter(|f| f["close_context"] == true)
        .count();
    assert_eq!(closes, 1, "the flush already closed the context");
}

#[tokio::test]
async fn shutdown_ends_a_session_that_is_waiting_for_a_connect() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(5000);
    let shutdown = cfg.shutdown.clone();
    let mut session = ElevenLabsTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    mock.kick(None);
    tokio::time::sleep(ms(150)).await;
    mock.inject(Fault::ConnectStall { duration: ms(4000) });
    session.input.send(text(2, "two")).await.unwrap();
    tokio::time::sleep(ms(150)).await;
    shutdown.cancel();
    let end = tokio::time::timeout(ms(1000), async {
        while session.events.recv().await.is_some() {}
    })
    .await;
    assert!(end.is_ok(), "shutdown waited for the stalled connect");
}

// ---------------------------------------------------------------------------------------------
// Warm-up
// ---------------------------------------------------------------------------------------------

fn warm_provider(mock: &Mock, change: impl FnOnce(&mut ElevenLabsConfig)) -> ElevenLabsTts {
    let mut cfg = config(mock);
    cfg.warm_target = Some((Arc::from("voice-1"), SampleRate::Hz16000));
    change(&mut cfg);
    ElevenLabsTts::new(cfg).unwrap()
}

#[tokio::test]
async fn warm_parks_one_socket_for_the_target_voice_and_connect_takes_it() {
    let mock = Mock::start();
    let provider = warm_provider(&mock, |_| {});
    provider.warm().await.unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1, "warm is idempotent");
    let mut session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 1, "connect reused the parked socket");
    session.input.send(text(1, "ok")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // The next connect has nothing parked.
    let _second = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 2);
}

#[tokio::test]
async fn a_socket_parked_for_another_voice_or_rate_stays_parked() {
    let mock = Mock::start();
    let provider = warm_provider(&mock, |_| {});
    provider.warm().await.unwrap();
    let other = TtsConfig {
        voice: Arc::from("voice-2"),
        ..tts_config()
    };
    let _a = provider.connect(other).await.unwrap();
    assert_eq!(mock.connections(), 2);
    let rate = TtsConfig {
        sample_rate: SampleRate::Hz24000,
        ..tts_config()
    };
    let _b = provider.connect(rate).await.unwrap();
    assert_eq!(mock.connections(), 3);
    let _c = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 3, "the parked socket was still there");
}

#[tokio::test]
async fn a_stale_or_closed_parked_socket_is_replaced() {
    let mock = Mock::start();
    let provider = warm_provider(&mock, |c| c.warm_max_age = ms(60));
    provider.warm().await.unwrap();
    tokio::time::sleep(ms(150)).await;
    let _a = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 2, "stale: a new connection");

    let provider = warm_provider(&mock, |_| {});
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 3);
    mock.kick(None);
    tokio::time::sleep(ms(150)).await;
    let _b = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 4, "closed: a new connection");
}

#[tokio::test]
async fn warm_without_a_target_does_nothing() {
    let mock = Mock::start();
    let provider = provider(&mock);
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn shutdown_stops_warming_and_the_parked_socket_is_not_handed_out() {
    // warm() after shutdown opens nothing.
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.warm_target = Some((Arc::from("voice-1"), SampleRate::Hz16000));
    let shutdown = cfg.shutdown.clone();
    let provider = ElevenLabsTts::new(cfg).unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1);
    shutdown.cancel();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1, "warm opened a socket after shutdown");

    // The socket parked before shutdown is not handed to a connect after it.
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.warm_target = Some((Arc::from("voice-1"), SampleRate::Hz16000));
    let shutdown = cfg.shutdown.clone();
    let provider = ElevenLabsTts::new(cfg).unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1);
    shutdown.cancel();
    let _session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 2, "connect reused a parked socket");

    // Shut down before the first warm: nothing is ever opened.
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.warm_target = Some((Arc::from("voice-1"), SampleRate::Hz16000));
    cfg.shutdown.cancel();
    let provider = ElevenLabsTts::new(cfg).unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 0);
}

// ---------------------------------------------------------------------------------------------
// Hostile server messages: size caps
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_alignment_with_an_absurd_number_of_characters_is_a_protocol_error() {
    let mock = Mock::start();
    reply_to(&mock, "Hello ", |ctx| {
        // Within the size cap of a message, far above anything a real chunk carries.
        let chars: Vec<&str> = (0..20_000)
            .map(|i| if i % 2 == 0 { "a" } else { " " })
            .collect();
        let starts: Vec<u64> = (0..20_000).collect();
        vec![
            json!({
                "audio": null,
                "alignment": {"chars": chars, "charStartTimesMs": starts},
                "contextId": ctx
            })
            .to_string(),
        ]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello")).await.unwrap();
    // No Word event is queued from it: the first (and only) event is the error.
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Protocol { .. }))
    ));
    assert!(next(&mut session.events, 2000).await.is_none());
}

#[tokio::test]
async fn a_server_message_over_the_size_cap_ends_the_session() {
    let mock = Mock::start();
    reply_to(&mock, "Hello ", |ctx| {
        // 1.5 MiB of base64 text.
        vec![json!({"audio": "A".repeat(1536 * 1024), "contextId": ctx}).to_string()]
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello")).await.unwrap();
    let event = next(&mut session.events, 3000).await;
    assert!(
        matches!(&event, Some(TtsEvent::Error(Error::Protocol { detail, .. })) if detail.contains("websocket")),
        "{event:?}"
    );
    assert!(next(&mut session.events, 2000).await.is_none());
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1, ELEVENLABS_API_KEY, ELEVENLABS_VOICE_ID)
// ---------------------------------------------------------------------------------------------

/// A short real session: two chunks and a flush with alignment, a barge-in and a second
/// utterance. Prints measurements for the open items of docs/providers/elevenlabs.md; never
/// prints the key.
#[tokio::test]
async fn live_elevenlabs_session() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set BRISVOX_LIVE_TESTS=1 (and ELEVENLABS_API_KEY, ELEVENLABS_VOICE_ID)"
        );
        return;
    }
    let (Ok(key), Ok(voice)) = (
        std::env::var("ELEVENLABS_API_KEY"),
        std::env::var("ELEVENLABS_VOICE_ID"),
    ) else {
        eprintln!("skipped: ELEVENLABS_API_KEY and ELEVENLABS_VOICE_ID must both be set");
        return;
    };
    let mut cfg = ElevenLabsConfig::new(Secret::new(key), clock());
    if let Ok(model) = std::env::var("ELEVENLABS_MODEL") {
        cfg.model_id = model;
    }
    if let Ok(language) = std::env::var("ELEVENLABS_LANGUAGE") {
        cfg.language_code = Some(language);
    }
    // ELEVENLABS_SYNC_ALIGNMENT=1 runs the session with `sync_alignment=true`.
    cfg.sync_alignment = std::env::var("ELEVENLABS_SYNC_ALIGNMENT").as_deref() == Ok("1");
    cfg.warm_target = Some((Arc::from(voice.as_str()), SampleRate::Hz24000));
    let provider = ElevenLabsTts::new(cfg).unwrap();
    let config = TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from(voice.as_str()),
        language: None,
        model: None,
    };

    let warm_started = tokio::time::Instant::now();
    provider.warm().await.unwrap();
    let warm_took = warm_started.elapsed();
    let mut session = provider.connect(config).await.unwrap();

    // Utterance 1: two chunks (the second starts with a space), then the close.
    let started = tokio::time::Instant::now();
    session
        .input
        .send(text(1, "Hello there, this is a short test."))
        .await
        .unwrap();
    session
        .input
        .send(text(1, " It has two chunks."))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let mut first_audio = None;
    let mut audio_bytes = 0_usize;
    let mut words = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), session.events.recv())
            .await
            .expect("live: no event within 20 s")
            .expect("live: stream ended");
        match event {
            TtsEvent::Audio { audio, .. } => {
                first_audio.get_or_insert_with(|| started.elapsed());
                audio_bytes += audio.len();
            }
            TtsEvent::Word { word, start, .. } => words.push((word, start)),
            TtsEvent::Flushed { .. } => break,
            other => panic!("live: unexpected {other:?}"),
        }
    }
    let flushed_after = started.elapsed();
    let audio_secs = audio_bytes as f64 / (24_000.0 * 2.0);
    eprintln!(
        "live: warm {warm_took:?}, first audio {first_audio:?}, flushed {flushed_after:?}, \
         {audio_secs:.2} s of audio, {} words",
        words.len()
    );
    eprintln!(
        "live: first word at {:?}, last word at {:?}",
        words.first().map(|w| w.1),
        words.last().map(|w| w.1)
    );
    assert!(
        audio_bytes > 0 && audio_bytes.is_multiple_of(2),
        "PCM16 samples"
    );
    assert!(!words.is_empty(), "alignment was expected");
    assert!(words.windows(2).all(|w| w[0].1 <= w[1].1));
    if let Some((_, last)) = words.last() {
        assert!(
            last.as_secs_f64() <= audio_secs + 0.5,
            "words run past the audio"
        );
    }

    // Utterance 2: barge in after the first audio.
    session
        .input
        .send(text(
            2,
            "This sentence is long enough that we can interrupt it while it is still being generated and played.",
        ))
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), session.events.recv())
            .await
            .expect("live: no audio for utterance 2")
            .expect("live: stream ended");
        if matches!(event, TtsEvent::Audio { .. }) {
            break;
        }
    }
    let cancelled_at = tokio::time::Instant::now();
    session.cancel.send(ContextId(2)).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), session.events.recv())
            .await
            .expect("live: no Cancelled")
            .expect("live: stream ended");
        if matches!(event, TtsEvent::Cancelled { .. }) {
            break;
        }
    }
    eprintln!(
        "live: cancel acknowledged after {:?}",
        cancelled_at.elapsed()
    );
    expect_silence(&mut session.events, 1500).await;

    // Utterance 3 works after the barge-in.
    session
        .input
        .send(text(3, "And we are back."))
        .await
        .unwrap();
    session.input.send(flush(3)).await.unwrap();
    until_flushed(&mut session.events, 3).await;
}
