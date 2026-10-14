//! An 8 kHz mu-law WebSocket call through the production pipeline with a 24 kHz TTS.
//!
//! Real sockets on 127.0.0.1 and the real clock: a `WebSocketServer` hands the call (client
//! `start` with `sample_rate` 8000 and `encoding` `mulaw`, `docs/WEBSOCKET_PROTOCOL.md`) to
//! `run_call` with the simulator's mock providers and energy VAD. The mock TTS speaks at the
//! engine's 24 kHz provider rate (a 600 Hz square wave), so the bot audio has to be resampled to
//! the call's 8 kHz by the `TtsStage`'s `OutputFramer` and mu-law encoded by the transport. The
//! scripted client reads the wire as a phone would:
//!
//! 1. an answer played to its end: every audio frame is exactly 160 bytes (20 ms of 8 kHz
//!    mu-law), the frames add up to the TTS's audio duration (one frame of tolerance), they are
//!    paced in real time, the decoded audio is the 600 Hz tone (the resampled TTS, not noise or a
//!    wrong-rate stretch of it), and the server sent no error (no `output_rate_mismatch`);
//! 2. a barge-in in the middle of a long answer: the audio the client received before the
//!    `clear` is the heard position, and the next LLM request holds the answer cut after
//!    exactly the words the TTS had started by then (word timestamps and the played-text
//!    positions stay aligned at 8 kHz, the same oracle as `playout_oracle.rs`);
//! 3. the per-frame cost of the output path of such a call (resample 24 kHz to 8 kHz, frame,
//!    mu-law encode) against the 50 us rule of CONTRIBUTING.md.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::disallowed_methods // the scripted phone's own real clock, not engine logic
)] // test helpers (see clippy.toml)

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use brisvox_audio::codec::Codec;
use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SystemClock};
use brisvox_engine::sim::energy_vad_setup;
use brisvox_engine::{
    EngineConfig, InterruptionConfig, Offload, OutputFramer, SessionBuilder, run_call,
};
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::Role;
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::mock_stt::SttUtterance;
use brisvox_sim::session::SimSession;
use brisvox_transport::websocket::{ServerConfig, WebSocketServer};
use brisvox_transport::{SessionConfig, Transport, TransportSession};
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait (real time).
const WAIT: Duration = Duration::from_secs(15);

/// One frame of a 20 ms call.
const FRAME: Duration = Duration::from_millis(20);

/// Bytes of 20 ms of 8 kHz mu-law.
const FRAME_BYTES: usize = 160;

/// Audio per character of the mock TTS.
const MS_PER_CHAR: u64 = 40;

/// The mock TTS's tone: 20 samples high, 20 low at 24 kHz.
const TONE_HZ: f64 = 600.0;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

// --- the scripted phone -----------------------------------------------------------------------

/// What the phone received, in wire order.
#[derive(Debug, Clone)]
enum Received {
    /// A binary frame and when it arrived.
    Audio(Bytes, Instant),
    /// A text frame (a server message).
    Text(Value),
}

#[derive(Default)]
struct Wire {
    events: Mutex<Vec<Received>>,
}

impl Wire {
    fn snapshot(&self) -> Vec<Received> {
        self.events.lock().unwrap().clone()
    }

    /// Waits until `pred` holds for the received events.
    async fn until(&self, what: &str, pred: impl Fn(&[Received]) -> bool) {
        let started = Instant::now();
        loop {
            if pred(&self.events.lock().unwrap()) {
                return;
            }
            assert!(
                started.elapsed() < WAIT,
                "timed out waiting for {what}; received {} events",
                self.events.lock().unwrap().len()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn is_type(event: &Received, kind: &str) -> bool {
    matches!(event, Received::Text(v) if v["type"] == kind)
}

fn audio_frames(events: &[Received]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Received::Audio(..)))
        .count()
}

/// Reads the socket into `wire`; a `mark` is echoed at once (the server paces the output at real
/// time, so everything before the mark has been sent when it arrives).
async fn read_loop(
    mut stream: SplitStream<Ws>,
    sink: Arc<tokio::sync::Mutex<SplitSink<Ws, Message>>>,
    wire: Arc<Wire>,
) {
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Binary(bytes)) => {
                let now = Instant::now();
                wire.events
                    .lock()
                    .unwrap()
                    .push(Received::Audio(bytes, now));
            }
            Ok(Message::Text(text)) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "mark" {
                    let echo = json!({"type": "mark", "id": value["id"]});
                    let sent = sink
                        .lock()
                        .await
                        .send(Message::Text(echo.to_string().into()))
                        .await;
                    if sent.is_err() {
                        return;
                    }
                }
                wire.events.lock().unwrap().push(Received::Text(value));
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// 20 ms of 8 kHz mu-law: a 440 Hz tone (speech for the energy VAD) or silence.
fn mic_frame(speech: bool) -> Bytes {
    let pcm: Vec<u8> = (0..160)
        .flat_map(|n| {
            let v = if speech {
                (f64::from(n) / 8_000.0 * 440.0 * std::f64::consts::TAU).sin() * 6000.0
            } else {
                0.0
            };
            (v as i16).to_le_bytes()
        })
        .collect();
    let mut out = vec![0_u8; FRAME_BYTES];
    let written = Codec::Ulaw.encode(&pcm, &mut out).unwrap();
    assert_eq!(written, FRAME_BYTES);
    Bytes::from(out)
}

async fn mic_loop(
    sink: Arc<tokio::sync::Mutex<SplitSink<Ws, Message>>>,
    speech_until: Arc<Mutex<Instant>>,
) {
    let speech = mic_frame(true);
    let silence = mic_frame(false);
    let mut tick = tokio::time::interval(FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let speaking = Instant::now() < *speech_until.lock().unwrap();
        let frame = if speaking { &speech } else { &silence };
        let sent = sink.lock().await.send(Message::Binary(frame.clone())).await;
        if sent.is_err() {
            return;
        }
    }
}

struct Phone {
    wire: Arc<Wire>,
    speech_until: Arc<Mutex<Instant>>,
    sink: Arc<tokio::sync::Mutex<SplitSink<Ws, Message>>>,
}

impl Phone {
    /// Connects, sends `start` (8 kHz mu-law) and waits for `started`.
    async fn connect(url: &str) -> Self {
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
            .await
            .unwrap()
            .unwrap();
        let (sink, stream) = ws.split();
        let sink = Arc::new(tokio::sync::Mutex::new(sink));
        let start = json!({"type": "start", "version": 1, "sample_rate": 8000,
                           "encoding": "mulaw"});
        sink.lock()
            .await
            .send(Message::Text(start.to_string().into()))
            .await
            .unwrap();
        let wire = Arc::new(Wire::default());
        tokio::spawn(read_loop(stream, Arc::clone(&sink), Arc::clone(&wire)));
        wire.until("started", |e| e.iter().any(|m| is_type(m, "started")))
            .await;
        Self {
            wire,
            speech_until: Arc::new(Mutex::new(Instant::now())),
            sink,
        }
    }

    fn start_mic(&self) {
        tokio::spawn(mic_loop(
            Arc::clone(&self.sink),
            Arc::clone(&self.speech_until),
        ));
    }

    /// Speech for `duration` from now.
    fn speak(&self, duration: Duration) {
        *self.speech_until.lock().unwrap() = Instant::now() + duration;
    }
}

// --- the engine side --------------------------------------------------------------------------

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 2,
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

/// A running 8 kHz mu-law call: the phone, the mocks the test scripts, the engine's call.
struct Rig {
    phone: Phone,
    sim: SimSession,
    call: brisvox_engine::Call,
    cancel: CancellationToken,
    _server: WebSocketServer,
    /// The inference pool of a call started with [`Rig::start_pooled`] (joined at hang-up).
    pool: Option<InferencePool>,
}

impl Rig {
    async fn start(sim: SimSession, clock: SharedClock) -> Self {
        let offload = Offload::Inline(clock.clone());
        Self::start_with(sim, clock, offload, None).await
    }

    /// Like [`Rig::start`], but the VAD and the TTS stage's resampler build run on a real
    /// `brisvox-inference` pool, as in production.
    async fn start_pooled(sim: SimSession, clock: SharedClock) -> Self {
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 2,
                queue_capacity: 64,
                name: "ws-mulaw".to_owned(),
            },
            clock.clone(),
        )
        .unwrap();
        let offload = Offload::Pool(pool.handle());
        Self::start_with(sim, clock, offload, Some(pool)).await
    }

    async fn start_with(
        sim: SimSession,
        clock: SharedClock,
        offload: Offload,
        pool: Option<InferencePool>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let mut server = WebSocketServer::bind(server_config(), clock.clone(), &cancel)
            .await
            .unwrap();
        let url = format!("ws://{}/call", server.local_addr());
        let phone = Phone::connect(&url).await;
        let link = tokio::time::timeout(WAIT, server.accept(&cancel))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let config = engine_config();
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
        let session = TransportSession::open(clock, link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
        let call = run_call(session, builder, &cancel).await.unwrap();
        phone.start_mic();
        Self {
            phone,
            sim,
            call,
            cancel,
            _server: server,
            pool,
        }
    }

    /// The phone hangs up and the engine's call ends cleanly.
    async fn hang_up(mut self) {
        self.phone
            .sink
            .lock()
            .await
            .send(Message::Text(json!({"type": "stop"}).to_string().into()))
            .await
            .unwrap();
        tokio::time::timeout(WAIT, self.call.wait())
            .await
            .expect("the call ends after the phone's stop")
            .unwrap();
        self.cancel.cancel();
        if let Some(pool) = self.pool.take() {
            tokio::task::spawn_blocking(move || pool.shutdown())
                .await
                .unwrap();
        }
    }
}

fn sim_session(clock: &SharedClock) -> SimSession {
    let sim = SimSession::new(clock.clone());
    // 40 ms of audio per character in 40 ms chunks (24 periods of the tone each, so the chunks
    // join without a phase jump).
    let _ = sim
        .tts
        .clone()
        .with_audio(MS_PER_CHAR, Duration::from_millis(40));
    sim
}

/// The text the mock TTS was sent, in order: the audio it made is `MS_PER_CHAR` per character
/// of it, and a word starts at its character offset times that.
fn spoken_text(sim: &SimSession) -> String {
    sim.tts.texts().into_iter().map(|(_, text)| text).collect()
}

/// Start offsets of the words of `text` in the TTS's audio.
fn word_starts(text: &str) -> Vec<Duration> {
    let mut starts = Vec::new();
    let mut after_space = true;
    for (i, c) in text.chars().enumerate() {
        if !c.is_whitespace() && after_space {
            starts.push(Duration::from_millis(i as u64 * MS_PER_CHAR));
        }
        after_space = c.is_whitespace();
    }
    starts
}

// --- audio analysis ---------------------------------------------------------------------------

/// The amplitude of the `hz` component of `samples` (8 kHz), by the Goertzel algorithm.
fn amplitude_at(samples: &[f64], hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / 8_000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0_f64, 0.0_f64);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / samples.len() as f64
}

fn decode(frames: &[&Bytes]) -> Vec<f64> {
    let mut mulaw = Vec::new();
    for frame in frames {
        mulaw.extend_from_slice(frame);
    }
    let mut pcm = vec![0_u8; Codec::Ulaw.decoded_len(mulaw.len())];
    Codec::Ulaw.decode(&mulaw, &mut pcm).unwrap();
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|b| f64::from(i16::from_le_bytes(*b)))
        .collect()
}

// --- the tests --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_8k_mulaw_call_gets_exact_20ms_frames_of_the_resampled_24k_tts() {
    let clock: SharedClock = SystemClock::shared();
    let sim = sim_session(&clock);
    sim.stt.push(SttUtterance::words("hello there"));
    sim.llm.push(LlmScript::text([
        "Hello there, this is a short answer ",
        "that plays out completely.",
    ]));
    // On the inference pool, as in production: the 24 kHz to 8 kHz resampler is built there.
    let rig = Rig::start_pooled(sim, clock).await;
    let wire = Arc::clone(&rig.phone.wire);

    rig.phone.speak(Duration::from_millis(600));
    // The audio, then the end-of-utterance mark: everything before it is on the wire.
    wire.until("the answer's mark", |e| {
        e.iter().any(|m| is_type(m, "mark"))
    })
    .await;

    let events = wire.snapshot();
    let frames: Vec<(&Bytes, Instant)> = events
        .iter()
        .filter_map(|e| match e {
            Received::Audio(b, at) => Some((b, *at)),
            Received::Text(_) => None,
        })
        .collect();

    // The call was opened as the client asked, and the server had nothing to complain about.
    let started = events.iter().find(|m| is_type(m, "started")).unwrap();
    if let Received::Text(started) = started {
        assert_eq!(started["sample_rate"], 8000);
        assert_eq!(started["encoding"], "mulaw");
    }
    for event in &events {
        assert!(
            !is_type(event, "error"),
            "the server reported a fault (output_rate_mismatch?): {event:?}"
        );
    }

    // Every frame is exactly 20 ms of 8 kHz mu-law.
    assert!(!frames.is_empty(), "no bot audio reached the phone");
    for (i, (frame, _)) in frames.iter().enumerate() {
        assert_eq!(frame.len(), FRAME_BYTES, "frame {i}");
    }

    // The frames add up to the TTS's audio (24 kHz in, 8 kHz out: the duration is unchanged),
    // give or take the resampler's edge frame.
    let spoken = spoken_text(&rig.sim);
    let expected = spoken.chars().count() as u64 * MS_PER_CHAR / FRAME.as_millis() as u64;
    let got = frames.len() as u64;
    assert!(
        got.abs_diff(expected) <= 1,
        "{got} frames of 20 ms for {expected} frames of TTS audio"
    );

    // Paced in real time by the transport (not dumped at once): the arrival span covers most
    // of the audio's duration.
    let span = frames[frames.len() - 1].1 - frames[0].1;
    let audio = FRAME * u32::try_from(frames.len() - 1).unwrap();
    assert!(
        span >= audio.mul_f64(0.8) && span <= audio + Duration::from_secs(1),
        "{} frames arrived over {span:?}, real time is {audio:?}",
        frames.len()
    );

    // The content is the TTS's tone at the call's rate: the 600 Hz fundamental of the square
    // wave (4/pi of its 1000 amplitude) dominates, and nothing sits between the harmonics.
    let steady: Vec<&Bytes> = frames[5..frames.len() - 5].iter().map(|f| f.0).collect();
    let pcm = decode(&steady);
    let fundamental = amplitude_at(&pcm, TONE_HZ);
    let expected_amplitude = 1_000.0 * 4.0 / std::f64::consts::PI;
    assert!(
        (0.8 * expected_amplitude..1.2 * expected_amplitude).contains(&fundamental),
        "600 Hz amplitude {fundamental:.0}, expected about {expected_amplitude:.0}"
    );
    for off in [300.0, 450.0, 750.0, 900.0, 1_200.0, 2_400.0] {
        let other = amplitude_at(&pcm, off);
        assert!(
            other < 0.06 * fundamental,
            "{off} Hz amplitude {other:.0} against the tone's {fundamental:.0}"
        );
    }
    // The third harmonic (1800 Hz, below the 4 kHz Nyquist) survives; the filter does not eat
    // the band, and the 24 kHz audio was not played at a wrong rate (that would move the tone).
    let third = amplitude_at(&pcm, 3.0 * TONE_HZ);
    assert!(
        (0.2 * fundamental..0.5 * fundamental).contains(&third),
        "1800 Hz amplitude {third:.0} against the tone's {fundamental:.0}"
    );

    // The 24 kHz to 8 kHz resampler was built on the pool, and every build worked.
    let tts = rig.call.stats().stages.tts;
    assert!(tts.resampler_builds >= 1, "{tts:?}");
    assert_eq!(tts.resampler_failures, 0, "{tts:?}");

    rig.hang_up().await;
}

/// The assistant messages of an LLM request.
fn assistant_texts(request: &brisvox_providers::LlmRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_barge_in_on_an_8k_mulaw_call_keeps_exactly_the_words_the_phone_heard() {
    const STORY: [&str; 3] = [
        "Here is a very long answer, ",
        "with many words, punctuation! ",
        "and it keeps going on and on and on and on and on.",
    ];
    let clock: SharedClock = SystemClock::shared();
    let sim = sim_session(&clock);
    sim.stt.push(SttUtterance::words("tell me everything"));
    sim.stt.push(SttUtterance::words("stop please"));
    sim.llm.push(LlmScript::text(STORY));
    sim.llm.push(LlmScript::text(["Sure."]));
    let rig = Rig::start(sim, clock).await;
    let wire = Arc::clone(&rig.phone.wire);

    // The long answer starts; 1.2 s of it (60 frames) reach the phone; then it barges in.
    rig.phone.speak(Duration::from_millis(600));
    wire.until("1.2 s of the answer", |e| audio_frames(e) >= 60)
        .await;
    rig.phone.speak(Duration::from_millis(500));
    wire.until("the clear", |e| e.iter().any(|m| is_type(m, "clear")))
        .await;
    // The reply to the interruption plays, its mark ends the call's audio.
    wire.until("the reply's mark", |e| {
        let after_clear = e.iter().skip_while(|m| !is_type(m, "clear"));
        after_clear.filter(|m| is_type(m, "mark")).count() >= 1
    })
    .await;

    let events = wire.snapshot();
    // Frames before the first clear: the position the phone was at.
    let clear_at = events.iter().position(|m| is_type(m, "clear")).unwrap();
    let before_clear = audio_frames(&events[..clear_at]);
    assert!(before_clear >= 60, "{before_clear} frames before the clear");
    for event in &events {
        match event {
            Received::Audio(b, _) => assert_eq!(b.len(), FRAME_BYTES),
            Received::Text(_) => assert!(!is_type(event, "error"), "{event:?}"),
        }
    }
    // Nothing of the old answer after the clear: the reply is the only audio after it, and it
    // is short ("Sure." is 5 characters, 10 frames).
    let after_clear = audio_frames(&events[clear_at..]);
    assert!(
        (1..=12).contains(&after_clear),
        "{after_clear} frames after the clear, the reply is about 10"
    );

    // The oracle: audio the phone received before the clear is the heard position (the engine
    // runs with a zero lead margin); the transport reports positions every 20 ms, so one frame
    // of tolerance either way.
    let spoken = spoken_text(&rig.sim);
    let starts = word_starts(&spoken);
    let heard_at = |p: Duration| starts.iter().filter(|s| **s < p).count();
    let position = FRAME * u32::try_from(before_clear).unwrap();
    let lo = heard_at(position.saturating_sub(FRAME));
    let hi = heard_at(position + FRAME);

    // The next turn's LLM request holds what was heard.
    let requests = rig.sim.llm.requests();
    assert_eq!(requests.len(), 2, "one request per user turn");
    let assistants = assistant_texts(&requests[1]);
    assert_eq!(assistants.len(), 1, "{assistants:?}");
    let kept = &assistants[0];
    let full: String = STORY.concat();
    let words = kept.split_whitespace().count();
    assert!(
        (lo..=hi).contains(&words),
        "the context kept {words} words ({kept:?}); the phone heard {position:?} = words {lo}..={hi}"
    );
    assert!(
        full.starts_with(kept.as_str()),
        "{kept:?} is not a prefix of the answer"
    );
    // The cut is a real cut: the old answer is not in the context whole.
    assert!(words < full.split_whitespace().count(), "{kept:?}");

    rig.hang_up().await;
}

/// Median of `op` over `n` single calls (a tenth of the calls, 50 at least, in a debug build).
fn median_of(n: usize, mut op: impl FnMut()) -> Duration {
    let n = if cfg!(debug_assertions) {
        (n / 10).max(50)
    } else {
        n
    };
    for _ in 0..n / 10 {
        op();
    }
    let mut best = Duration::MAX;
    // Best of three runs, so one scheduling hiccup on a loaded machine does not count.
    for _ in 0..3 {
        let mut times: Vec<Duration> = (0..n)
            .map(|_| {
                let start = Instant::now();
                op();
                start.elapsed()
            })
            .collect();
        times.sort();
        best = best.min(times[times.len() / 2]);
    }
    best
}

#[test]
fn the_per_frame_output_path_of_an_8k_mulaw_call_stays_within_50_us() {
    // The TTS stage's reader: a 40 ms chunk of 24 kHz audio (the mock's, a typical provider
    // chunk) through the resampler and framer gives two 20 ms frames of 8 kHz PCM16 ...
    let resampler = OutputFramer::build_resampler(SampleRate::Hz24000, SampleRate::Hz8000).unwrap();
    let mut framer = OutputFramer::with_resampler(SampleRate::Hz8000, 160, resampler);
    let chunk: Vec<u8> = (0..960_i32)
        .flat_map(|i| (if (i / 20) % 2 == 0 { 1_000_i16 } else { -1_000 }).to_le_bytes())
        .collect();
    let mut frames: Vec<Bytes> = Vec::new();
    let push = median_of(2_000, || {
        frames.clear();
        framer.push(&chunk, &mut |f| frames.push(f)).unwrap();
        std::hint::black_box(&frames);
    });
    assert!(!frames.is_empty() && frames.iter().all(|f| f.len() == 2 * 160));
    let per_frame_push = push / 2;

    // ... and the transport's connection encodes each frame to mu-law.
    let pcm = vec![0x10_u8; 2 * 160];
    let mut out = vec![0_u8; FRAME_BYTES];
    // (A single encode is below the clock's resolution: time 100 frames and divide.)
    let encode = median_of(1_000, || {
        for _ in 0..100 {
            std::hint::black_box(Codec::Ulaw.encode(&pcm, &mut out).unwrap());
        }
    }) / 100;

    let total = per_frame_push + encode;
    println!(
        "8 kHz mu-law output, per 20 ms frame: resample+frame {per_frame_push:?}, mu-law {encode:?}, \
         together {total:?}"
    );
    // Release: the 50 us rule. Debug (the quality gate): 100 times slower DSP, so only a gross
    // regression is caught (the same policy as hot_path_cost.rs).
    let bound = if cfg!(debug_assertions) {
        Duration::from_millis(10)
    } else {
        Duration::from_micros(50)
    };
    assert!(total < bound, "per frame {total:?} against {bound:?}");
}
