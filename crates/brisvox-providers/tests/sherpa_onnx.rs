//! sherpa-onnx STT adapter against local mocks of the two servers' WebSocket protocols (the
//! Python `streaming_server.py` and the C++ `sherpa-onnx-online-websocket-server`): audio
//! encoding, results, the `Done` finalize exchange and connection replacement, endpoints,
//! reconnect with replay, errors, liveness, the provider contract suite, and one live test
//! (`BRISVOX_LIVE_TESTS=1` and `SHERPA_ONNX_URL`) plus a Turkish phone-quality live run
//! (`SHERPA_ONNX_TR_URL`).
//!
//! The mocks are the protocols as read from the server sources (2026-10-14, see
//! `docs/providers/sherpa-onnx.md`), not captures of a running server.
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

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::sherpa_onnx::{SherpaOnnxConfig, SherpaOnnxStt};
use brisvox_providers::{Error, SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::contract::{Fault, SttHarness, Utterance, stt_contract};
use bytes::Bytes;
use mock_ws::{Conn, MockServer, What, handler, lock};
use serde_json::json;
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
fn config(server: &MockServer) -> SherpaOnnxConfig {
    let mut c = SherpaOnnxConfig::new();
    c.base_url = server.base_url();
    c.io_timeout = Duration::from_secs(2);
    c.keepalive_interval = Duration::from_secs(9);
    c.finalize_timeout = Duration::from_millis(400);
    c.close_timeout = Duration::from_millis(300);
    c.reconnect_backoff = Duration::from_millis(10);
    // The mocks count audio messages one per frame; the padding has its own tests.
    c.finalize_padding = Duration::ZERO;
    c
}

fn adapter(server: &MockServer, tweak: impl FnOnce(&mut SherpaOnnxConfig)) -> SherpaOnnxStt {
    let mut c = config(server);
    tweak(&mut c);
    SherpaOnnxStt::new(c, clock())
}

async fn connect(stt: &SherpaOnnxStt) -> SttSession {
    stt.connect(stt_cfg()).await.expect("connect")
}

/// 20 ms of 16 kHz PCM16 with every sample `value`.
fn frame(value: i16) -> SttInput {
    SttInput::Audio(Bytes::from(
        (0..320)
            .flat_map(|_| value.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
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

/// The f32 samples of a binary message.
fn f32s(message: &Bytes) -> Vec<f32> {
    message
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The Python server
// ---------------------------------------------------------------------------------------------

/// How a mock server behaves.
#[derive(Clone)]
struct Mock {
    /// Words heard so far after frame `k` is `words[..k]`.
    words: Vec<&'static str>,
    /// What `Done` is answered with (the tail decode).
    tail: &'static str,
    /// Delay before the answer to `Done`.
    done_delay: Duration,
    /// Answer `Done` at all.
    answer_done: bool,
    /// After this frame the server endpoints (the segment number goes up).
    endpoint_after: Option<usize>,
    /// Drop the connection (no close frame) after this many frames, on the first connection only.
    drop_after: Option<usize>,
    /// Send this garbage after the first frame.
    garbage_after_first: bool,
    /// Never reply to audio (but read, so pings are answered).
    mute: bool,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            words: vec!["merhaba", "dunya", "nasilsin"],
            tail: "merhaba dunya nasilsin iyi misin",
            done_delay: Duration::ZERO,
            answer_done: true,
            endpoint_after: None,
            drop_after: None,
            garbage_after_first: false,
            mute: false,
        }
    }
}

fn py_message(text: &str, segment: u64) -> String {
    json!({"text": text, "segment": segment}).to_string()
}

/// The Python example server: one `{text, segment}` per frame, `Done` -> last result -> close.
async fn python(conn: Conn, mock: Mock) {
    let index = conn.index;
    let Some(mut s) = conn.accept().await else {
        return;
    };
    let mut frames = 0_usize;
    let mut segment = 0_u64;
    let mut heard = 0_usize;
    loop {
        match s.next().await {
            None => return,
            Some(What::Audio(_)) => {
                frames += 1;
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
                heard = frames.min(mock.words.len()).max(heard);
                let text = mock.words[..heard].join(" ");
                s.send(py_message(&text, segment)).await;
                if mock.endpoint_after == Some(frames) {
                    segment += 1;
                    heard = 0;
                    frames = 0;
                }
            }
            Some(What::Text(text)) if text == "Done" => {
                if !mock.answer_done {
                    s.drain().await;
                    return;
                }
                tokio::time::sleep(mock.done_delay).await;
                s.send(py_message(mock.tail, segment)).await;
                s.close_with(1000, "").await;
                return;
            }
            Some(_) => {}
        }
    }
}

async fn start_python(mock: Mock) -> MockServer {
    MockServer::start(handler(move |conn| python(conn, mock.clone()))).await
}

// ---------------------------------------------------------------------------------------------
// Audio and results
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn audio_is_sent_as_little_endian_float32_one_message_per_frame() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(16_384)).await.unwrap();
    session.input.send(frame(-16_384)).await.unwrap();
    // Two results come back, so the two frames were read.
    event(&mut session).await;
    event(&mut session).await;
    let audio = server.log.audio(0);
    assert_eq!(audio.len(), 2);
    for (message, expected) in audio.iter().zip([0.5_f32, -0.5]) {
        let samples = f32s(message);
        assert_eq!(samples.len(), 320);
        assert!(samples.iter().all(|s| (s - expected).abs() < 1e-6));
    }
}

#[tokio::test]
async fn results_become_interims_and_a_repeated_hypothesis_is_not_forwarded_again() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    for _ in 0..5 {
        session.input.send(frame(100)).await.unwrap();
    }
    // Words after frames 1..3, then the same hypothesis twice more.
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
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        session.events.try_recv().is_err(),
        "the repeats are not forwarded"
    );
}

#[tokio::test]
async fn a_higher_segment_number_is_the_servers_endpoint_and_a_final_segment() {
    let server = start_python(Mock {
        endpoint_after: Some(2),
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(100)).await.unwrap();
    session.input.send(frame(100)).await.unwrap();
    session.input.send(frame(100)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya", false, false, false)
    );
    // The third result carries segment 1 and the new segment's first word: the previous
    // segment is over.
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya", true, true, false)
    );
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
}

// ---------------------------------------------------------------------------------------------
// Finalize: the Done exchange
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn finalize_sends_done_answers_with_the_last_result_and_continues_on_a_new_connection() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(100)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya nasilsin iyi misin", true, true, true)
    );
    // The next turn runs on a replacement connection.
    session.input.send(frame(200)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya nasilsin iyi misin", true, true, true)
    );
    // The replacement of the second turn's connection is only opened when audio follows.
    assert_eq!(server.connections(), 2);
    assert_eq!(server.log.texts(0), ["Done"]);
    assert_eq!(server.log.texts(1), ["Done"]);
    assert_eq!(server.log.audio(0).len(), 1);
    assert_eq!(server.log.audio(1).len(), 1);
    assert!((f32s(&server.log.audio(1)[0])[0] - 200.0 / 32_768.0).abs() < 1e-6);
}

#[tokio::test]
async fn audio_that_arrives_during_the_done_exchange_goes_to_the_replacement_in_order() {
    let server = start_python(Mock {
        done_delay: Duration::from_millis(250),
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_secs(2));
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    // The next turn's first frames come while the server is still thinking.
    for v in [11_i16, 12, 13] {
        session.input.send(frame(v)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya nasilsin iyi misin", true, true, true)
    );
    // The held frames arrive on the new connection, none lost, in order, none on the old one.
    assert!(server.wait_connections(2).await);
    let mut got = Vec::new();
    for _ in 0..100 {
        got = server.log.audio(1);
        if got.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let firsts: Vec<i32> = got
        .iter()
        .map(|b| (f32s(b)[0] * 32_768.0).round() as i32)
        .collect();
    assert_eq!(firsts, [11, 12, 13]);
    assert_eq!(server.log.audio(0).len(), 1);
}

/// The Python server's chunking, reduced to its observable rule: the result after `Done` has
/// text only when the connection received at least `needed_samples` samples in all (the decoder
/// needs a full chunk and its right context, which the server's own 0.3 s padding does not
/// always provide). Live: "..CHRIST" for "..CHRISTIAN", "HIE" for "Hi there".
async fn chunky(conn: Conn, needed_samples: usize) {
    let Some(mut s) = conn.accept().await else {
        return;
    };
    let mut samples = 0_usize;
    loop {
        match s.next().await {
            None => return,
            Some(What::Audio(bytes)) => samples += bytes.len() / 4,
            Some(What::Text(text)) if text == "Done" => {
                // The server's own tail padding of 0.3 s.
                samples += 4_800;
                let text = if samples >= needed_samples {
                    "hello there"
                } else {
                    ""
                };
                s.send(py_message(text, 0)).await;
                s.close_with(1000, "").await;
                return;
            }
            Some(_) => {}
        }
    }
}

/// A short first utterance (0.5 s of audio) against a server that needs 0.875 s in all before it
/// shows the last words: without the padding the answer lacks them, with it the server has
/// what it needs. Fails with `finalize_padding = 0`.
#[tokio::test]
async fn the_padding_before_done_gives_a_short_utterance_the_audio_the_decoder_needs() {
    for (padding_ms, expected) in [(0_u64, ""), (300, "hello there")] {
        let server = MockServer::start(handler(|conn| chunky(conn, 14_000))).await;
        let stt = adapter(&server, |c| {
            c.finalize_padding = Duration::from_millis(padding_ms);
        });
        let mut session = connect(&stt).await;
        for _ in 0..25 {
            session.input.send(frame(1_000)).await.unwrap();
        }
        session.input.send(SttInput::Finalize).await.unwrap();
        assert_eq!(
            flat(event(&mut session).await),
            t(expected, true, true, true),
            "padding {padding_ms} ms"
        );
    }
}

/// The wire order: the audio of the turn, then the silence (zero `float32` samples, exactly the
/// configured length), then `Done`. Nothing is added when the connection heard nothing.
#[tokio::test]
async fn the_padding_is_zeros_of_the_configured_length_between_the_audio_and_done() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |c| c.finalize_padding = Duration::from_millis(250));
    let mut session = connect(&stt).await;
    session.input.send(frame(100)).await.unwrap();
    session.input.send(frame(100)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    loop {
        if let SttEvent::Transcript {
            from_finalize: true,
            ..
        } = event(&mut session).await
        {
            break;
        }
    }
    let audio = server.log.audio(0);
    assert_eq!(audio.len(), 3, "two frames and the padding");
    let padding = f32s(&audio[2]);
    assert_eq!(padding.len(), 4_000, "250 ms at 16 kHz");
    assert!(padding.iter().all(|s| *s == 0.0));
    assert_eq!(server.log.texts(0), ["Done"]);

    // A connection that heard nothing: no padding, no Done (the shortcut).
    let mut idle = connect(&stt).await;
    idle.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut idle).await), t("", true, true, true));
    assert!(server.log.audio(2).is_empty());
    assert!(server.log.texts(2).is_empty());
}

#[tokio::test]
async fn finalize_on_a_connection_that_heard_nothing_is_answered_empty_without_a_round_trip() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(flat(event(&mut session).await), t("", true, true, true));
    assert_eq!(server.connections(), 1);
    assert!(server.log.texts(0).is_empty());
}

#[tokio::test]
async fn an_unanswered_done_is_answered_with_the_latest_hypothesis_after_the_timeout() {
    let server = start_python(Mock {
        answer_done: false,
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| c.finalize_timeout = Duration::from_millis(300));
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    event(&mut session).await;
    let asked = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba dunya", true, true, true)
    );
    // Only the lower bound: an upper bound on a real clock flakes on a loaded machine, and a
    // late answer would hang this test's `event` timeout anyway.
    let waited = asked.elapsed();
    assert!(
        waited >= Duration::from_millis(250),
        "answered after {waited:?}, before the finalize timeout"
    );
    // The stuck connection was replaced; a new turn works.
    session.input.send(frame(2)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("merhaba", false, false, false)
    );
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
async fn two_finalizes_in_a_row_are_answered_once_each_in_order() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    session.input.send(SttInput::Finalize).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    let mut finals = Vec::new();
    while finals.len() < 2 {
        let (text, is_final, speech_final, from_finalize) = flat(event(&mut session).await);
        if from_finalize {
            assert!(is_final && speech_final);
            finals.push(text);
        }
    }
    assert_eq!(finals[0], "merhaba dunya nasilsin iyi misin");
    assert_eq!(finals[1], "merhaba dunya nasilsin iyi misin");
}

// ---------------------------------------------------------------------------------------------
// The C++ server
// ---------------------------------------------------------------------------------------------

fn cpp_message(text: &str, segment: u64, is_final: bool, is_eof: bool) -> String {
    json!({"text": text, "tokens": [], "timestamps": [], "ys_probs": [], "lm_probs": [],
           "context_scores": [], "segment": segment, "words": [], "num_trailing_blanks": 0,
           "start_time": 0.0, "is_final": is_final, "is_eof": is_eof})
    .to_string()
}

/// The C++ server: full result JSON per decode, `is_final` at its endpoint, `is_eof` and a
/// `Done!` text at the end of a `Done` exchange (no close).
async fn cpp(conn: Conn, endpoint_after: Option<usize>) {
    let Some(mut s) = conn.accept().await else {
        return;
    };
    let mut frames = 0_usize;
    let mut segment = 0_u64;
    let words = ["hello", "there", "friend"];
    loop {
        match s.next().await {
            None => return,
            Some(What::Audio(_)) => {
                frames += 1;
                let text = words[..frames.min(words.len())].join(" ");
                let endpoint = endpoint_after == Some(frames);
                s.send(cpp_message(&text, segment, endpoint, false)).await;
                if endpoint {
                    segment += 1;
                    frames = 0;
                }
            }
            Some(What::Text(text)) if text == "Done" => {
                let text = words[..frames.min(words.len())].join(" ");
                s.send(cpp_message(&format!("{text} tail"), segment, true, true))
                    .await;
                s.send("Done!").await;
                s.drain().await;
                return;
            }
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn the_cpp_server_uses_is_final_for_endpoints_and_is_eof_for_the_end_of_done() {
    let server = MockServer::start(handler(|conn| cpp(conn, Some(2)))).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", false, false, false)
    );
    // The endpoint result is final at once (no waiting for the next segment number).
    assert_eq!(
        flat(event(&mut session).await),
        t("hello there", true, true, false)
    );
    session.input.send(frame(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", false, false, false)
    );
    session.input.send(SttInput::Finalize).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello tail", true, true, true)
    );
    // The connection was replaced after the end marker.
    session.input.send(frame(1)).await.unwrap();
    assert_eq!(
        flat(event(&mut session).await),
        t("hello", false, false, false)
    );
    assert_eq!(server.connections(), 2);
}

// ---------------------------------------------------------------------------------------------
// Reconnect, errors, liveness
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_connection_is_replaced_and_the_open_utterance_is_replayed() {
    let server = start_python(Mock {
        drop_after: Some(3),
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    for v in [1_i16, 2, 3, 4] {
        session.input.send(frame(v)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Wait for the replacement to have everything.
    let mut replayed = Vec::new();
    for _ in 0..200 {
        replayed = server.log.audio(1);
        if replayed.len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let firsts: Vec<i32> = replayed
        .iter()
        .map(|b| (f32s(b)[0] * 32_768.0).round() as i32)
        .collect();
    assert_eq!(
        firsts,
        [1, 2, 3, 4],
        "the frames of the lost connection come first"
    );
    assert_eq!(server.log.audio(0).len(), 3);
    // No error reached the caller.
    let mut saw_error = false;
    while let Ok(ev) = session.events.try_recv() {
        saw_error |= matches!(ev, SttEvent::Error(_));
    }
    assert!(!saw_error);
}

#[tokio::test]
async fn a_pending_finalize_survives_a_reconnect_and_is_asked_again_after_the_replay() {
    let server = start_python(Mock {
        drop_after: Some(2),
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 2);
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    session.input.send(frame(2)).await.unwrap();
    session.input.send(SttInput::Finalize).await.unwrap();
    let answer = loop {
        let (text, is_final, _, from_finalize) = flat(event(&mut session).await);
        if from_finalize {
            assert!(is_final);
            break text;
        }
    };
    assert_eq!(answer, "merhaba dunya nasilsin iyi misin");
    assert_eq!(server.log.audio(1).len(), 2, "the replay precedes the Done");
    assert_eq!(server.log.texts(1), ["Done"]);
}

#[tokio::test]
async fn with_no_reconnect_left_the_session_ends_disconnected() {
    let server = start_python(Mock {
        drop_after: Some(1),
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| c.max_reconnects = 0);
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Disconnected { .. }
    ));
    expect_closed(&mut session).await;
}

#[tokio::test]
async fn a_message_that_is_not_json_is_a_protocol_error() {
    let server = start_python(Mock {
        garbage_after_first: true,
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    assert!(matches!(
        expect_error(&mut session).await,
        Error::Protocol { .. }
    ));
    expect_closed(&mut session).await;
}

#[tokio::test]
async fn refusals_map_to_rejected() {
    for status in [401_u16, 503] {
        let server = MockServer::start(handler(move |conn: Conn| async move {
            conn.reject(status).await
        }))
        .await;
        let stt = adapter(&server, |_| {});
        match stt.connect(stt_cfg()).await {
            Err(Error::Rejected { status: got, .. }) => assert_eq!(got, status),
            other => panic!("expected Rejected({status}), got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_server_that_never_answers_audio_is_a_timeout_once_it_stops_answering_pings_too() {
    // This mock reads (so it pongs) but never answers anything: still alive.
    let server = start_python(Mock {
        mute: true,
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| {
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    // Audio flows for well over io_timeout: the half-way ping proves the server alive.
    for _ in 0..40 {
        session.input.send(frame(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        session.events.try_recv().is_err(),
        "a silent but live server is not dropped"
    );
    assert!(server.log.pings(0) >= 1, "a ping went out");
    server.abort_handlers();
}

#[tokio::test]
async fn a_dead_server_is_a_timeout_after_io_timeout() {
    // Accepts, then stops reading entirely (a hung process): nothing, pongs included.
    let server = MockServer::start(handler(|conn: Conn| async move {
        let Some(mut s) = conn.accept().await else {
            return;
        };
        let _ = s.next().await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }))
    .await;
    let stt = adapter(&server, |c| {
        c.io_timeout = Duration::from_millis(400);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    let start = Instant::now();
    session.input.send(frame(1)).await.unwrap();
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
async fn a_pause_in_the_audio_sends_a_keepalive_ping_and_is_not_a_silent_server() {
    let server = start_python(Mock {
        mute: true,
        ..Mock::default()
    })
    .await;
    let stt = adapter(&server, |c| {
        c.io_timeout = Duration::from_millis(300);
        c.keepalive_interval = Duration::from_millis(100);
        c.max_reconnects = 0;
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    // Nothing more is sent for several io_timeouts.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(session.events.try_recv().is_err());
    assert!(server.log.pings(0) >= 2);
}

// ---------------------------------------------------------------------------------------------
// Rates, hang-up, auth
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn telephony_audio_is_resampled_to_the_model_rate() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz8000,
            language: Some("tr".into()),
        })
        .await
        .unwrap();
    // 1 s of 8 kHz audio in 20 ms frames.
    for _ in 0..50 {
        let frame: Vec<u8> = (0..160_i16).flat_map(|s| (s * 100).to_le_bytes()).collect();
        session
            .input
            .send(SttInput::Audio(Bytes::from(frame)))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let samples: usize = server.log.audio(0).iter().map(|b| b.len() / 4).sum();
    // About 16000 samples come out; the filter holds back a block or two.
    assert!((15_000..=16_000).contains(&samples), "{samples} samples");
}

#[tokio::test]
async fn hanging_up_closes_the_events_at_once_and_the_socket_soon_after() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    let SttSession { input, mut events } = session;
    drop(input);
    let closed = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
    assert!(matches!(closed, Ok(None)), "{closed:?}");
    // The socket ends with `Done` (the server answers and closes), not with a bare close frame,
    // which the Python server logs as a failed connection.
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.log.texts(0).is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(server.log.texts(0), ["Done"]);
}

#[tokio::test]
async fn a_bearer_token_is_sent_as_an_authorization_header_and_the_path_is_used() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |c| {
        c.bearer = Some(Secret::new("proxy-token".to_owned()));
        c.path = "/asr/stream".to_owned();
    });
    let mut session = connect(&stt).await;
    session.input.send(frame(1)).await.unwrap();
    event(&mut session).await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].uri, "/asr/stream");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer proxy-token")
    );
    // The Debug output of the config never shows the token.
    assert!(!format!("{:?}", stt.config()).contains("proxy-token"));
}

#[tokio::test]
async fn warm_resolves_the_host_and_is_repeatable() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |_| {});
    stt.warm().await.unwrap();
    stt.warm().await.unwrap();
    assert_eq!(server.connections(), 0, "warm opens no connection");
}

#[tokio::test]
async fn an_invalid_config_is_refused_before_any_connection() {
    let server = start_python(Mock::default()).await;
    let stt = adapter(&server, |c| c.base_url = "http://nope".to_owned());
    assert!(matches!(
        stt.connect(stt_cfg()).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------------------------
// The provider contract suite
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
    provider: Arc<SherpaOnnxStt>,
}

impl Harness {
    async fn start() -> Self {
        let script = Arc::new(Mutex::new(Script::default()));
        let s = Arc::clone(&script);
        let server = MockServer::start(handler(move |conn| scripted(conn, Arc::clone(&s)))).await;
        let provider = Arc::new(adapter(&server, |c| {
            c.io_timeout = CONTRACT_DEADLINE;
            c.finalize_timeout = Duration::from_millis(400);
            c.close_timeout = Duration::from_millis(300);
            // The suite's timeout checks expect a silent provider to be reported as
            // Core(Timeout) at the deadline, and a drop as Disconnected: no reconnect.
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
    type P = SherpaOnnxStt;

    fn provider(&self) -> Arc<SherpaOnnxStt> {
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

/// The scripted sherpa-onnx of the contract suite: on the first frame the interims (cumulative
/// hypotheses) and a repeat of the last, one repeat per later frame (the Python server answers
/// every decode step), and the finals joined as the answer to `Done`, then close. Faults count
/// the result messages sent; a `Stall` makes the server deaf (it does not even read), like a
/// hung process.
async fn scripted(conn: Conn, script: Arc<Mutex<Script>>) {
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
    let mut last = String::new();
    let mut first_frame = true;
    loop {
        match s.next().await {
            None => return,
            Some(What::Audio(_)) => {
                let mut messages: Vec<String> = Vec::new();
                if first_frame {
                    first_frame = false;
                    if let Some(u) = &utterance {
                        for interim in &u.interims {
                            last.clone_from(interim);
                            messages.push(py_message(interim, 0));
                        }
                    }
                }
                messages.push(py_message(&last, 0));
                for message in messages {
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
            Some(What::Text(text)) if text == "Done" => {
                let answer = utterance
                    .as_ref()
                    .map(|u| u.finals.join(" "))
                    .unwrap_or_default();
                s.send(py_message(&answer, 0)).await;
                s.close_with(1000, "").await;
                return;
            }
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn the_provider_contract_suite_passes() {
    let harness = Harness::start().await;
    let report = stt_contract(&harness).await;
    let failures: Vec<_> = report.failures().collect();
    assert!(failures.is_empty(), "contract failures: {failures:#?}");
    assert_eq!(report.checks.len(), 7);
}

// ---------------------------------------------------------------------------------------------
// Live (BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_URL)
// ---------------------------------------------------------------------------------------------

/// The samples of a 16 kHz mono PCM16 WAV fixture (44-byte header) between two sample indexes.
fn fixture_pcm(name: &str, from: usize, to: usize) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    let wav = std::fs::read(path).expect("fixture");
    wav[44 + from * 2..44 + to * 2].to_vec()
}

/// Word error rate of `hyp` against `reference` (see [`word_edits`]).
fn wer(reference: &str, hyp: &str) -> f64 {
    let (edits, words) = word_edits(reference, hyp);
    edits as f64 / words.max(1) as f64
}

/// Word edit distance and reference length of `hyp` against `reference`, both folded to
/// lowercase letters and digits (Turkish dotted/dotless i folded to ASCII, punctuation dropped).
fn word_edits(reference: &str, hyp: &str) -> (usize, usize) {
    let fold = |s: &str| -> Vec<String> {
        s.chars()
            .map(|c| match c {
                'İ' | 'I' | 'ı' => 'i',
                'ç' | 'Ç' => 'c',
                'ğ' | 'Ğ' => 'g',
                'ö' | 'Ö' => 'o',
                'ş' | 'Ş' => 's',
                'ü' | 'Ü' => 'u',
                c => c.to_ascii_lowercase(),
            })
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let (r, h) = (fold(reference), fold(hyp));
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    (prev[h.len()], r.len())
}

#[test]
fn the_wer_helper_counts_substitutions_insertions_and_deletions() {
    assert!(wer("bir iki üç", "Bir iki ÜÇ.").abs() < 1e-9);
    assert!((wer("bir iki üç dört", "bir iki") - 0.5).abs() < 1e-9);
    assert!((wer("bir iki", "bir kırk iki") - 0.5).abs() < 1e-9);
}

#[tokio::test]
async fn live_sherpa_onnx_streams_the_fixture_and_finalizes_per_turn() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_URL to run");
        return;
    }
    let Ok(url) = std::env::var("SHERPA_ONNX_URL") else {
        eprintln!("skipped: SHERPA_ONNX_URL is not set (e.g. ws://127.0.0.1:6006)");
        return;
    };
    // `tr` runs the Turkish fixture (seda model), anything else the English one.
    let language = std::env::var("SHERPA_ONNX_LANGUAGE").unwrap_or_else(|_| "en".to_owned());
    let (fixture, segments): (&str, [(usize, usize, &str); 2]) = if language == "tr" {
        (
            "tr_two_utterances.wav",
            [
                (
                    8000,
                    100_800,
                    "Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki oranlar üzerinden vergilendirildi.",
                ),
                (
                    124_800,
                    233_440,
                    "Zamanla, yeni nüfus yeni çevresine uyum sağlamaya başladıkça diğer popülasyondan gittikçe daha farklı görünmeye başlarlar.",
                ),
            ],
        )
    } else {
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/en_two_utterances.json"),
            )
            .expect("fixture json"),
        )
        .expect("fixture json");
        let seg = |i: usize| {
            let s = &json["segments"][i];
            (
                s["start"].as_u64().unwrap() as usize,
                s["end"].as_u64().unwrap() as usize,
                Box::leak(s["text"].as_str().unwrap().to_owned().into_boxed_str()) as &'static str,
            )
        };
        ("en_two_utterances.wav", [seg(0), seg(1)])
    };
    let mut cfg = SherpaOnnxConfig::new();
    cfg.base_url = url;
    let stt = SherpaOnnxStt::new(cfg, clock());
    stt.warm().await.expect("warm");
    let mut session = stt.connect(stt_cfg()).await.expect("connect");
    for (turn, (from, to, reference)) in segments.iter().enumerate() {
        let mut pcm = fixture_pcm(fixture, *from, *to);
        // The VAD would stop after about 600 ms of silence: the engine keeps sending audio.
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
                        text,
                        is_final: false,
                        ..
                    } => {
                        partials += 1;
                        first_partial.get_or_insert((started.elapsed(), text.to_string()));
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
            "live turn {turn}: {partials} partials, first partial {:?}, finalize latency {finalize_latency:?}",
            first_partial.as_ref().map(|(d, _)| *d)
        );
        eprintln!("  reference: {reference}");
        eprintln!("  heard:     {heard}");
        eprintln!("  WER (folded): {:.2}", wer(reference, &heard));
        assert!(!heard.trim().is_empty(), "no text for turn {turn}");
    }
}

/// Live: the end of a turn cut where the VAD would cut it (the speech and 300 ms of
/// silence, the Python server then pads 0.3 s) loses its last word without the adapter's
/// padding before `Done` ("... COPE WITH THE IMP"); with the default padding the word is whole.
#[tokio::test]
async fn live_sherpa_onnx_keeps_the_last_word_of_a_turn_with_the_default_padding() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_URL to run");
        return;
    }
    let Ok(url) = std::env::var("SHERPA_ONNX_URL") else {
        eprintln!("skipped: SHERPA_ONNX_URL is not set (e.g. ws://127.0.0.1:6006)");
        return;
    };
    if std::env::var("SHERPA_ONNX_LANGUAGE").as_deref() == Ok("tr") {
        eprintln!("skipped: the English fixture needs the English model");
        return;
    }
    let mut cfg = SherpaOnnxConfig::new();
    cfg.base_url = url;
    assert_eq!(cfg.finalize_padding, Duration::from_millis(300));
    let stt = SherpaOnnxStt::new(cfg, clock());
    let mut session = connect(&stt).await;
    // "The U.N. also hopes to finalize a fund ... to cope with the impacts."
    let mut pcm = fixture_pcm("en_two_utterances.wav", 8000, 106_720);
    pcm.extend(vec![0_u8; 9_600]);
    for chunk in pcm.chunks(640) {
        session
            .input
            .send(SttInput::Audio(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    session.input.send(SttInput::Finalize).await.unwrap();
    let heard = loop {
        match tokio::time::timeout(Duration::from_secs(10), session.events.recv())
            .await
            .expect("the Finalize must be answered")
        {
            Some(SttEvent::Transcript {
                text,
                from_finalize: true,
                ..
            }) => break text.to_string(),
            Some(SttEvent::Error(e)) => panic!("live session failed: {e}"),
            Some(_) => {}
            None => panic!("the events closed"),
        }
    };
    eprintln!("heard: {heard}");
    assert!(
        heard.contains("THE IMPACT"),
        "the last word was cut: {heard}"
    );
}

// ---------------------------------------------------------------------------------------------
// Live, Turkish, 16 kHz against 8 kHz mu-law phone audio
// (BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_TR_URL)
// ---------------------------------------------------------------------------------------------

/// One reference utterance cut from a Turkish fixture.
struct TrUtterance {
    file: String,
    index: usize,
    pcm16k: Vec<u8>,
    reference: String,
}

/// The utterances of `fixtures/tr_two_utterances` and, when present, `tr_long_*` (the larger
/// set is not tracked: `fixtures/large/` or the directory in `BRISVOX_FIXTURES_LARGE`).
fn tr_utterances() -> Vec<TrUtterance> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let large = std::env::var_os("BRISVOX_FIXTURES_LARGE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| fixtures.join("large"));
    let mut sources = vec![("tr_two_utterances".to_owned(), fixtures)];
    for n in 0..8 {
        let name = format!("tr_long_{n}");
        if large.join(format!("{name}.wav")).exists() {
            sources.push((name, large.clone()));
        }
    }
    let mut out = Vec::new();
    for (name, dir) in sources {
        let wav = std::fs::read(dir.join(format!("{name}.wav"))).expect("fixture wav");
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.json"))).expect("fixture json"),
        )
        .expect("fixture json");
        for (index, seg) in json["segments"]
            .as_array()
            .expect("segments")
            .iter()
            .enumerate()
        {
            let from = seg["start"].as_u64().expect("start") as usize;
            let to = seg["end"].as_u64().expect("end") as usize;
            out.push(TrUtterance {
                file: name.clone(),
                index,
                pcm16k: wav[44 + from * 2..44 + to * 2].to_vec(),
                reference: seg["text"].as_str().expect("text").to_owned(),
            });
        }
    }
    out
}

/// What a Twilio call does to 16 kHz speech: down to 8 kHz, G.711 mu-law, and back to PCM16 as
/// the engine hears it (the adapter then resamples 8 kHz to the model's rate).
fn phone_pcm(pcm16k: &[u8]) -> Vec<u8> {
    use brisvox_audio::{Codec, StreamResampler};
    let samples: Vec<i16> = pcm16k
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    let mut resampler =
        StreamResampler::new(SampleRate::Hz16000, SampleRate::Hz8000).expect("resampler");
    let mut narrow: Vec<i16> = Vec::new();
    let mut buf = vec![0_i16; resampler.max_output_len(320) + 4096];
    for chunk in samples.chunks(320) {
        let n = resampler.process(chunk, &mut buf).expect("resample");
        narrow.extend_from_slice(&buf[..n]);
    }
    let mut tail = vec![0_i16; resampler.max_flush_len() + 16];
    let n = resampler.flush(&mut tail).expect("flush");
    narrow.extend_from_slice(&tail[..n]);
    let mut wire = vec![0_u8; narrow.len()];
    Codec::Ulaw
        .encode_samples(&narrow, &mut wire)
        .expect("mu-law encode");
    let mut decoded = vec![0_i16; wire.len()];
    let n = Codec::Ulaw
        .decode_samples(&wire, &mut decoded)
        .expect("mu-law decode");
    decoded[..n].iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Streams `pcm` (PCM16 at `rate`) in real time in 20 ms frames, then 0.3 s of silence (what
/// the VAD leaves), then `Finalize`; returns the text of the turn and the milliseconds from
/// `Finalize` to the final transcript.
async fn tr_turn(session: &mut SttSession, pcm: &[u8], rate: SampleRate) -> (String, f64) {
    let frame_bytes = rate.hz() as usize / 50 * 2;
    let mut audio = pcm.to_vec();
    audio.extend(vec![0_u8; rate.hz() as usize * 3 / 10 * 2]);
    let mut finals: Vec<String> = Vec::new();
    for chunk in audio.chunks(frame_bytes) {
        session
            .input
            .send(SttInput::Audio(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(ev) = session.events.try_recv() {
            match ev {
                SttEvent::Transcript {
                    text,
                    is_final: true,
                    from_finalize: false,
                    ..
                } => finals.push(text.to_string()),
                SttEvent::Transcript { .. } => {}
                SttEvent::Error(e) => panic!("live session failed: {e}"),
            }
        }
    }
    let asked = Instant::now();
    session.input.send(SttInput::Finalize).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), session.events.recv())
            .await
            .expect("the Finalize must be answered")
        {
            Some(SttEvent::Transcript {
                text,
                is_final: true,
                from_finalize: true,
                ..
            }) => {
                let ms = asked.elapsed().as_secs_f64() * 1000.0;
                finals.push(text.to_string());
                return (finals.join(" "), ms);
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
    }
}

/// Turkish phone-quality speech recognition (the seda model): every Turkish fixture utterance at
/// 16 kHz as recorded and as phone audio (8 kHz, G.711 mu-law round trip, session configured for
/// 8 kHz so the adapter resamples to the server's rate). Prints WER (folded) and finalize
/// latency per utterance. Run: `BRISVOX_LIVE_TESTS=1 SHERPA_ONNX_TR_URL=ws://127.0.0.1:6006
/// cargo test -p brisvox-providers --features provider-sherpa-onnx --test sherpa_onnx
/// live_sherpa_onnx_turkish -- --nocapture`.
#[tokio::test]
async fn live_sherpa_onnx_turkish_16khz_against_8khz_mulaw_phone_audio() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_TR_URL to run");
        return;
    }
    let Ok(url) = std::env::var("SHERPA_ONNX_TR_URL") else {
        eprintln!("skipped: SHERPA_ONNX_TR_URL is not set (e.g. ws://127.0.0.1:6006)");
        return;
    };
    let utterances = tr_utterances();
    assert!(!utterances.is_empty());
    // Per path: (word edits, reference words, finalize ms sum, turns).
    let mut totals = [(0_usize, 0_usize, 0.0_f64, 0_usize); 2];
    let paths = [
        ("16k", SampleRate::Hz16000, false),
        ("8k-mulaw", SampleRate::Hz8000, true),
    ];
    let mut heard_all: Vec<[String; 2]> = vec![Default::default(); utterances.len()];
    let mut per_utt: Vec<[(usize, f64); 2]> = vec![Default::default(); utterances.len()];
    for (p, (label, rate, phone)) in paths.iter().enumerate() {
        let mut cfg = SherpaOnnxConfig::new();
        cfg.base_url = url.clone();
        let stt = SherpaOnnxStt::new(cfg, clock());
        stt.warm().await.expect("warm");
        let mut session = stt
            .connect(SttConfig {
                sample_rate: *rate,
                language: Some("tr".into()),
            })
            .await
            .expect("connect");
        for (i, u) in utterances.iter().enumerate() {
            let pcm = if *phone {
                phone_pcm(&u.pcm16k)
            } else {
                u.pcm16k.clone()
            };
            let (heard, ms) = tr_turn(&mut session, &pcm, *rate).await;
            let (edits, words) = word_edits(&u.reference, &heard);
            totals[p].0 += edits;
            totals[p].1 += words;
            totals[p].2 += ms;
            totals[p].3 += 1;
            per_utt[i][p] = (edits, ms);
            eprintln!("[{label}] {}#{}: {heard}", u.file, u.index);
            heard_all[i][p] = heard;
        }
    }
    let mut rows = vec![format!(
        "{:<22} {:>5} | {:>8} {:>12} | {:>8} {:>12}",
        "utterance", "words", "WER 16k", "finalize ms", "WER 8k", "finalize ms"
    )];
    for (i, u) in utterances.iter().enumerate() {
        let words = word_edits(&u.reference, "").1;
        rows.push(format!(
            "{:<22} {:>5} | {:>8.3} {:>12.1} | {:>8.3} {:>12.1}",
            format!("{}#{}", u.file, u.index),
            words,
            per_utt[i][0].0 as f64 / words.max(1) as f64,
            per_utt[i][0].1,
            per_utt[i][1].0 as f64 / words.max(1) as f64,
            per_utt[i][1].1,
        ));
    }
    let agg = |p: usize| totals[p].0 as f64 / totals[p].1.max(1) as f64;
    let mean_ms = |p: usize| totals[p].2 / totals[p].3.max(1) as f64;
    rows.push(format!(
        "{:<22} {:>5} | {:>8.3} {:>12.1} | {:>8.3} {:>12.1}",
        "TOTAL / mean",
        totals[0].1,
        agg(0),
        mean_ms(0),
        agg(1),
        mean_ms(1),
    ));
    eprintln!("\n{}", rows.join("\n"));
    for (i, u) in utterances.iter().enumerate() {
        assert!(
            !heard_all[i][0].trim().is_empty() && !heard_all[i][1].trim().is_empty(),
            "no text for {}#{}",
            u.file,
            u.index
        );
    }
    // Loose bounds: clean speech reads well, and phone audio must stay usable.
    assert!(agg(0) < 0.35, "16 kHz WER {:.3}", agg(0));
    assert!(agg(1) < 0.60, "8 kHz mu-law WER {:.3}", agg(1));
}
