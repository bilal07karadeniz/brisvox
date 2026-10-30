//! Deepgram Aura adapter tests: the provider contract suite and protocol-level
//! checks against a local mock of the Aura TTS WebSocket, plus a live test gated on
//! `BRISVOX_LIVE_TESTS=1`.
//!
//! The sockets are real, so these tests use the real system clock and short deadlines instead
//! of paused time (the `disallowed_methods` lint guards engine logic, not test pacing).
#![cfg(feature = "provider-deepgram-aura")]
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

#[path = "deepgram_aura/mock.rs"]
mod mock;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::deepgram_aura::{
    DeepgramAuraConfig, DeepgramAuraTts, FlushPolicy, OutputEncoding, supports_language,
};
use brisvox_providers::{ContextId, Error, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use brisvox_sim::contract::{CONTRACT_TIMEOUT, Fault, TtsHarness, tts_contract};
use mock::{ClearMode, Mock, Out};
use serde_json::{Value, json};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Bytes of one mock word at 16 kHz PCM16 (0.2 s).
const WORD: usize = 6400;

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn config(mock: &Mock) -> DeepgramAuraConfig {
    let mut config = DeepgramAuraConfig::new(Secret::new(mock.api_key.clone()), clock());
    config.url = mock.url.clone();
    config.language = Some("en".to_owned());
    config.io_timeout = Duration::from_millis(1500);
    config
}

fn provider(mock: &Mock) -> DeepgramAuraTts {
    DeepgramAuraTts::new(config(mock)).unwrap()
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
}

/// Reads events up to `Flushed` of `context`. Any `Word` (the provider has none) or event of
/// another context is a failure.
async fn until_flushed(events: &mut mpsc::Receiver<TtsEvent>, context: u64) -> Utterance {
    until_flushed_within(events, context, 3000).await
}

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
            TtsEvent::Flushed { context: c } if c == ContextId(context) => return utterance,
            other => panic!("unexpected event {}", short(&other)),
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
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Run-length encoding of the audio: the mock fills every word with the word's first byte.
fn runs(audio: &[u8]) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::new();
    for byte in audio {
        match out.last_mut() {
            Some((c, n)) if *c == char::from(*byte) => *n += 1,
            _ => out.push((char::from(*byte), 1)),
        }
    }
    out
}

/// A bounded Debug form for failure messages (audio events carry kilobytes).
fn short(value: &impl std::fmt::Debug) -> String {
    format!("{value:?}").chars().take(160).collect()
}

fn speak(text: &str) -> Value {
    json!({"type": "Speak", "text": text})
}

fn labels_contain(mock: &Mock, label: &str) -> bool {
    mock.labels().iter().any(|l| l == label)
}

fn position(labels: &[String], label: &str) -> usize {
    labels
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("{label} not in {labels:#?}"))
}

/// A responder that answers `kind` with `frames` (anything else keeps the default behaviour).
fn on(kind: &'static str, frames: Vec<Out>) -> mock::Responder {
    Arc::new(move |request: &Value| (request["type"] == kind).then(|| frames.clone()))
}

fn json_text(value: &Value) -> Out {
    Out::Text(value.to_string())
}

/// The first error event of a session, and whether the stream ended after it.
async fn first_error(session: &mut TtsSession) -> (Error, bool) {
    loop {
        match next(&mut session.events, 4000).await {
            Some(TtsEvent::Error(e)) => {
                let ended = next(&mut session.events, 1000).await.is_none();
                return (e, ended);
            }
            Some(_) => {}
            None => panic!("stream ended without an error"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite
// ---------------------------------------------------------------------------------------------

struct Harness {
    mock: Mock,
    provider: Arc<DeepgramAuraTts>,
    clock: SharedClock,
}

impl Harness {
    fn new(policy: FlushPolicy) -> Self {
        let mock = Mock::start();
        mock.configure(|c| c.interval = ms(20));
        let mut cfg = config(&mock);
        cfg.io_timeout = CONTRACT_TIMEOUT;
        cfg.flush_policy = policy;
        cfg.warm_target = Some(TtsConfig {
            sample_rate: SampleRate::Hz16000,
            voice: Arc::from("contract"),
            language: None,
            model: None,
        });
        let provider = Arc::new(DeepgramAuraTts::new(cfg).unwrap());
        Self {
            mock,
            provider,
            clock: clock(),
        }
    }
}

impl TtsHarness for Harness {
    type P = DeepgramAuraTts;

    fn provider(&self) -> Arc<DeepgramAuraTts> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn emits_words(&self) -> bool {
        false
    }

    fn reset(&self) {
        self.mock.reset();
    }

    fn inject(&self, fault: Fault) {
        self.mock.inject(fault);
    }
}

/// Runs a future on a thread with a big stack: in debug builds the suite's futures are large
/// and overflow the default 2 MiB test-thread stack.
fn on_big_stack(future: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Runs the whole suite (about 15 s: it contains several real-time deadline checks). The
/// harness flushes after every text (`FlushPolicy::PerText`) so that a silent server is a
/// stall from the first text on, and declares that the provider reports no word times.
#[test]
fn passes_the_provider_contract_suite() {
    on_big_stack(async {
        let harness = Harness::new(FlushPolicy::PerText);
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
    });
}

/// The default `PerTurn` policy passes the whole suite too: a bare text owes nothing, but the
/// session probes the connection with a WebSocket `Ping` while text is on the server, so a
/// silent server is a `Timeout` at the deadline (the `timeout` check stalls after a bare text).
#[test]
fn the_default_per_turn_policy_passes_the_contract_suite_too() {
    on_big_stack(async {
        let harness = Harness::new(FlushPolicy::PerTurn);
        let report = tts_contract(&harness).await;
        let failed: Vec<&str> = report.failures().map(|c| c.name).collect();
        assert!(failed.is_empty(), "{report:#?}");
    });
}

// ---------------------------------------------------------------------------------------------
// Handshake, auth, configuration
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn handshake_carries_the_query_and_the_token_header_but_never_the_key_in_the_url() {
    let mock = Mock::start();
    let session = open(&mock).await;
    let handshakes = mock.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert!(handshakes[0].auth_ok);
    assert!(
        handshakes[0]
            .uri
            .ends_with("/v1/speak?model=voice-1&encoding=linear16&sample_rate=16000"),
        "uri {}",
        handshakes[0].uri
    );
    assert!(!handshakes[0].uri.contains(&mock.api_key));
    assert!(!handshakes[0].uri.contains("container"));
    drop(session);
}

#[tokio::test]
async fn encoding_rate_and_speed_reach_the_query() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    cfg.speed = Some(1.25);
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let session = provider
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz8000,
            voice: Arc::from("aura-2-thalia-en"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    assert!(
        mock.handshakes()[0]
            .uri
            .ends_with("?model=aura-2-thalia-en&encoding=mulaw&sample_rate=8000&speed=1.25"),
        "{}",
        mock.handshakes()[0].uri
    );
    drop(session);
    assert_eq!(provider.name(), "deepgram-aura");
}

#[tokio::test]
async fn a_wrong_key_is_rejected_with_401() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("wrong".to_owned());
    let provider = DeepgramAuraTts::new(cfg).unwrap();
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
    assert!(debug.contains("REDACTED"), "{debug}");
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("bad\nkey-SECRET".to_owned());
    let err = DeepgramAuraTts::new(cfg).unwrap_err().to_string();
    assert!(!err.contains("SECRET"), "{err}");
    let mut cfg = config(&mock);
    cfg.api_key = Secret::new("wrong-SECRET".to_owned());
    let err = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.contains("SECRET"), "{err}");
}

#[tokio::test]
async fn invalid_configuration_is_refused_up_front() {
    let mock = Mock::start();
    type Change = Box<dyn Fn(&mut DeepgramAuraConfig)>;
    let bad: Vec<(&str, Change)> = vec![
        (
            "zero io_timeout",
            Box::new(|c| c.io_timeout = Duration::ZERO),
        ),
        ("zero window", Box::new(|c| c.rate_window = Duration::ZERO)),
        (
            "zero age",
            Box::new(|c| c.max_connection_age = Duration::ZERO),
        ),
        ("zero segment", Box::new(|c| c.max_speak_chars = 0)),
        ("zero flushes", Box::new(|c| c.max_flushes_per_window = 0)),
        ("zero chars", Box::new(|c| c.chars_per_window = 0)),
        (
            "segment above budget",
            Box::new(|c| {
                c.max_speak_chars = 3000;
                c.chars_per_window = 2400;
            }),
        ),
        ("slow speed", Box::new(|c| c.speed = Some(0.5))),
        ("fast speed", Box::new(|c| c.speed = Some(1.6))),
        ("nan speed", Box::new(|c| c.speed = Some(f32::NAN))),
        ("turkish", Box::new(|c| c.language = Some("tr".to_owned()))),
        (
            "turkish tag",
            Box::new(|c| c.language = Some("tr-TR".to_owned())),
        ),
        (
            "portuguese",
            Box::new(|c| c.language = Some("pt".to_owned())),
        ),
        (
            "http url",
            Box::new(|c| c.url = "http://example.com".to_owned()),
        ),
        (
            "remote ws",
            Box::new(|c| c.url = "ws://example.com/v1/speak".to_owned()),
        ),
        (
            "turkish warm target",
            Box::new(|c| {
                c.warm_target = Some(TtsConfig {
                    sample_rate: SampleRate::Hz16000,
                    voice: Arc::from("aura-2-x-tr"),
                    language: None,
                    model: None,
                });
            }),
        ),
    ];
    for (name, change) in bad {
        let mut cfg = config(&mock);
        change(&mut cfg);
        let err = DeepgramAuraTts::new(cfg).unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{name}: {err}");
        assert!(!err.to_string().contains(&mock.api_key), "{name}: {err}");
    }
    for language in ["en", "EN", "es-419", "de", "ja"] {
        let mut cfg = config(&mock);
        cfg.language = Some(language.to_owned());
        assert!(DeepgramAuraTts::new(cfg).is_ok(), "{language}");
    }
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn turkish_is_refused_at_connect_with_a_clear_message() {
    let mock = Mock::start();
    let err = provider(&mock)
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz16000,
            voice: Arc::from("aura-2-ozge-tr"),
            language: None,
            model: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Protocol { detail, .. } if detail.contains("Turkish")),
        "{err}"
    );
    // The language suffix of the model string is matched case-insensitively.
    for voice in ["aura-2-ozge-TR", "aura-2-ozge-Tr"] {
        let err = provider(&mock)
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from(voice),
                language: None,
                model: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Protocol { detail, .. } if detail.contains("Turkish")),
            "{voice}: {err}"
        );
    }
    assert!(!supports_language("tr"));
    assert!(supports_language("en"));
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn every_supported_language_connects_in_either_case() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let mut connections = 0;
    for lang in ["en", "es", "de", "fr", "nl", "it", "ja", "FR", "Nl", "IT"] {
        let voice = format!("aura-2-x-{lang}");
        let session = provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from(voice.as_str()),
                language: None,
                model: None,
            })
            .await
            .unwrap_or_else(|e| panic!("{voice}: {e}"));
        drop(session);
        connections += 1;
        assert!(supports_language(lang), "{lang}");
    }
    assert_eq!(mock.connections(), connections);
}

#[tokio::test]
async fn invalid_session_configuration_is_refused_before_any_connection() {
    let mock = Mock::start();
    let provider = provider(&mock);
    for voice in [
        "",
        "a&b=c",
        "bad voice",
        "aura-2-x-pt",
        "aura-2-x-PT",
        "aura-2-ozge-TR",
        "aura-2-ozge-Tr",
        "tr_TR",
    ] {
        let err = provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from(voice),
                language: None,
                model: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{voice:?}: {err}");
    }
    for rate in [SampleRate::Hz22050, SampleRate::Hz44100] {
        let err = provider
            .connect(TtsConfig {
                sample_rate: rate,
                voice: Arc::from("voice-1"),
                language: None,
                model: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Protocol { detail, .. } if detail.contains("Hz")),
            "{err}"
        );
    }
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Alaw;
    let err = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz24000,
            voice: Arc::from("voice-1"),
            language: None,
            model: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Protocol { .. }), "{err}");
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn plain_ws_to_a_remote_host_is_refused_unless_allowed() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.url = "ws://example.com/v1/speak".to_owned();
    assert!(DeepgramAuraTts::new(cfg.clone()).is_err());
    cfg.allow_insecure_ws = true;
    assert!(DeepgramAuraTts::new(cfg).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Wire format and flushing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn text_goes_out_verbatim_and_the_flush_only_when_the_engine_flushes() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "Hello there. ")).await.unwrap();
    session.input.send(text(1, "It is me.")).await.unwrap();
    eventually("two Speak messages", || mock.received().len() == 2).await;
    tokio::time::sleep(ms(100)).await;
    assert_eq!(
        mock.received(),
        [speak("Hello there. "), speak("It is me.")],
        "no Flush before the engine flushes"
    );
    session.input.send(flush(1)).await.unwrap();
    let utterance = until_flushed(&mut session.events, 1).await;
    assert_eq!(
        mock.received(),
        [
            speak("Hello there. "),
            speak("It is me."),
            json!({"type": "Flush"})
        ]
    );
    // Five words, each filled with its first letter: only audio, no word events.
    assert_eq!(
        runs(&utterance.audio),
        [
            ('H', WORD),
            ('t', WORD),
            ('I', WORD),
            ('i', WORD),
            ('m', WORD)
        ]
    );
    expect_silence(&mut session.events, 300).await;
}

#[tokio::test]
async fn per_text_flushes_after_each_text_and_reports_one_flushed() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let mut session = provider.connect(tts_config()).await.unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(text(1, " beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let utterance = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&utterance.audio), [('a', WORD), ('b', WORD)]);
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush"],
        "the engine's flush adds none: everything was flushed already"
    );
    expect_silence(&mut session.events, 300).await;
}

#[tokio::test]
async fn long_text_is_split_and_the_per_text_flush_follows_the_last_piece() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    cfg.max_speak_chars = 20;
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let mut session = provider.connect(tts_config()).await.unwrap();
    let long = "one two three. four five six. seven eight nine ten";
    session.input.send(text(1, long)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    let received = mock.received();
    let pieces: Vec<&str> = received
        .iter()
        .filter(|v| v["type"] == "Speak")
        .map(|v| v["text"].as_str().unwrap())
        .collect();
    assert!(pieces.len() >= 3, "{pieces:?}");
    assert!(pieces.iter().all(|p| p.chars().count() <= 20), "{pieces:?}");
    assert_eq!(pieces.concat(), long);
    let types = mock.received_types();
    assert_eq!(
        types.iter().filter(|t| *t == "Flush").count(),
        1,
        "{types:?}"
    );
    assert_eq!(types.last().map(String::as_str), Some("Flush"));
}

#[tokio::test]
async fn a_flush_without_text_reports_flushed_at_once_and_sends_nothing() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(flush(7)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(7)
    ));
    expect_silence(&mut session.events, 200).await;
    assert!(mock.received().is_empty());
}

#[tokio::test]
async fn empty_text_and_text_after_the_flush_of_the_same_context_send_nothing() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "")).await.unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // Queued behind the flush of the same context: its server stream is closed.
    session.input.send(text(1, "late words")).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    expect_silence(&mut session.events, 300).await;
    assert_eq!(mock.received_types(), ["Speak", "Flush"]);
}

#[tokio::test]
async fn text_the_server_holds_until_the_flush_is_still_delivered_on_flush() {
    // The other possibility the docs leave open: no audio before the Flush.
    let mock = Mock::start();
    mock.configure(|c| c.auto = false);
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    expect_silence(&mut session.events, 300).await;
    session.input.send(flush(1)).await.unwrap();
    let utterance = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&utterance.audio), [('a', WORD), ('b', WORD)]);
}

#[tokio::test]
async fn no_word_events_are_ever_made_up() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    // `until_flushed` panics on any Word.
    let utterance = until_flushed(&mut session.events, 1).await;
    assert_eq!(utterance.audio.len(), 3 * WORD);
}

// ---------------------------------------------------------------------------------------------
// Contexts on a single stream
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn contexts_are_served_one_at_a_time_and_audio_is_attributed_in_order() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(30));
    let mut session = open(&mock).await;
    session
        .input
        .send(text(1, "alpha beta gamma delta"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(2, "echo foxtrot")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    // The first audio of context 1: context 2's text is still held back.
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { context, .. }) if context == ContextId(1)
    ));
    assert!(
        !mock
            .labels()
            .iter()
            .any(|l| l.starts_with("recv:Speak:echo"))
    );
    // Rest of context 1 (the first frame was consumed above).
    let mut first = Vec::new();
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Audio {
                context: ContextId(1),
                audio,
            } => {
                first.extend_from_slice(&audio);
            }
            TtsEvent::Flushed {
                context: ContextId(1),
            } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert_eq!(first.len(), 3 * WORD, "three more words after the first");
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('e', WORD), ('f', WORD)]);
    let labels = mock.labels();
    assert!(
        position(&labels, "send:Flushed") < position(&labels, "recv:Speak:echo foxtrot"),
        "context 2 is sent only after the Flushed of context 1: {labels:#?}"
    );
}

#[tokio::test]
async fn a_context_the_engine_has_not_flushed_holds_the_next_one_back() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { context, .. }) if context == ContextId(1)
    ));
    expect_silence(&mut session.events, 400).await;
    assert!(!labels_contain(&mock, "recv:Speak:bravo"));
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Flushed { context }) if context == ContextId(1)
    ));
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
}

#[tokio::test]
async fn text_waiting_behind_an_unflushed_context_is_bounded_and_a_cancel_releases_it() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    // Context 2 is queued behind the unflushed context 1: the adapter stops reading input
    // once 128 pieces wait (plus the 64 the channel holds), so the sender blocks.
    let mut accepted = 0_usize;
    for _ in 0..400 {
        match tokio::time::timeout(ms(200), session.input.send(text(2, "x "))).await {
            Ok(Ok(())) => accepted += 1,
            Ok(Err(_)) => panic!("input closed"),
            Err(_) => break,
        }
    }
    assert!(
        (128..=200).contains(&accepted),
        "{accepted} texts were accepted before the sender blocked"
    );
    assert!(!labels_contain(&mock, "recv:Speak:x "));
    // The cancel channel is never gated: cancelling context 2 frees the queue.
    session.cancel.send(ContextId(2)).await.unwrap();
    let mut got_ack = false;
    while !got_ack {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Cancelled {
                context: ContextId(2),
            } => got_ack = true,
            TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            other => panic!("unexpected {}", short(&other)),
        }
    }
    session.input.send(flush(1)).await.unwrap();
    // Drain what the stuck sends left behind, then context 1 completes.
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Flushed {
                context: ContextId(1),
            } => break,
            TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            other => panic!("unexpected {}", short(&other)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Audio framing and server messages
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn odd_sized_frames_become_whole_samples() {
    let mock = Mock::start();
    mock.configure(|c| c.frame_bytes = Some(1001));
    let mut session = open(&mock).await;
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let mut audio = Vec::new();
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Audio { audio: a, .. } => {
                assert!(a.len() % 2 == 0, "{} bytes", a.len());
                audio.extend_from_slice(&a);
            }
            TtsEvent::Flushed { .. } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert_eq!(
        runs(&audio),
        [('a', WORD), ('b', WORD), ('g', WORD)],
        "no byte lost or reordered"
    );
}

#[tokio::test]
async fn mulaw_frames_pass_through_untouched() {
    let mock = Mock::start();
    mock.configure(|c| c.frame_bytes = Some(333));
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let mut session = provider
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz8000,
            voice: Arc::from("voice-1"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let sizes = audio_sizes_until_flushed(&mut session.events, 1).await;
    // 8 kHz x 0.2 s = 1600 samples = 1600 bytes per word, in the server's own 333 byte frames:
    // every event is exactly one frame of the server (nothing joined, split or carried).
    let word = [333, 333, 333, 333, 268];
    assert_eq!(sizes, [word, word].concat());
}

/// Mu-law has one byte per sample, so an utterance can have an odd number of bytes. Applying
/// the PCM16 alignment to it would carry the odd byte of a frame to the next one and lose the
/// last byte of the utterance.
#[tokio::test]
async fn mulaw_frames_of_odd_size_and_an_odd_total_lose_no_byte() {
    let mock = Mock::start();
    // 8000 Hz x (1601.5 / 8000) s = 1601 samples = 1601 bytes (odd) in frames of 333.
    mock.configure(|c| {
        c.frame_bytes = Some(333);
        c.word_secs = 1601.5 / 8000.0;
    });
    let mut cfg = config(&mock);
    cfg.encoding = OutputEncoding::Mulaw;
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let mut session = provider
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz8000,
            voice: Arc::from("voice-1"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let sizes = audio_sizes_until_flushed(&mut session.events, 1).await;
    assert_eq!(sizes, [333, 333, 333, 333, 269]);
    assert_eq!(sizes.iter().sum::<usize>(), 1601);
}

/// The size of every audio event up to `Flushed` of `context`.
async fn audio_sizes_until_flushed(
    events: &mut mpsc::Receiver<TtsEvent>,
    context: u64,
) -> Vec<usize> {
    let mut sizes = Vec::new();
    loop {
        match next(events, 3000).await.expect("stream ended early") {
            TtsEvent::Audio { context: c, audio } if c == ContextId(context) => {
                sizes.push(audio.len());
            }
            TtsEvent::Flushed { context: c } if c == ContextId(context) => return sizes,
            other => panic!("unexpected event {}", short(&other)),
        }
    }
}

#[tokio::test]
async fn warnings_metadata_and_unknown_messages_are_ignored() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(on(
            "Flush",
            vec![
                json_text(&json!({"type": "Warning", "description": "slow down", "code": "X"})),
                json_text(&json!({"type": "Metadata", "request_id": "r"})),
                json_text(&json!({"type": "SomethingNew", "x": 1})),
                json_text(&json!({"type": "Flushed", "sequence_id": 0})),
            ],
        ));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    expect_silence(&mut session.events, 300).await;
}

#[tokio::test]
async fn malformed_frames_are_protocol_errors() {
    for bad in ["{not json", "[1, 2]", "42"] {
        let mock = Mock::start();
        mock.configure(|c| c.responder = Some(on("Speak", vec![Out::Text(bad.to_owned())])));
        let mut session = open(&mock).await;
        session.input.send(text(1, "alpha")).await.unwrap();
        let (error, ended) = first_error(&mut session).await;
        assert!(matches!(error, Error::Protocol { .. }), "{bad}: {error}");
        assert!(ended, "{bad}");
    }
}

#[tokio::test]
async fn an_error_message_is_a_protocol_error_with_its_code() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(on(
            "Speak",
            vec![json_text(
                &json!({"type": "Error", "code": "DATA-0000", "description": "bad"}),
            )],
        ));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    let (error, _) = first_error(&mut session).await;
    assert!(
        matches!(&error, Error::Protocol { detail, .. } if detail.contains("DATA-0000")),
        "{error}"
    );
}

#[tokio::test]
async fn close_codes_map_to_protocol_or_disconnected() {
    for (code, reason, protocol) in [
        (1008_u16, "DATA-0001", true),
        (1009, "BIG-0001", true),
        (1003, "MESSAGE-0000", true),
        (1011, "NET-0000", false),
        (1011, "NET-0003", false),
        (1000, "", false),
    ] {
        let mock = Mock::start();
        mock.configure(|c| {
            c.responder = Some(on("Speak", vec![Out::Close(code, reason.to_owned())]));
        });
        let mut session = open(&mock).await;
        session.input.send(text(1, "alpha")).await.unwrap();
        let (error, ended) = first_error(&mut session).await;
        assert!(ended);
        if protocol {
            assert!(
                matches!(&error, Error::Protocol { detail, .. }
                    if detail.contains(&code.to_string()) && detail.contains(reason)),
                "{code}: {error}"
            );
        } else {
            assert!(
                matches!(error, Error::Disconnected { .. }),
                "{code}: {error}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cancel (barge-in)
// ---------------------------------------------------------------------------------------------

const TEN_WORDS: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa";

async fn barge_in(mode: ClearMode) {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(40);
        c.clear = mode;
        // The docs allow the server to discard text that arrives between `Clear` and
        // `Cleared`: the adapter must hold the next utterance back until `Cleared`.
        c.discard_speak_until_cleared = true;
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, TEN_WORDS)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { context, .. }) if context == ContextId(1)
    ));
    let started = tokio::time::Instant::now();
    session.cancel.send(ContextId(1)).await.unwrap();
    // The acknowledgement does not wait for `Cleared`, which the server sends after the
    // remaining nine frames (about 360 ms).
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            TtsEvent::Cancelled { context } => {
                assert_eq!(context, ContextId(1));
                break;
            }
            // Already handed over before the cancel.
            TtsEvent::Audio { .. } => {}
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(started.elapsed() < ms(200), "{:?}", started.elapsed());
    // The next utterance is sent right away, while `Cleared` is still on its way (about
    // 360 ms in `KeepSending` mode). It is held until `Cleared`, then plays clean; the tail of
    // the cancelled utterance (audio, Flushed) never reaches the consumer (`until_flushed`
    // fails on an event of another context).
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    expect_silence(&mut session.events, 300).await;
    let labels = mock.labels();
    let clear = position(&labels, "recv:Clear");
    let cleared = position(&labels, "send:Cleared");
    let bravo = position(&labels, "recv:Speak:bravo");
    assert!(clear < cleared && cleared < bravo, "{labels:#?}");
    assert!(
        !labels.iter().any(|l| l.starts_with("drop:")),
        "the server discarded text sent between Clear and Cleared: {labels:#?}"
    );
}

#[tokio::test]
async fn cancel_acks_at_once_clears_and_drops_the_tail_the_server_keeps_sending() {
    barge_in(ClearMode::KeepSending).await;
}

#[tokio::test]
async fn cancel_works_against_a_server_that_stops_at_once_too() {
    barge_in(ClearMode::Honor).await;
}

#[tokio::test]
async fn cancel_purges_events_the_adapter_still_holds_for_a_slow_consumer() {
    let mock = Mock::start();
    mock.configure(|c| c.frame_bytes = Some(100));
    let mut session = open(&mock).await;
    session
        .input
        .send(text(1, "alpha beta gamma delta"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    // Nobody reads: the channel (64) and the adapter's queue (about 64) fill up.
    eventually("the server to be held back", || mock.frames_sent() > 150).await;
    tokio::time::sleep(ms(200)).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    let (mut audio_events, mut cancelled) = (0_usize, false);
    while let Ok(Some(event)) = tokio::time::timeout(ms(600), session.events.recv()).await {
        match event {
            TtsEvent::Audio { .. } => {
                assert!(!cancelled, "audio after Cancelled");
                audio_events += 1;
            }
            TtsEvent::Cancelled { .. } => cancelled = true,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(cancelled);
    assert!(
        audio_events <= 66,
        "{audio_events} audio events: the adapter's held events were not purged"
    );
}

/// The adapter stops reading the socket while its outgoing queue is full, so a consumer that
/// does not read bounds the memory (the server is held back by TCP flow control). Without the
/// gate the adapter would drain the whole socket into an unbounded queue.
#[tokio::test]
async fn a_consumer_that_does_not_read_holds_the_server_back_after_a_bounded_amount() {
    const FRAME: usize = 8000;
    // What the sockets alone absorb of data nobody reads depends on the operating system and
    // the machine: under 1000 frames on Windows loopback (a quarter of the script), and on Linux
    // up to the kernel's autotuned buffer maxima (1044 frames seen on a development machine,
    // `loopback_buffer_bytes` is ~10 MiB there; a GitHub runner's maxima add up to ~37 MiB, so
    // 4700 frames). The adapter's own queue and channel add 128 frames. The client's receive
    // buffer belongs to the adapter's socket, so the test cannot make it small (the tests that
    // own both ends do: websocket.rs, rtvi.rs). Instead the script is always more than twice the
    // bound, so an adapter that drained the socket into an unbounded queue (everything sent)
    // still fails it on every machine.
    let bound = if cfg!(target_os = "linux") {
        brisvox_sim::platform::loopback_buffer_bytes() / FRAME + 192
    } else {
        1000
    };
    // Words of 2 MB each (62.5 s at 16 kHz PCM16), at least 16 (32 MB).
    let word_bytes = 2_000_000_usize;
    let words_count = (2 * bound * FRAME).div_ceil(word_bytes).max(15) + 1;
    let total_frames = words_count * word_bytes / FRAME;
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = Duration::ZERO;
        c.frame_bytes = Some(FRAME);
        c.word_secs = 62.5;
    });
    let mut session = open(&mock).await;
    let words = vec!["w"; words_count].join(" ");
    session.input.send(text(1, &words)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    // Nobody reads. The server is blocked as soon as the adapter's queue (64), the channel
    // (64) and the socket buffers are full: the count plateaus, far below the total.
    eventually("the server to send something", || mock.frames_sent() > 100).await;
    let mut previous = 0;
    for _ in 0..40 {
        tokio::time::sleep(ms(150)).await;
        let now = mock.frames_sent();
        if now == previous {
            break;
        }
        previous = now;
    }
    let plateau = mock.frames_sent();
    tokio::time::sleep(ms(300)).await;
    assert_eq!(mock.frames_sent(), plateau, "the server is still sending");
    assert!(
        plateau < bound && bound * 2 < total_frames,
        "{plateau} of {total_frames} frames were sent to a consumer that reads nothing (bound \
         {bound}): the adapter's queue is not bounded"
    );
    // Nothing is lost once the consumer reads again.
    let utterance = until_flushed_within(&mut session.events, 1, 10_000).await;
    assert_eq!(utterance.audio.len(), words_count * word_bytes);
}

#[tokio::test]
async fn cancelling_a_queued_context_sends_nothing_and_cancelling_unknown_ones_still_acks() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(text(2, "bravo")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(2)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(2)
    ));
    session.cancel.send(ContextId(99)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(99)
    ));
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // A finished context can still be cancelled.
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(1)
    ));
    expect_silence(&mut session.events, 300).await;
    let labels = mock.labels();
    assert!(!labels.iter().any(|l| l == "recv:Clear"), "{labels:#?}");
    assert!(
        !labels.iter().any(|l| l.starts_with("recv:Speak:bravo")),
        "{labels:#?}"
    );
}

#[tokio::test]
async fn input_for_a_cancelled_context_is_ignored_even_if_it_arrives_later() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.cancel.send(ContextId(5)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { .. })
    ));
    session.input.send(text(5, "alpha")).await.unwrap();
    session.input.send(flush(5)).await.unwrap();
    session.input.send(text(6, "bravo")).await.unwrap();
    session.input.send(flush(6)).await.unwrap();
    let utterance = until_flushed(&mut session.events, 6).await;
    assert_eq!(runs(&utterance.audio), [('b', WORD)]);
    assert_eq!(mock.received_types(), ["Speak", "Flush"]);
}

#[tokio::test]
async fn a_cleared_that_never_comes_does_not_end_the_session() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(40);
        c.answer_clear = false;
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(500);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let started = tokio::time::Instant::now();
    loop {
        match next(&mut session.events, 4000).await.unwrap() {
            TtsEvent::Cancelled { .. }
            | TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            TtsEvent::Audio {
                context: ContextId(2),
                ..
            } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(
        started.elapsed() >= ms(400),
        "context 2 went out before the deadline for Cleared: {:?}",
        started.elapsed()
    );
    // The rest of context 2.
    loop {
        if let TtsEvent::Flushed { context } = next(&mut session.events, 2000).await.unwrap() {
            assert_eq!(context, ContextId(2));
            break;
        }
    }
}

#[tokio::test]
async fn the_cancel_ack_does_not_wait_for_a_stalled_reconnect() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    eventually("the connection", || mock.connections() == 1).await;
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    mock.inject(Fault::ConnectStall {
        duration: Duration::from_secs(3600),
    });
    session.input.send(text(1, "alpha")).await.unwrap();
    eventually("the reconnect attempt", || mock.connections() == 2).await;
    let started = tokio::time::Instant::now();
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 1000).await,
        Some(TtsEvent::Cancelled { .. })
    ));
    assert!(started.elapsed() < ms(300), "{:?}", started.elapsed());
}

// ---------------------------------------------------------------------------------------------
// Limits kept by the client
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_flush_budget_makes_a_turn_flush_wait_for_a_free_slot() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.max_flushes_per_window = 2;
    cfg.rate_window = ms(600);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    for ctx in 1..=3_u64 {
        session.input.send(text(ctx, "alpha")).await.unwrap();
        session.input.send(flush(ctx)).await.unwrap();
    }
    for ctx in 1..=3_u64 {
        until_flushed_within(&mut session.events, ctx, 3000).await;
    }
    let flushes: Vec<_> = mock
        .timeline()
        .into_iter()
        .filter(|(_, l)| l == "recv:Flush")
        .map(|(t, _)| t)
        .collect();
    assert_eq!(flushes.len(), 3);
    assert!(
        flushes[2].duration_since(flushes[0]) >= ms(550),
        "third flush {:?} after the first, inside the 600 ms window",
        flushes[2].duration_since(flushes[0])
    );
}

#[tokio::test]
async fn per_text_flushes_leave_a_reserve_for_the_turn_flush() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    cfg.max_flushes_per_window = 6;
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    for piece in ["alpha", " beta", " gamma"] {
        session.input.send(text(1, piece)).await.unwrap();
    }
    eventually("three Speak messages", || {
        mock.received_types()
            .iter()
            .filter(|t| *t == "Speak")
            .count()
            == 3
    })
    .await;
    tokio::time::sleep(ms(100)).await;
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush", "Speak"],
        "the third text is not flushed on its own: only the reserve is left"
    );
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush", "Speak", "Flush"]
    );
}

#[tokio::test]
async fn the_character_budget_delays_a_speak_until_old_characters_leave_the_window() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.chars_per_window = 20;
    cfg.max_speak_chars = 10;
    cfg.rate_window = ms(600);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session
        .input
        .send(text(1, "aaaa bbbb cccc dddd eeee"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let utterance = until_flushed_within(&mut session.events, 1, 3000).await;
    assert_eq!(utterance.audio.len(), 5 * WORD);
    let speaks: Vec<_> = mock
        .timeline()
        .into_iter()
        .filter(|(_, l)| l.starts_with("recv:Speak:"))
        .collect();
    assert_eq!(speaks.len(), 3, "{speaks:?}");
    let joined: String = speaks
        .iter()
        .map(|(_, l)| l.trim_start_matches("recv:Speak:"))
        .collect();
    assert_eq!(joined, "aaaa bbbb cccc dddd eeee");
    assert!(
        speaks[2].0.duration_since(speaks[0].0) >= ms(550),
        "the third piece went out {:?} after the first",
        speaks[2].0.duration_since(speaks[0].0)
    );
    assert!(speaks[1].0.duration_since(speaks[0].0) < ms(300));
}

/// The budget (2400 characters a minute at Deepgram) counts characters: text whose bytes exceed
/// the budget but whose characters fit is not delayed.
#[tokio::test]
async fn the_character_budget_counts_characters_not_bytes() {
    for text_of_pieces in [
        // 2 bytes per character.
        "\u{f1}\u{f1}\u{f1}\u{f1} \u{f1}\u{f1}\u{f1}\u{f1} \u{f1}\u{f1}\u{f1}\u{f1} \u{f1}\u{f1}\u{f1}\u{f1} \u{f1}\u{f1}\u{f1}\u{f1}",
        // 3 bytes per character.
        "\u{3042}\u{3042}\u{3042}\u{3042} \u{3044}\u{3044}\u{3044}\u{3044} \u{3046}\u{3046}\u{3046}\u{3046} \u{3048}\u{3048}\u{3048}\u{3048} \u{304a}\u{304a}\u{304a}\u{304a}",
    ] {
        let mock = Mock::start();
        let mut cfg = config(&mock);
        cfg.chars_per_window = 20;
        cfg.max_speak_chars = 10;
        cfg.rate_window = ms(600);
        let mut session = DeepgramAuraTts::new(cfg)
            .unwrap()
            .connect(tts_config())
            .await
            .unwrap();
        session.input.send(text(1, text_of_pieces)).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        let utterance = until_flushed_within(&mut session.events, 1, 3000).await;
        assert_eq!(utterance.audio.len(), 5 * WORD);
        let speaks: Vec<_> = mock
            .timeline()
            .into_iter()
            .filter(|(_, l)| l.starts_with("recv:Speak:"))
            .collect();
        // 10 + 10 characters fit the budget of 20 at once; the last 4 wait for the window.
        assert_eq!(speaks.len(), 3, "{speaks:?}");
        let joined: String = speaks
            .iter()
            .map(|(_, l)| l.trim_start_matches("recv:Speak:"))
            .collect();
        assert_eq!(joined, text_of_pieces);
        assert!(
            speaks[1].0.duration_since(speaks[0].0) < ms(300),
            "the second piece (10 characters) was delayed {:?}: bytes are counted, not characters",
            speaks[1].0.duration_since(speaks[0].0)
        );
        assert!(
            speaks[2].0.duration_since(speaks[0].0) >= ms(550),
            "the third piece went out {:?} after the first",
            speaks[2].0.duration_since(speaks[0].0)
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Deadlines, idle, reconnect
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_flush_the_server_swallows_is_a_timeout() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(on(
            "Flush",
            vec![json_text(
                &json!({"type": "Warning", "description": "cannot process more flushes", "code": "X"}),
            )],
        ));
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(500);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let started = tokio::time::Instant::now();
    let (error, ended) = first_error(&mut session).await;
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error}"
    );
    assert!(ended);
    assert!(started.elapsed() >= ms(450) && started.elapsed() < ms(1500));
}

#[tokio::test]
async fn a_pause_between_texts_longer_than_the_deadline_is_not_a_stall() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(500);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    // A slow LLM: nothing is owed, so nothing times out.
    tokio::time::sleep(ms(1100)).await;
    session.input.send(text(1, " beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let rest = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&rest.audio), [('b', WORD)]);
}

/// With `PerText`, once `Flushed` has come the server owes nothing and holds nothing: a slow
/// LLM (silence far above the deadline) is neither a timeout nor a reason to ping. This is the
/// "nothing is owed" case; the `PerTurn` test above is the probe path (unflushed text waits).
#[tokio::test]
async fn a_pause_after_flushed_under_per_text_is_neither_a_stall_nor_probed() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    cfg.io_timeout = ms(500);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    eventually("Flushed", || labels_contain(&mock, "send:Flushed")).await;
    tokio::time::sleep(ms(100)).await;
    // Far above the 500 ms deadline: no event (no Timeout error), no ping.
    expect_silence(&mut session.events, 1300).await;
    assert_eq!(mock.pings(), 0, "a ping for a server that owes nothing");
    // The session is alive and the same context goes on.
    session.input.send(text(1, " beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let rest = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&rest.audio), [('b', WORD)]);
    assert_eq!(mock.connections(), 1);
}

#[tokio::test]
async fn an_idle_close_by_the_server_is_not_a_failure_and_the_next_text_reconnects() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    expect_silence(&mut session.events, 100).await;
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    assert_eq!(mock.connections(), 2);
}

/// Only a plain close of an idle socket is benign. A protocol-class close (1008, 1009, 1003) is
/// an error even while nothing is owed: the session must not quietly reconnect and hit the same
/// limit again.
#[tokio::test]
async fn a_protocol_class_close_of_an_idle_socket_is_an_error_but_other_closes_are_not() {
    for (code, reason, protocol) in [
        (1008_u16, "DATA-0001", true),
        (1009, "BIG-0001", true),
        (1003, "MESSAGE-0000", true),
        (1011, "NET-0000", false),
        (1000, "", false),
    ] {
        let mock = Mock::start();
        let mut session = open(&mock).await;
        session.input.send(text(1, "alpha")).await.unwrap();
        session.input.send(flush(1)).await.unwrap();
        until_flushed(&mut session.events, 1).await;
        mock.kick_with_close(code, reason);
        if protocol {
            let (error, ended) = first_error(&mut session).await;
            assert!(ended, "{code}");
            assert!(
                matches!(&error, Error::Protocol { detail, .. }
                    if detail.contains(&code.to_string()) && detail.contains(reason)),
                "{code}: {error}"
            );
        } else {
            tokio::time::sleep(ms(200)).await;
            expect_silence(&mut session.events, 100).await;
            session.input.send(text(2, "bravo")).await.unwrap();
            session.input.send(flush(2)).await.unwrap();
            let second = until_flushed(&mut session.events, 2).await;
            assert_eq!(runs(&second.audio), [('b', WORD)], "{code}");
            assert_eq!(mock.connections(), 2, "{code}");
        }
    }
}

/// A connection that dies while a cancelled context's `Clear` is unanswered must not wedge the
/// next utterance behind a `Cleared` that can never come: the new connection starts clean.
#[tokio::test]
async fn a_connection_lost_while_a_clear_is_pending_does_not_hold_the_next_utterance() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(40);
        c.clear = ClearMode::KeepSending;
    });
    // The stall deadline (1.5 s) is what would rescue a wedged session.
    let mut session = open(&mock).await;
    session.input.send(text(1, TEN_WORDS)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { context, .. }) if context == ContextId(1)
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    eventually("the Clear", || labels_contain(&mock, "recv:Clear")).await;
    // `Cleared` is about 360 ms away: the connection ends first.
    assert!(!labels_contain(&mock, "send:Cleared"));
    mock.kick();
    tokio::time::sleep(ms(100)).await;
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let started = tokio::time::Instant::now();
    let mut audio = Vec::new();
    loop {
        match next(&mut session.events, 1000).await.unwrap() {
            // What the consumer had received before the cancel.
            TtsEvent::Cancelled {
                context: ContextId(1),
            }
            | TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            TtsEvent::Audio {
                context: ContextId(2),
                audio: a,
            } => audio.extend_from_slice(&a),
            TtsEvent::Flushed {
                context: ContextId(2),
            } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert_eq!(runs(&audio), [('b', WORD)]);
    assert!(
        started.elapsed() < ms(1000),
        "the next utterance waited for the stall deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(mock.connections(), 2);
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Clear", "Speak", "Flush"],
        "no second Clear for the dead connection"
    );
}

/// The server closed an idle socket and the session has not seen the close yet (its socket is
/// not being read because the consumer is behind). The first `Speak` of the next context fails
/// to write: exactly one retry on a new connection recovers it.
#[tokio::test]
async fn the_first_speak_of_a_context_is_retried_once_on_a_dead_socket() {
    let mock = Mock::start();
    let mut session = held_up_by_a_slow_consumer(&mock).await;
    // The server is gone (reset), and the session still believes in the socket.
    mock.kick_with_reset();
    // Context 2's `Speak` is released by the character budget after about a second and written
    // to the dead socket, while the session still does not read the socket (nobody consumes).
    tokio::time::sleep(ms(1200)).await;
    assert_eq!(
        mock.connections(),
        2,
        "the retry must already have happened"
    );
    // The consumer catches up.
    let first = until_flushed_within(&mut session.events, 1, 3000).await;
    assert_eq!(first.audio.len(), WORD_FRAMES * FRAME_BYTES);
    let second = until_flushed_within(&mut session.events, 2, 4000).await;
    assert_eq!(
        runs(&second.audio),
        [('b', WORD_FRAMES * FRAME_BYTES)],
        "the text was lost with the dead socket"
    );
    assert_eq!(mock.connections(), 2);
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush"],
        "context 2 reached the server once, on the new connection"
    );
}

/// The retry is a single one: a server that drops every connection ends the session with an
/// error instead of an endless reconnect loop.
#[tokio::test]
async fn a_server_that_drops_every_connection_gets_exactly_one_retry() {
    let mock = Mock::start();
    let mut session = held_up_by_a_slow_consumer(&mock).await;
    mock.kick_with_reset();
    // Every further connection is reset right after the upgrade.
    mock.configure(|c| c.reset_after_handshake = true);
    // The budget releases context 2's `Speak` after about a second: written to the dead socket.
    tokio::time::sleep(ms(1200)).await;
    assert_eq!(mock.connections(), 2, "one connection plus one retry");
    let first = until_flushed_within(&mut session.events, 1, 3000).await;
    assert_eq!(first.audio.len(), WORD_FRAMES * FRAME_BYTES);
    let (error, ended) = first_error(&mut session).await;
    assert!(matches!(error, Error::Disconnected { .. }), "{error}");
    assert!(ended);
    tokio::time::sleep(ms(300)).await;
    assert_eq!(mock.connections(), 2, "one connection plus one retry");
}

const FRAME_BYTES: usize = 100;
/// Audio frames of one mock word in [`held_up_by_a_slow_consumer`]. The session stops reading
/// the socket once 64 events wait in its queue and 64 in the events channel: 127 audio events
/// plus the `Flushed` of context 1 fill both exactly, with `Flushed` read just before the stop.
const WORD_FRAMES: usize = 127;

/// A session whose context 1 is fully answered and whose socket is no longer being read, with
/// context 2's first `Speak` waiting for the character budget (about 1 s):
/// the consumer has not read anything, so the adapter's queue is full (the `Flushed` of
/// context 1 is the 64th event in it).
async fn held_up_by_a_slow_consumer(mock: &Mock) -> TtsSession {
    mock.configure(|c| {
        c.interval = Duration::ZERO;
        c.frame_bytes = Some(FRAME_BYTES);
        // 16 kHz x 0.396875 s = 6350 samples = 12700 bytes = 127 frames.
        c.word_secs = 0.396_875;
    });
    let mut cfg = config(mock);
    cfg.max_speak_chars = 3;
    cfg.chars_per_window = 3;
    cfg.rate_window = ms(1000);
    let session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    // One character for context 1; "bbb" does not fit beside it for a second.
    session.input.send(text(1, "a")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(2, "bbb")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    eventually("the whole answer of context 1", || {
        mock.labels().iter().any(|l| l == "send:Flushed")
    })
    .await;
    // Give the session time to take the `Flushed` and stop reading.
    tokio::time::sleep(ms(200)).await;
    session
}

#[tokio::test]
async fn a_close_while_text_of_an_unfinished_utterance_is_on_the_server_is_a_disconnect() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    mock.kick();
    let (error, ended) = first_error(&mut session).await;
    assert!(matches!(error, Error::Disconnected { .. }), "{error}");
    assert!(ended);
}

#[tokio::test]
async fn a_connection_near_the_server_time_limit_is_replaced_between_utterances() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.max_connection_age = ms(300);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    assert_eq!(mock.connections(), 1);
    tokio::time::sleep(ms(400)).await;
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    assert_eq!(mock.connections(), 2);
}

#[tokio::test]
async fn a_slow_consumer_loses_nothing() {
    let mock = Mock::start();
    mock.configure(|c| c.frame_bytes = Some(400));
    let mut session = open(&mock).await;
    let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    session.input.send(text(1, words)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    tokio::time::sleep(ms(800)).await;
    let utterance = until_flushed(&mut session.events, 1).await;
    assert_eq!(utterance.audio.len(), 10 * WORD);
    let got = runs(&utterance.audio);
    assert_eq!(got.len(), 10);
    assert!(got.iter().all(|(_, n)| *n == WORD));
}

// ---------------------------------------------------------------------------------------------
// Warm sockets, shutdown, end of input
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn warm_parks_one_socket_that_the_next_matching_connect_takes() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.warm_target = Some(tts_config());
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    provider.warm().await.unwrap();
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1, "warm is idempotent");
    // The server's Metadata has arrived on the parked socket by now: it is still alive.
    tokio::time::sleep(ms(150)).await;
    let mut session = provider.connect(tts_config()).await.unwrap();
    assert_eq!(mock.connections(), 1, "the parked socket was used");
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // After a connect, warm prepares for the same voice and rate.
    drop(session);
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 2);
}

#[tokio::test]
async fn a_parked_socket_for_another_voice_or_a_stale_or_dead_one_is_not_used() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.warm_target = Some(tts_config());
    cfg.warm_max_age = ms(300);
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    // Another voice.
    provider.warm().await.unwrap();
    drop(
        provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from("voice-2"),
                language: None,
                model: None,
            })
            .await
            .unwrap(),
    );
    assert_eq!(mock.connections(), 2);
    // Stale (the latest connect was for voice-2, so warm now targets voice-2).
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 3);
    tokio::time::sleep(ms(400)).await;
    drop(
        provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from("voice-2"),
                language: None,
                model: None,
            })
            .await
            .unwrap(),
    );
    assert_eq!(mock.connections(), 4);
    // Dead.
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 5);
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    drop(
        provider
            .connect(TtsConfig {
                sample_rate: SampleRate::Hz16000,
                voice: Arc::from("voice-2"),
                language: None,
                model: None,
            })
            .await
            .unwrap(),
    );
    assert_eq!(mock.connections(), 6);
}

#[tokio::test]
async fn warm_without_a_target_or_a_session_does_nothing() {
    let mock = Mock::start();
    provider(&mock).warm().await.unwrap();
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn the_shutdown_token_ends_the_session_and_releases_a_parked_socket() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    let shutdown = cfg.shutdown.clone();
    cfg.warm_target = Some(tts_config());
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let mut session = provider.connect(tts_config()).await.unwrap();
    shutdown.cancel();
    assert!(next(&mut session.events, 1000).await.is_none());
    provider.warm().await.unwrap();
    assert_eq!(mock.connections(), 1, "nothing is warmed after shutdown");
}

#[tokio::test]
async fn dropping_the_input_after_the_flush_still_delivers_the_rest_and_closes() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(20));
    let session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let TtsSession { input, events, .. } = session;
    drop(input);
    let mut events = events;
    let utterance = until_flushed(&mut events, 1).await;
    assert_eq!(runs(&utterance.audio), [('a', WORD), ('b', WORD)]);
    assert!(next(&mut events, 2000).await.is_none(), "the stream ends");
    eventually("the Close message", || {
        mock.received_types().last().map(String::as_str) == Some("Close")
    })
    .await;
}

#[tokio::test]
async fn dropping_the_input_without_a_flush_flushes_silently_and_ends_the_stream() {
    let mock = Mock::start();
    let session = open(&mock).await;
    session.input.send(text(1, "alpha beta")).await.unwrap();
    session.input.send(text(2, "gamma")).await.unwrap();
    let TtsSession {
        input, mut events, ..
    } = session;
    drop(input);
    let mut audio = Vec::new();
    while let Some(event) = next(&mut events, 3000).await {
        match event {
            TtsEvent::Audio { audio: a, .. } => audio.extend_from_slice(&a),
            other => panic!(
                "no Flushed is made up for contexts the engine never flushed: {}",
                short(&other)
            ),
        }
    }
    assert_eq!(runs(&audio), [('a', WORD), ('b', WORD), ('g', WORD)]);
    // The stream ends before the mock has necessarily read the Close that follows it.
    eventually("the Close message", || {
        mock.received_types().last().map(String::as_str) == Some("Close")
    })
    .await;
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush", "Close"]
    );
}

#[tokio::test]
async fn a_server_that_never_answers_the_flush_after_the_input_was_dropped_is_a_timeout() {
    let mock = Mock::start();
    mock.configure(|c| c.responder = Some(on("Flush", vec![])));
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(400);
    let session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    let TtsSession {
        input, mut events, ..
    } = session;
    drop(input);
    loop {
        match next(&mut events, 3000).await {
            Some(TtsEvent::Audio { .. }) => {}
            Some(TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. }))) => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Review fixes: Clear only when the server may hold text, answered contexts, probes, shutdown
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_during_a_slow_reconnect_sends_no_clear_and_the_next_utterance_goes_out() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    eventually("the connection", || mock.connections() == 1).await;
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    // The reconnect completes (after 500 ms) but the text of context 1 never reaches the wire.
    mock.inject(Fault::ConnectStall { duration: ms(500) });
    session.input.send(text(1, "alpha")).await.unwrap();
    eventually("the reconnect attempt", || mock.connections() == 2).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 300).await,
        Some(TtsEvent::Cancelled { context }) if context == ContextId(1)
    ));
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let started = tokio::time::Instant::now();
    let second = until_flushed_within(&mut session.events, 2, 1400).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    assert!(
        started.elapsed() < ms(1000),
        "the next utterance was held back: {:?}",
        started.elapsed()
    );
    assert!(
        !labels_contain(&mock, "recv:Clear"),
        "a Clear for text that never left: {:#?}",
        mock.labels()
    );
    assert_eq!(mock.received_types(), ["Speak", "Flush"]);
}

#[tokio::test]
async fn an_idle_close_while_a_flushed_per_text_context_stays_open_is_not_a_failure() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    // `Flushed` has come: context 1 is open only because the engine has not flushed it (a slow
    // LLM), and the server owes nothing.
    tokio::time::sleep(ms(150)).await;
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    expect_silence(&mut session.events, 100).await;
    session.input.send(text(1, " bravo")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let rest = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&rest.audio), [('b', WORD)]);
    assert_eq!(mock.connections(), 2);
    assert_eq!(
        mock.received_types(),
        ["Speak", "Flush", "Speak", "Flush"],
        "nothing was sent twice"
    );
}

#[tokio::test]
async fn cancelling_a_context_the_server_has_fully_answered_sends_no_clear() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.flush_policy = FlushPolicy::PerText;
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    tokio::time::sleep(ms(150)).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 300).await,
        Some(TtsEvent::Cancelled { .. })
    ));
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let started = tokio::time::Instant::now();
    let second = until_flushed_within(&mut session.events, 2, 800).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    assert!(started.elapsed() < ms(600), "{:?}", started.elapsed());
    assert!(!labels_contain(&mock, "recv:Clear"), "{:#?}", mock.labels());
}

#[tokio::test]
async fn a_very_long_turn_is_flushed_midway_so_no_flush_interval_passes_the_request_limit() {
    let mock = Mock::start();
    let mut session = open(&mock).await;
    // 693 characters per text; the third would bring the unflushed total to 2079 (> 1800).
    let chunk = "abcdefghij ".repeat(63);
    assert_eq!(chunk.chars().count(), 693);
    for _ in 0..3 {
        session.input.send(text(1, &chunk)).await.unwrap();
    }
    session.input.send(flush(1)).await.unwrap();
    let all = until_flushed_within(&mut session.events, 1, 10_000).await;
    assert_eq!(runs(&all.audio), [('a', 3 * 63 * WORD)]);
    assert_eq!(
        mock.received_types(),
        ["Speak", "Speak", "Flush", "Speak", "Flush"],
        "a flush between the second and the third text, the turn flush at the end"
    );
    assert_flush_intervals_within(&mock, 1800);
    expect_silence(&mut session.events, 300).await;
}

/// The characters of the `Speak`s between consecutive `Flush`es (and after the last one).
fn flush_intervals(mock: &Mock) -> Vec<usize> {
    let mut intervals = Vec::new();
    let mut chars = 0_usize;
    for message in mock.received() {
        match message["type"].as_str() {
            Some("Speak") => chars += message["text"].as_str().unwrap_or("").chars().count(),
            Some("Flush") => intervals.push(std::mem::take(&mut chars)),
            _ => {}
        }
    }
    if chars > 0 {
        intervals.push(chars);
    }
    intervals
}

/// No flush interval carries more than `limit` characters (the UNVERIFIED 2000 character
/// request limit is what the adapter's 1800 keeps clear of).
fn assert_flush_intervals_within(mock: &Mock, limit: usize) {
    let intervals = flush_intervals(mock);
    assert!(
        intervals.iter().all(|n| *n <= limit),
        "flush intervals (characters) {intervals:?} exceed {limit}"
    );
}

/// Sends `first` and `second` as two texts of one turn and returns the message types the server
/// saw. Both policies are run by the callers.
async fn turn_of_two_texts(policy: FlushPolicy, first: &str, second: &str) -> (Mock, Vec<String>) {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(1));
    let mut cfg = config(&mock);
    cfg.flush_policy = policy;
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, first)).await.unwrap();
    session.input.send(text(1, second)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let _ = until_flushed_within(&mut session.events, 1, 5000).await;
    let types = mock.received_types();
    (mock, types)
}

/// Exactly 1800 unflushed characters are allowed, one more forces a flush before the text that
/// would pass it: the threshold is pinned from both sides, in characters (not bytes).
#[tokio::test]
async fn the_mid_turn_flush_threshold_is_1800_characters_not_bytes() {
    // (first, second, mid-turn flush expected)
    let cases = [
        ("a".repeat(1000), "b".repeat(800), false),
        ("a".repeat(1000), "b".repeat(801), true),
        // 3 bytes per character: byte counting would flush here.
        ("\u{3042}".repeat(1000), "\u{3044}".repeat(800), false),
        ("\u{3042}".repeat(1000), "\u{3044}".repeat(801), true),
        ("\u{f1}".repeat(1000), "\u{f3}".repeat(800), false),
    ];
    for (first, second, mid_flush) in cases {
        let (mock, types) = turn_of_two_texts(FlushPolicy::PerTurn, &first, &second).await;
        let expected: &[&str] = if mid_flush {
            &["Speak", "Flush", "Speak", "Flush"]
        } else {
            &["Speak", "Speak", "Flush"]
        };
        assert_eq!(
            types,
            expected,
            "{} + {} characters",
            first.chars().count(),
            second.chars().count()
        );
        assert_flush_intervals_within(&mock, 1800);
        // Never a single Speak above the 1000 character limit, whatever the alphabet.
        for message in mock.received() {
            if message["type"] == "Speak" {
                assert!(message["text"].as_str().unwrap().chars().count() <= 1000);
            }
        }
    }
}

/// `PerText` flushes after every text, so no interval comes near the limit either.
#[tokio::test]
async fn per_text_flush_intervals_stay_within_the_limit_too() {
    let (mock, types) =
        turn_of_two_texts(FlushPolicy::PerText, &"a".repeat(1000), &"b".repeat(1000)).await;
    assert_eq!(types, ["Speak", "Flush", "Speak", "Flush"]);
    assert_flush_intervals_within(&mock, 1000);
}

/// A single text above `max_speak_chars` characters is split by characters: 1500 Japanese
/// characters (4500 bytes) become two `Speak`s of at most 1000 characters.
#[tokio::test]
async fn long_non_ascii_text_is_split_by_characters() {
    let mock = Mock::start();
    mock.configure(|c| c.interval = ms(1));
    let mut session = open(&mock).await;
    let long = "\u{3042}".repeat(1500);
    session.input.send(text(1, &long)).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed_within(&mut session.events, 1, 5000).await;
    let counts: Vec<usize> = mock
        .received()
        .iter()
        .filter(|m| m["type"] == "Speak")
        .map(|m| m["text"].as_str().unwrap().chars().count())
        .collect();
    assert_eq!(counts, [1000, 500]);
}

#[tokio::test]
async fn a_live_server_is_probed_while_unflushed_text_waits_and_is_never_timed_out() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(600);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    // A slow LLM, far longer than the deadline: pings keep the deadline alive.
    expect_silence(&mut session.events, 1800).await;
    assert!(
        mock.pings() >= 3,
        "expected a ping every half deadline, got {}",
        mock.pings()
    );
    session.input.send(text(1, " beta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let rest = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&rest.audio), [('b', WORD)]);
}

#[tokio::test]
async fn a_server_that_goes_silent_with_unflushed_text_is_a_timeout_at_the_deadline() {
    let mock = Mock::start();
    // The mock freezes before its second audio frame: no data, no pong.
    mock.inject(Fault::Stall {
        before_event: 1,
        duration: Duration::from_secs(3600),
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(600);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha beta")).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    let started = tokio::time::Instant::now();
    let (error, ended) = first_error(&mut session).await;
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error}"
    );
    assert!(ended);
    assert!(
        started.elapsed() >= ms(500) && started.elapsed() < ms(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_shutdown_token_ends_a_session_stuck_in_a_reconnect_at_once() {
    let mock = Mock::start();
    let cfg = config(&mock);
    let shutdown = cfg.shutdown.clone();
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    eventually("the connection", || mock.connections() == 1).await;
    mock.kick();
    tokio::time::sleep(ms(200)).await;
    mock.inject(Fault::ConnectStall {
        duration: Duration::from_secs(3600),
    });
    session.input.send(text(1, "alpha")).await.unwrap();
    eventually("the reconnect attempt", || mock.connections() == 2).await;
    let started = tokio::time::Instant::now();
    shutdown.cancel();
    // The end of the stream, not a timeout error after the 1.5 s deadline.
    assert!(next(&mut session.events, 1000).await.is_none());
    assert!(started.elapsed() < ms(500), "{:?}", started.elapsed());
}

#[tokio::test]
async fn an_unbounded_rate_window_does_not_panic_the_session() {
    let mock = Mock::start();
    let mut cfg = config(&mock);
    cfg.rate_window = Duration::MAX;
    cfg.max_flushes_per_window = 1;
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session.events, 1).await;
    // The only flush of the (endless) window is used: the next flush waits, forever, quietly.
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { context, .. }) if context == ContextId(2)
    ));
    expect_silence(&mut session.events, 400).await;
    assert_eq!(mock.received_types(), ["Speak", "Flush", "Speak"]);
}

#[tokio::test]
async fn audio_after_flushed_with_nothing_in_flight_is_dropped() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.responder = Some(on(
            "Flush",
            vec![
                json_text(&json!({"type": "Flushed", "sequence_id": 0})),
                Out::Binary(vec![b'z'; 640]),
            ],
        ));
    });
    let mut session = open(&mock).await;
    session.input.send(text(1, "alpha")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let first = until_flushed(&mut session.events, 1).await;
    assert_eq!(runs(&first.audio), [('a', WORD)]);
    // The stray frame arrives after the context ended: nobody owns it.
    expect_silence(&mut session.events, 300).await;
    session.input.send(text(2, "bravo")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let second = until_flushed(&mut session.events, 2).await;
    assert_eq!(runs(&second.audio), [('b', WORD)]);
    expect_silence(&mut session.events, 300).await;
}

#[tokio::test]
async fn a_flushed_that_follows_cleared_while_nothing_is_owed_is_ignored() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.interval = ms(40);
        c.responder = Some(on(
            "Clear",
            vec![
                json_text(&json!({"type": "Cleared", "sequence_id": 0})),
                json_text(&json!({"type": "Flushed", "sequence_id": 1})),
            ],
        ));
    });
    let mut session = open(&mock).await;
    session
        .input
        .send(text(1, "alpha beta gamma delta"))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    session.input.send(text(2, "bravo")).await.unwrap();
    // Context 2 is not flushed yet: the stray `Flushed` finds nothing owed.
    loop {
        match next(&mut session.events, 2000).await.unwrap() {
            TtsEvent::Cancelled { .. }
            | TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            TtsEvent::Audio {
                context: ContextId(2),
                audio,
            } => {
                assert_eq!(runs(&audio), [('b', WORD)]);
                break;
            }
            other => panic!("unexpected {}", short(&other)),
        }
    }
    expect_silence(&mut session.events, 300).await;
    session.input.send(flush(2)).await.unwrap();
    until_flushed(&mut session.events, 2).await;
    expect_silence(&mut session.events, 200).await;
}

#[tokio::test]
async fn a_second_cancel_works_after_a_cleared_was_given_up_on() {
    let mock = Mock::start();
    let clears = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&clears);
    mock.configure(|c| {
        c.interval = ms(40);
        // The first Clear is never answered; later ones are (default behaviour).
        c.responder = Some(Arc::new(move |request: &Value| {
            (request["type"] == "Clear"
                && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0)
                .then(Vec::new)
        }));
    });
    let mut cfg = config(&mock);
    cfg.io_timeout = ms(500);
    let mut session = DeepgramAuraTts::new(cfg)
        .unwrap()
        .connect(tts_config())
        .await
        .unwrap();
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut session.events, 2000).await,
        Some(TtsEvent::Audio { .. })
    ));
    session.cancel.send(ContextId(1)).await.unwrap();
    // Context 2 goes out after the deadline for the lost Cleared, then is cancelled itself.
    session
        .input
        .send(text(2, "bravo charlie delta echo foxtrot"))
        .await
        .unwrap();
    loop {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Cancelled { .. }
            | TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            TtsEvent::Audio {
                context: ContextId(2),
                ..
            } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    session.cancel.send(ContextId(2)).await.unwrap();
    session.input.send(text(3, "golf")).await.unwrap();
    session.input.send(flush(3)).await.unwrap();
    // Context 2's tail and the rest are dropped; context 3 plays clean once the second Clear
    // was answered.
    let mut audio = Vec::new();
    loop {
        match next(&mut session.events, 3000).await.unwrap() {
            TtsEvent::Cancelled { context } => assert_eq!(context, ContextId(2)),
            TtsEvent::Audio {
                context: ContextId(2),
                ..
            } => {}
            TtsEvent::Audio {
                context: ContextId(3),
                audio: data,
            } => audio.extend_from_slice(&data),
            TtsEvent::Flushed { context } => {
                assert_eq!(context, ContextId(3));
                break;
            }
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert_eq!(runs(&audio), [('g', WORD)]);
    assert_eq!(clears.load(std::sync::atomic::Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1, DEEPGRAM_API_KEY, optional DEEPGRAM_AURA_MODEL)
// ---------------------------------------------------------------------------------------------

/// A short real session: two sentences and a flush, a barge-in and a second utterance. Prints
/// measurements for the open items of docs/providers/deepgram-aura.md; never prints the key.
#[tokio::test]
async fn live_deepgram_aura_session() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 (and DEEPGRAM_API_KEY)");
        return;
    }
    let Ok(key) = std::env::var("DEEPGRAM_API_KEY") else {
        eprintln!("skipped: DEEPGRAM_API_KEY must be set");
        return;
    };
    let model = std::env::var("DEEPGRAM_AURA_MODEL")
        .unwrap_or_else(|_| brisvox_providers::deepgram_aura::DEFAULT_MODEL.to_owned());
    let cfg = DeepgramAuraConfig::new(Secret::new(key), clock());
    let provider = DeepgramAuraTts::new(cfg).unwrap();
    let config = TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from(model.as_str()),
        language: None,
        model: None,
    };
    let mut session = provider.connect(config).await.unwrap();

    // Utterance 0: text WITHOUT a flush (the `PerTurn` default relies on the server generating
    // on its own, open item 3 of the provider doc) held for a few seconds, which also shows
    // whether the server answers the liveness `Ping`s (a failure here would be a Timeout).
    let probe_started = tokio::time::Instant::now();
    session
        .input
        .send(text(
            10,
            "This is a sentence that is complete without any flush.",
        ))
        .await
        .unwrap();
    let auto = tokio::time::timeout(Duration::from_secs(6), session.events.recv()).await;
    match auto {
        Ok(Some(TtsEvent::Audio { .. })) => eprintln!(
            "live: audio without a Flush after {:?} (PerTurn default is sound)",
            probe_started.elapsed()
        ),
        Err(_) => eprintln!(
            "live: NO audio within 6 s without a Flush: the PerTurn default needs FlushPolicy::PerText"
        ),
        Ok(other) => panic!("live: unexpected {}", short(&other)),
    }
    session.input.send(flush(10)).await.unwrap();
    until_flushed_within(&mut session.events, 10, 20_000).await;

    // Utterance 1: two chunks (the second starts with a space), then the flush.
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
            TtsEvent::Flushed { .. } => break,
            other => panic!("live: unexpected {}", short(&other)),
        }
    }
    let audio_secs = audio_bytes as f64 / (24_000.0 * 2.0);
    eprintln!(
        "live: first audio {first_audio:?}, {audio_secs:.2} s of audio, flushed after {:?}",
        started.elapsed()
    );
    assert!(audio_bytes > 0 && audio_bytes.is_multiple_of(2));

    // Utterance 2: barge in after the first audio.
    session
        .input
        .send(text(
            2,
            "This sentence is long enough that we can interrupt it while it is still being generated and played.",
        ))
        .await
        .unwrap();
    session.input.send(flush(2)).await.unwrap();
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

    // Utterance 3 works after the barge-in (it waits for the server's Cleared first).
    let resumed = tokio::time::Instant::now();
    session
        .input
        .send(text(3, "And we are back."))
        .await
        .unwrap();
    session.input.send(flush(3)).await.unwrap();
    until_flushed_within(&mut session.events, 3, 20_000).await;
    eprintln!(
        "live: utterance after the barge-in took {:?}",
        resumed.elapsed()
    );
}
