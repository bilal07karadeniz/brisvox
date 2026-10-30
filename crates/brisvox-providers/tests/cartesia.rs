//! Cartesia adapter tests: the provider contract suite and protocol-level checks
//! against a local mock of the Cartesia TTS WebSocket, plus a live test gated on
//! `BRISVOX_LIVE_TESTS=1`.
//!
//! The sockets are real, so these tests use the real system clock and short deadlines instead
//! of paused time (the `disallowed_methods` lint guards engine logic, not test pacing).
#![cfg(feature = "provider-cartesia")]
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

#[path = "cartesia/mock.rs"]
mod mock;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::cartesia::{CartesiaConfig, CartesiaTts, OutputEncoding};
use brisvox_providers::{ContextId, Error, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use brisvox_sim::contract::{CONTRACT_TIMEOUT, Fault, TtsHarness, tts_contract};
use mock::{CancelMode, ExpiryMode, Mock, TimeBase, TsOrder};
use serde_json::{Value, json};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn config(mock: &Mock) -> CartesiaConfig {
    let mut config = CartesiaConfig::new(Secret::new(mock.api_key.clone()), clock());
    config.url = mock.url.clone();
    config.language = Some("en".to_owned());
    config.io_timeout = Duration::from_millis(1500);
    config.concurrency_retry_delay = Duration::from_millis(10);
    config
}

fn provider(mock: &Mock) -> CartesiaTts {
    CartesiaTts::new(config(mock)).unwrap()
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

/// The next event within `ms` milliseconds.
async fn next(events: &mut mpsc::Receiver<TtsEvent>, ms: u64) -> Option<TtsEvent> {
    tokio::time::timeout(Duration::from_millis(ms), events.recv())
        .await
        .unwrap_or_else(|_| panic!("no event within {ms} ms"))
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

/// Like [`until_flushed`], with `ms` milliseconds allowed between two events.
async fn until_flushed_within(
    events: &mut mpsc::Receiver<TtsEvent>,
    context: u64,
    ms: u64,
) -> Utterance {
    let mut utterance = Utterance::default();
    loop {
        match next(events, ms).await.expect("stream ended early") {
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

async fn expect_silence(events: &mut mpsc::Receiver<TtsEvent>, ms: u64) {
    if let Ok(event) = tokio::time::timeout(Duration::from_millis(ms), events.recv()).await {
        panic!("unexpected {event:?}");
    }
}

async fn open(mock: &Mock) -> TtsSession {
    provider(mock).connect(tts_config()).await.unwrap()
}

/// Waits (real time) until `condition` holds.
async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The `flush_done` a real server sends for a flush marker request (it always precedes the
/// `done` of the context).
fn flush_done(request: &Value) -> String {
    json!({
        "type": "flush_done", "done": false, "flush_done": true, "flush_id": 1,
        "status_code": 206, "context_id": request["context_id"]
    })
    .to_string()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Offsets in whole milliseconds (the mock computes seconds as floats).
fn rounded_ms(offsets: &[(String, Duration)]) -> Vec<u64> {
    offsets
        .iter()
        .map(|(_, d)| (d.as_secs_f64() * 1000.0).round() as u64)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite
// ---------------------------------------------------------------------------------------------

struct Harness {
    mock: Mock,
    provider: Arc<CartesiaTts>,
    clock: SharedClock,
}

impl Harness {
    fn new() -> Self {
        let mock = Mock::start();
        mock.configure(|c| c.interval = ms(20));
        let mut cfg = config(&mock);
        cfg.io_timeout = CONTRACT_TIMEOUT;
        let provider = Arc::new(CartesiaTts::new(cfg).unwrap());
        Self {
            mock,
            provider,
            clock: clock(),
        }
    }
}

impl TtsHarness for Harness {
    type P = CartesiaTts;

    fn provider(&self) -> Arc<CartesiaTts> {
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
async fn handshake_pins_the_version_and_sends_the_key_only_in_a_header() {
    let mock = Mock::start();
    let session = open(&mock).await;
    let handshakes = mock.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert!(handshakes[0].key_ok);
    assert!(
        handshakes[0]
            .uri
            .ends_with("/tts/websocket?cartesia_version=2026-08-14"),
        "uri {}",
        handshakes[0].uri
    );
    assert!(!handshakes[0].uri.contains(&mock.api_key));
    drop(session);
}

#[tokio::test]
async fn a_wrong_key_is_rejected_with_401() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("wrong".to_owned());
    let provider = CartesiaTts::new(cfg).unwrap();
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
    let err = CartesiaTts::new(cfg).unwrap_err().to_string();
    assert!(!err.contains("bad"), "{err}");
}

#[tokio::test]
async fn invalid_configuration_is_refused_up_front() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.max_buffer_delay_ms = 5001;
    assert!(CartesiaTts::new(cfg).is_err());
    let mut cfg = config(&mock);
    cfg.io_timeout = Duration::ZERO;
    assert!(CartesiaTts::new(cfg).is_err());
    // mu-law needs an 8 kHz session; an empty voice is refused.
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    let provider = CartesiaTts::new(cfg).unwrap();
    assert!(matches!(
        provider.connect(tts_config()).await,
        Err(Error::Protocol { .. })
    ));
    let empty = TtsConfig {
        voice: Arc::from(""),
        ..tts_config()
    };
    assert!(matches!(
        self::provider(&mock).connect(empty).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(mock.connections(), 0, "no connection for a bad config");
}

#[tokio::test]
async fn config_reaches_the_wire_unchanged_on_every_request() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.model_id = "sonic-3.5".to_owned();
    cfg.language = Some("tr".to_owned());
    cfg.max_buffer_delay_ms = 250;
    cfg.use_normalized_timestamps = true;
    cfg.encoding = OutputEncoding::Mulaw;
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz8000,
            voice: Arc::from("tr-voice"),
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
    for frame in mock.received() {
        assert_eq!(frame["model_id"], "sonic-3.5");
        assert_eq!(frame["voice"], "tr-voice");
        assert_eq!(frame["language"], "tr");
        assert_eq!(frame["max_buffer_delay_ms"], 250);
        assert_eq!(frame["add_timestamps"], true);
        assert_eq!(frame["use_normalized_timestamps"], true);
        assert_eq!(
            frame["output_format"],
            json!({"container": "raw", "encoding": "pcm_mulaw", "sample_rate": 8000})
        );
    }
}

#[tokio::test]
async fn the_session_language_voice_and_model_win_over_the_adapter_defaults() {
    // A call that follows the caller into Turkish opens a session with the Turkish voice, the
    // language and (optionally) another model; none of it needs the adapter to be reconfigured.
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.language = Some("en".to_owned());
    let provider = CartesiaTts::new(cfg).unwrap();
    for (language, model, want_language, want_model) in [
        (None, None, "en", "sonic-3.6"),
        (Some("tr"), Some("sonic-3.5"), "tr", "sonic-3.5"),
        (Some("tr"), None, "tr", "sonic-3.6"),
        // Anything that is not a plain code is ignored, never put into the request.
        (Some("t\"r"), Some("a b"), "en", "sonic-3.6"),
    ] {
        let mut session = provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from("v"),
                language: language.map(Arc::from),
                model: model.map(Arc::from),
            })
            .await
            .unwrap();
        session.input.send(text(1, "Merhaba.")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let _ = until_flushed(&mut session.events, 1).await;
        let frames = mock.received();
        let frame = frames.last().unwrap();
        assert_eq!(frame["language"], want_language);
        assert_eq!(frame["model_id"], want_model);
    }
}

// ---------------------------------------------------------------------------------------------
// Requests: contexts, continuation, flush
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn text_is_continued_verbatim_then_closed_with_an_empty_transcript() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(7, "Hello, ")).await.unwrap();
    session
        .input
        .send(text(7, "brave new world."))
        .await
        .unwrap();
    session.input.send(flush(7)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 7).await;
    let words: Vec<&str> = spoken.words.iter().map(|(w, _)| w.as_str()).collect();
    assert_eq!(words, ["Hello,", "brave", "new", "world."]);
    expect_silence(&mut session.events, 100).await;

    let frames = mock.received();
    let shape: Vec<(String, bool, bool)> = frames
        .iter()
        .map(|f| {
            (
                f["transcript"].as_str().unwrap().to_owned(),
                f["continue"].as_bool().unwrap(),
                f.get("flush").is_some_and(|v| v == true),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("Hello, ".to_owned(), true, false),
            (String::new(), true, true), // flush marker
            ("brave new world.".to_owned(), true, false),
            (String::new(), true, true),
            (String::new(), false, false), // the close
        ]
    );
    let context = frames[0]["context_id"].as_str().unwrap();
    assert!(frames.iter().all(|f| f["context_id"] == context));
    // The server context is not the engine's number, and is unique per session.
    assert!(context.starts_with("bv"), "{context}");
}

#[tokio::test]
async fn each_engine_context_gets_its_own_server_context() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    for c in [1, 2] {
        session.input.send(text(c, "one two")).await.unwrap();
        session.input.send(flush(c)).await.unwrap();
        until_flushed(&mut session.events, c).await;
    }
    // A context id used again after Flushed starts a fresh server context, and a new engine
    // context with it: word offsets and audio restart from zero.
    session.input.send(text(1, "again")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let again = until_flushed(&mut session.events, 1).await;
    assert_eq!(rounded_ms(&again.words), [20]);
    assert_eq!(
        again.audio.len(),
        6400,
        "one word of audio, none carried over"
    );
    let mut ids: Vec<String> = mock
        .received()
        .iter()
        .map(|f| f["context_id"].as_str().unwrap().to_owned())
        .collect();
    ids.dedup();
    assert_eq!(ids.len(), 3, "{ids:?}");
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn flush_without_text_reports_flushed_at_once() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(flush(3)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 500).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(3)
    ));
    assert!(mock.received().is_empty(), "nothing needs the server");
}

#[tokio::test]
async fn text_after_the_flush_of_the_same_context_is_not_sent() {
    let mock = Mock::start();
    // The server never answers, so the context stays open between the flush and the late text.
    mock.configure(|c| c.responder = Some(Arc::new(|_| Some(vec![]))));
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(5000);
    let session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "first")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(1, "too late")).await.unwrap();
    eventually("text, marker and close", || mock.received().len() >= 3).await;
    tokio::time::sleep(ms(100)).await;
    let sent: Vec<Value> = mock.received();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent.iter().all(|f| f["transcript"] != "too late"));
}

#[tokio::test]
async fn empty_text_sends_nothing() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 500).await,
        Some(TtsEvent::Flushed { .. })
    ));
    assert!(mock.received().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Audio and word timestamps
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn audio_is_decoded_and_words_arrive_with_offsets_from_the_context_start() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    // 2 words x 0.2 s x 16 kHz x 2 bytes; the mock fills word i with byte i + 1.
    assert_eq!(spoken.audio.len(), 12_800);
    assert!(spoken.audio[..6400].iter().all(|b| *b == 1));
    assert!(spoken.audio[6400..].iter().all(|b| *b == 2));
    assert_eq!(rounded_ms(&spoken.words), [20, 220]);
    assert_eq!(spoken.words[0].0, "alpha");
}

#[tokio::test]
async fn word_offsets_are_the_same_whichever_time_base_the_server_uses() {
    let mut results = Vec::new();
    for base in [TimeBase::Context, TimeBase::Chunk] {
        let mock = Mock::start();
        mock.configure(|c| c.time_base = base);
        let mut session = open(&mock).await;
        session.input.send(text(1, "one two")).await.unwrap();
        session.input.send(text(1, " three")).await.unwrap();
        session.input.send(text(1, " four five")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let spoken = until_flushed(&mut session.events, 1).await;
        let offsets = rounded_ms(&spoken.words);
        assert_eq!(offsets, [20, 220, 420, 620, 820], "{base:?}");
        results.push(offsets);
    }
    assert_eq!(results[0], results[1]);
}

#[tokio::test]
async fn without_flush_markers_times_are_taken_as_cumulative() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_after_text = false;
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one two")).await.unwrap();
    session.input.send(text(1, " three")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(rounded_ms(&spoken.words), [20, 220, 420]);
    // No marker on the wire.
    assert!(
        mock.received().iter().all(|f| f.get("flush").is_none()),
        "{:?}",
        mock.received()
    );
}

#[tokio::test]
async fn odd_sized_chunks_become_whole_samples() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(Arc::new(|request: &Value| {
            let ctx = request["context_id"].as_str().unwrap();
            if request["flush"] == true {
                return Some(vec![flush_done(request)]);
            }
            if request["continue"] == false {
                return Some(vec![mock::done(ctx)]);
            }
            // "AQID" = [1,2,3] (an odd number of bytes for PCM16), "BAUG" = [4,5,6].
            Some(vec![
                json!({"type":"chunk","data":"AQID","context_id":ctx}).to_string(),
                format!(r#"{{"type":"chunk","data":"BAUG","context_id":"{ctx}"}}"#),
            ])
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "x")).await.unwrap();
    // Wait for the chunks, then close the context.
    session.input.send(flush(1)).await.unwrap();
    let mut audio = Vec::new();
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Audio { audio: a, .. } => {
                assert_eq!(a.len() % 2, 0, "events carry whole samples");
                audio.extend_from_slice(&a);
            }
            TtsEvent::Flushed { .. } => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(audio, [1, 2, 3, 4, 5, 6]);
}

#[tokio::test]
async fn unknown_messages_other_contexts_and_phoneme_timestamps_are_ignored() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(Arc::new(|request: &Value| {
            let ctx = request["context_id"].as_str().unwrap();
            if request["flush"] == true {
                return Some(vec![flush_done(request)]);
            }
            if request["continue"] == false {
                return Some(vec![mock::done(ctx)]);
            }
            Some(vec![
                json!({"type":"phoneme_timestamps","context_id":ctx,
                       "phoneme_timestamps":{"phonemes":["h"],"start":[0.1],"end":[0.2]}})
                .to_string(),
                json!({"type":"brand_new_message","context_id":ctx,"x":1}).to_string(),
                mock::chunk("somebody-else", &[9, 9]),
                json!({"type":"ping-like","no_context":true}).to_string(),
                mock::chunk(ctx, &[1, 1]),
            ])
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "x")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(spoken.audio, [1, 1]);
}

#[tokio::test]
async fn a_slow_consumer_loses_nothing() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = Duration::ZERO;
        c.word_secs = 0.02;
    });
    let mut session = open(&mock).await;
    let text_of_words = (0..600)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    session.input.send(text(1, &text_of_words)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // Let the socket run ahead of the consumer: channels fill, reading pauses.
    tokio::time::sleep(ms(400)).await;
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(spoken.words.len(), 600);
    assert_eq!(spoken.audio_events, 600);
    let words: Vec<String> = spoken.words.iter().map(|(w, _)| w.clone()).collect();
    assert_eq!(words[0], "w0");
    assert_eq!(words[599], "w599");
    assert!(spoken.words.windows(2).all(|w| w[0].1 <= w[1].1));
}

#[tokio::test]
async fn the_stall_timer_does_not_run_while_the_consumer_is_behind() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = Duration::ZERO;
        c.word_secs = 0.02;
    });
    let mut cfg = config(&mock);
    // 1 s, not tighter: under a loaded parallel test run the in-process mock can pause for
    // a few hundred ms between messages while the drain below reads, which is a real stall.
    cfg.io_timeout = ms(1_000);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    let text_of_words = (0..600)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    session.input.send(text(1, &text_of_words)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // The adapter stops reading once the consumer is behind; output is owed the whole time.
    eventually("the server to fill the pipe", || mock.frames_sent() >= 300).await;
    // Three times the deadline with nobody reading: not a stall (`until_flushed` fails on an
    // `Error` event).
    tokio::time::sleep(ms(3_000)).await;
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(spoken.words.len(), 600);
    assert_eq!(spoken.audio_events, 600);
}

// ---------------------------------------------------------------------------------------------
// Cancel (barge-in)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_acks_at_once_sends_the_wire_cancel_and_drops_what_the_server_keeps_sending() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(25);
        c.cancel = CancelMode::KeepSending;
    });
    let mut session = open(&mock).await;
    let long = (0..40)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    session.input.send(text(1, &long)).await.unwrap();
    loop {
        if matches!(
            next(&mut session.events, 2000).await.unwrap(),
            TtsEvent::Audio { .. }
        ) {
            break;
        }
    }
    let sent_before = mock.frames_sent();
    session.cancel.send(ContextId(1)).await.unwrap();
    // The ack does not wait for the server (which ignores the cancel and keeps generating).
    // Causality instead of a stopwatch: below, the server demonstrably still had frames to
    // send after the ack, so the ack cannot have waited for it to finish.
    // Only what the adapter had already handed to the channel can precede the ack: the server
    // has about 80 events (25 ms apart) left, so an ack that waited for the server (or a purge
    // that ran after it) would let dozens through.
    let mut stale = 0;
    loop {
        match next(&mut session.events, 5000).await.unwrap() {
            TtsEvent::Cancelled { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => stale += 1,
            other => panic!("{other:?}"),
        }
    }
    assert!(stale <= 4, "{stale} stale events preceded the ack");
    // The cancel message is exactly the documented one, for the server's context id.
    eventually("the cancel on the wire", || {
        mock.received().iter().any(|f| f.get("cancel").is_some())
    })
    .await;
    let frames = mock.received();
    let cancel = frames.iter().find(|f| f.get("cancel").is_some()).unwrap();
    assert_eq!(cancel["cancel"], true);
    assert_eq!(cancel["context_id"], frames[0]["context_id"]);
    assert_eq!(cancel.as_object().unwrap().len(), 2);
    // The server really did keep talking, and nothing of it reached the consumer.
    expect_silence(&mut session.events, 400).await;
    assert!(
        mock.frames_sent() >= sent_before + 3,
        "the mock stopped by itself; the test proves nothing"
    );
    // The session still works.
    session.input.send(text(2, "ok then")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    // The cancelled context's backlog ahead of it on the wire delays this by about a second.
    let spoken = until_flushed(&mut session.events, 2).await;
    assert_eq!(spoken.words.len(), 2);
}

#[tokio::test]
async fn cancel_purges_events_the_adapter_still_holds_for_a_slow_consumer() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = Duration::ZERO;
        c.word_secs = 0.02;
        c.cancel = CancelMode::KeepSending;
    });
    let mut session = open(&mock).await;
    let words = (0..600)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    session.input.send(text(1, &words)).await.unwrap();
    // Nobody reads: the events channel (64) and the adapter's queue fill up and it stops
    // reading the socket.
    eventually("the server to fill the pipe", || mock.frames_sent() >= 300).await;
    tokio::time::sleep(ms(300)).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    let mut stale = 0;
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Cancelled { .. } => break,
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => stale += 1,
            other => panic!("{other:?}"),
        }
    }
    // Only what was already in the channel (64) was delivered; the queue behind it, the rest of
    // the 1200 events, was dropped by the adapter.
    assert!(stale <= 64, "{stale} stale events were delivered");
    expect_silence(&mut session.events, 300).await;
}

#[tokio::test]
async fn input_for_a_cancelled_context_is_ignored_even_if_it_arrives_later() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(1)
    ));
    // Text that was queued before the cancel was seen.
    session.input.send(text(1, "stale words")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(2, "fresh")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 2).await;
    assert_eq!(spoken.words.len(), 1);
    assert!(
        mock.received()
            .iter()
            .all(|f| f["transcript"] != "stale words"),
        "stale text reached the server"
    );
}

#[tokio::test]
async fn cancelling_an_unknown_context_still_acknowledges() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.cancel.send(ContextId(99)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 500).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(99)
    ));
    assert!(
        mock.received().is_empty(),
        "no cancel for something never sent"
    );
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn error_responder(error: Value) -> mock::Responder {
    Arc::new(move |request: &Value| {
        if request["flush"] == true {
            return Some(vec![]);
        }
        let mut error = error.clone();
        error["context_id"] = request["context_id"].clone();
        Some(vec![error.to_string()])
    })
}

async fn first_error(mock: &Mock) -> (Error, bool) {
    let mut session = open(mock).await;
    session.input.send(text(1, "hi")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let error = loop {
        if let TtsEvent::Error(e) = next(&mut session.events, 2000).await.expect("no error") {
            break e;
        }
    };
    let ended = next(&mut session.events, 2000).await.is_none();
    (error, ended)
}

#[tokio::test]
async fn concurrency_limit_maps_to_rejected_429() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(error_responder(json!({
            "type": "error", "done": true, "error_code": "concurrency_limited",
            "status_code": 429, "title": "Too many", "message": "over the limit"
        })));
    });
    let (error, ended) = first_error(&mock).await;
    assert!(
        matches!(error, Error::Rejected { status: 429, .. }),
        "{error}"
    );
    assert!(ended, "the stream ends after an error");
}

#[tokio::test]
async fn auth_and_server_errors_in_messages_map_to_rejected() {
    for (status, code) in [(401_u16, "unauthorized"), (503, "overloaded")] {
        let mock = Mock::start();
        mock.configure(|c| {
            c.responder = Some(error_responder(json!({
                "type": "error", "done": true, "error_code": code,
                "status_code": status, "title": "t", "message": "m"
            })));
        });
        let (error, _) = first_error(&mock).await;
        assert!(
            matches!(error, Error::Rejected { status: s, .. } if s == status),
            "{status}: {error}"
        );
    }
}

#[tokio::test]
async fn request_errors_map_to_protocol_with_the_server_code() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(error_responder(json!({
            "type": "error", "done": true, "error_code": "model_not_found",
            "status_code": 400, "title": "Invalid model",
            "message": "The model is not valid, make sure it is a valid model ID."
        })));
    });
    let (error, ended) = first_error(&mock).await;
    let Error::Protocol { detail, .. } = &error else {
        panic!("{error}");
    };
    assert!(
        detail.contains("model_not_found") && detail.contains("Invalid model"),
        "{detail}"
    );
    assert!(!detail.contains(&mock.api_key));
    assert!(ended);
}

#[tokio::test]
async fn an_error_for_a_cancelled_context_is_not_a_session_failure() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(30);
        c.responder = Some(Arc::new(|request: &Value| {
            if request["cancel"] == true {
                return Some(vec![]);
            }
            if request["flush"] == true {
                return Some(vec![flush_done(request)]);
            }
            let ctx = request["context_id"].as_str().unwrap();
            if request["transcript"] == "doomed" {
                // Audio first, then an error that arrives after the client cancelled.
                return Some(vec![
                    mock::chunk(ctx, &[1, 1]),
                    mock::chunk(ctx, &[1, 1]),
                    mock::chunk(ctx, &[1, 1]),
                    json!({"type":"error","status_code":500,"title":"late","message":"late",
                           "context_id":ctx})
                    .to_string(),
                ]);
            }
            None
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "doomed")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    loop {
        if matches!(
            next(&mut session.events, 1000).await,
            Some(TtsEvent::Cancelled { .. })
        ) {
            break;
        }
    }
    expect_silence(&mut session.events, 300).await;
    session.input.send(text(2, "fine")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
}

// ---------------------------------------------------------------------------------------------
// Time: stalls, idle, reconnect, warm, shutdown
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pause_between_chunks_longer_than_the_deadline_is_not_a_stall() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(300);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "first part")).await.unwrap();
    // The chunk is answered (2 audio, 2 words, then flush_done); the LLM thinks for 3 deadlines.
    for _ in 0..4 {
        next(&mut session.events, 1000).await.unwrap();
    }
    tokio::time::sleep(ms(1000)).await;
    expect_silence(&mut session.events, 10).await;
    session.input.send(text(1, " second part")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(spoken.words.len(), 2);
    assert!(spoken.words[0].1 > ms(400), "{:?}", spoken.words);
}

#[tokio::test]
async fn a_silent_server_while_output_is_owed_is_a_timeout() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(Arc::new(|_| Some(vec![])));
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(300);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "hello")).await.unwrap();
    let started = tokio::time::Instant::now();
    match next(&mut session.events, 2000).await.unwrap() {
        TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. })) => {}
        other => panic!("{other:?}"),
    }
    // Waited for the deadline (a lower bound cannot flake under load).
    assert!(started.elapsed() >= ms(250), "{:?}", started.elapsed());
    assert!(next(&mut session.events, 1000).await.is_none());
}

#[tokio::test]
async fn without_flush_markers_the_first_response_and_the_close_are_still_timed() {
    let mock = Mock::start();
    mock.configure(|c| c.responder = Some(Arc::new(|_| Some(vec![]))));
    let mut cfg = config(&mock);
    cfg.flush_after_text = false;
    cfg.io_timeout = ms(300);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "hello")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Core(
            brisvox_core::Error::Timeout { .. }
        )))
    ));
}

#[tokio::test]
async fn an_idle_close_by_the_server_is_not_a_failure_and_the_next_text_reconnects() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "one")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    assert_eq!(mock.connections(), 1);
    // The server closes the idle socket (its 5-minute rule, forced here).
    mock.kick();
    expect_silence(&mut session.events, 300).await;
    session.input.send(text(2, "two words")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 2).await;
    assert_eq!(spoken.words.len(), 2);
    assert_eq!(mock.connections(), 2, "one lazy reconnect");
}

#[tokio::test]
async fn a_close_while_a_context_is_live_is_a_disconnect() {
    let mock = Mock::start();
    mock.configure(|c| c.responder = Some(Arc::new(|_| Some(vec![]))));
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(5000);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "hello")).await.unwrap();
    eventually("the request", || !mock.received().is_empty()).await;
    mock.kick();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Error(Error::Disconnected { .. }))
    ));
    assert!(next(&mut session.events, 1000).await.is_none());
}

#[tokio::test]
async fn warm_parks_one_socket_that_the_next_connect_takes() {
    let mock = Mock::start();
    let provider = provider(&mock);
    provider.warm().await.unwrap();
    provider.warm().await.unwrap(); // idempotent: still one connection
    assert_eq!(mock.connections(), 1);
    let first = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 1, "connect used the parked socket");
    let second = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 2, "only one socket is parked");
    drop((first, second));
}

#[tokio::test]
async fn a_parked_socket_that_died_or_aged_out_is_replaced() {
    let mock = Mock::start();
    let provider = provider(&mock);
    provider.warm().await.unwrap();
    mock.kick();
    tokio::time::sleep(ms(150)).await;
    let mut session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(
        mock.connections(),
        2,
        "the closed parked socket was not reused"
    );
    session.input.send(text(1, "alive")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;

    let mut cfg = config(&mock);
    cfg.warm_max_age = Duration::ZERO;
    let old = CartesiaTts::new(cfg).unwrap();
    old.warm().await.unwrap();
    tokio::time::sleep(ms(20)).await;
    drop(old.connect(tts_config()).await.unwrap());
    assert_eq!(mock.connections(), 4, "an aged socket is replaced");
}

#[tokio::test]
async fn the_shutdown_token_ends_the_session() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    let token = cfg.shutdown.clone();
    cfg.io_timeout = ms(2000);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    token.cancel();
    assert!(next(&mut session.events, 1000).await.is_none());
}

#[tokio::test]
async fn dropping_the_input_after_the_flush_still_delivers_the_rest() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(10));
    let session = open(&mock).await;
    session.input.send(text(1, "one two three")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let TtsSession { input, events, .. } = session;
    drop(input);
    let mut events = events;
    let spoken = until_flushed(&mut events, 1).await;
    assert_eq!(spoken.words.len(), 3);
    // Then the stream ends.
    assert!(next(&mut events, 2000).await.is_none());
}

// ---------------------------------------------------------------------------------------------
// Review fixes: end of input, expiry, ordering, lenient parsing, cancel during I/O, many contexts
// ---------------------------------------------------------------------------------------------

/// Everything up to the end of the stream.
async fn drain_to_end(events: &mut mpsc::Receiver<TtsEvent>, ms: u64) -> Vec<TtsEvent> {
    let mut all = Vec::new();
    while let Some(event) = next(events, ms).await {
        all.push(event);
    }
    all
}

/// Like `until_flushed`, but events of other contexts (and their `Cancelled`) are skipped.
async fn collect_context(
    events: &mut mpsc::Receiver<TtsEvent>,
    context: u64,
    ms: u64,
) -> Utterance {
    let mut utterance = Utterance::default();
    loop {
        match next(events, ms).await.expect("stream ended early") {
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
            TtsEvent::Error(error) => panic!("session error: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn dropping_the_input_without_a_flush_closes_the_open_contexts_and_ends_the_stream() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(10));
    let session = open(&mock).await;
    session.input.send(text(1, "hi there")).await.unwrap();
    session.input.send(text(2, "second one")).await.unwrap();
    let TtsSession {
        input, mut events, ..
    } = session;
    // The engine goes away without flushing (a torn-down call): the stream must still end.
    drop(input);
    let all = drain_to_end(&mut events, 3000).await;
    let words = all
        .iter()
        .filter(|e| matches!(e, TtsEvent::Word { .. }))
        .count();
    assert_eq!(words, 4, "what was said before the end is still delivered");
    assert!(
        all.iter()
            .all(|e| matches!(e, TtsEvent::Audio { .. } | TtsEvent::Word { .. })),
        "no Flushed for a flush nobody asked for: {all:?}"
    );
    // Both server contexts were closed on the wire (continue false, empty transcript).
    let closes = mock
        .received()
        .iter()
        .filter(|f| f["continue"] == false && f["transcript"] == "")
        .count();
    assert_eq!(closes, 2, "{:?}", mock.received());
}

#[tokio::test]
async fn dropping_the_input_right_after_the_text_also_ends_the_stream() {
    let mock = Mock::start();
    let session = open(&mock).await;
    session.input.send(text(1, "one")).await.unwrap();
    let TtsSession {
        input, mut events, ..
    } = session;
    drop(input);
    let all = drain_to_end(&mut events, 3000).await;
    assert!(
        !all.iter().any(|e| matches!(e, TtsEvent::Error(_))),
        "{all:?}"
    );
}

#[tokio::test]
async fn a_server_that_never_answers_the_close_after_the_input_was_dropped_is_a_timeout() {
    let mock = Mock::start();
    mock.configure(|c| {
        // Text and markers are answered; the closing request never is.
        c.responder = Some(Arc::new(|request: &Value| {
            (request["continue"] == false).then(Vec::new)
        }));
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(300);
    let session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one two")).await.unwrap();
    let TtsSession {
        input, mut events, ..
    } = session;
    read_first_chunk(&mut events).await;
    // The marker's flush_done is in: nothing is owed, the stall timer is off.
    // (2 chunks, the timestamps, the flush_done)
    eventually("the marker to be answered", || mock.frames_sent() >= 4).await;
    tokio::time::sleep(ms(100)).await;
    drop(input);
    let started = tokio::time::Instant::now();
    match next(&mut events, 3000).await {
        Some(TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. }))) => {}
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() >= ms(250), "{:?}", started.elapsed());
    assert!(next(&mut events, 1000).await.is_none());
}

/// A mock that expires the context just as the second sentence arrives: it answers that text
/// of the first server context with an unsolicited `done` only (its marker and the close that
/// may follow stay unanswered).
fn done_instead_of_the_answer(lost: &'static str) -> mock::Responder {
    let expired = std::sync::atomic::AtomicBool::new(false);
    Arc::new(move |request: &Value| {
        let ctx = request["context_id"].as_str().unwrap_or("");
        if !ctx.ends_with("-0") {
            return None;
        }
        if request["transcript"] == lost {
            expired.store(true, std::sync::atomic::Ordering::SeqCst);
            return Some(vec![mock::done(ctx)]);
        }
        let silent = request["flush"] == true || request["continue"] == false;
        (silent && expired.load(std::sync::atomic::Ordering::SeqCst)).then(Vec::new)
    })
}

#[tokio::test]
async fn an_unsolicited_done_that_finds_text_unanswered_sends_it_again() {
    let mock = Mock::start();
    mock.configure(|c| c.responder = Some(done_instead_of_the_answer(" second sentence.")));
    let mut cfg = config(&mock);
    cfg.context_expiry = Duration::from_secs(3600);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "first one.")).await.unwrap();
    read_first_chunk(&mut session.events).await;
    // The first sentence is fully answered (2 chunks, the timestamps, the flush_done).
    eventually("the first marker to be answered", || {
        mock.frames_sent() >= 4
    })
    .await;
    tokio::time::sleep(ms(100)).await;
    session
        .input
        .send(text(1, " second sentence."))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    // The lost sentence was spoken after all: its two words follow the first sentence's.
    assert_eq!(spoken.words.len(), 2, "{spoken:?}");
    assert_eq!(spoken.audio.len(), 2 * 6400);
    let frames = mock.received();
    let resent: Vec<&Value> = frames
        .iter()
        .filter(|f| f["transcript"] == " second sentence.")
        .collect();
    assert_eq!(resent.len(), 2, "{frames:?}");
    assert_ne!(resent[0]["context_id"], resent[1]["context_id"]);
    // The closing request was repeated on the new server context.
    let closes: Vec<&Value> = frames.iter().filter(|f| f["continue"] == false).collect();
    assert_eq!(
        closes.last().map(|f| &f["context_id"]),
        Some(&resent[1]["context_id"]),
        "{frames:?}"
    );
    expect_silence(&mut session.events, 200).await;
}

#[tokio::test]
async fn a_server_that_answers_every_text_with_done_fails_the_session_instead_of_looping() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(Arc::new(|request: &Value| {
            let ctx = request["context_id"].as_str().unwrap_or("");
            if request["flush"] == true {
                return Some(vec![]);
            }
            Some(vec![mock::done(ctx)])
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "hello there")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    loop {
        match next(&mut session.events, 3000).await {
            Some(TtsEvent::Error(_)) => break,
            Some(TtsEvent::Flushed { .. }) => panic!("a lost sentence was reported as flushed"),
            Some(_) => {}
            None => panic!("the stream ended without an error"),
        }
    }
    // Bounded: the original request plus a handful of re-sends, not an endless stream.
    let attempts = mock
        .received()
        .iter()
        .filter(|f| f["transcript"] == "hello there")
        .count();
    assert!(attempts <= 4, "{attempts} attempts");
}

/// Reads the four events a two-word chunk produces (two audio, two words).
async fn read_first_chunk(events: &mut mpsc::Receiver<TtsEvent>) {
    for _ in 0..4 {
        next(events, 2000).await.unwrap();
    }
}

#[tokio::test]
async fn a_context_idle_for_the_expiry_is_reopened_and_word_offsets_continue() {
    for base in [TimeBase::Context, TimeBase::Chunk] {
        let mock = Mock::start();
        mock.configure(|c| {
            c.time_base = base;
            c.expiry = Some((ms(200), ExpiryMode::Error));
        });
        let mut cfg = config(&mock);
        cfg.context_expiry = ms(200);
        let mut session = CartesiaTts::new(cfg)
            .unwrap()
            .connect(tts_config())
            .await
            .unwrap();
        session.input.send(text(1, "one two")).await.unwrap();
        // Wait for the chunk to be answered, then think for longer than the expiry.
        read_first_chunk(&mut session.events).await;
        tokio::time::sleep(ms(450)).await;
        session.input.send(text(1, " three four")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let spoken = until_flushed(&mut session.events, 1).await;
        // 0.4 s of audio came before, so the second chunk's words continue at 420 and 620 ms.
        assert_eq!(rounded_ms(&spoken.words), [420, 620], "{base:?}");
        let ids: Vec<String> = mock
            .received()
            .iter()
            .map(|f| f["context_id"].as_str().unwrap().to_owned())
            .collect();
        let mut distinct = ids.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), 2, "a second server context: {ids:?}");
        // Never sent to the expired one: the mock would have answered with an error.
        let sends_to_old = ids.iter().filter(|id| **id == ids[0]).count();
        assert_eq!(sends_to_old, 2, "text and marker only: {ids:?}");
        // The new server context is closed by the flush, once.
        let closes = mock
            .received()
            .iter()
            .filter(|f| f["continue"] == false)
            .count();
        assert_eq!(closes, 1);
    }
}

#[tokio::test]
async fn an_expiry_error_reopens_the_context_and_sends_the_unanswered_text_again() {
    for base in [TimeBase::Context, TimeBase::Chunk] {
        let mock = Mock::start();
        mock.configure(|c| {
            c.time_base = base;
            c.expiry = Some((ms(150), ExpiryMode::Error));
        });
        let mut cfg = config(&mock);
        // The adapter's own estimate is off (never expires): the server's error is what tells.
        cfg.context_expiry = Duration::from_secs(3600);
        let mut session = CartesiaTts::new(cfg)
            .unwrap()
            .connect(tts_config())
            .await
            .unwrap();
        session.input.send(text(1, "one two")).await.unwrap();
        read_first_chunk(&mut session.events).await;
        tokio::time::sleep(ms(400)).await;
        session.input.send(text(1, " three four")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let spoken = until_flushed(&mut session.events, 1).await;
        assert_eq!(rounded_ms(&spoken.words), [420, 620], "{base:?}");
        assert_eq!(spoken.audio.len(), 2 * 6400);
        // The unanswered text went out again, verbatim, on a different server context.
        let frames = mock.received();
        let resent: Vec<&Value> = frames
            .iter()
            .filter(|f| f["transcript"] == " three four")
            .collect();
        assert_eq!(resent.len(), 2, "{frames:?}");
        assert_ne!(resent[0]["context_id"], resent[1]["context_id"]);
    }
}

#[tokio::test]
async fn an_unsolicited_done_closes_the_server_context_and_the_next_text_reopens_it() {
    let mock = Mock::start();
    mock.configure(|c| c.expiry = Some((ms(150), ExpiryMode::Done)));
    let mut cfg = config(&mock);
    cfg.context_expiry = Duration::from_secs(3600);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one two")).await.unwrap();
    read_first_chunk(&mut session.events).await;
    // The server's `done` arrives; the engine context lives on and no Flushed is made up.
    eventually("the unsolicited done", || mock.frames_sent() >= 5).await;
    expect_silence(&mut session.events, 300).await;
    session.input.send(text(1, " three four")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(rounded_ms(&spoken.words), [420, 620]);
}

#[tokio::test]
async fn flushing_an_expired_context_reports_flushed_without_touching_the_wire() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.context_expiry = ms(100);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one two")).await.unwrap();
    read_first_chunk(&mut session.events).await;
    tokio::time::sleep(ms(300)).await;
    let before = mock.received().len();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(1)
    ));
    assert_eq!(mock.received().len(), before, "nothing was sent");
}

#[tokio::test]
async fn word_offsets_do_not_depend_on_timestamps_arriving_before_the_flush_done() {
    for base in [TimeBase::Context, TimeBase::Chunk] {
        for order in [TsOrder::BeforeFlush, TsOrder::AfterFlush] {
            let mock = Mock::start();
            mock.configure(|c| {
                c.time_base = base;
                c.ts_order = order;
            });
            let mut session = open(&mock).await;
            session.input.send(text(1, "one two")).await.unwrap();
            session.input.send(text(1, " three")).await.unwrap();
            session.input.send(text(1, " four five")).await.unwrap();
            session.input.send(flush(1)).await.unwrap();
            let spoken = until_flushed(&mut session.events, 1).await;
            assert_eq!(
                rounded_ms(&spoken.words),
                [20, 220, 420, 620, 820],
                "{base:?} {order:?}"
            );
        }
    }
}

#[tokio::test]
async fn odd_word_times_degrade_instead_of_ending_the_call() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(Arc::new(|request: &Value| {
            let ctx = request["context_id"].as_str().unwrap().to_owned();
            if request["flush"] == true {
                return Some(vec![
                    json!({"type":"flush_done","flush_done":true,"flush_id":1,"context_id":ctx})
                        .to_string(),
                ]);
            }
            if request["continue"] == false {
                return Some(vec![mock::done(&ctx)]);
            }
            // Three words, two start times (one negative, one absurd), one missing.
            Some(vec![
                mock::chunk(&ctx, &[1, 1, 1, 1]),
                json!({"type":"timestamps","status_code":206,"context_id":ctx,
                       "word_timestamps":{"words":["a","b","c"],"start":[-0.001,1e300]}})
                .to_string(),
            ])
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "a b c")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let spoken = until_flushed(&mut session.events, 1).await;
    assert_eq!(rounded_ms(&spoken.words), [0, 0, 0]);
    assert_eq!(spoken.words[2].0, "c");
}

#[tokio::test]
async fn a_malformed_message_for_a_cancelled_context_does_not_end_the_session() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(50);
        c.responder = Some(Arc::new(|request: &Value| {
            if request["cancel"] == true {
                return Some(vec![]);
            }
            if request["flush"] == true {
                return Some(vec![flush_done(request)]);
            }
            let ctx = request["context_id"].as_str().unwrap();
            if request["transcript"] == "doomed" {
                return Some(vec![
                    mock::chunk(ctx, &[1, 1]),
                    mock::chunk(ctx, &[1, 1]),
                    mock::chunk(ctx, &[1, 1]),
                    // Wrong JSON types in every field the typed parse reads.
                    json!({"type":"timestamps","status_code":"206","context_id":ctx,
                           "word_timestamps":{"words":null,"start":"x"}})
                    .to_string(),
                    json!({"type":"done","done":"yes","context_id":ctx}).to_string(),
                    json!({"type":"chunk","data":5,"status_code":[1],"context_id":ctx}).to_string(),
                    // Not even a message: dropped as well.
                    "{\"type\":{\"a\":1},\"context_id\":{\"b\":2}}".to_owned(),
                ]);
            }
            None
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "doomed")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    loop {
        if matches!(
            next(&mut session.events, 1000).await,
            Some(TtsEvent::Cancelled { .. })
        ) {
            break;
        }
    }
    eventually("the late messages to be sent", || mock.frames_sent() >= 7).await;
    expect_silence(&mut session.events, 200).await;
    session.input.send(text(2, "fine")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
}

#[tokio::test]
async fn the_cancel_ack_does_not_wait_for_a_stalled_connect() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = Duration::from_secs(20);
    let mut session = CartesiaTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "one")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // The server drops the idle socket; the next text reconnects, and that connect stalls.
    mock.kick();
    expect_silence(&mut session.events, 300).await;
    mock.inject(Fault::ConnectStall {
        duration: Duration::from_secs(4),
    });
    session.input.send(text(2, "never spoken")).await.unwrap();
    eventually("the reconnect to start", || mock.connections() == 2).await;
    session.cancel.send(ContextId(2)).await.unwrap();
    // The connect stalls for 4 s: an ack inside 2.5 s cannot have waited for it.
    assert!(matches!(
        next(&mut session.events, 2500).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(2)
    ));
    // Once connected, the cancelled text is not sent and the session works.
    session.input.send(text(3, "after it")).await.unwrap();
    session.input.send(flush(3)).await.unwrap();
    let spoken = until_flushed_within(&mut session.events, 3, 6000).await;
    assert_eq!(spoken.words.len(), 2);
    assert!(
        mock.received()
            .iter()
            .all(|f| f["transcript"] != "never spoken")
    );
}

#[tokio::test]
async fn interleaved_contexts_are_routed_independently_and_a_cancel_touches_only_its_own() {
    interleaved_contexts_with_a_cancel(CancelMode::KeepSending).await;
}

/// With a server that honours the cancel, a wire cancel aimed at the wrong server context would
/// drop context 2's queued output: context 2 would never complete.
#[tokio::test]
async fn a_wire_cancel_hits_only_the_cancelled_context_on_a_server_that_honours_it() {
    interleaved_contexts_with_a_cancel(CancelMode::Honor).await;
}

async fn interleaved_contexts_with_a_cancel(mode: CancelMode) {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(15);
        c.cancel = mode;
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "a b c")).await.unwrap();
    session.input.send(text(2, "d e f g")).await.unwrap();
    session.input.send(text(1, " h i")).await.unwrap();
    session.input.send(text(2, " j")).await.unwrap();
    // Both are generating: wait until audio of each has arrived.
    let (mut seen1, mut seen2) = (false, false);
    let mut ctx2_audio = 0_usize;
    let mut ctx2_words: Vec<(String, Duration)> = Vec::new();
    while !(seen1 && seen2) {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => seen1 = true,
            TtsEvent::Audio {
                context: ContextId(2),
                audio,
            } => {
                ctx2_audio += audio.len();
                seen2 = true;
            }
            TtsEvent::Word {
                context: ContextId(2),
                word,
                start,
            } => ctx2_words.push((word.to_string(), start)),
            TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    // Barge-in on context 1 while context 2 still has output coming; flush 2.
    session.cancel.send(ContextId(1)).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let mut cancelled = false;
    loop {
        match next(&mut session.events, 5000).await.unwrap() {
            TtsEvent::Cancelled { context } => {
                assert_eq!(context, ContextId(1));
                assert!(!cancelled);
                cancelled = true;
            }
            TtsEvent::Audio {
                context: ContextId(2),
                audio,
            } => {
                ctx2_audio += audio.len();
            }
            TtsEvent::Word {
                context: ContextId(2),
                word,
                start,
            } => ctx2_words.push((word.to_string(), start)),
            TtsEvent::Flushed { context } => {
                assert_eq!(context, ContextId(2));
                break;
            }
            TtsEvent::Audio { context, .. } | TtsEvent::Word { context, .. } => {
                assert_eq!(context, ContextId(1));
                assert!(!cancelled, "context 1 produced output after its Cancelled");
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(cancelled);
    // Context 2 lost nothing: 5 words, in order, continuous offsets, all its audio.
    assert_eq!(ctx2_audio, 5 * 6400);
    assert_eq!(rounded_ms(&ctx2_words), [20, 220, 420, 620, 820]);
    let names: Vec<&str> = ctx2_words.iter().map(|(w, _)| w.as_str()).collect();
    assert_eq!(names, ["d", "e", "f", "g", "j"]);
    expect_silence(&mut session.events, 300).await;
    // Exactly one cancel went to the wire, and it names context 1's server context.
    eventually("the cancel on the wire", || {
        mock.received().iter().any(|f| f.get("cancel").is_some())
    })
    .await;
    let frames = mock.received();
    let server_of = |transcript: &str| {
        frames
            .iter()
            .find(|f| f["transcript"] == transcript)
            .unwrap_or_else(|| panic!("no request with {transcript:?}: {frames:?}"))["context_id"]
            .clone()
    };
    let (ctx1, ctx2) = (server_of("a b c"), server_of("d e f g"));
    assert_ne!(ctx1, ctx2);
    let cancels: Vec<&Value> = frames
        .iter()
        .filter(|f| f.get("cancel").is_some())
        .collect();
    assert_eq!(cancels.len(), 1, "{frames:?}");
    assert_eq!(cancels[0]["cancel"], true);
    assert_eq!(cancels[0]["context_id"], ctx1, "{mode:?}: wrong target");
}

#[tokio::test]
async fn a_concurrency_refusal_after_a_barge_in_is_retried_for_the_new_context_only() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(10);
        c.responder = Some(Arc::new(|request: &Value| {
            // The second server context ("...-1") is refused while the cancelled first one is
            // still counted; the retry gets a new id ("...-2") and is served.
            let ctx = request["context_id"].as_str().unwrap_or("");
            if ctx.ends_with("-1") {
                if request["flush"] == true {
                    return Some(vec![]);
                }
                return Some(vec![
                    json!({
                        "type": "error", "done": true, "error_code": "concurrency_limited",
                        "status_code": 429, "title": "Too many", "message": "limit",
                        "context_id": ctx
                    })
                    .to_string(),
                ]);
            }
            None
        }));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "a b c d e f")).await.unwrap();
    loop {
        if matches!(
            next(&mut session.events, 2000).await.unwrap(),
            TtsEvent::Audio { .. }
        ) {
            break;
        }
    }
    session.cancel.send(ContextId(1)).await.unwrap();
    session.input.send(text(2, "x y")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let spoken = collect_context(&mut session.events, 2, 5000).await;
    assert_eq!(rounded_ms(&spoken.words), [20, 220]);
    let frames = mock.received();
    let to_ctx = |suffix: &str| {
        frames
            .iter()
            .filter(|f| f["context_id"].as_str().unwrap().ends_with(suffix))
            .count()
    };
    // The refused context got its text and marker (and perhaps the close, if the engine's
    // flush was already on the wire); the retry on a new id got everything once.
    assert!((2..=3).contains(&to_ctx("-1")), "{frames:?}");
    assert_eq!(to_ctx("-2"), 3, "retry: text, marker, close: {frames:?}");
}

#[tokio::test]
async fn a_concurrency_refusal_that_persists_ends_the_session_as_rejected_429() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(error_responder(json!({
            "type": "error", "done": true, "error_code": "concurrency_limited",
            "status_code": 429, "title": "Too many", "message": "limit"
        })));
    });
    let (error, ended) = first_error(&mock).await;
    assert!(
        matches!(error, Error::Rejected { status: 429, .. }),
        "{error}"
    );
    assert!(ended);
    let attempts = mock
        .received()
        .iter()
        .filter(|f| f["transcript"] == "hi")
        .count();
    assert_eq!(attempts, 5, "the first try and four retries");
}

#[tokio::test]
async fn a_bad_request_error_is_not_retried() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(error_responder(json!({
            "type": "error", "done": true, "error_code": "voice_not_found",
            "status_code": 400, "title": "Voice", "message": "no such voice"
        })));
    });
    let (error, _) = first_error(&mock).await;
    assert!(matches!(error, Error::Protocol { .. }), "{error}");
    let attempts = mock
        .received()
        .iter()
        .filter(|f| f["transcript"] == "hi")
        .count();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn plain_ws_to_a_remote_host_is_refused_unless_allowed() {
    let mock = Mock::start();
    for url in [
        "ws://api.cartesia.ai/tts/websocket",
        "ws://10.0.0.5:8080/tts/websocket",
        "http://api.cartesia.ai/tts/websocket",
        "not a url",
    ] {
        let mut cfg = config(&mock);
        cfg.url = url.to_owned();
        let err = CartesiaTts::new(cfg).unwrap_err().to_string();
        assert!(!err.contains(&mock.api_key), "{err}");
    }
    let mut cfg = config(&mock);
    cfg.url = "ws://api.cartesia.ai/tts/websocket".to_owned();
    cfg.allow_insecure_ws = true;
    assert!(CartesiaTts::new(cfg).is_ok(), "explicit opt-in");
    for url in [
        "wss://api.cartesia.ai/tts/websocket",
        "ws://localhost:9/x",
        "ws://127.0.0.1:9/x",
        "ws://[::1]:9/x",
    ] {
        let mut cfg = config(&mock);
        cfg.url = url.to_owned();
        assert!(CartesiaTts::new(cfg).is_ok(), "{url}");
    }
}

#[tokio::test]
async fn a_lazy_reconnect_does_not_consume_the_socket_parked_for_the_next_connect() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let mut session = provider.connect(tts_config()).await.unwrap();
    session.input.send(text(1, "one")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    mock.kick();
    expect_silence(&mut session.events, 300).await;
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 2, "first session, parked socket");
    session.input.send(text(2, "two words")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
    assert_eq!(mock.connections(), 3, "the reconnect opened its own socket");
    let next_session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 3, "the parked socket was still there");
    drop((session, next_session));
}

#[tokio::test]
async fn a_cancelled_shutdown_token_drops_the_parked_socket() {
    let mock = Mock::start();
    let cfg = config(&mock);
    let token = cfg.shutdown.clone();
    let provider = CartesiaTts::new(cfg).unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1);
    token.cancel();
    // Nothing new is parked; connect does not reuse the parked socket, and the session ends.
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1);
    let mut session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 2, "the parked socket was dropped");
    assert!(next(&mut session.events, 1000).await.is_none());
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1, CARTESIA_API_KEY, CARTESIA_VOICE_ID)
// ---------------------------------------------------------------------------------------------

/// A short real session: two chunks and a flush with timestamps, a barge-in and a second
/// utterance. Prints measurements for the open items of docs/providers/cartesia.md; never
/// prints the key.
#[tokio::test]
async fn live_cartesia_session() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 (and CARTESIA_API_KEY, CARTESIA_VOICE_ID)");
        return;
    }
    let (Ok(key), Ok(voice)) = (
        std::env::var("CARTESIA_API_KEY"),
        std::env::var("CARTESIA_VOICE_ID"),
    ) else {
        eprintln!("skipped: CARTESIA_API_KEY and CARTESIA_VOICE_ID must both be set");
        return;
    };
    let mut cfg = CartesiaConfig::new(Secret::new(key), clock());
    cfg.language = Some(std::env::var("CARTESIA_LANGUAGE").unwrap_or_else(|_| "en".to_owned()));
    let provider = CartesiaTts::new(cfg).unwrap();
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
    let audio_secs = audio_bytes as f64 / (24_000.0 * 2.0);
    eprintln!(
        "live: warm {warm_took:?}, first audio {first_audio:?}, {audio_secs:.2} s of audio, {} words",
        words.len()
    );
    eprintln!(
        "live: first word at {:?}, last word at {:?}",
        words.first().map(|w| w.1),
        words.last().map(|w| w.1)
    );
    assert!(audio_bytes > 0 && audio_bytes.is_multiple_of(2));
    assert!(!words.is_empty(), "timestamps were requested");
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
