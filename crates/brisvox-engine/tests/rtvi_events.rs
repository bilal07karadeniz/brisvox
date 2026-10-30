//! A Pipecat RTVI client sees the full event sequence of a call driven by the engine.
//!
//! Real sockets on 127.0.0.1 and the real clock: an `RtviServer` (default config: the bridge
//! switches the adapter's own speaking events off, the engine sends them) hands the call to
//! `run_call` with the simulator's mock providers and energy VAD, wired to the client through
//! the `RtviBridge`. The scripted browser speaks the wire format of `docs/providers/rtvi.md` with
//! its own small protobuf codec (binary frames holding a `pipecat.Frame`: RTVI JSON in `message`
//! (4) / `data` (1), audio in `audio` (2): audio 3, sample_rate 4, num_channels 5), streams
//! microphone audio in real time, and plays a call:
//!
//! 1. a spoken question, answered by a long response;
//! 2. a barge-in while the bot speaks, answered by a short response;
//! 3. noise (an empty STT final): a user turn opened and closed, nothing answered;
//! 4. a slow `client-message` and then a typed `send-text`, which does not wait for the slow
//!    reply, reaches the context as a user message and is answered (no user speaking events);
//! 5. a `client-message`, answered by the application's handler;
//! 6. `disconnect-bot`, which ends the call.
#![cfg(feature = "rtvi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)] // test helpers (see clippy.toml)

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock};
use brisvox_engine::rtvi::{ClientMessageHandler, RtviBridge, RtviBridgeConfig};
use brisvox_engine::sim::energy_vad_setup;
use brisvox_engine::{EngineConfig, InterruptionConfig, Offload, SessionBuilder, run_call};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::mock_stt::SttUtterance;
use brisvox_sim::session::SimSession;
use brisvox_transport::rtvi::{RtviConfig, RtviServer};
use brisvox_transport::websocket::ServerConfig;
use brisvox_transport::{SessionConfig, TransportSession};
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait (real time).
const WAIT: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

// --- a minimal protobuf codec, independent of the crates under test -------------------------

fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn pb_bytes(tag: u32, payload: &[u8], out: &mut Vec<u8>) {
    varint(u64::from(tag << 3 | 2), out);
    varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

fn pb_varint(tag: u32, value: u64, out: &mut Vec<u8>) {
    varint(u64::from(tag << 3), out);
    varint(value, out);
}

fn pb_message(json: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    pb_bytes(1, json.as_bytes(), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(4, &inner, &mut frame);
    frame
}

fn pb_audio(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let mut inner = Vec::new();
    pb_bytes(2, b"audio", &mut inner);
    pb_bytes(3, pcm, &mut inner);
    pb_varint(4, u64::from(sample_rate), &mut inner);
    pb_varint(5, 1, &mut inner);
    let mut frame = Vec::new();
    pb_bytes(2, &inner, &mut frame);
    frame
}

fn read_varint(buf: &[u8], at: &mut usize) -> u64 {
    let mut value = 0_u64;
    let mut shift = 0;
    loop {
        let byte = buf[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// The length-delimited fields of `buf` as `(tag, bytes)`; varints are skipped.
fn pb_fields(buf: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let key = read_varint(buf, &mut at);
        match key & 7 {
            0 => {
                read_varint(buf, &mut at);
            }
            2 => {
                let len = read_varint(buf, &mut at) as usize;
                fields.push(((key >> 3) as u32, buf[at..at + len].to_vec()));
                at += len;
            }
            other => panic!("unexpected wire type {other}"),
        }
    }
    fields
}

/// What the browser received.
#[derive(Debug, Clone)]
enum Incoming {
    Rtvi(Value),
    Audio,
}

fn decode(bytes: &[u8]) -> Incoming {
    let frame = pb_fields(bytes);
    assert_eq!(frame.len(), 1, "a Frame has exactly one variant");
    let (tag, inner) = &frame[0];
    match tag {
        4 => {
            let fields = pb_fields(inner);
            let json = &fields.iter().find(|(t, _)| *t == 1).expect("data").1;
            Incoming::Rtvi(serde_json::from_slice(json).expect("RTVI JSON"))
        }
        2 => Incoming::Audio,
        other => panic!("the server sent Frame field {other}, which the JS client cannot decode"),
    }
}

// --- the scripted browser --------------------------------------------------------------------

/// Everything the server sent, in order, plus a wake-up for waiters.
#[derive(Default)]
struct Received {
    messages: Mutex<Vec<Value>>,
    audio: Mutex<usize>,
    changed: Notify,
}

struct Browser {
    sink: Arc<tokio::sync::Mutex<SplitSink<Ws, Message>>>,
    received: Arc<Received>,
    /// The microphone sends speech until this instant, silence afterwards.
    speech_until: Arc<Mutex<Instant>>,
    next_id: u32,
}

/// 20 ms of 16 kHz PCM16: a 440 Hz tone (speech for the energy VAD) or silence.
fn mic_frame(speech: bool) -> Vec<u8> {
    (0..320)
        .flat_map(|n| {
            let v = if speech {
                (f64::from(n) / 16_000.0 * 440.0 * std::f64::consts::TAU).sin() * 6000.0
            } else {
                0.0
            };
            (v as i16).to_le_bytes()
        })
        .collect()
}

async fn read_loop(mut stream: SplitStream<Ws>, received: Arc<Received>) {
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Binary(bytes)) => match decode(&bytes) {
                Incoming::Rtvi(value) => received.messages.lock().unwrap().push(value),
                Incoming::Audio => *received.audio.lock().unwrap() += 1,
            },
            Ok(Message::Text(text)) => panic!("text frame from the server: {text}"),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
        received.changed.notify_waiters();
    }
    received.changed.notify_waiters();
}

async fn mic_loop(
    sink: Arc<tokio::sync::Mutex<SplitSink<Ws, Message>>>,
    speech_until: Arc<Mutex<Instant>>,
) {
    let speech = Bytes::from(pb_audio(&mic_frame(true), 16_000));
    let silence = Bytes::from(pb_audio(&mic_frame(false), 16_000));
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        #[allow(clippy::disallowed_methods)] // the browser's own clock, not engine logic
        let speaking = Instant::now() < *speech_until.lock().unwrap();
        let frame = if speaking { &speech } else { &silence };
        if sink
            .lock()
            .await
            .send(Message::Binary(frame.clone()))
            .await
            .is_err()
        {
            return;
        }
    }
}

impl Browser {
    async fn connect(url: &str) -> Self {
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
            .await
            .unwrap()
            .unwrap();
        let (sink, stream) = ws.split();
        let received = Arc::new(Received::default());
        tokio::spawn(read_loop(stream, Arc::clone(&received)));
        #[allow(clippy::disallowed_methods)]
        let now = Instant::now();
        Self {
            sink: Arc::new(tokio::sync::Mutex::new(sink)),
            received,
            speech_until: Arc::new(Mutex::new(now)),
            next_id: 0,
        }
    }

    /// Starts streaming the microphone (silence unless [`Self::speak`]).
    fn start_mic(&self) {
        tokio::spawn(mic_loop(
            Arc::clone(&self.sink),
            Arc::clone(&self.speech_until),
        ));
    }

    /// Speech for `duration` from now.
    fn speak(&self, duration: Duration) {
        #[allow(clippy::disallowed_methods)]
        let until = Instant::now() + duration;
        *self.speech_until.lock().unwrap() = until;
    }

    async fn rtvi(&mut self, kind: &str, data: Value) -> String {
        self.next_id += 1;
        let id = format!("{:08x}", 0xc0de_0000_u32 + self.next_id);
        let message = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data});
        self.sink
            .lock()
            .await
            .send(Message::Binary(Bytes::from(pb_message(
                &message.to_string(),
            ))))
            .await
            .unwrap();
        id
    }

    fn messages(&self) -> Vec<Value> {
        self.received.messages.lock().unwrap().clone()
    }

    /// Waits until a message at index `from` or later satisfies `pred`; returns its index.
    async fn wait_for(&self, from: usize, what: &str, pred: impl Fn(&Value) -> bool) -> usize {
        let wait = async {
            loop {
                let changed = self.received.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(at) = self
                    .messages()
                    .iter()
                    .enumerate()
                    .skip(from)
                    .find_map(|(n, m)| pred(m).then_some(n))
                {
                    return at;
                }
                changed.await;
            }
        };
        match tokio::time::timeout(WAIT, wait).await {
            Ok(at) => at,
            Err(_) => panic!("no {what} after #{from}: {:#?}", kinds(&self.messages())),
        }
    }

    async fn wait_type(&self, from: usize, kind: &str) -> usize {
        self.wait_for(from, kind, |m| m["type"] == kind).await
    }
}

fn kinds(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m["type"].as_str().unwrap() {
            kind @ ("user-transcription" | "user-llm-text" | "bot-tts-text") => {
                format!("{kind} {}", m["data"]["text"])
            }
            kind => kind.to_owned(),
        })
        .collect()
}

/// Asserts that `chain` occurs in order (not necessarily adjacent) in `messages[range]`; each
/// entry is a type, or a type and the `data.text` it must carry.
fn assert_chain(messages: &[Value], range: std::ops::Range<usize>, chain: &[(&str, Option<&str>)]) {
    let mut at = range.start;
    for (kind, text) in chain {
        let found = messages[at..range.end]
            .iter()
            .position(|m| m["type"] == *kind && text.is_none_or(|text| m["data"]["text"] == text));
        match found {
            Some(offset) => at += offset + 1,
            None => panic!(
                "{kind} {text:?} missing (in order) in #{}..#{}: {:#?}",
                range.start,
                range.end,
                kinds(&messages[range.clone()])
            ),
        }
    }
}

fn count(messages: &[Value], kind: &str) -> usize {
    messages.iter().filter(|m| m["type"] == kind).count()
}

// --- the call ---------------------------------------------------------------------------------

const LONG: [&str; 4] = [
    "This is a long answer ",
    "that keeps going for a while. ",
    "It has several sentences, ",
    "so that the caller can interrupt it.",
];

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 4,
        handshake_timeout: Duration::from_secs(2),
        start_timeout: Duration::from_secs(5),
        ping_interval: Duration::from_millis(500),
        ping_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(2),
        send_timeout: Duration::from_secs(2),
        close_grace: Duration::from_millis(500),
        ..ServerConfig::default()
    }
}

fn engine_config() -> EngineConfig {
    let mut config = EngineConfig::DEFAULT;
    config.lead_margin = Duration::ZERO;
    config.llm.retries = 0;
    config.interruption = InterruptionConfig::OFF;
    config.turn = brisvox_agent::TurnPolicy::fixed(Duration::from_millis(300));
    config
}

/// How long the `slow` client message takes to answer.
const SLOW: Duration = Duration::from_millis(1500);

/// The application's `client-message` handler: `ping` answers `pong`, `slow` answers `done`
/// after [`SLOW`] (a slow database call), anything else fails.
fn handler(clock: SharedClock) -> ClientMessageHandler {
    Arc::new(move |t: String, d: Option<Value>| {
        let clock = clock.clone();
        Box::pin(async move {
            match t.as_str() {
                "ping" => Ok(Some(json!({"pong": d}))),
                "slow" => {
                    clock.sleep(SLOW).await;
                    Ok(Some(json!("done")))
                }
                _ => Err(format!("unknown message {t}")),
            }
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pipecat_client_sees_the_full_event_sequence_of_a_call_with_one_barge_in() {
    let clock: SharedClock = SystemClock::shared();
    let cancel = CancellationToken::new();
    // The adapter's defaults (`auto_events` on): the bridge switches the derivation off for the
    // call, so the bot speaking events are not sent twice.
    let mut server = RtviServer::bind(
        server_config(),
        RtviConfig::default(),
        clock.clone(),
        &cancel,
    )
    .await
    .unwrap();
    let url = format!("ws://{}/rtvi", server.local_addr());

    // The browser connects and sends `client-ready` (its mic starts at once, as the JS client's).
    let mut browser = Browser::connect(&url).await;
    let ready_id = browser
        .rtvi(
            "client-ready",
            json!({"version": "2.1.0",
                   "about": {"library": "@pipecat-ai/client-js", "library_version": "1.13.1"}}),
        )
        .await;
    browser.start_mic();
    let call = tokio::time::timeout(WAIT, server.accept_call(&cancel))
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    // The engine: mock providers, energy VAD, the bridge.
    let sim = SimSession::new(clock.clone());
    sim.stt.push(SttUtterance::words("hello there"));
    sim.stt.push(SttUtterance {
        interims: vec![],
        finals: vec!["wait stop".to_owned()],
        language: None,
    });
    // A cough: the VAD hears speech, STT returns an empty final.
    sim.stt.push(SttUtterance {
        interims: vec![],
        finals: vec![],
        language: None,
    });
    sim.llm.push(LlmScript::text(LONG));
    sim.llm.push(LlmScript::text(["Okay, ", "I stopped."]));
    sim.llm.push(LlmScript::text(["It is noon."]));
    let config = engine_config();
    let offload = Offload::Inline(clock.clone());
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        brisvox_core::TimelineRecorder::new(),
        offload.clone(),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(energy_vad_setup(offload, &sim.vad, &config));
    let (builder, bridge) = RtviBridge::attach(
        builder,
        call.sender,
        call.inbox,
        RtviBridgeConfig::default(),
        Some(handler(clock.clone())),
        clock.clone(),
        &cancel,
    );
    let session =
        TransportSession::open(clock.clone(), call.link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    let mut call = run_call(session, builder, &cancel).await.unwrap();

    // Handshake.
    let ready = browser.wait_type(0, "bot-ready").await;
    assert_eq!(browser.messages()[ready]["id"], ready_id.as_str());

    // 1. A spoken question; the long answer starts playing.
    browser.speak(Duration::from_millis(600));
    let speaking = browser.wait_type(ready, "bot-started-speaking").await;
    let output = browser
        .wait_for(speaking, "bot-output", |m| m["type"] == "bot-output")
        .await;

    // 2. The caller barges in while the bot speaks.
    clock.sleep(Duration::from_millis(500)).await;
    let barge = browser.messages().len();
    browser.speak(Duration::from_millis(400));
    let interrupted = browser.wait_type(barge, "bot-interrupted").await;
    let answered = browser
        .wait_for(interrupted, "the answer to the barge-in", |m| {
            m["type"] == "bot-tts-text" && m["data"]["text"].as_str().unwrap().contains("stopped")
        })
        .await;
    let done = browser.wait_type(answered, "bot-stopped-speaking").await;

    // 3. Noise: a user turn without words is opened and closed, and nothing is answered.
    let noise = browser.messages().len();
    browser.speak(Duration::from_millis(400));
    let noise_started = browser.wait_type(noise, "user-started-speaking").await;
    let noise_done = browser
        .wait_type(noise_started, "user-stopped-speaking")
        .await;

    // 4. A slow app message, then a typed message: the typed one does not wait for the reply.
    let typed = browser.messages().len();
    let slow = browser.rtvi("client-message", json!({"t": "slow"})).await;
    browser
        .rtvi(
            "send-text",
            json!({"content": "what time is it", "options": {"run_immediately": true}}),
        )
        .await;
    let typed_text = browser
        .wait_for(typed, "the typed user-llm-text", |m| {
            m["type"] == "user-llm-text" && m["data"]["text"] == "what time is it"
        })
        .await;
    let typed_done = browser.wait_type(typed_text, "bot-stopped-speaking").await;
    let slow_reply = browser
        .wait_for(typed, "the slow server-response", |m| {
            m["id"] == slow.as_str()
        })
        .await;
    assert!(
        typed_text < slow_reply,
        "the typed message waited for the slow handler: {:#?}",
        kinds(&browser.messages()[typed..])
    );
    assert_eq!(browser.messages()[slow_reply]["data"]["d"], "done");

    // 5. An app message and its answer.
    let request = browser
        .rtvi("client-message", json!({"t": "ping", "d": {"n": 1}}))
        .await;
    let response = browser
        .wait_for(typed_done.max(slow_reply), "server-response", |m| {
            m["id"] == request.as_str()
        })
        .await;
    let messages = browser.messages();
    assert_eq!(messages[response]["type"], "server-response");
    assert_eq!(
        messages[response]["data"],
        json!({"t": "ping", "d": {"pong": {"n": 1}}})
    );
    let unknown = browser.rtvi("client-message", json!({"t": "nope"})).await;
    let error = browser
        .wait_for(response, "error-response", |m| m["id"] == unknown.as_str())
        .await;
    assert_eq!(browser.messages()[error]["type"], "error-response");

    // 6. The client ends the call.
    browser.rtvi("disconnect-bot", json!({})).await;
    tokio::time::timeout(WAIT, call.wait())
        .await
        .expect("the call ends after disconnect-bot")
        .unwrap();

    let messages = browser.messages();
    // Phase 1: the spoken question and the start of the long answer, in Pipecat's order.
    assert_chain(
        &messages,
        ready..output + 1,
        &[
            ("user-started-speaking", None),
            ("user-transcription", Some("hello there")),
            ("user-llm-text", Some("hello there")),
            ("user-stopped-speaking", None),
            ("bot-llm-started", None),
            ("bot-llm-text", Some(LONG[0])),
            ("bot-tts-started", None),
            ("bot-tts-text", None),
            ("bot-started-speaking", None),
            ("bot-output", None),
        ],
    );
    let final_transcript = messages[ready..speaking]
        .iter()
        .find(|m| m["type"] == "user-transcription" && m["data"]["final"] == true)
        .expect("a final user transcription");
    assert_eq!(final_transcript["data"]["user_id"], "");
    assert!(
        final_transcript["data"]["timestamp"]
            .as_str()
            .unwrap()
            .ends_with("+00:00")
    );
    assert!(
        messages[speaking..barge]
            .iter()
            .any(|m| m["type"] == "metrics" && m["data"]["ttfb"].is_array()),
        "the turn's metrics follow the bot's first audio: {:#?}",
        kinds(&messages[speaking..barge])
    );
    // The long answer was not finished when the caller barged in.
    assert!(
        !messages[speaking..interrupted]
            .iter()
            .any(|m| m["type"] == "bot-stopped-speaking"),
        "the bot was still speaking at the barge-in"
    );

    // Phase 2: the barge-in. The interruption comes first, the bot stops, then the user's turn
    // and the new answer; nothing of the old answer follows the interruption.
    assert_chain(
        &messages,
        interrupted..done + 1,
        &[
            ("bot-interrupted", None),
            ("bot-stopped-speaking", None),
            ("user-started-speaking", None),
            ("user-transcription", Some("wait stop")),
            ("user-llm-text", Some("wait stop")),
            ("user-stopped-speaking", None),
            ("bot-llm-started", None),
            ("bot-tts-started", None),
            ("bot-started-speaking", None),
            ("bot-stopped-speaking", None),
        ],
    );
    for message in &messages[interrupted..] {
        let text = message["data"]["text"].as_str().unwrap_or_default();
        assert!(
            !LONG.iter().any(|part| text.contains(part.trim())) || text.is_empty(),
            "old answer after the interruption: {message}"
        );
    }

    // Phase 3: the noise opened and closed a user turn, and nothing answered it.
    for kind in ["user-llm-text", "bot-llm-started", "bot-started-speaking"] {
        assert_eq!(
            count(&messages[noise..typed], kind),
            0,
            "{kind} after noise: {:#?}",
            kinds(&messages[noise..typed])
        );
    }
    assert!(noise_started < noise_done);

    // Phase 4: the typed message is a user turn of its own, answered with speech. It is not
    // speech: no user speaking events (Pipecat raises none for send-text either).
    assert_chain(
        &messages,
        typed..typed_done + 1,
        &[
            ("user-llm-text", Some("what time is it")),
            ("bot-llm-started", None),
            ("bot-llm-text", Some("It is noon.")),
            ("bot-tts-text", Some("It is noon.")),
            ("bot-started-speaking", None),
            ("bot-output", None),
            ("bot-stopped-speaking", None),
        ],
    );
    for kind in [
        "user-transcription",
        "user-started-speaking",
        "user-stopped-speaking",
    ] {
        assert_eq!(
            count(&messages[typed..typed_done], kind),
            0,
            "nothing was spoken: {kind}"
        );
    }
    let requests = sim.llm.requests();
    assert_eq!(requests.len(), 3, "three answers");
    let last = format!("{:?}", requests[2].messages);
    assert!(
        last.contains("what time is it"),
        "the typed text is a user message in the context: {last}"
    );

    // The whole call: pairs balance, exactly one barge-in, no event lost.
    for (start, stop) in [
        ("user-started-speaking", "user-stopped-speaking"),
        ("bot-started-speaking", "bot-stopped-speaking"),
        ("bot-llm-started", "bot-llm-stopped"),
        ("bot-tts-started", "bot-tts-stopped"),
    ] {
        assert_eq!(
            count(&messages, start),
            count(&messages, stop),
            "{start} / {stop}: {:#?}",
            kinds(&messages)
        );
    }
    // The question, the barge-in and the noise; the typed message is not speech.
    assert_eq!(count(&messages, "user-started-speaking"), 3);
    assert_eq!(count(&messages, "bot-started-speaking"), 3);
    assert_eq!(count(&messages, "bot-interrupted"), 1);
    assert!(*browser.received.audio.lock().unwrap() > 0, "bot audio");
    let stats = bridge.stats();
    assert_eq!(stats.events_lost, 0, "{stats:?}");
    assert_eq!(stats.events_failed, 0, "{stats:?}");
    assert_eq!(stats.texts, 1, "{stats:?}");
    assert_eq!(stats.client_messages, 3, "{stats:?}");
    assert_eq!(stats.client_errors, 1, "{stats:?}");
    let driver = call.handles().driver.snapshot();
    assert_eq!(driver.text_turns, 1, "{driver:?}");
    assert_eq!(driver.interrupts, 1, "{driver:?}");
    assert_eq!(driver.empty_turns, 1, "{driver:?}");

    drop(call);
    server.shutdown(Duration::from_secs(2)).await.unwrap();
    cancel.cancel();
}
