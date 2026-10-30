//! OpenAI-Realtime-transcription STT adapter against local mocks of each dialect (OpenAI, NVIDIA
//! Speech NIM, Speaches, vLLM): URLs and session messages, audio encoding and resampling,
//! interims, the commit that answers a `Finalize`, server-side turns, errors, timeouts,
//! reconnect with replay, the provider contract suite per dialect, and one live test
//! (`BRISVOX_LIVE_TESTS=1`, `LOCAL_REALTIME_STT_URL`, `LOCAL_REALTIME_STT_DIALECT`).
//!
//! The mocks are the dialects as read from the documentation and server sources (2026-10-14, see
//! `docs/providers/openai-realtime-stt.md`), not captures of running servers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::result_large_err
)]

mod mock_ws;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::openai_realtime_stt::{Dialect, OpenAiRealtimeStt, OpenAiRealtimeSttConfig};
use brisvox_providers::{Error, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::contract::{Fault, SttHarness, Utterance, stt_contract};
use bytes::Bytes;
use mock_ws::{Conn, MockServer, What, handler, lock};
use serde_json::{Value, json};
use tokio::time::Instant;

// ---------------------------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------------------------

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn stt_cfg() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

/// Adapter settings for tests: short waits.
fn config(dialect: Dialect, server: &MockServer) -> OpenAiRealtimeSttConfig {
    let mut c = OpenAiRealtimeSttConfig::new(dialect);
    c.base_url = Some(server.base_url());
    if dialect == Dialect::OpenAi {
        c.api_key = Some(Secret::new("sk-test-key".to_owned()));
    }
    if matches!(dialect, Dialect::Speaches | Dialect::Vllm) {
        c.model = Some("test/model".to_owned());
    }
    c.io_timeout = Duration::from_secs(2);
    c.keepalive_interval = Duration::from_secs(9);
    c.finalize_timeout = Duration::from_millis(400);
    c.close_timeout = Duration::from_millis(300);
    c.reconnect_backoff = Duration::from_millis(10);
    c
}

fn adapter(
    dialect: Dialect,
    server: &MockServer,
    tweak: impl FnOnce(&mut OpenAiRealtimeSttConfig),
) -> OpenAiRealtimeStt {
    let mut c = config(dialect, server);
    tweak(&mut c);
    OpenAiRealtimeStt::new(c, clock())
}

async fn connect(stt: &OpenAiRealtimeStt) -> SttSession {
    stt.connect(stt_cfg()).await.expect("connect")
}

/// `ms` of 16 kHz PCM16 with every sample `value`.
fn audio(value: i16, ms: usize) -> SttInput {
    let samples = 16 * ms;
    SttInput::Audio(Bytes::from(
        (0..samples)
            .flat_map(|_| value.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
}

/// 200 ms: enough to commit on every dialect.
fn frame(value: i16) -> SttInput {
    audio(value, 200)
}

async fn event(session: &mut SttSession) -> SttEvent {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("no event within 5 s")
        .expect("the events closed")
}

/// (text, is_final, speech_final, from_finalize) of a transcript event.
fn flat(event: SttEvent) -> (String, bool, bool, bool) {
    match event {
        SttEvent::Transcript {
            text,
            is_final,
            speech_final,
            from_finalize,
            ..
        } => (text.to_string(), is_final, speech_final, from_finalize),
        SttEvent::Error(e) => panic!("unexpected error event: {e:?}"),
    }
}

fn t(text: &str, f: bool, s: bool, ff: bool) -> (String, bool, bool, bool) {
    (text.to_owned(), f, s, ff)
}

async fn expect_closed(session: &mut SttSession) {
    let next = tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("the events did not close within 5 s");
    assert!(next.is_none(), "expected the events to close, got {next:?}");
}

async fn expect_error(session: &mut SttSession) -> Error {
    match event(session).await {
        SttEvent::Error(e) => e,
        other => panic!("expected an error event, got {other:?}"),
    }
}

/// The JSON messages connection `conn` received, in order.
fn messages(server: &MockServer, conn: usize) -> Vec<Value> {
    server
        .log
        .texts(conn)
        .iter()
        .map(|t| serde_json::from_str(t).unwrap_or(Value::Null))
        .collect()
}

/// Their `type`s.
fn types(server: &MockServer, conn: usize) -> Vec<String> {
    messages(server, conn)
        .iter()
        .map(|m| m["type"].as_str().unwrap_or("?").to_owned())
        .collect()
}

/// The PCM bytes of the `input_audio_buffer.append` messages of a connection.
fn appended(server: &MockServer, conn: usize) -> Vec<Vec<u8>> {
    messages(server, conn)
        .iter()
        .filter(|m| m["type"] == "input_audio_buffer.append")
        .map(|m| {
            STANDARD
                .decode(m["audio"].as_str().unwrap_or_default())
                .unwrap_or_default()
        })
        .collect()
}

/// The first sample of each PCM16 chunk.
fn first_samples(chunks: &[Vec<u8>]) -> Vec<i16> {
    chunks
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The OpenAI-style mock (OpenAI, NIM, Speaches)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Oai {
    /// Stream a delta per audio frame (the next word).
    deltas: bool,
    words: Vec<&'static str>,
    /// The transcript of every commit.
    answer: &'static str,
    commit_delay: Duration,
    /// Answer commits at all.
    answer_commit: bool,
    /// A commit of less audio than this (bytes of PCM16) is an error.
    min_commit_bytes: Option<usize>,
    /// The server's own turn (VAD): after this many frames it commits and transcribes by itself.
    vad_turn_after: Option<usize>,
    /// Drop the first connection (no close frame) after this many frames.
    drop_after: Option<usize>,
    garbage_after_first: bool,
    /// Read, but never answer.
    mute: bool,
    /// Send this error event after the first frame.
    error_after_first: Option<Value>,
}

impl Default for Oai {
    fn default() -> Self {
        Self {
            deltas: true,
            words: vec!["merhaba", "dunya", "nasilsin"],
            answer: "Merhaba dünya, nasılsın?",
            commit_delay: Duration::ZERO,
            answer_commit: true,
            min_commit_bytes: None,
            vad_turn_after: None,
            drop_after: None,
            garbage_after_first: false,
            mute: false,
            error_after_first: None,
        }
    }
}

fn delta(item: &str, text: &str) -> String {
    json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": item,
           "content_index": 0, "delta": text})
    .to_string()
}

fn completed(item: &str, text: &str) -> String {
    json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": item,
           "content_index": 0, "transcript": text})
    .to_string()
}

fn committed(item: &str) -> String {
    json!({"type": "input_audio_buffer.committed", "item_id": item, "previous_item_id": null})
        .to_string()
}

async fn oai(conn: Conn, mock: Oai) {
    let index = conn.index;
    let Some(mut s) = conn.accept().await else {
        return;
    };
    s.send(json!({"type": "session.created", "session": {"id": "s"}}).to_string())
        .await;
    let mut frames = 0_usize;
    let mut buffered = 0_usize;
    let mut commits = 0_usize;
    loop {
        let text = match s.next().await {
            None => return,
            Some(What::Text(text)) => text,
            Some(_) => continue,
        };
        let message: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match message["type"].as_str().unwrap_or_default() {
            "session.update" | "transcription_session.update" => {
                s.send(json!({"type": "session.updated"}).to_string()).await;
            }
            "input_audio_buffer.append" => {
                frames += 1;
                buffered += STANDARD
                    .decode(message["audio"].as_str().unwrap_or_default())
                    .map_or(0, |b| b.len());
                if mock.drop_after == Some(frames) && index == 0 {
                    return;
                }
                if mock.mute {
                    continue;
                }
                if mock.garbage_after_first && frames == 1 {
                    s.send("<<not json>>").await;
                    continue;
                }
                if let Some(error) = mock.error_after_first.clone()
                    && frames == 1
                {
                    s.send(error.to_string()).await;
                }
                if mock.deltas
                    && let Some(word) = mock.words.get(frames - 1)
                {
                    let piece = if frames == 1 {
                        (*word).to_owned()
                    } else {
                        format!(" {word}")
                    };
                    s.send(delta("item_live", &piece)).await;
                }
                if mock.vad_turn_after == Some(frames) {
                    s.send(committed("vad")).await;
                    s.send(completed("vad", "server turn")).await;
                    buffered = 0;
                }
            }
            "input_audio_buffer.commit" => {
                if mock.min_commit_bytes.is_some_and(|m| buffered < m) {
                    s.send(
                        json!({"type": "error", "error": {"type": "invalid_request_error",
                               "code": "input_audio_buffer_commit_empty",
                               "message": "buffer too small"}})
                        .to_string(),
                    )
                    .await;
                    continue;
                }
                if !mock.answer_commit {
                    continue;
                }
                commits += 1;
                let item = format!("item_{commits}");
                s.send(committed(&item)).await;
                tokio::time::sleep(mock.commit_delay).await;
                s.send(completed(&item, mock.answer)).await;
                buffered = 0;
            }
            _ => {}
        }
    }
}

async fn start_oai(mock: Oai) -> MockServer {
    MockServer::start(handler(move |conn| oai(conn, mock.clone()))).await
}

// ---------------------------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn openai_connects_with_a_bearer_key_configures_a_transcription_session_and_resamples_to_24_khz()
 {
    let server = start_oai(Oai::default()).await;
    let stt = adapter(Dialect::OpenAi, &server, |c| {
        c.model = Some("gpt-transcribe".to_owned());
    });
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr-TR".into()),
        })
        .await
        .unwrap();
    session.input.send(frame(1000)).await.unwrap();
    event(&mut session).await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].uri, "/v1/realtime?model=gpt-transcribe");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-key")
    );
    let first = &messages(&server, 0)[0];
    assert_eq!(first["type"], "session.update");
    assert_eq!(first["session"]["type"], "transcription");
    let input = &first["session"]["audio"]["input"];
    assert_eq!(input["format"], json!({"type": "audio/pcm", "rate": 24000}));
    assert_eq!(input["transcription"]["model"], "gpt-transcribe");
    assert_eq!(input["transcription"]["language"], "tr");
    assert!(input["turn_detection"].is_null());
    // 200 ms at 16 kHz become about 200 ms at 24 kHz (the filter holds back a block).
    let pcm = appended(&server, 0);
    let samples: usize = pcm.iter().map(|c| c.len() / 2).sum();
    assert!((4_000..=4_800).contains(&samples), "{samples} samples");
    // The key never shows up in the Debug output.
    assert!(!format!("{:?}", stt.config()).contains("sk-test-key"));
}

#[tokio::test]
async fn deltas_become_interims_and_the_completed_event_of_a_commit_answers_the_finalize() {
    let server = start_oai(Oai::default()).await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    for v in [1_i16, 2, 3] {
        session.input.send(frame(v)).await.unwrap();
    }
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya nasilsin", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba dünya, nasılsın?", true, true, true)
    );
    let kinds = types(&server, 0);
    assert_eq!(kinds.first().map(String::as_str), Some("session.update"));
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("input_audio_buffer.commit")
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "input_audio_buffer.append")
            .count(),
        3
    );
    // The same connection carries the next turn.
    session.input.send(frame(4)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    loop {
        let (text, is_final, _, from_finalize) = flat(event(&mut session).await);
        if from_finalize {
            assert!(is_final);
            assert_eq!(text, "Merhaba dünya, nasılsın?");
            break;
        }
    }
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn finalize_with_nothing_or_too_little_buffered_is_answered_empty_without_a_commit() {
    let server = start_oai(Oai::default()).await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    // 50 ms is below OpenAI's 100 ms minimum.
    session.input.send(audio(5, 50)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    loop {
        let (text, _, _, from_finalize) = flat(event(&mut session).await);
        if from_finalize {
            assert_eq!(text, "");
            break;
        }
    }
    assert!(!types(&server, 0).contains(&"input_audio_buffer.commit".to_owned()));
}

#[tokio::test]
async fn an_error_while_a_commit_is_open_ends_the_turn_empty_and_the_session_goes_on() {
    let server = start_oai(Oai {
        min_commit_bytes: Some(1_000_000),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |c| {
        c.min_commit = Some(Duration::ZERO)
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    session.input.send(frame(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("dunya", false, false, false)
    );
}

#[tokio::test]
async fn an_unanswered_commit_is_answered_with_the_hypothesis_and_the_late_transcript_is_dropped() {
    let server = start_oai(Oai {
        commit_delay: Duration::from_millis(900),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |c| {
        c.finalize_timeout = Duration::from_millis(300);
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    event(&mut session).await;
    event(&mut session).await;
    let asked = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya", true, true, true)
    );
    let waited = asked.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_millis(800),
        "answered after {waited:?}"
    );
    // The real answer arrives at about 900 ms: dropped, nothing follows.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        session.events.try_recv().is_err(),
        "the late transcript must not surface"
    );
}

#[tokio::test]
async fn a_dropped_connection_is_replaced_with_the_session_message_first_and_the_audio_replayed() {
    let server = start_oai(Oai {
        drop_after: Some(3),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::Nim, &server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    for v in [1_i16, 2, 3, 4] {
        session.input.send(frame(v)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    for _ in 0..200 {
        if appended(&server, 1).len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let kinds = types(&server, 1);
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("transcription_session.update")
    );
    assert_eq!(first_samples(&appended(&server, 1)), [1, 2, 3, 4]);
    while let Ok(ev) = session.events.try_recv() {
        assert!(!matches!(ev, SttEvent::Error(_)), "{ev:?}");
    }
}

#[tokio::test]
async fn with_no_reconnect_left_the_session_ends_disconnected() {
    let server = start_oai(Oai {
        drop_after: Some(1),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |c| c.max_reconnects = 0);
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    expect_closed(&mut session).await;
}

#[tokio::test]
async fn malformed_json_and_credential_errors_end_the_session_with_the_right_error() {
    let server = start_oai(Oai {
        garbage_after_first: true,
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Protocol { .. }
    ));
    expect_closed(&mut session).await;

    let server = start_oai(Oai {
        error_after_first: Some(json!({"type": "error", "error": {
            "type": "invalid_request_error", "code": "invalid_api_key", "message": "bad key"}})),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Rejected { status: 401, .. }
    ));

    // Other errors are logged and the session carries on.
    let server = start_oai(Oai {
        error_after_first: Some(json!({"type": "error", "error": {
            "type": "server_error", "code": "whatever", "message": "x"}})),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
}

#[tokio::test]
async fn refusals_map_to_rejected() {
    for status in [401_u16, 404, 503] {
        let server = MockServer::start(handler(move |conn: Conn| async move {
            conn.reject(status).await
        }))
        .await;
        let stt = adapter(Dialect::Nim, &server, |_| {});
        match stt.connect(stt_cfg()).await {
            Err(Error::Rejected { status: got, .. }) => assert_eq!(got, status),
            other => panic!("expected Rejected({status}), got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_live_but_silent_server_is_kept_and_a_dead_one_times_out() {
    // Reads (so it pongs) but never answers: a server silent by design (no deltas).
    let server = start_oai(Oai {
        mute: true,
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::Speaches, &server, |c| {
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    for _ in 0..40 {
        session.input.send(audio(1, 20)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(session.events.try_recv().is_err());
    assert!(server.log.pings(0) >= 1);
    drop(session);

    let dead = MockServer::start(handler(|conn: Conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        let _ = s.next().await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }))
    .await;
    let stt = adapter(Dialect::Speaches, &dead, |c| {
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(audio(1, 20)).await.unwrap();
    match expect_error(&mut session).await {
        Error::Core(brisvox_core::Error::Timeout { .. }) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(350) && waited < Duration::from_millis(1200),
        "timed out after {waited:?}"
    );
}

#[tokio::test]
async fn hanging_up_closes_the_events_at_once() {
    let server = start_oai(Oai::default()).await;
    let stt = adapter(Dialect::OpenAi, &server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    let SttSession { input, mut events } = session;
    drop(input);
    let closed = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
    assert!(matches!(closed, Ok(None)), "{closed:?}");
}

#[tokio::test]
async fn an_invalid_config_is_refused_before_any_connection() {
    let server = start_oai(Oai::default()).await;
    // OpenAI without a key.
    let stt = adapter(Dialect::OpenAi, &server, |c| c.api_key = None);
    assert!(matches!(
        stt.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    // Speaches without a model.
    let stt = adapter(Dialect::Speaches, &server, |c| c.model = None);
    assert!(matches!(
        stt.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    // A key over plain ws:// to a non-loopback host.
    let stt = adapter(Dialect::Nim, &server, |c| {
        c.api_key = Some(Secret::new("k".to_owned()));
        c.base_url = Some("ws://10.1.2.3:9000".to_owned());
    });
    assert!(matches!(
        stt.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------------------------
// NVIDIA Speech NIM
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nim_uses_the_intent_query_a_flat_16_khz_session_and_sends_the_audio_unchanged() {
    let server = start_oai(Oai::default()).await;
    let stt = adapter(Dialect::Nim, &server, |c| {
        c.prompt = Some("domain words".to_owned())
    });
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr".into()),
        })
        .await
        .unwrap();
    session.input.send(frame(7)).await.unwrap();
    event(&mut session).await;
    let requests = server.requests();
    assert_eq!(requests[0].uri, "/v1/realtime?intent=transcription");
    assert_eq!(requests[0].header("authorization"), None);
    let first = &messages(&server, 0)[0];
    assert_eq!(first["type"], "transcription_session.update");
    let s = &first["session"];
    assert_eq!(s["input_audio_format"], "pcm16");
    assert_eq!(s["input_audio_params"]["sample_rate_hz"], 16000);
    assert_eq!(s["input_audio_transcription"]["language"], "tr-TR");
    assert_eq!(s["input_audio_transcription"]["prompt"], "domain words");
    // No resampling: the PCM arrives byte for byte (200 ms = 6400 bytes).
    let pcm = appended(&server, 0);
    assert_eq!(pcm.len(), 1);
    assert_eq!(pcm[0].len(), 6400);
    assert_eq!(first_samples(&pcm), [7]);
}

// ---------------------------------------------------------------------------------------------
// Speaches
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn speaches_is_configured_by_its_url_has_no_interims_and_its_vad_turns_are_endpoints() {
    let server = start_oai(Oai {
        deltas: false,
        vad_turn_after: Some(2),
        min_commit_bytes: Some(4800),
        ..Oai::default()
    })
    .await;
    let stt = adapter(Dialect::Speaches, &server, |c| {
        c.model = Some("Systran/faster-whisper-small".to_owned());
    });
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some("tr-TR".into()),
        })
        .await
        .unwrap();
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    // The server's own turn: a final that did not answer a Finalize.
    assert_eq!(
        flat(event(&mut session).await),
        t("server turn", true, true, false)
    );
    let requests = server.requests();
    assert_eq!(
        requests[0].uri,
        "/v1/realtime?intent=transcription&model=Systran%2Ffaster-whisper-small&language=tr"
    );
    // No session message: audio is the first thing it gets.
    assert_eq!(
        types(&server, 0).first().map(String::as_str),
        Some("input_audio_buffer.append")
    );
    // 24 kHz in, so a 200 ms frame is about 4800 samples * 2 bytes.
    let total: usize = appended(&server, 0).iter().map(Vec::len).sum();
    assert!((17_000..=19_200).contains(&total), "{total} bytes");
    // A Finalize with enough audio is answered by the commit's transcript.
    session.input.send(frame(3)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("Merhaba dünya, nasılsın?", true, true, true)
    );
}

// ---------------------------------------------------------------------------------------------
// vLLM
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Vllm {
    done_delay: Duration,
    tail: &'static str,
}

/// vLLM's `/v1/realtime` (source `connection.py`): `commit` starts a generation that consumes
/// the audio queue, `commit` with `final: true` ends it, `transcription.done` carries the text.
async fn vllm(conn: Conn, mock: Vllm) {
    let Some(mut s) = conn.accept().await else {
        return;
    };
    s.send(json!({"type": "session.created", "id": "sess-1", "created": 1}).to_string())
        .await;
    let mut model_ok = false;
    let mut running = false;
    let mut frames = 0_usize;
    let words = ["bir", "iki", "uc"];
    loop {
        let text = match s.next().await {
            None => return,
            Some(What::Text(text)) => text,
            Some(_) => continue,
        };
        let message: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match message["type"].as_str().unwrap_or_default() {
            "session.update" => model_ok = message["model"].is_string(),
            "input_audio_buffer.append" => {
                if running {
                    frames += 1;
                    if let Some(w) = words.get(frames - 1) {
                        let piece = if frames == 1 {
                            (*w).to_owned()
                        } else {
                            format!(" {w}")
                        };
                        s.send(json!({"type": "transcription.delta", "delta": piece}).to_string())
                            .await;
                    }
                }
            }
            "input_audio_buffer.commit" => {
                if !model_ok {
                    s.send(
                        json!({"type": "error", "error": "Model not validated.",
                               "code": "model_not_validated"})
                        .to_string(),
                    )
                    .await;
                } else if message["final"] == true {
                    if running {
                        tokio::time::sleep(mock.done_delay).await;
                        s.send(
                            json!({"type": "transcription.done", "text": mock.tail,
                                   "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
                            .to_string(),
                        )
                        .await;
                        running = false;
                        frames = 0;
                    }
                } else if !running {
                    running = true;
                    frames = 0;
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn vllm_names_its_model_starts_a_generation_finalizes_with_a_final_commit_and_restarts() {
    let server = MockServer::start(handler(|conn| {
        vllm(
            conn,
            Vllm {
                done_delay: Duration::from_millis(200),
                tail: "bir iki uc dort",
            },
        )
    }))
    .await;
    let stt = adapter(Dialect::Vllm, &server, |c| {
        c.model = Some("mistralai/Voxtral-Mini-4B-Realtime-2602".to_owned());
        c.finalize_timeout = Duration::from_secs(2);
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    // Audio of the next turn arrives while the server finishes the generation.
    session.input.send(frame(11)).await.unwrap();
    session.input.send(frame(12)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki uc dort", true, true, true)
    );
    // The held frames reach the new generation, after its start commit.
    assert_eq!(
        flat(event(&mut session).await),
        t("bir", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("bir iki", false, false, false)
    );
    let requests = server.requests();
    assert_eq!(requests[0].uri, "/v1/realtime");
    let kinds: Vec<(String, bool)> = messages(&server, 0)
        .iter()
        .map(|m| {
            (
                m["type"].as_str().unwrap_or("?").to_owned(),
                m["final"] == true,
            )
        })
        .collect();
    let names: Vec<&str> = kinds
        .iter()
        .map(|(k, f)| match (k.as_str(), f) {
            ("input_audio_buffer.commit", true) => "commit_final",
            (k, _) => k,
        })
        .collect();
    assert_eq!(
        names,
        [
            "session.update",
            "input_audio_buffer.commit",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
            "commit_final",
            "input_audio_buffer.commit",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
        ]
    );
    assert_eq!(
        messages(&server, 0)[0]["model"],
        "mistralai/Voxtral-Mini-4B-Realtime-2602"
    );
    // 16 kHz in and out: byte for byte.
    assert_eq!(first_samples(&appended(&server, 0)), [1, 2, 11, 12]);
}

#[tokio::test]
async fn vllm_replaces_the_connection_when_a_generation_does_not_finish() {
    let server = MockServer::start(handler(|conn| {
        vllm(
            conn,
            Vllm {
                done_delay: Duration::from_secs(5),
                tail: "late",
            },
        )
    }))
    .await;
    let stt = adapter(Dialect::Vllm, &server, |c| {
        c.finalize_timeout = Duration::from_millis(300);
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("bir", true, true, true));
    // A new turn works on a fresh connection.
    session.input.send(frame(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("bir", false, false, false)
    );
    assert_eq!(server.connections(), 2);
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite (per dialect with interims)
// ---------------------------------------------------------------------------------------------

/// The deadline the contract checks hold the adapter to. Shorter than the suite's default so the
/// timeout checks stay quick on a real clock.
const CONTRACT_DEADLINE: Duration = Duration::from_millis(600);

#[derive(Default)]
struct Script {
    utterance: Option<Utterance>,
    faults: Vec<Fault>,
}

struct Harness {
    server: MockServer,
    script: Arc<Mutex<Script>>,
    provider: Arc<OpenAiRealtimeStt>,
}

impl Harness {
    async fn start(dialect: Dialect) -> Self {
        let script = Arc::new(Mutex::new(Script::default()));
        let s = Arc::clone(&script);
        let server = MockServer::start(handler(move |conn| {
            scripted(conn, Arc::clone(&s), dialect == Dialect::Vllm)
        }))
        .await;
        let provider = Arc::new(adapter(dialect, &server, |c| {
            c.io_timeout = CONTRACT_DEADLINE;
            c.finalize_timeout = Duration::from_millis(400);
            c.close_timeout = Duration::from_millis(300);
            // The suite sends 40 ms of audio and expects it flushed.
            c.min_commit = Some(Duration::ZERO);
            // Timeouts must surface as Core(Timeout) and drops as Disconnected: no reconnect.
            c.max_reconnects = 0;
        }));
        Self {
            server,
            script,
            provider,
        }
    }
}

impl SttHarness for Harness {
    type P = OpenAiRealtimeStt;

    fn provider(&self) -> Arc<OpenAiRealtimeStt> {
        Arc::clone(&self.provider)
    }

    fn clock(&self) -> SharedClock {
        clock()
    }

    fn io_timeout(&self) -> Duration {
        CONTRACT_DEADLINE
    }

    fn reset(&self) {
        *lock(&self.script) = Script::default();
        self.server.abort_handlers();
    }

    fn script(&self, utterance: Utterance) {
        lock(&self.script).utterance = Some(utterance);
    }

    fn inject(&self, fault: Fault) {
        lock(&self.script).faults.push(fault);
    }
}

/// The scripted server of the contract suite, in the OpenAI or the vLLM event names: on the first
/// frame a delta per interim (cumulative texts as increments) and then an event that carries no
/// text, one such event per later frame; the transcript of the finals as the answer to a commit.
/// Faults count the events sent. A `Stall` makes the server deaf (it does not even read), like a
/// hung process.
async fn scripted(conn: Conn, script: Arc<Mutex<Script>>, vllm_names: bool) {
    let (utterance, faults) = {
        let mut s = lock(&script);
        (s.utterance.take(), std::mem::take(&mut s.faults))
    };
    let mut disconnect_after = None;
    let mut protocol_after = None;
    let mut stalls: Vec<(usize, Duration)> = Vec::new();
    let mut reject = None;
    let mut connect_stall = None;
    for fault in faults {
        match fault {
            Fault::Disconnect { after_events } => disconnect_after = Some(after_events),
            Fault::Protocol { after_events } => protocol_after = Some(after_events),
            Fault::Stall {
                before_event,
                duration,
            } => stalls.push((before_event, duration)),
            Fault::Reject { status } => reject = Some(status),
            Fault::ConnectStall { duration } => connect_stall = Some(duration),
        }
    }
    if let Some(d) = connect_stall {
        tokio::time::sleep(d).await;
    }
    if let Some(status) = reject {
        conn.reject(status).await;
        return;
    }
    let Some(mut s) = conn.accept().await else {
        return;
    };
    let mut delivered = 0_usize;
    let mut applied: HashSet<usize> = HashSet::new();
    let mut first_frame = true;
    let mut commits = 0_usize;
    loop {
        let text = match s.next().await {
            None => return,
            Some(What::Text(text)) => text,
            Some(_) => continue,
        };
        let message: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let mut out: Vec<String> = Vec::new();
        match message["type"].as_str().unwrap_or_default() {
            "input_audio_buffer.append" => {
                if first_frame {
                    first_frame = false;
                    let mut previous = String::new();
                    if let Some(u) = &utterance {
                        for interim in &u.interims {
                            let piece = interim
                                .strip_prefix(&previous)
                                .unwrap_or(interim)
                                .to_owned();
                            previous.clone_from(interim);
                            out.push(if vllm_names {
                                json!({"type": "transcription.delta", "delta": piece}).to_string()
                            } else {
                                delta("i1", &piece)
                            });
                        }
                    }
                }
                out.push(json!({"type": "rate_limits.updated"}).to_string());
            }
            "input_audio_buffer.commit" if vllm_names && message["final"] != true => {}
            "input_audio_buffer.commit" => {
                let answer = utterance
                    .as_ref()
                    .map(|u| u.finals.join(" "))
                    .unwrap_or_default();
                if vllm_names {
                    out.push(json!({"type": "transcription.done", "text": answer}).to_string());
                } else {
                    commits += 1;
                    out.push(committed("i1"));
                    out.push(completed("i1", &answer));
                    let _ = commits;
                }
            }
            _ => {}
        }
        for message in out {
            if disconnect_after == Some(delivered) {
                return;
            }
            if protocol_after == Some(delivered) {
                protocol_after = None;
                s.send("<<not json>>").await;
            }
            if applied.insert(delivered)
                && let Some((_, d)) = stalls.iter().find(|(n, _)| *n == delivered)
            {
                tokio::time::sleep(*d).await;
            }
            s.send(message).await;
            delivered += 1;
        }
    }
}

async fn run_contract(dialect: Dialect) {
    let harness = Harness::start(dialect).await;
    let report = stt_contract(&harness).await;
    let failures: Vec<_> = report.failures().collect();
    assert!(
        failures.is_empty(),
        "contract failures ({}): {failures:#?}",
        dialect.name()
    );
    assert_eq!(report.checks.len(), 7);
}

#[tokio::test]
async fn the_provider_contract_suite_passes_for_openai() {
    run_contract(Dialect::OpenAi).await;
}

#[tokio::test]
async fn the_provider_contract_suite_passes_for_nim() {
    run_contract(Dialect::Nim).await;
}

#[tokio::test]
async fn the_provider_contract_suite_passes_for_vllm() {
    // The vLLM dialect needs a model; the harness config supplies one.
    run_contract(Dialect::Vllm).await;
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1, LOCAL_REALTIME_STT_URL, LOCAL_REALTIME_STT_DIALECT)
// ---------------------------------------------------------------------------------------------

fn fixture_pcm(name: &str, from: usize, to: usize) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    let wav = std::fs::read(path).expect("fixture");
    wav[44 + from * 2..44 + to * 2].to_vec()
}

#[tokio::test]
async fn live_realtime_transcription_server_streams_the_fixture_and_finalizes_per_turn() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and LOCAL_REALTIME_STT_URL to run");
        return;
    }
    let Ok(url) = std::env::var("LOCAL_REALTIME_STT_URL") else {
        eprintln!("skipped: LOCAL_REALTIME_STT_URL is not set (e.g. ws://127.0.0.1:9000)");
        return;
    };
    let dialect_name =
        std::env::var("LOCAL_REALTIME_STT_DIALECT").unwrap_or_else(|_| "nim".to_owned());
    let dialect = Dialect::from_name(&dialect_name)
        .expect("LOCAL_REALTIME_STT_DIALECT: openai|nim|speaches|vllm");
    let language = std::env::var("LOCAL_REALTIME_STT_LANGUAGE").unwrap_or_else(|_| "en".to_owned());
    let mut cfg = OpenAiRealtimeSttConfig::new(dialect);
    cfg.base_url = Some(url);
    cfg.model = std::env::var("LOCAL_REALTIME_STT_MODEL").ok();
    if let Ok(key) = std::env::var("LOCAL_REALTIME_STT_API_KEY") {
        cfg.api_key = Some(Secret::new(key));
    }
    // Local models may take longer than the defaults on a cold start.
    cfg.finalize_timeout = Duration::from_secs(3);
    let (fixture, segments): (&str, [(usize, usize); 2]) = if language == "tr" {
        (
            "tr_two_utterances.wav",
            [(8000, 100_800), (124_800, 233_440)],
        )
    } else {
        let json: Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/en_two_utterances.json"),
            )
            .expect("fixture json"),
        )
        .expect("fixture json");
        let seg = |i: usize| {
            (
                json["segments"][i]["start"].as_u64().unwrap() as usize,
                json["segments"][i]["end"].as_u64().unwrap() as usize,
            )
        };
        ("en_two_utterances.wav", [seg(0), seg(1)])
    };
    let stt = OpenAiRealtimeStt::new(cfg, clock());
    stt.warm().await.expect("warm");
    let mut session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some(language.as_str().into()),
        })
        .await
        .expect("connect");
    for (turn, (from, to)) in segments.iter().enumerate() {
        let mut pcm = fixture_pcm(fixture, *from, *to);
        pcm.extend(vec![0_u8; 19_200]);
        let started = Instant::now();
        let mut first_partial = None;
        let mut partials = 0_usize;
        let mut finals: Vec<String> = Vec::new();
        for chunk in pcm.chunks(640) {
            session
                .input
                .send(SttInput::Audio(Bytes::copy_from_slice(chunk)))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok(ev) = session.events.try_recv() {
                match ev {
                    SttEvent::Transcript {
                        is_final: false, ..
                    } => {
                        partials += 1;
                        first_partial.get_or_insert(started.elapsed());
                    }
                    SttEvent::Transcript {
                        text,
                        is_final: true,
                        from_finalize: false,
                        ..
                    } => {
                        finals.push(text.to_string());
                    }
                    SttEvent::Transcript { .. } => {}
                    SttEvent::Error(e) => panic!("live session failed: {e}"),
                }
            }
        }
        let asked = Instant::now();
        session.input.send(SttInput::Finalize).await.unwrap();
        let answer = loop {
            match tokio::time::timeout(Duration::from_secs(10), session.events.recv())
                .await
                .expect("the Finalize must be answered")
            {
                Some(SttEvent::Transcript {
                    text,
                    is_final: true,
                    from_finalize: true,
                    speech_final,
                    ..
                }) => {
                    assert!(speech_final);
                    break text.to_string();
                }
                Some(SttEvent::Transcript {
                    text,
                    is_final: true,
                    ..
                }) => finals.push(text.to_string()),
                Some(SttEvent::Transcript { .. }) => {}
                Some(SttEvent::Error(e)) => panic!("live session failed: {e}"),
                None => panic!("the events closed"),
            }
        };
        let finalize_latency = asked.elapsed();
        finals.push(answer);
        let heard = finals.join(" ");
        eprintln!(
            "live {} turn {turn}: {partials} partials, first partial {first_partial:?}, finalize latency {finalize_latency:?}",
            dialect.name()
        );
        eprintln!("  heard: {heard}");
        assert!(!heard.trim().is_empty(), "no text for turn {turn}");
    }
}
