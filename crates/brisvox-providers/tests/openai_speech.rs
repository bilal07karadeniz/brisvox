//! OpenAI-compatible speech adapter tests: the provider contract suite and
//! protocol-level checks against a local HTTP mock of `POST /v1/audio/speech`, plus a live test
//! gated on `BRISVOX_LIVE_TESTS=1` and `LOCAL_TTS_BASE_URL`.
//!
//! The sockets are real, so these tests use the real system clock and short deadlines instead
//! of paused time (the `disallowed_methods` lint guards engine logic, not test pacing).
#![cfg(feature = "provider-openai-speech")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    missing_docs
)]

#[path = "openai_speech/mock.rs"]
mod mock;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::openai_speech::{OpenAiSpeechConfig, OpenAiSpeechTts, Preset};
use brisvox_providers::{ContextId, Error, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession};
use brisvox_sim::contract::{CONTRACT_TIMEOUT, Fault, TtsHarness, tts_contract};
use bytes::Bytes;
use mock::{Mock, Mode};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Bytes of one mock word (the mock default): 0.1 s at 24 kHz.
const WORD: usize = 4_800;

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn config(mock: &Mock) -> OpenAiSpeechConfig {
    let mut config = OpenAiSpeechConfig::preset(Preset::Kokoro, mock.base_url.clone(), clock());
    config.first_audio_timeout = ms(1500);
    config.idle_timeout = ms(1500);
    config.connect_timeout = ms(1000);
    // The model warm-up (Kokoro preset default) has its own tests; off keeps request counts exact.
    config.warm_text.clear();
    config
}

fn provider(mock: &Mock) -> OpenAiSpeechTts {
    OpenAiSpeechTts::new(config(mock)).unwrap()
}

fn tts_config() -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from("af_heart"),
        language: None,
        model: None,
    }
}

async fn open(provider: &OpenAiSpeechTts) -> TtsSession {
    provider.connect(tts_config()).await.unwrap()
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

fn short(event: &TtsEvent) -> String {
    match event {
        TtsEvent::Audio { context, audio } => format!("Audio({context:?}, {} bytes)", audio.len()),
        other => format!("{other:?}"),
    }
}

async fn next(session: &mut TtsSession) -> TtsEvent {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("no event within 5 s")
        .expect("the event stream ended")
}

/// Audio of `context` until its `Flushed`, as the chunks received.
async fn until_flushed(session: &mut TtsSession, context: u64) -> Vec<Bytes> {
    let mut audio = Vec::new();
    loop {
        match next(session).await {
            TtsEvent::Audio {
                context: c,
                audio: a,
            } if c == ContextId(context) => audio.push(a),
            TtsEvent::Flushed { context: c } if c == ContextId(context) => return audio,
            other => panic!("unexpected {}", short(&other)),
        }
    }
}

fn total(chunks: &[Bytes]) -> usize {
    chunks.iter().map(Bytes::len).sum()
}

/// The values of the bytes, run-length encoded: `[(fill, count)]`.
fn runs(chunks: &[Bytes]) -> Vec<(u8, usize)> {
    let mut out: Vec<(u8, usize)> = Vec::new();
    for byte in chunks.iter().flat_map(|c| c.iter().copied()) {
        match out.last_mut() {
            Some((value, count)) if *value == byte => *count += 1,
            _ => out.push((byte, 1)),
        }
    }
    out
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(ms(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite
// ---------------------------------------------------------------------------------------------

struct Harness {
    mock: Mock,
    provider: Arc<OpenAiSpeechTts>,
    clock: SharedClock,
}

impl Harness {
    fn new() -> Self {
        let mock = Mock::start();
        // The suite asks for 16 kHz: the Speaches preset sends `sample_rate` so any rate is fine.
        let mut cfg = OpenAiSpeechConfig::preset(Preset::Speaches, mock.base_url.clone(), clock());
        cfg.first_audio_timeout = CONTRACT_TIMEOUT;
        cfg.idle_timeout = CONTRACT_TIMEOUT;
        cfg.connect_timeout = CONTRACT_TIMEOUT;
        let provider = Arc::new(OpenAiSpeechTts::new(cfg).unwrap());
        mock.configure(|c| c.chunk_delay = ms(20));
        Self {
            mock,
            provider,
            clock: clock(),
        }
    }
}

impl TtsHarness for Harness {
    type P = OpenAiSpeechTts;

    fn provider(&self) -> Arc<OpenAiSpeechTts> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn emits_words(&self) -> bool {
        false
    }

    fn faults_at_connect(&self) -> bool {
        false
    }

    fn reset(&self) {
        self.mock.reset();
        // A paced response, so that a cancel is observable (the cancel check sends ten words).
        self.mock.configure(|c| c.chunk_delay = ms(60));
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

#[test]
fn passes_the_provider_contract_suite() {
    on_big_stack(async {
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
    });
}

// ---------------------------------------------------------------------------------------------
// Request shape, auth, configuration
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_request_is_a_json_post_to_audio_speech_with_the_preset_fields() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "Hello there.")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let audio = until_flushed(&mut session, 1).await;
    assert_eq!(runs(&audio), [(1, WORD), (2, WORD)]);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/audio/speech");
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("authorization"), None, "no key, no header");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(
        body,
        json!({"model":"kokoro","input":"Hello there.","voice":"af_heart",
               "response_format":"pcm","stream":true})
    );
}

#[tokio::test]
async fn the_api_key_goes_in_a_bearer_header_only() {
    let mock = Mock::start();
    mock.configure(|c| c.require_auth = Some("Bearer sk-local-test".to_owned()));
    let mut cfg = config(&mock);
    cfg.api_key = Some(Secret::new("sk-local-test".to_owned()));
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    assert!(!format!("{provider:?}").contains("sk-local-test"));
    let mut session = open(&provider).await;
    session.input.send(text(1, "hi")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session, 1).await;
    let requests = mock.requests();
    assert!(!requests[0].path.contains("sk-local-test"));
    assert!(!requests[0].body.contains("sk-local-test"));

    // A wrong key is a 401 event, and the session ends.
    let mut cfg = config(&mock);
    cfg.api_key = Some(Secret::new("wrong".to_owned()));
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session.input.send(text(2, "hi")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Error(Error::Rejected { status: 401, .. }) => {}
        other => panic!("unexpected {}", short(&other)),
    }
    assert!(session.events.recv().await.is_none());
}

#[tokio::test]
async fn the_session_rate_must_match_the_server_rate_unless_the_preset_sends_it() {
    let mock = Mock::start();
    let kokoro = provider(&mock);
    let at_16k = TtsConfig {
        sample_rate: SampleRate::Hz16000,
        voice: Arc::from("af_heart"),
        language: None,
        model: None,
    };
    assert!(matches!(
        kokoro.connect(at_16k.clone()).await,
        Err(Error::Protocol { .. })
    ));
    let empty_voice = TtsConfig {
        voice: Arc::from(" "),
        ..tts_config()
    };
    assert!(matches!(
        kokoro.connect(empty_voice).await,
        Err(Error::Protocol { .. })
    ));
    let speaches = OpenAiSpeechTts::new(OpenAiSpeechConfig::preset(
        Preset::Speaches,
        mock.base_url.clone(),
        clock(),
    ))
    .unwrap();
    let mut session = speaches.connect(at_16k).await.unwrap();
    session.input.send(text(1, "hi")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session, 1).await;
    let body: Value = serde_json::from_str(&mock.requests()[0].body).unwrap();
    assert_eq!(body["sample_rate"], json!(16_000));
    assert_eq!(body["model"], json!("speaches-ai/Kokoro-82M-v1.0-ONNX"));
}

// ---------------------------------------------------------------------------------------------
// Streaming, ordering, joining, reuse
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn audio_is_forwarded_as_the_body_arrives_not_when_it_ends() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(100));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    let started = tokio::time::Instant::now();
    session
        .input
        .send(text(1, "one two three four five"))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let first = match next(&mut session).await {
        TtsEvent::Audio { audio, .. } => audio,
        other => panic!("unexpected {}", short(&other)),
    };
    let first_at = started.elapsed();
    assert_eq!(first.len(), WORD);
    assert!(first_at < ms(350), "first audio after {first_at:?}");
    let rest = until_flushed(&mut session, 1).await;
    assert!(
        started.elapsed() >= ms(450),
        "the body took {:?}",
        started.elapsed()
    );
    assert_eq!(rest.len(), 4);
    assert_eq!(runs(&rest), [(2, WORD), (3, WORD), (4, WORD), (5, WORD)]);
}

#[tokio::test]
async fn pieces_that_wait_for_a_busy_server_are_joined_into_one_request() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(80));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session
        .input
        .send(text(1, "alpha beta gamma"))
        .await
        .unwrap();
    // The first request is in flight; these three pile up behind it.
    match next(&mut session).await {
        TtsEvent::Audio { .. } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    session.input.send(text(1, " delta")).await.unwrap();
    session.input.send(text(1, " epsilon")).await.unwrap();
    session.input.send(text(1, " zeta")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let rest = until_flushed(&mut session, 1).await;
    let inputs: Vec<String> = mock.requests().iter().map(mock::Recorded::input).collect();
    assert_eq!(inputs, ["alpha beta gamma", " delta epsilon zeta"]);
    // 2 words left of the first request + 3 of the second.
    assert_eq!(total(&rest), 5 * WORD);
}

#[tokio::test]
async fn joining_can_be_turned_off_and_never_crosses_contexts() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(60));
    let mut cfg = config(&mock);
    cfg.coalesce_chars = 0;
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b")).await.unwrap();
    session.input.send(text(1, " c")).await.unwrap();
    session.input.send(text(1, " d")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session, 1).await;
    assert_eq!(mock.requests().len(), 3);

    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(60));
    let provider = provider_for(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(2, "c")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    // Context 1 first, then context 2: one request each, audio in order, never interleaved.
    let one = until_flushed(&mut session, 1).await;
    let two = until_flushed(&mut session, 2).await;
    assert_eq!(total(&one), 2 * WORD);
    assert_eq!(total(&two), WORD);
    assert_eq!(mock.requests().len(), 2);
}

fn provider_for(mock: &Mock) -> OpenAiSpeechTts {
    provider(mock)
}

#[tokio::test]
async fn requests_reuse_one_connection_and_warm_opens_it() {
    let mock = Mock::start();
    let provider = provider(&mock);
    provider.warm().await.unwrap();
    assert_eq!(mock.accepted(), 1, "warm opened the connection");
    assert_eq!(mock.requests()[0].method, "HEAD");
    assert_eq!(mock.requests()[0].header("authorization"), None);
    let mut session = open(&provider).await;
    for context in 1..=3 {
        session.input.send(text(context, "one two")).await.unwrap();
        session.input.send(flush(context)).await.unwrap();
        until_flushed(&mut session, context).await;
    }
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(mock.accepted(), 1, "every request used the warm connection");
    // A second session of the same provider shares the pool too.
    let mut second = open(&provider).await;
    second.input.send(text(9, "again")).await.unwrap();
    second.input.send(flush(9)).await.unwrap();
    until_flushed(&mut second, 9).await;
    assert_eq!(mock.accepted(), 1);
}

// ---------------------------------------------------------------------------------------------
// Model warm-up
// ---------------------------------------------------------------------------------------------

fn warm_provider(mock: &Mock, change: impl FnOnce(&mut OpenAiSpeechConfig)) -> OpenAiSpeechTts {
    let mut cfg = config(mock);
    cfg.warm_text = "Hi.".to_owned();
    cfg.warm_voice = Some("af_heart".to_owned());
    change(&mut cfg);
    OpenAiSpeechTts::new(cfg).unwrap()
}

fn posts(mock: &Mock) -> Vec<mock::Recorded> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "POST")
        .collect()
}

/// Waits (real time, bounded) until the mock has seen `n` POSTs.
async fn wait_for_posts(mock: &Mock, n: usize) {
    for _ in 0..200 {
        if posts(mock).len() >= n {
            return;
        }
        tokio::time::sleep(ms(10)).await;
    }
    panic!("the mock saw {} POSTs, wanted {n}", posts(mock).len());
}

#[test]
fn the_kokoro_preset_warms_the_model_and_the_others_do_not() {
    let kokoro = OpenAiSpeechConfig::preset(Preset::Kokoro, "http://127.0.0.1:1/v1", clock());
    assert_eq!(kokoro.warm_text, "Hi.");
    assert_eq!(kokoro.warm_voice.as_deref(), Some("af_heart"));
    assert_eq!(kokoro.warm_interval, Duration::from_secs(60));
    for preset in [
        Preset::Generic,
        Preset::Chatterbox,
        Preset::Speaches,
        Preset::VllmOmni,
    ] {
        let c = OpenAiSpeechConfig::preset(preset, "http://127.0.0.1:1/v1", clock());
        assert!(c.warm_text.is_empty(), "{preset:?}");
    }
    let mut long = kokoro;
    long.warm_text = "x".repeat(201);
    assert!(OpenAiSpeechTts::new(long).is_err());
}

#[tokio::test]
async fn warm_returns_at_once_and_the_synthesis_runs_in_the_background() {
    let mock = Mock::start();
    // The synthesis takes 5 s; warm() must not wait for it (the engine gives it 400 ms). The
    // bound below is far from both numbers so that a loaded machine cannot flip it.
    mock.configure(|c| c.chunk_delay = ms(5_000));
    let provider = warm_provider(&mock, |_| {});
    let started = std::time::Instant::now();
    provider.warm().await.unwrap();
    assert!(
        started.elapsed() < ms(3_000),
        "warm waited {:?}",
        started.elapsed()
    );
    wait_for_posts(&mock, 1).await;
    let post = &posts(&mock)[0];
    assert_eq!(post.path, "/v1/audio/speech");
    let body: Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!(body["input"], json!("Hi."));
    assert_eq!(body["voice"], json!("af_heart"));
    assert_eq!(body["model"], json!("kokoro"));
    assert_eq!(body["response_format"], json!("pcm"));
}

#[tokio::test]
async fn warm_ups_are_one_at_a_time_and_spaced_by_the_interval() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(300));
    // Interval 1 s (real clock): the calls right away are inside it, the one after the longer
    // sleep below is after it (sleeps only ever run long, so this cannot flip on a slow machine).
    let provider = warm_provider(&mock, |c| c.warm_interval = ms(1_000));
    for _ in 0..3 {
        provider.warm().await.unwrap();
    }
    wait_for_posts(&mock, 1).await;
    tokio::time::sleep(ms(100)).await;
    assert_eq!(posts(&mock).len(), 1, "one in flight, the rest skipped");
    tokio::time::sleep(ms(1_050)).await;
    provider.warm().await.unwrap();
    wait_for_posts(&mock, 2).await;
    // Many sessions starting together do not each fire one.
    for _ in 0..5 {
        provider.warm().await.unwrap();
    }
    tokio::time::sleep(ms(150)).await;
    assert_eq!(posts(&mock).len(), 2);
}

#[tokio::test]
async fn the_warm_up_uses_the_latest_sessions_voice_and_empty_text_turns_it_off() {
    let mock = Mock::start();
    let provider = warm_provider(&mock, |c| c.warm_voice = None);
    // No voice known yet: nothing to warm with.
    provider.warm().await.unwrap();
    tokio::time::sleep(ms(100)).await;
    assert!(posts(&mock).is_empty());
    let _session = provider
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz24000,
            voice: Arc::from("bf_emma"),
            language: None,
            model: None,
        })
        .await
        .unwrap();
    provider.warm().await.unwrap();
    wait_for_posts(&mock, 1).await;
    let body: Value = serde_json::from_str(&posts(&mock)[0].body).unwrap();
    assert_eq!(body["voice"], json!("bf_emma"));

    let off_mock = Mock::start();
    let off = warm_provider(&off_mock, |c| c.warm_text.clear());
    off.warm().await.unwrap();
    tokio::time::sleep(ms(100)).await;
    assert!(posts(&off_mock).is_empty());
}

#[tokio::test]
async fn a_failed_warm_up_is_silent_and_retried_by_the_next_warm() {
    let mock = Mock::start();
    mock.configure(|c| c.refuse = vec![("Hi.".to_owned(), 500)]);
    let provider = warm_provider(&mock, |_| {});
    provider.warm().await.unwrap();
    wait_for_posts(&mock, 1).await;
    tokio::time::sleep(ms(100)).await;
    // The interval (60 s) has not passed, but a failure does not use it up.
    provider.warm().await.unwrap();
    wait_for_posts(&mock, 2).await;
    // And the provider still works for real requests.
    mock.configure(|c| c.refuse.clear());
    let mut session = open(&provider).await;
    session.input.send(text(1, "Hello there.")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    until_flushed(&mut session, 1).await;
}

#[tokio::test]
async fn flush_waits_for_every_earlier_piece_and_needs_no_text() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(30));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    // A flush of a context that never had text is answered at once.
    session.input.send(flush(5)).await.unwrap();
    assert!(matches!(
        next(&mut session).await,
        TtsEvent::Flushed {
            context: ContextId(5)
        }
    ));
    // Whitespace alone is never sent to the server.
    session.input.send(text(6, "  \n")).await.unwrap();
    session.input.send(flush(6)).await.unwrap();
    assert!(matches!(
        next(&mut session).await,
        TtsEvent::Flushed {
            context: ContextId(6)
        }
    ));
    assert!(mock.requests().is_empty());
    // Text then flush: all the audio comes first.
    session.input.send(text(7, "x y z")).await.unwrap();
    session.input.send(flush(7)).await.unwrap();
    assert_eq!(total(&until_flushed(&mut session, 7).await), 3 * WORD);
}

#[tokio::test]
async fn closing_the_input_still_delivers_everything_sent_and_ends_the_stream() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let TtsSession {
        input,
        cancel,
        mut events,
    } = open(&provider).await;
    input.send(text(1, "a b c")).await.unwrap();
    input.send(flush(1)).await.unwrap();
    drop(input);
    let mut bytes = 0;
    let mut flushed = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("hang")
    {
        match event {
            TtsEvent::Audio { audio, .. } => bytes += audio.len(),
            TtsEvent::Flushed { .. } => flushed = true,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(flushed);
    assert_eq!(bytes, 3 * WORD);
    drop(cancel);
}

// ---------------------------------------------------------------------------------------------
// Audio shape
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn audio_events_hold_whole_samples_whatever_the_chunking() {
    let mock = Mock::start();
    // Odd chunk sizes: a sample straddles every chunk boundary.
    mock.configure(|c| {
        c.chunk_bytes = 4_801;
        c.chunk_delay = ms(5);
    });
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b c d e f")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let audio = until_flushed(&mut session, 1).await;
    assert!(audio.iter().all(|a| a.len() % 2 == 0 && !a.is_empty()));
    assert_eq!(
        total(&audio),
        6 * 4_801,
        "nothing is lost at the chunk boundaries"
    );
    // Order and content survive the re-cutting.
    let values: Vec<u8> = runs(&audio).iter().map(|(v, _)| *v).collect();
    assert_eq!(values, [1, 2, 3, 4, 5, 6]);
}

#[tokio::test]
async fn a_server_that_answers_with_the_whole_segment_at_once_is_forwarded() {
    let mock = Mock::start();
    mock.configure(|c| c.mode = Mode::Whole);
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b c")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert_eq!(total(&until_flushed(&mut session, 1).await), 3 * WORD);
}

#[tokio::test]
async fn wav_responses_are_read_through_their_header() {
    let mock = Mock::start();
    // The header arrives a byte at a time.
    mock.configure(|c| {
        c.mode = Mode::Wav {
            rate: 24_000,
            header_piece: 1,
        }
    });
    let mut cfg = OpenAiSpeechConfig::preset(Preset::Chatterbox, mock.base_url.clone(), clock());
    cfg.first_audio_timeout = ms(1500);
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let audio = until_flushed(&mut session, 1).await;
    assert_eq!(runs(&audio), [(1, WORD), (2, WORD)]);
    let body: Value = serde_json::from_str(&mock.requests()[0].body).unwrap();
    assert_eq!(body["response_format"], json!("wav"));
    assert_eq!(body["model"], json!("chatterbox"));
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn a_wav_at_the_wrong_rate_is_a_protocol_error() {
    let mock = Mock::start();
    mock.configure(|c| {
        c.mode = Mode::Wav {
            rate: 22_050,
            header_piece: 64,
        }
    });
    let cfg = OpenAiSpeechConfig::preset(Preset::Chatterbox, mock.base_url.clone(), clock());
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Error(Error::Protocol { detail, .. }) => {
            assert!(detail.contains("22050"), "{detail}");
        }
        other => panic!("unexpected {}", short(&other)),
    }
    assert!(session.events.recv().await.is_none());
}

// ---------------------------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_aborts_the_request_in_flight_and_the_session_goes_on() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(100));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session
        .input
        .send(text(1, "a b c d e f g h i j"))
        .await
        .unwrap();
    match next(&mut session).await {
        TtsEvent::Audio { .. } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    let cancelled_at = tokio::time::Instant::now();
    session.cancel.send(ContextId(1)).await.unwrap();
    // Cancelled comes at once, before the rest of the body could have.
    loop {
        match next(&mut session).await {
            TtsEvent::Cancelled {
                context: ContextId(1),
            } => break,
            TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(cancelled_at.elapsed() < ms(300));
    wait_for("the server to see the abort", || mock.aborted() == 1).await;
    assert_eq!(mock.completed(), 0);
    // Nothing of the cancelled context arrives afterwards, and input for it is ignored.
    session.input.send(text(1, "late words")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    session.input.send(text(2, "ok now")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    let audio = until_flushed(&mut session, 2).await;
    assert_eq!(total(&audio), 2 * WORD);
    assert_eq!(mock.requests().len(), 2, "the late text was never sent");
}

#[tokio::test]
async fn cancel_while_the_server_has_not_answered_yet_drops_the_request() {
    let mock = Mock::start();
    mock.inject(Fault::ConnectStall {
        duration: Duration::from_secs(30),
    });
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "slow start")).await.unwrap();
    wait_for("the request", || !mock.requests().is_empty()).await;
    session.cancel.send(ContextId(1)).await.unwrap();
    assert!(matches!(
        next(&mut session).await,
        TtsEvent::Cancelled {
            context: ContextId(1)
        }
    ));
    wait_for("the abort", || mock.aborted() == 1).await;
}

#[tokio::test]
async fn cancel_forgets_the_queued_text_of_the_context_and_keeps_the_others() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(100));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b c d")).await.unwrap();
    session.input.send(text(2, "keep me")).await.unwrap();
    session.input.send(flush(2)).await.unwrap();
    // Context 1 is in flight, context 2 waits behind it.
    match next(&mut session).await {
        TtsEvent::Audio {
            context: ContextId(1),
            ..
        } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    session.cancel.send(ContextId(1)).await.unwrap();
    let mut audio = Vec::new();
    let mut cancelled = false;
    loop {
        match next(&mut session).await {
            TtsEvent::Audio {
                context: ContextId(1),
                ..
            } => {}
            TtsEvent::Cancelled {
                context: ContextId(1),
            } => cancelled = true,
            TtsEvent::Audio {
                context: ContextId(2),
                audio: a,
            } => audio.push(a),
            TtsEvent::Flushed {
                context: ContextId(2),
            } => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert!(cancelled);
    assert_eq!(total(&audio), 2 * WORD);
    let inputs: Vec<String> = mock.requests().iter().map(mock::Recorded::input).collect();
    assert_eq!(inputs, ["a b c d", "keep me"]);
}

#[tokio::test]
async fn cancel_of_an_unknown_context_is_still_acknowledged() {
    let mock = Mock::start();
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.cancel.send(ContextId(77)).await.unwrap();
    assert!(matches!(
        next(&mut session).await,
        TtsEvent::Cancelled {
            context: ContextId(77)
        }
    ));
}

#[tokio::test]
async fn dropping_the_session_mid_utterance_abandons_the_request() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(100));
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session
        .input
        .send(text(1, "a b c d e f g h"))
        .await
        .unwrap();
    match next(&mut session).await {
        TtsEvent::Audio { .. } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    drop(session);
    wait_for("the abort", || mock.aborted() == 1).await;
}

#[tokio::test]
async fn shutdown_ends_a_session_and_its_request() {
    let mock = Mock::start();
    mock.configure(|c| c.chunk_delay = ms(100));
    let cfg = config(&mock);
    let shutdown = cfg.shutdown.clone();
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session
        .input
        .send(text(1, "a b c d e f g h"))
        .await
        .unwrap();
    match next(&mut session).await {
        TtsEvent::Audio { .. } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    shutdown.cancel();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), session.events.recv())
            .await
            .expect("hang after shutdown")
        {
            None => break,
            Some(TtsEvent::Audio { .. }) => {}
            Some(other) => panic!("unexpected {}", short(&other)),
        }
    }
    wait_for("the abort", || mock.aborted() == 1).await;
}

// ---------------------------------------------------------------------------------------------
// Slow servers and failures
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_slow_first_byte_inside_the_deadline_is_waited_for() {
    let mock = Mock::start();
    mock.inject(Fault::ConnectStall { duration: ms(400) });
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    let started = tokio::time::Instant::now();
    session.input.send(text(1, "a b")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let audio = until_flushed(&mut session, 1).await;
    assert!(started.elapsed() >= ms(400));
    assert_eq!(total(&audio), 2 * WORD);
}

#[tokio::test]
async fn a_body_cut_mid_stream_is_disconnected_after_the_audio_before_it() {
    let mock = Mock::start();
    mock.inject(Fault::Disconnect { after_events: 2 });
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b c d")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    let mut bytes = 0;
    loop {
        match next(&mut session).await {
            TtsEvent::Audio { audio, .. } => bytes += audio.len(),
            TtsEvent::Error(Error::Disconnected { .. }) => break,
            other => panic!("unexpected {}", short(&other)),
        }
    }
    assert_eq!(
        bytes,
        2 * WORD,
        "the audio before the cut is delivered first"
    );
    assert!(session.events.recv().await.is_none());
}

#[tokio::test]
async fn a_dead_server_is_disconnected() {
    let mock = Mock::start();
    let base_url = mock.base_url.clone();
    drop(mock);
    let mut cfg = OpenAiSpeechConfig::preset(Preset::Kokoro, base_url, clock());
    cfg.connect_timeout = ms(500);
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    assert!(provider.warm().await.is_err());
    let mut session = open(&provider).await;
    session.input.send(text(1, "hello")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Error(Error::Disconnected { .. } | Error::Core(_)) => {}
        other => panic!("unexpected {}", short(&other)),
    }
}

#[tokio::test]
async fn server_errors_end_the_session_with_the_status() {
    for status in [404_u16, 500, 503] {
        let mock = Mock::start();
        mock.inject(Fault::Reject { status });
        let provider = provider(&mock);
        let mut session = open(&provider).await;
        session.input.send(text(1, "hello")).await.unwrap();
        match next(&mut session).await {
            TtsEvent::Error(Error::Rejected { status: got, .. }) => assert_eq!(got, status),
            other => panic!("unexpected {}", short(&other)),
        }
        assert!(session.events.recv().await.is_none());
    }
}

#[tokio::test]
async fn a_refused_text_piece_is_skipped_but_two_in_a_row_fail_the_session() {
    let mock = Mock::start();
    mock.configure(|c| c.refuse = vec![("BAD".to_owned(), 400)]);
    let mut cfg = config(&mock);
    cfg.coalesce_chars = 0;
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    // One refusal between good pieces: the audio of the others arrives, no error.
    session.input.send(text(1, "good one. ")).await.unwrap();
    session.input.send(text(1, "BAD ... ")).await.unwrap();
    session.input.send(text(1, "good two.")).await.unwrap();
    session.input.send(flush(1)).await.unwrap();
    assert_eq!(total(&until_flushed(&mut session, 1).await), 4 * WORD);
    // Two in a row: the session fails with the status (a wrong model or voice looks like this).
    session.input.send(text(2, "BAD one")).await.unwrap();
    session.input.send(text(2, "BAD two")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Error(Error::Rejected { status: 400, .. }) => {}
        other => panic!("unexpected {}", short(&other)),
    }
}

#[tokio::test]
async fn a_200_that_is_not_audio_is_a_protocol_error() {
    let mock = Mock::start();
    mock.inject(Fault::Protocol { after_events: 0 });
    let provider = provider(&mock);
    let mut session = open(&provider).await;
    session.input.send(text(1, "hello")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Error(Error::Protocol { .. }) => {}
        other => panic!("unexpected {}", short(&other)),
    }
}

#[tokio::test]
async fn silence_after_the_first_audio_times_out() {
    let mock = Mock::start();
    mock.inject(Fault::Stall {
        before_event: 1,
        duration: Duration::from_secs(30),
    });
    let mut cfg = config(&mock);
    cfg.idle_timeout = ms(300);
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    let mut session = open(&provider).await;
    session.input.send(text(1, "a b")).await.unwrap();
    match next(&mut session).await {
        TtsEvent::Audio { .. } => {}
        other => panic!("unexpected {}", short(&other)),
    }
    let stalled = tokio::time::Instant::now();
    match next(&mut session).await {
        TtsEvent::Error(Error::Core(brisvox_core::Error::Timeout { what, .. })) => {
            assert_eq!(what, "tts idle");
        }
        other => panic!("unexpected {}", short(&other)),
    }
    // Only a lower bound (the idle timeout is 300 ms): an upper bound on a real clock flakes.
    assert!(stalled.elapsed() >= ms(250), "{:?}", stalled.elapsed());
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1, LOCAL_TTS_BASE_URL)
// ---------------------------------------------------------------------------------------------

/// Two sentences, a barge-in and a second utterance against a real server. Environment:
/// `LOCAL_TTS_BASE_URL` (required, e.g. `http://localhost:8880/v1`), `LOCAL_TTS_PRESET`
/// (`kokoro` default, `chatterbox`, `speaches`, `vllm_omni`, `generic`), `LOCAL_TTS_VOICE`,
/// `LOCAL_TTS_MODEL`, `LOCAL_TTS_API_KEY` (never printed). Prints the time to first audio.
#[tokio::test]
async fn live_local_tts_server() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and LOCAL_TTS_BASE_URL");
        return;
    }
    let Ok(base_url) = std::env::var("LOCAL_TTS_BASE_URL") else {
        eprintln!("skipped: LOCAL_TTS_BASE_URL must be set (for example http://localhost:8880/v1)");
        return;
    };
    let preset = match std::env::var("LOCAL_TTS_PRESET")
        .as_deref()
        .unwrap_or("kokoro")
    {
        "chatterbox" => Preset::Chatterbox,
        "speaches" => Preset::Speaches,
        "vllm_omni" => Preset::VllmOmni,
        "generic" => Preset::Generic,
        _ => Preset::Kokoro,
    };
    let mut cfg = OpenAiSpeechConfig::preset(preset, base_url, clock());
    if let Ok(model) = std::env::var("LOCAL_TTS_MODEL") {
        cfg.model = model;
    }
    if let Ok(key) = std::env::var("LOCAL_TTS_API_KEY") {
        cfg.api_key = Some(Secret::new(key));
    }
    let voice = std::env::var("LOCAL_TTS_VOICE")
        .ok()
        .or_else(|| preset.example_voice().map(str::to_owned))
        .expect("set LOCAL_TTS_VOICE for this preset");
    let provider = OpenAiSpeechTts::new(cfg).unwrap();
    match provider.warm().await {
        Ok(()) => eprintln!("live: warm ok"),
        Err(e) => eprintln!("live: warm failed ({e}); the server may be down"),
    }
    let mut session = provider
        .connect(TtsConfig {
            sample_rate: SampleRate::Hz24000,
            voice: Arc::from(voice.as_str()),
            language: None,
            model: None,
        })
        .await
        .unwrap();

    // Utterance 1: two pieces and a flush.
    let started = tokio::time::Instant::now();
    session
        .input
        .send(text(
            1,
            "Hello there, this is a short test of the local voice.",
        ))
        .await
        .unwrap();
    session
        .input
        .send(text(1, " It has two sentences."))
        .await
        .unwrap();
    session.input.send(flush(1)).await.unwrap();
    let mut first_audio = None;
    let mut bytes = 0_usize;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), session.events.recv())
            .await
            .expect("live: no event within 60 s")
            .expect("live: stream ended");
        match event {
            TtsEvent::Audio { audio, .. } => {
                first_audio.get_or_insert_with(|| started.elapsed());
                bytes += audio.len();
            }
            TtsEvent::Flushed { .. } => break,
            other => panic!("live: unexpected {}", short(&other)),
        }
    }
    eprintln!(
        "live: time to first audio {first_audio:?}, {:.2} s of audio, done after {:?}",
        bytes as f64 / 48_000.0,
        started.elapsed()
    );
    assert!(bytes > 0 && bytes.is_multiple_of(2));

    // Utterance 2: barge in after the first audio.
    session
        .input
        .send(text(
            2,
            "This sentence is long enough that we can interrupt it while it is still being generated.",
        ))
        .await
        .unwrap();
    loop {
        if let TtsEvent::Audio { .. } = next(&mut session).await {
            break;
        }
    }
    let cancelled = tokio::time::Instant::now();
    session.cancel.send(ContextId(2)).await.unwrap();
    loop {
        if let TtsEvent::Cancelled { .. } = next(&mut session).await {
            break;
        }
    }
    eprintln!("live: cancel acknowledged after {:?}", cancelled.elapsed());

    // Utterance 3: the session still works, over the same connection.
    let again = tokio::time::Instant::now();
    session.input.send(text(3, "Back again.")).await.unwrap();
    session.input.send(flush(3)).await.unwrap();
    let audio = until_flushed(&mut session, 3).await;
    eprintln!(
        "live: after the cancel, {} bytes in {:?}",
        total(&audio),
        again.elapsed()
    );
    assert!(total(&audio) > 0);
}
