//! Live test against the real Gemini Live API. Runs only with `BRISVOX_LIVE_TESTS=1` and
//! `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) in the environment (never printed); skipped otherwise.
//! Optional `GEMINI_LIVE_MODEL` overrides the model. This is the live check: real
//! speech from the FLEURS fixtures goes in at real time with provider-side turn detection, and
//! the test checks the tool round trip, a barge-in (the response ends `Cancelled`), a Turkish
//! turn, and prints the latencies and the event order that `docs/providers/gemini-live.md`
//! marks UNVERIFIED.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SystemClock};
use brisvox_providers::gemini_live::{GeminiLive, GeminiLiveConfig};
use brisvox_providers::llm::ToolSpec;
use brisvox_providers::{
    S2sConfig, S2sControl, S2sEvent, S2sProvider, S2sResponseStatus, S2sSession,
};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

/// 20 ms of 16 kHz mono PCM16.
const CHUNK_SAMPLES: usize = 320;

/// The PCM16 samples `start..end` of a fixture WAV (16 kHz mono, see `fixtures/*.json`).
fn fixture(name: &str, start: usize, end: usize) -> Vec<i16> {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).expect("fixture");
    let mut at = 12;
    let data = loop {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            break &bytes[at + 8..at + 8 + len];
        }
        at += 8 + len + (len & 1);
    };
    let samples: Vec<i16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    samples[start..end.min(samples.len())].to_vec()
}

fn pcm(samples: &[i16]) -> Bytes {
    Bytes::from(
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>(),
    )
}

/// Streams queued speech at real time, and silence whenever nothing is queued (the provider's
/// VAD needs the silence to end the turn). Returns when the session's audio channel closes or
/// the speech channel is dropped.
fn spawn_feeder(
    audio: mpsc::Sender<Bytes>,
) -> mpsc::Sender<(Vec<i16>, tokio::sync::oneshot::Sender<Instant>)> {
    let (tx, mut rx) = mpsc::channel::<(Vec<i16>, tokio::sync::oneshot::Sender<Instant>)>(4);
    tokio::spawn(async move {
        let silence = pcm(&[0; CHUNK_SAMPLES]);
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut speech: Vec<i16> = Vec::new();
        let mut at = 0;
        let mut done: Option<tokio::sync::oneshot::Sender<Instant>> = None;
        loop {
            tick.tick().await;
            if at >= speech.len() {
                if let Some(done) = done.take() {
                    let _ = done.send(Instant::now());
                }
                match rx.try_recv() {
                    Ok((next, notify)) => {
                        speech = next;
                        at = 0;
                        done = Some(notify);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            let chunk = if at < speech.len() {
                let end = (at + CHUNK_SAMPLES).min(speech.len());
                let chunk = pcm(&speech[at..end]);
                at = end;
                chunk
            } else {
                silence.clone()
            };
            if audio.send(chunk).await.is_err() {
                return;
            }
        }
    });
    tx
}

/// Queues `speech` and returns when its last chunk was sent.
async fn say(
    feeder: &mpsc::Sender<(Vec<i16>, tokio::sync::oneshot::Sender<Instant>)>,
    speech: Vec<i16>,
) -> tokio::sync::oneshot::Receiver<Instant> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    feeder.send((speech, tx)).await.expect("feeder alive");
    rx
}

fn short(event: &S2sEvent) -> String {
    match event {
        S2sEvent::SpeechStarted => "SpeechStarted".into(),
        S2sEvent::SpeechStopped => "SpeechStopped".into(),
        S2sEvent::ResponseStarted { .. } => "ResponseStarted".into(),
        S2sEvent::Audio { .. } => "Audio".into(),
        S2sEvent::AudioDone { .. } => "AudioDone".into(),
        S2sEvent::AssistantTranscript { is_final, .. } => {
            format!("AssistantTranscript(final={is_final})")
        }
        S2sEvent::UserTranscript { .. } => "UserTranscript".into(),
        S2sEvent::ToolCall { name, .. } => format!("ToolCall({name})"),
        S2sEvent::ResponseDone { status, .. } => format!("ResponseDone({status:?})"),
        S2sEvent::Warning { code, message } => format!("Warning({code}: {message})"),
        S2sEvent::SessionEnding { time_left } => format!("SessionEnding({time_left:?})"),
        S2sEvent::Error(e) => format!("Error({e})"),
    }
}

/// Collapses runs of the same event name for a readable order log.
fn push_order(order: &mut Vec<String>, name: String) {
    match order.last_mut() {
        Some(last) if last.starts_with(&name) => {
            if !last.ends_with('+') {
                last.push('+');
            }
        }
        _ => order.push(name),
    }
}

fn live_key() -> Option<String> {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
        return None;
    }
    let key = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|k| !k.is_empty()));
    if key.is_none() {
        eprintln!("skipped: GEMINI_API_KEY is not set");
    }
    key
}

/// `key_in_header`: send the key as `x-goog-api-key` on the upgrade instead of in the URL.
fn live_provider(key: String, key_in_header: bool) -> GeminiLive {
    let mut config = GeminiLiveConfig::new(Secret::new(key));
    config.key_in_header = key_in_header;
    if let Some(model) = std::env::var("GEMINI_LIVE_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
    {
        config.model = model;
    }
    config.io_timeout = Duration::from_secs(20);
    eprintln!(
        "live: model {}, key in header: {key_in_header}",
        config.model
    );
    GeminiLive::new(config, SystemClock::shared())
}

async fn open(provider: &GeminiLive, config: S2sConfig) -> S2sSession {
    let started = Instant::now();
    let session = timeout(Duration::from_secs(30), provider.connect(config))
        .await
        .expect("connect in time")
        .expect("session accepted");
    eprintln!("live: session set up in {:?}", started.elapsed());
    session
}

fn lookup_tool() -> ToolSpec {
    ToolSpec {
        name: Arc::from("lookup_country"),
        description: Arc::from("Facts about a country: capital, population, languages."),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"country":{"type":"string"}},"required":["country"]}"#,
        ),
    }
}

#[tokio::test]
async fn a_real_session_calls_a_tool_answers_and_is_barged_in() {
    let Some(key) = live_key() else { return };
    let provider = live_provider(key, false);
    let mut config = S2sConfig::new(
        "You are a voice assistant on a phone call. Whenever the caller mentions a country, \
         call lookup_country with that country's English name BEFORE you answer, then answer \
         in two or three spoken sentences using the result.",
    );
    config.tools = vec![lookup_tool()];
    config.input_rate = SampleRate::Hz16000;
    config.output_rate = SampleRate::Hz24000;
    config.input_transcription = true;
    let S2sSession {
        audio,
        control,
        mut events,
    } = open(&provider, config).await;
    let feeder = spawn_feeder(audio);

    // "The major religion in Moldova is Orthodox Christian." (fixtures/en_two_utterances.json)
    let moldova = fixture("en_two_utterances.wav", 130_720, 174_880);
    // The other, longer utterance of the same file: the barge-in.
    let barge = fixture("en_two_utterances.wav", 8_000, 106_720);
    let mut speech_end = say(&feeder, moldova).await;
    let mut spoken_at: Option<Instant> = None;

    let mut order = Vec::new();
    let mut user_text = String::new();
    let mut tool_call: Option<(String, String)> = None;
    let mut tool_sent_at: Option<Instant> = None;
    let mut first_audio_after_speech: Option<Duration> = None;
    let mut first_audio_after_tool: Option<Duration> = None;
    let mut answer_audio = 0usize;
    let mut answer_text = String::new();
    let mut barge_sent: Option<Instant> = None;
    let mut barge_end: Option<tokio::sync::oneshot::Receiver<Instant>> = None;
    let mut cancelled_after: Option<Duration> = None;
    let mut statuses = Vec::new();

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if spoken_at.is_none()
            && let Ok(at) = speech_end.try_recv()
        {
            spoken_at = Some(at);
        }
        let event = match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("the session closed; order so far: {order:?}"),
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "no end in 60 s; order: {order:?}"
                );
                continue;
            }
        };
        push_order(&mut order, short(&event));
        match event {
            S2sEvent::UserTranscript { text } => user_text.push_str(&text),
            S2sEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(&*name, "lookup_country");
                tool_call = Some((name.to_string(), arguments.to_string()));
                control
                    .send(S2sControl::ToolResult {
                        call_id,
                        output: Arc::from(
                            r#"{"country":"Moldova","capital":"Chisinau","population":"2.4 million","languages":["Romanian"]}"#,
                        ),
                    })
                    .await
                    .expect("control");
                control
                    .send(S2sControl::CreateResponse)
                    .await
                    .expect("control");
                tool_sent_at = Some(Instant::now());
            }
            S2sEvent::Audio { pcm, .. } => {
                if first_audio_after_speech.is_none()
                    && let Some(at) = spoken_at
                {
                    first_audio_after_speech = Some(at.elapsed());
                }
                if let Some(sent) = tool_sent_at {
                    if first_audio_after_tool.is_none() {
                        first_audio_after_tool = Some(sent.elapsed());
                    }
                    answer_audio += pcm.len();
                    // Barge in once about a second of the answer has arrived.
                    if barge_sent.is_none() && answer_audio >= 48_000 {
                        barge_end = Some(say(&feeder, barge.clone()).await);
                        barge_sent = Some(Instant::now());
                    }
                }
            }
            S2sEvent::AssistantTranscript { text, is_final, .. } => {
                if tool_sent_at.is_some() && !is_final {
                    answer_text.push_str(&text);
                }
            }
            S2sEvent::ResponseDone { status, .. } => {
                statuses.push(status.clone());
                if status == S2sResponseStatus::Cancelled
                    && let Some(sent) = barge_sent
                {
                    cancelled_after = Some(sent.elapsed());
                    break;
                }
                if barge_sent.is_none() && tool_sent_at.is_some() && answer_audio > 0 {
                    // The answer finished before a second of audio: barge into nothing.
                    break;
                }
            }
            S2sEvent::Error(e) => panic!("session error: {e}; order: {order:?}"),
            _ => {}
        }
    }
    drop(barge_end);
    drop(feeder);
    drop(control);

    eprintln!("live: event order {order:?}");
    eprintln!("live: user transcript {user_text:?}");
    eprintln!("live: tool call {tool_call:?}");
    eprintln!("live: speech end -> first audio {first_audio_after_speech:?}");
    eprintln!("live: tool result -> first answer audio {first_audio_after_tool:?}");
    eprintln!("live: answer text {answer_text:?}");
    eprintln!("live: response statuses {statuses:?}");
    eprintln!("live: barge-in -> Cancelled after {cancelled_after:?}");

    let (_, arguments) = tool_call.expect("the model called the tool");
    let parsed: serde_json::Value = serde_json::from_str(&arguments).expect("arguments are JSON");
    assert!(
        parsed["country"]
            .as_str()
            .is_some_and(|c| c.to_lowercase().contains("moldova")),
        "{arguments}"
    );
    assert!(answer_audio > 0, "no answer audio after the tool result");
    if barge_sent.is_some() {
        assert!(
            cancelled_after.is_some(),
            "the barge-in did not cancel the response; statuses {statuses:?}"
        );
    }
}

#[tokio::test]
async fn a_real_turkish_turn_gets_a_spoken_answer() {
    let Some(key) = live_key() else { return };
    let provider = live_provider(key, true);
    let mut config =
        S2sConfig::new("Sen bir telefon asistanısın. Türkçe, bir iki kısa cümleyle cevap ver.");
    config.input_rate = SampleRate::Hz16000;
    config.output_rate = SampleRate::Hz24000;
    config.input_transcription = true;
    let S2sSession {
        audio,
        control,
        mut events,
    } = open(&provider, config).await;
    let feeder = spawn_feeder(audio);
    // "Ürünlerimizin yüzde seksenlik bir bölümü ..." (fixtures/tr_two_utterances.json)
    let speech = fixture("tr_two_utterances.wav", 8_000, 100_800);
    // A system note mid-session (`clientContent` without turnComplete) must be accepted.
    control
        .send(S2sControl::Note {
            text: Arc::from("Arayanın adı Ada."),
        })
        .await
        .expect("control");
    let mut speech_end = say(&feeder, speech).await;
    let mut spoken_at = None;
    let mut first_audio = None;
    let mut audio_bytes = 0usize;
    let mut user_text = String::new();
    let mut answer = String::new();
    let mut order = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if spoken_at.is_none()
            && let Ok(at) = speech_end.try_recv()
        {
            spoken_at = Some(at);
        }
        let event = match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("the session closed; order so far: {order:?}"),
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "no answer in 45 s; order: {order:?}"
                );
                continue;
            }
        };
        push_order(&mut order, short(&event));
        match event {
            S2sEvent::UserTranscript { text } => user_text.push_str(&text),
            S2sEvent::Audio { pcm, .. } => {
                if first_audio.is_none() {
                    first_audio = spoken_at.map(|at: Instant| at.elapsed());
                }
                audio_bytes += pcm.len();
            }
            S2sEvent::AssistantTranscript { text, is_final, .. } => {
                if !is_final {
                    answer.push_str(&text);
                }
            }
            S2sEvent::ResponseDone { .. } => break,
            S2sEvent::Warning { code, message } => {
                panic!("warning {code}: {message}; order: {order:?}")
            }
            S2sEvent::Error(e) => panic!("session error: {e}; order: {order:?}"),
            _ => {}
        }
    }
    drop(feeder);
    drop(control);
    eprintln!("live tr: event order {order:?}");
    eprintln!("live tr: user transcript {user_text:?}");
    eprintln!("live tr: speech end -> first audio {first_audio:?}");
    eprintln!(
        "live tr: answer {answer:?} ({:.1} s of audio)",
        audio_bytes as f64 / 2.0 / 24_000.0
    );
    assert!(audio_bytes > 0, "no spoken answer");
}
