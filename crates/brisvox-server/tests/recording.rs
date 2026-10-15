//! Call recording end to end: a mock call through the engine runner with the transcript
//! and audio sinks on, then the files it left. The typed user turn carries an email address and
//! a phone number, which must not reach the disk.
#![cfg(all(
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::resilience::{Resilience, ResilienceConfig};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::hosting::Listener;
use brisvox_server::{
    CallProviders, EngineRunner, HostSettings, ManagerConfig, Recording, RecordingSettings,
    ResilientProviders, RunnerConfig, Server, ServerSettings, ShutdownPolicy, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::rtvi::RtviConfig;
use brisvox_transport::rtvi::proto::{Frame, frame};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(10);

const AGENT: &str = r#"
[agent]
name = "recording-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "REC_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "REC_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "REC_TEST_TTS_KEY"
voice = "voice-123"
"#;

fn agent() -> AgentConfig {
    let keys: BTreeMap<String, String> = [
        ("REC_TEST_STT_KEY", "stt-key"),
        ("REC_TEST_LLM_KEY", "llm-key"),
        ("REC_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    config::load_str(AGENT, "agent.toml", &keys).unwrap()
}

// --- the RTVI client side (hand-encoded, as in tests/observability.rs) -------------------------

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

fn rtvi_message(kind: &str, id: &str, data: &Value) -> Message {
    let json = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data}).to_string();
    let mut inner = Vec::new();
    pb_bytes(1, json.as_bytes(), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(4, &inner, &mut frame);
    Message::binary(frame)
}

/// `Frame{ audio: AudioRawFrame{ name: "audio", audio, sample_rate: 16000, num_channels: 1 } }`.
fn rtvi_audio(pcm: &[u8]) -> Message {
    let mut inner = Vec::new();
    pb_bytes(2, b"audio", &mut inner);
    pb_bytes(3, pcm, &mut inner);
    varint(4 << 3, &mut inner);
    varint(16_000, &mut inner);
    varint(5 << 3, &mut inner);
    varint(1, &mut inner);
    let mut frame = Vec::new();
    pb_bytes(2, &inner, &mut frame);
    Message::binary(frame)
}

/// What an RTVI client gets.
enum FromBot {
    Rtvi(Value),
    /// Bot audio: its byte count.
    Audio(usize),
}

/// The next RTVI message or audio frame; `None` once the connection ended.
async fn next_from_bot(ws: &mut Client) -> Option<FromBot> {
    loop {
        let next = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no frame from the bot within the wait");
        let bytes = match next {
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        };
        match Frame::parse(bytes).unwrap().kind {
            Some(frame::Kind::Message(m)) => {
                return Some(FromBot::Rtvi(serde_json::from_str(&m.data).unwrap()));
            }
            Some(frame::Kind::Audio(a)) => return Some(FromBot::Audio(a.audio.len())),
            _ => {}
        }
    }
}

// --- the files -----------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("brisvox-rec-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The sample bytes of a finished WAV file and its rate.
fn wav(path: &Path) -> (u32, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
    assert_eq!(
        len,
        bytes.len() - 44,
        "{}: the header has the true length",
        path.display()
    );
    (rate, bytes[44..].to_vec())
}

/// Runs one call (audio in, a typed turn with PII, the bot's answer) against a server whose
/// runner records as `settings` say; returns the directory's file names once the call ended.
async fn record_one_call(settings: RecordingSettings) -> Vec<String> {
    let dir = settings.dir.clone();
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "rec-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let llm = MockLlm::new(clock.clone());
    llm.set_fallback(LlmScript::text(["Hello from the agent."]));
    let providers = ResilientProviders::new(
        Resilience::new(ResilienceConfig::DEFAULT, clock.clone()),
        CallProviders {
            stt: Arc::new(MockStt::new(clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(clock.clone())),
        },
    );
    let recording = Recording::new(settings, clock.clone()).unwrap();
    recording.prepare().await.unwrap();
    let runner = Arc::new(
        EngineRunner::new(
            agent(),
            providers,
            Offload::Pool(pool.handle()),
            Vad::Off,
            RunnerConfig::default(),
        )
        .unwrap()
        .with_recording(Arc::clone(&recording)),
    );
    let server_settings = ServerSettings {
        manager: ManagerConfig::new(4).unwrap(),
        health_addr: None,
        shutdown: ShutdownPolicy::default(),
        ..ServerSettings::default()
    };
    let host = HostSettings {
        rtvi: Some(Listener {
            addr: ([127, 0, 0, 1], 0).into(),
            config: RtviConfig::default(),
        }),
        ..HostSettings::none()
    };
    let server = Server::start_hosting(server_settings, &host, clock.clone(), &runner)
        .await
        .unwrap();

    let url = format!("ws://{}/rtvi", server.listener_addrs().rtvi.unwrap());
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    let ready = json!({"version": "2.1.0",
                       "about": {"library": "@pipecat-ai/client-js", "library_version": "1.13.1"}});
    ws.send(rtvi_message("client-ready", "ready-1", &ready))
        .await
        .unwrap();
    while let Some(from_bot) = next_from_bot(&mut ws).await {
        if matches!(&from_bot, FromBot::Rtvi(m) if m["type"] == "bot-ready") {
            break;
        }
    }
    // Caller audio: 25 frames of 20 ms, every byte 1.
    for _ in 0..25 {
        ws.send(rtvi_audio(&[1_u8; 640])).await.unwrap();
    }
    ws.send(rtvi_message(
        "send-text",
        "text-1",
        &json!({"content": "my email is jane@example.com and my number is 0532 123 45 67",
                "options": {"run_immediately": true}}),
    ))
    .await
    .unwrap();
    let (mut bot_text, mut audio_bytes) = (false, 0_usize);
    while !(bot_text && audio_bytes > 0) {
        match next_from_bot(&mut ws).await {
            Some(FromBot::Rtvi(m)) => {
                assert_ne!(m["type"], "error", "{m}");
                bot_text |= m["type"] == "bot-llm-text";
            }
            Some(FromBot::Audio(n)) => audio_bytes += n,
            None => panic!("the call ended before the bot answered"),
        }
    }
    ws.send(rtvi_message("disconnect-bot", "bye-1", &json!(null)))
        .await
        .unwrap();
    let ended = async {
        while server.manager().stats().ended.total() == 0 {
            clock.sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WAIT, ended)
        .await
        .expect("the call did not end");
    files_in(&dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_call_leaves_a_redacted_transcript_and_two_finished_wav_files() {
    let dir = temp_dir("full");
    let mut settings = RecordingSettings::new(&dir);
    settings.audio = true;
    let names = record_one_call(settings).await;

    assert_eq!(names.len(), 3, "{names:?}");
    let find = |suffix: &str| -> PathBuf {
        let name = names.iter().find(|n| n.ends_with(suffix)).unwrap();
        assert!(name.starts_with("call-"), "{name}");
        dir.join(name)
    };

    // The transcript: header, the typed turn redacted, the bot's answer, an end line.
    let raw = std::fs::read_to_string(find(".transcript.jsonl")).unwrap();
    assert!(!raw.contains("jane@example.com"), "{raw}");
    assert!(!raw.contains("0532"), "{raw}");
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "call");
    assert_eq!(lines[0]["agent"], "recording-test");
    assert_eq!(lines[0]["pii_redacted"], true);
    let user = lines
        .iter()
        .find(|l| l["type"] == "user")
        .expect("a user line");
    assert_eq!(user["text"], "my email is [EMAIL] and my number is [PHONE]");
    let bot = lines
        .iter()
        .find(|l| l["type"] == "bot")
        .expect("a bot line");
    // The client hung up while the answer was playing: the line holds what the engine settled
    // as heard (a prefix of the answer, all of it when the pacer got it out before the hang-up:
    // the first frame leaves at once), or nothing when it could not tell; never more.
    let text = bot["text"].as_str().unwrap();
    assert!(
        "Hello from the agent.".starts_with(text.trim_end()),
        "{bot}"
    );
    match bot["heard"].as_str().unwrap() {
        "unknown" => assert_eq!(text, "", "{bot}"),
        "partial" | "full" => assert!(!text.is_empty(), "{bot}"),
        other => panic!("heard = {other}: {bot}"),
    }
    let end = lines.last().unwrap();
    assert_eq!(end["type"], "end");
    // The summary of the call: numbers and the configured providers, no conversation text.
    let summary = &end["summary"];
    assert!(summary["duration_ms"].as_u64().unwrap() > 0, "{end}");
    assert!(summary["turns"].as_u64().unwrap() >= 1, "{end}");
    assert_eq!(summary["usage"]["llm"]["provider"], "openai");
    assert_eq!(summary["usage"]["stt"]["provider"], "deepgram");
    assert_eq!(summary["usage"]["tts"]["provider"], "cartesia");
    assert!(
        summary["usage"]["tts"]["characters"].as_u64().unwrap() > 0,
        "{end}"
    );
    assert!(summary["usage"]["stt"]["audio_seconds"].is_f64(), "{end}");
    assert!(
        !summary.to_string().contains("Hello from the agent"),
        "{end}"
    );

    // The caller's audio: 16 kHz, what the client sent, header finished.
    let (rate, caller) = wav(&find(".caller.wav"));
    assert_eq!(rate, 16_000);
    assert!(caller.len() <= 25 * 640, "not more than was sent");
    assert!(
        caller.iter().all(|&b| b == 1 || b == 0),
        "the client's samples (and silence)"
    );
    assert!(
        caller.iter().filter(|&&b| b == 1).count() >= 640,
        "audio, not just silence"
    );

    // The bot's audio: a finished WAV on the call's timeline.
    let (_, bot_audio) = wav(&find(".bot.wav"));
    assert!(!bot_audio.is_empty(), "bot audio was recorded");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_redaction_off_and_no_audio_only_the_transcript_is_written_as_spoken() {
    let dir = temp_dir("plain");
    let mut settings = RecordingSettings::new(&dir);
    settings.redact_pii = false;
    let names = record_one_call(settings).await;
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].ends_with(".transcript.jsonl"));
    let raw = std::fs::read_to_string(dir.join(&names[0])).unwrap();
    assert!(raw.contains("jane@example.com"), "{raw}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The settings come from the environment variables (as `main` reads them), the call
/// runs, and the directory holds only `.age` files that the age crate decrypts to the same
/// WAV files and transcript a plain recording has.
#[cfg(feature = "recording-encryption")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_call_leaves_only_age_files_that_decrypt_to_the_plain_recording() {
    use std::io::Read as _;

    use brisvox_server::privacy::{
        ENV_RECORDING_AGE_RECIPIENTS, ENV_RECORDING_AUDIO, ENV_RECORDING_DIR,
    };

    let dir = temp_dir("age");
    let identity = age::x25519::Identity::generate();
    let env: BTreeMap<String, String> = [
        (ENV_RECORDING_DIR, dir.to_str().unwrap().to_owned()),
        (ENV_RECORDING_AUDIO, "true".to_owned()),
        (
            ENV_RECORDING_AGE_RECIPIENTS,
            identity.to_public().to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let settings = RecordingSettings::from_env(&env).unwrap().unwrap();
    assert!(settings.age_recipients.is_some());
    let names = record_one_call(settings).await;

    // Three files, every one `<plain name>.age`; no plaintext sibling anywhere.
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.iter().all(|n| n.ends_with(".age")), "{names:?}");
    let decrypt = |suffix: &str| -> Vec<u8> {
        let name = names
            .iter()
            .find(|n| n.ends_with(&format!("{suffix}.age")))
            .unwrap_or_else(|| panic!("no {suffix} file in {names:?}"));
        let bytes = std::fs::read(dir.join(name)).unwrap();
        assert!(bytes.starts_with(b"age-encryption.org/v1\n"), "{name}");
        let decryptor = age::Decryptor::new(&bytes[..]).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    };

    // The transcript: intact JSON lines, redacted, with its end line.
    let raw = String::from_utf8(decrypt(".transcript.jsonl")).unwrap();
    assert!(!raw.contains("jane@example.com"), "{raw}");
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "call");
    assert_eq!(lines[0]["agent"], "recording-test");
    let user = lines.iter().find(|l| l["type"] == "user").unwrap();
    assert_eq!(user["text"], "my email is [EMAIL] and my number is [PHONE]");
    assert_eq!(lines.last().unwrap()["type"], "end");

    // The WAV files: the streaming header (an age stream cannot be patched afterwards, so the
    // length field stays at its streaming value) followed by samples.
    for suffix in [".caller.wav", ".bot.wav"] {
        let bytes = decrypt(suffix);
        assert_eq!(&bytes[0..4], b"RIFF", "{suffix}");
        assert_eq!(&bytes[8..12], b"WAVE", "{suffix}");
        assert_eq!(&bytes[36..40], b"data", "{suffix}");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            u32::MAX,
            "{suffix}: streaming length"
        );
        assert!(bytes.len() > 44, "{suffix}: samples follow the header");
    }
    let caller = decrypt(".caller.wav");
    assert_eq!(
        u32::from_le_bytes(caller[24..28].try_into().unwrap()),
        16_000
    );
    assert!(caller[44..].len() <= 25 * 640, "not more than was sent");
    assert!(
        caller[44..].iter().filter(|&&b| b == 1).count() >= 640,
        "the client's audio, not just silence"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
