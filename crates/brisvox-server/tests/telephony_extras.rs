//! The telephony extras of the binary, end to end through the [`EngineRunner`] the
//! binary uses, over mock providers and a mock carrier (an HTTP server standing in for the
//! carrier's REST API), with real sockets on 127.0.0.1:
//!
//! - Twilio `send_dtmf` over `ResumeStream`: the tool's REST update ends the stream, the
//!   adapter keeps the call's session, the call comes back on a new stream and the bot goes on;
//! - the warm transfer finished through the callback listener (`complete`, `abort`);
//! - the Telnyx `on_stream_stop` hook: the `streaming_stop` of a transfer retry is not a hang-up;
//! - answering-machine handling: the verdict as a stream parameter or as an AMD callback, the
//!   fixed message, the carrier hang-up;
//! - the callback listener's authentication and limits.
//!
//! A user turn needs speech: the caller's audio is the first words of `fixtures/en_two_utterances.wav`
//! through the production Silero VAD, and the mock STT answers with a scripted transcript.
#![cfg(all(
    feature = "call-control",
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::chunks_exact_to_as_chunks
)]
// test helpers

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent_link::AgentLinkConfig;
use brisvox_agent_link::client::AgentServer;
use brisvox_agent_link::protocol::{
    AgentMessage, CallIdentity, ErrorCode, ServerBody, SessionAccept, WireTransferMode,
};
use brisvox_core::{Clock, SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_server::callbacks::CallbackServer;
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::hosting::Listener;
use brisvox_server::tools::telephony::CallHub;
use brisvox_server::{
    CallProviders, EngineRunner, HostSettings, ManagerConfig, ProviderFactory, RunnerConfig,
    Server, ServerSettings, SessionManager, ShutdownPolicy, ToolSetup, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::telnyx::TelnyxConfig;
use brisvox_transport::twilio::TwilioConfig;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(20);

/// Waits `duration` on the real clock.
async fn pause(duration: Duration) {
    SystemClock.sleep(duration).await;
}
const CALL_SID: &str = "CA00000000000000000000000000000001";
const AGENT_SID: &str = "CA00000000000000000000000000000002";
const ACCOUNT: &str = "AC00000000000000000000000000000001";
const PUBLIC_URL: &str = "wss://agent.example.com/twilio/stream";
const CALLBACK_TOKEN: &str = "callback-secret-0123456789";
const TELNYX_CALL: &str = "v3:telnyx-call-0001";
const MESSAGE: &str = "Hello, this is the clinic. Please call us back tomorrow.";

// ---------------------------------------------------------------------------------------------
// The agent definition and the mock providers

fn agent_text(tools: &[&str], voicemail: bool) -> String {
    let tools = tools
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let voicemail = if voicemail {
        format!("\n[telephony.voicemail]\nmessage = \"{MESSAGE}\"\n")
    } else {
        String::new()
    };
    format!(
        r#"
[agent]
name = "extras-test"
language = "en"
system_prompt = "You are a test agent."
tools = [{tools}]

[stt]
provider = "deepgram"
api_key_env = "EXTRAS_STT_KEY"

[llm]
provider = "openai"
api_key_env = "EXTRAS_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "EXTRAS_TTS_KEY"
voice = "voice-123"

[turn]
policy = "fixed_silence"
silence_ms = 300

[telephony]
warm = true

[[telephony.destinations]]
name = "sales"
number = "+15555550100"
description = "the sales team"
{voicemail}"#
    )
}

fn fake_keys() -> BTreeMap<String, String> {
    [
        ("EXTRAS_STT_KEY", "stt-key"),
        ("EXTRAS_LLM_KEY", "llm-key"),
        ("EXTRAS_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn agent_file(text: &str) -> AgentConfig {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "brisvox-extras-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.toml");
    std::fs::write(&path, text).unwrap();
    let agent = config::load_file(&path, &fake_keys()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    agent
}

/// Every call's providers: the STT hears `heard`; the LLM answers with `script`, then
/// `follow_up`, then `OK.`
struct Mocks {
    clock: SharedClock,
    heard: String,
    script: Option<LlmScript>,
    follow_up: Option<LlmScript>,
    llms: Mutex<Vec<MockLlm>>,
}

impl Mocks {
    fn new(
        clock: SharedClock,
        heard: &str,
        script: Option<LlmScript>,
        follow_up: Option<LlmScript>,
    ) -> Self {
        Self {
            clock,
            heard: heard.to_owned(),
            script,
            follow_up,
            llms: Mutex::new(Vec::new()),
        }
    }

    /// What the calls' LLMs were asked, in `Debug` form.
    fn llm_requests(&self) -> String {
        let llms = self.llms.lock().unwrap_or_else(PoisonError::into_inner);
        format!(
            "{:?}",
            llms.iter().map(MockLlm::requests).collect::<Vec<_>>()
        )
    }
}

impl ProviderFactory for Mocks {
    type Stt = MockStt;
    type Llm = MockLlm;
    type Tts = MockTts;

    fn providers(&self) -> CallProviders<MockStt, MockLlm, MockTts> {
        let stt = MockStt::new(self.clock.clone());
        stt.push(SttUtterance::words(&self.heard));
        let llm = MockLlm::new(self.clock.clone());
        for script in [&self.script, &self.follow_up].into_iter().flatten() {
            llm.push(script.clone());
        }
        llm.set_fallback(LlmScript::text(["OK, anything else?"]));
        self.llms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(llm.clone());
        CallProviders {
            stt: Arc::new(stt),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(self.clock.clone())),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A mock carrier: an HTTP server standing in for the carrier's REST API

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

impl Recorded {
    fn form(&self, key: &str) -> Option<String> {
        brisvox_transport::call_control::form_decode(&self.body)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// What the mock carrier answers.
struct Reply {
    status: u16,
    body: Value,
    delay: Duration,
}

fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        body,
        delay: Duration::ZERO,
    }
}

impl Reply {
    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(&Recorded) -> Reply + Send + Sync;

struct Carrier {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Carrier {
    async fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(carrier_connection(
                    stream,
                    Arc::clone(&handler),
                    Arc::clone(&log),
                ));
            }
        });
        Self { addr, requests }
    }

    /// Twilio's REST API: calls are created (`/Calls.json`) or updated.
    async fn twilio() -> Self {
        Self::start(|r| {
            if r.path.ends_with("/Calls.json") {
                reply(201, json!({"sid": AGENT_SID, "status": "queued"}))
            } else {
                reply(200, json!({"sid": CALL_SID, "status": "in-progress"}))
            }
        })
        .await
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn wait_for(&self, count: usize) -> Vec<Recorded> {
        let clock = SystemClock::shared();
        let start = clock.now();
        while self.requests().len() < count {
            assert!(
                clock.now() - start < WAIT,
                "the carrier saw {} requests: {:?}",
                self.requests().len(),
                self.requests()
            );
            pause(Duration::from_millis(10)).await;
        }
        self.requests()
    }
}

async fn carrier_connection(
    mut stream: TcpStream,
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<Recorded>>>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let method = request_line.next().unwrap_or_default().to_owned();
        let path = request_line.next().unwrap_or_default().to_owned();
        let length = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let total = head_end + 4 + length;
        while buf.len() < total {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = String::from_utf8_lossy(&buf[head_end + 4..total]).into_owned();
        buf.drain(..total);
        let recorded = Recorded { method, path, body };
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(recorded.clone());
        let Reply {
            status,
            body,
            delay,
        } = handler(&recorded);
        if !delay.is_zero() {
            pause(delay).await;
        }
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The caller: a scripted phone on a carrier's media stream

/// What the server sent on a stream.
#[derive(Debug, PartialEq)]
enum Out {
    /// Decoded G.711 payload.
    Media(usize),
    Mark(String),
    Clear,
    Close(Option<u16>),
    Other(Value),
}

/// The caller's audio: the first 1.5 s of speech of `en_two_utterances.wav`, as 20 ms frames of
/// G.711 mu-law at 8 kHz.
fn speech_frames() -> Vec<Vec<u8>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/en_two_utterances.wav");
    let bytes = std::fs::read(path).unwrap();
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("a data chunk")
        + 8;
    let samples: Vec<i16> = bytes[data..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    // 16 kHz -> 8 kHz by taking every second sample; the speech starts at 0.5 s.
    let eight: Vec<i16> = samples[8_000..8_000 + 24_000]
        .iter()
        .step_by(2)
        .copied()
        .collect();
    eight
        .chunks_exact(160)
        .map(|frame| frame.iter().map(|s| linear_to_ulaw(*s)).collect())
        .collect()
}

fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32_635;
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && (s & mask) == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    u8::try_from(!(sign | (exponent << 4) | mantissa) & 0xFF).unwrap()
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The length of the bytes a base64 text decodes to.
fn base64_len(text: &str) -> usize {
    let pads = text.bytes().rev().take_while(|b| *b == b'=').count();
    text.len() / 4 * 3 - pads
}

/// Which carrier's media protocol a phone speaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Twilio,
    Telnyx,
}

struct Phone {
    wire: Wire,
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::UnboundedReceiver<Out>,
    stream_sid: String,
    call_id: String,
    seq: u64,
    chunk: u64,
}

impl Phone {
    /// Connects to `addr` + `path`, sends `connected` and `start` (with `custom` parameters).
    async fn open(
        wire: Wire,
        addr: SocketAddr,
        path: &str,
        stream_sid: &str,
        call_id: &str,
        custom: Value,
    ) -> Self {
        let url = format!("ws://{addr}{path}");
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        let (mut sink, mut stream) = ws.split();
        let (tx, mut writes) = mpsc::unbounded_channel::<Message>();
        let (out_tx, rx) = mpsc::unbounded_channel::<Out>();
        tokio::spawn(async move {
            while let Some(message) = writes.recv().await {
                if sink.send(message).await.is_err() {
                    return;
                }
            }
        });
        let echo_tx = tx.clone();
        let sid = stream_sid.to_owned();
        let call = call_id.to_owned();
        tokio::spawn(async move {
            let mut seq = 1_000;
            while let Some(message) = stream.next().await {
                let out = match message {
                    Ok(Message::Text(text)) => {
                        let value: Value = serde_json::from_str(text.as_str()).unwrap();
                        match value["event"].as_str() {
                            Some("media") => {
                                Out::Media(base64_len(value["media"]["payload"].as_str().unwrap()))
                            }
                            Some("mark") => {
                                let name = value["mark"]["name"].as_str().unwrap().to_owned();
                                // Echo at once: the audio "played".
                                seq += 1;
                                let echo = match wire {
                                    Wire::Twilio => json!({
                                        "event": "mark", "sequenceNumber": seq.to_string(),
                                        "streamSid": sid, "mark": {"name": name},
                                    }),
                                    Wire::Telnyx => json!({
                                        "event": "mark", "sequence_number": seq.to_string(),
                                        "stream_id": sid, "mark": {"name": name},
                                    }),
                                };
                                let _ = echo_tx.send(Message::text(echo.to_string()));
                                Out::Mark(name)
                            }
                            Some("clear") => Out::Clear,
                            _ => Out::Other(value),
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        let _ = out_tx.send(Out::Close(frame.map(|f| u16::from(f.code))));
                        return;
                    }
                    Ok(_) => continue,
                    Err(_) => return,
                };
                if out_tx.send(out).is_err() {
                    return;
                }
            }
            let _ = (call, out_tx.send(Out::Close(None)));
        });
        let mut phone = Self {
            wire,
            tx,
            rx,
            stream_sid: stream_sid.to_owned(),
            call_id: call_id.to_owned(),
            seq: 0,
            chunk: 0,
        };
        phone.hello(custom);
        phone
    }

    fn send(&self, value: &Value) {
        let _ = self.tx.send(Message::text(value.to_string()));
    }

    fn next_seq(&mut self) -> String {
        self.seq += 1;
        self.seq.to_string()
    }

    fn hello(&mut self, custom: Value) {
        match self.wire {
            Wire::Twilio => {
                self.send(&json!({"event": "connected", "protocol": "Call", "version": "1.0.0"}));
                let seq = self.next_seq();
                self.send(&json!({
                    "event": "start", "sequenceNumber": seq, "streamSid": self.stream_sid,
                    "start": {
                        "accountSid": ACCOUNT, "streamSid": self.stream_sid,
                        "callSid": self.call_id, "tracks": ["inbound"],
                        "mediaFormat": {"encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1},
                        "customParameters": custom,
                    },
                }));
            }
            Wire::Telnyx => {
                self.send(&json!({"event": "connected", "version": "1.0.0"}));
                let seq = self.next_seq();
                self.send(&json!({
                    "event": "start", "sequence_number": seq, "stream_id": self.stream_sid,
                    "start": {
                        "user_id": "u1", "call_control_id": self.call_id,
                        "call_session_id": "s1", "from": "+13122010094", "to": "+13122123456",
                        "custom_parameters": custom,
                        "media_format": {"encoding": "PCMU", "sample_rate": 8000, "channels": 1},
                    },
                }));
            }
        }
    }

    /// One 20 ms frame of caller audio.
    fn media(&mut self, frame: &[u8]) {
        self.chunk += 1;
        let seq = self.next_seq();
        let payload = base64_encode(frame);
        match self.wire {
            Wire::Twilio => self.send(&json!({
                "event": "media", "sequenceNumber": seq, "streamSid": self.stream_sid,
                "media": {"track": "inbound", "chunk": self.chunk.to_string(),
                          "timestamp": (self.chunk * 20).to_string(), "payload": payload},
            })),
            Wire::Telnyx => self.send(&json!({
                "event": "media", "sequence_number": seq, "stream_id": self.stream_sid,
                "media": {"track": "inbound", "chunk": self.chunk.to_string(),
                          "timestamp": (self.chunk * 160).to_string(), "payload": payload},
            })),
        }
    }

    /// The caller says a few words: speech, then silence, paced like a real call.
    async fn say_something(&mut self) {
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        for frame in speech_frames() {
            ticker.tick().await;
            self.media(&frame);
        }
        for _ in 0..60 {
            ticker.tick().await;
            self.media(&[0xFF; 160]);
        }
    }

    /// The carrier ends the stream.
    fn stop(&mut self) {
        let seq = self.next_seq();
        match self.wire {
            Wire::Twilio => self.send(&json!({
                "event": "stop", "sequenceNumber": seq, "streamSid": self.stream_sid,
                "stop": {"accountSid": ACCOUNT, "callSid": self.call_id},
            })),
            Wire::Telnyx => self.send(&json!({
                "event": "stop", "sequence_number": seq, "stream_id": self.stream_sid,
                "stop": {"user_id": "u1", "call_control_id": self.call_id},
            })),
        }
    }

    /// The next thing the server sent.
    async fn recv(&mut self) -> Out {
        tokio::time::timeout(WAIT, self.rx.recv())
            .await
            .expect("nothing from the server within the wait")
            .unwrap_or(Out::Close(None))
    }

    /// Waits for audio from the bot.
    async fn expect_media(&mut self) {
        loop {
            match self.recv().await {
                Out::Media(_) => return,
                Out::Close(code) => panic!("the stream was closed ({code:?}) before the bot spoke"),
                _ => {}
            }
        }
    }

    /// Waits for the server to close the stream.
    async fn expect_close(&mut self) -> Option<u16> {
        loop {
            if let Out::Close(code) = self.recv().await {
                return code;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The server

struct Rig {
    _server: Server,
    manager: SessionManager,
    twilio: Option<SocketAddr>,
    telnyx: Option<SocketAddr>,
    websocket: Option<SocketAddr>,
    callbacks: SocketAddr,
    hub: CallHub,
    runner: Arc<EngineRunner<Mocks>>,
    _pool: InferencePool,
}

struct Setup {
    tools: &'static [&'static str],
    voicemail: bool,
    heard: &'static str,
    script: Option<LlmScript>,
    follow_up: Option<LlmScript>,
    twilio_api: Option<String>,
    telnyx_api: Option<String>,
    /// The default out-of-process agent of the calls.
    agent_link: Option<Arc<AgentLinkConfig>>,
    /// Named agent processes a call may pick.
    agent_routes: Vec<(&'static str, Arc<AgentLinkConfig>)>,
    /// Also host the generic websocket listener (a call without a carrier binding).
    websocket: bool,
    /// The server redacts phone numbers (recording or log redaction).
    redact_numbers: bool,
}

impl Setup {
    fn new(tools: &'static [&'static str]) -> Self {
        Self {
            tools,
            voicemail: false,
            heard: "please press one two",
            script: None,
            follow_up: None,
            twilio_api: None,
            telnyx_api: None,
            agent_link: None,
            agent_routes: Vec::new(),
            websocket: false,
            redact_numbers: false,
        }
    }
}

impl Rig {
    async fn start(setup: Setup) -> Self {
        let clock = SystemClock::shared();
        let host = HostSettings {
            twilio: setup.twilio_api.as_ref().map(|_| Listener {
                addr: ([127, 0, 0, 1], 0).into(),
                config: TwilioConfig {
                    public_url: Some(PUBLIC_URL.to_owned()),
                    mark_interval: Duration::ZERO,
                    ..TwilioConfig::default()
                },
            }),
            telnyx: setup.telnyx_api.as_ref().map(|_| Listener {
                addr: ([127, 0, 0, 1], 0).into(),
                config: TelnyxConfig {
                    mark_interval: Duration::ZERO,
                    ..TelnyxConfig::default()
                },
            }),
            websocket: setup.websocket.then(|| ([127, 0, 0, 1], 0).into()),
            ..HostSettings::none()
        };
        let mut env: HashMap<String, String> = HashMap::new();
        let mut carriers = Vec::new();
        if let Some(api) = &setup.twilio_api {
            carriers.push("twilio");
            for (k, v) in [
                ("BRISVOX_TWILIO_ACCOUNT_SID", ACCOUNT),
                ("TWILIO_AUTH_TOKEN", "twilio-test-token"),
                ("BRISVOX_TWILIO_CALLER_ID", "+15555550199"),
                ("BRISVOX_TWILIO_API_BASE", api.as_str()),
            ] {
                env.insert(k.to_owned(), v.to_owned());
            }
        }
        if let Some(api) = &setup.telnyx_api {
            carriers.push("telnyx");
            for (k, v) in [
                ("TELNYX_API_KEY", "telnyx-test-key"),
                ("BRISVOX_TELNYX_CALLER_ID", "+15555550199"),
                ("BRISVOX_TELNYX_CONNECTION_ID", "conn-1"),
                ("BRISVOX_TELNYX_API_BASE", api.as_str()),
            ] {
                env.insert(k.to_owned(), v.to_owned());
            }
        }
        env.insert("BRISVOX_CALL_CONTROL".to_owned(), carriers.join(","));
        env.insert("BRISVOX_CALLBACK_ADDR".to_owned(), "127.0.0.1:0".to_owned());
        env.insert(
            "BRISVOX_CALLBACK_TOKEN".to_owned(),
            CALLBACK_TOKEN.to_owned(),
        );
        let tools = ToolSetup::from_env_linked(&env, &clock, None, &host.stream_links()).unwrap();
        let callbacks = tools.callbacks().cloned().unwrap();
        let hub = tools.call_hub().unwrap();
        let live_calls = hub.clone();

        let pool = InferencePool::new(
            &PoolConfig {
                threads: 2,
                queue_capacity: 64,
                name: "extras-test".to_owned(),
            },
            clock.clone(),
        )
        .unwrap();
        let vad = tokio::task::spawn_blocking(Vad::production)
            .await
            .unwrap()
            .unwrap();
        let mut runner = EngineRunner::with_tools(
            agent_file(&agent_text(setup.tools, setup.voicemail)),
            Mocks::new(clock.clone(), setup.heard, setup.script, setup.follow_up),
            Offload::Pool(pool.handle()),
            vad,
            RunnerConfig::default(),
            tools,
            &clock,
        )
        .unwrap();
        if let Some(link) = setup.agent_link {
            runner = runner.with_agent_link(link);
        }
        for (name, link) in setup.agent_routes {
            runner = runner.with_agent_route(name, link);
        }
        runner = runner.with_redact_caller_numbers(setup.redact_numbers);
        let runner = Arc::new(runner);
        let settings = ServerSettings {
            manager: ManagerConfig::new(4).unwrap(),
            health_addr: None,
            shutdown: ShutdownPolicy {
                drain_timeout: Duration::from_secs(5),
                cancel_grace: Duration::from_secs(5),
                abort_wait: Duration::from_secs(1),
            },
            ..ServerSettings::default()
        };
        let mut server = Server::start_hosting(settings, &host, clock.clone(), &runner)
            .await
            .unwrap();
        let listener = CallbackServer::bind(callbacks, hub, clock).await.unwrap();
        let callbacks = listener.local_addr();
        server.attach_callbacks(listener);
        let addrs = server.listener_addrs();
        Self {
            manager: server.manager().clone(),
            _server: server,
            twilio: addrs.twilio,
            telnyx: addrs.telnyx,
            websocket: addrs.websocket,
            callbacks,
            hub: live_calls,
            runner,
            _pool: pool,
        }
    }

    async fn twilio_phone(&self, stream_sid: &str, custom: Value) -> Phone {
        Phone::open(
            Wire::Twilio,
            self.twilio.unwrap(),
            "/twilio/stream",
            stream_sid,
            CALL_SID,
            custom,
        )
        .await
    }

    async fn telnyx_phone(&self, custom: Value) -> Phone {
        Phone::open(
            Wire::Telnyx,
            self.telnyx.unwrap(),
            "/telnyx/stream",
            "telnyx-stream-1",
            TELNYX_CALL,
            custom,
        )
        .await
    }

    /// Waits until `count` calls are in the hub, where the callback listener finds them.
    async fn until_registered(&self, count: usize) {
        let wait = async {
            while self.hub.len() != count {
                SystemClock.sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("the hub has {} calls", self.hub.len()));
    }

    async fn until(&self, what: &str, done: impl Fn(&SessionManager) -> bool) {
        let wait = async {
            while !done(&self.manager) {
                SystemClock.sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: {:?}", self.manager.stats()));
    }

    /// One HTTP request to the callback listener: the status and the body.
    async fn callback(
        &self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: &str,
    ) -> (u16, Value) {
        http(self.callbacks, method, path, auth, body).await
    }

    fn bearer() -> String {
        format!("Bearer {CALLBACK_TOKEN}")
    }
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let auth = auth.map_or_else(String::new, |a| format!("Authorization: {a}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("the callback listener did not answer")
        .unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------------------------
// Twilio send_dtmf over ResumeStream

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twilio_dtmf_ends_the_stream_and_the_call_comes_back_on_a_new_one() {
    let carrier = Carrier::twilio().await;
    let mut setup = Setup::new(&["send_dtmf", "end_call"]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.script = Some(LlmScript::text(Vec::<String>::new()).with_tool_call(
        "call-1",
        "send_dtmf",
        r#"{"digits":"12"}"#,
    ));
    setup.follow_up = Some(LlmScript::text(["I pressed one two for you."]));
    let rig = Rig::start(setup).await;

    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    phone.say_something().await;

    // The model's tool call became a REST update: tones, then the call back on a stream.
    let seen = carrier.wait_for(1).await;
    assert_eq!(seen[0].method, "POST");
    assert!(
        seen[0].path.ends_with(&format!("/Calls/{CALL_SID}.json")),
        "{}",
        seen[0].path
    );
    let twiml = seen[0].form("Twiml").unwrap();
    assert!(twiml.contains("<Play digits=\"12\"/>"), "{twiml}");
    assert!(
        twiml.contains(&format!("<Stream url=\"{PUBLIC_URL}\"")),
        "{twiml}"
    );
    assert!(
        twiml.contains("<Parameter name=\"resume\" value=\"dtmf\"/>"),
        "{twiml}"
    );

    // Twilio ends the stream: the call is not over. The bot's session waits.
    phone.stop();
    pause(Duration::from_millis(500)).await;
    assert_eq!(rig.manager.stats().active, 1, "the session was kept");

    // The call is back on a new stream: the bot goes on and speaks the tool's follow-up.
    let mut second = rig
        .twilio_phone(
            "MZ0000000000000000000000000000002b",
            json!({"resume": "dtmf"}),
        )
        .await;
    second.expect_media().await;
    assert_eq!(rig.manager.stats().active, 1);
    let asked = rig.runner.factory().llm_requests();
    assert!(asked.contains("send_dtmf"), "{asked}");

    // The new stream is an ordinary one: its stop is a hang-up.
    second.stop();
    rig.until("the call ended", |m| m.stats().active == 0).await;
    assert_eq!(rig.manager.stats().ended.completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_dtmf_update_does_not_hide_the_callers_hang_up() {
    // The update is refused for certain, so the stream never ends on purpose. The
    // announcement made before the update must be withdrawn: when the caller then hangs up, the
    // engine hears it at once and the call is not parked for the linger.
    let carrier =
        Carrier::start(|_| reply(400, json!({"code": 21220, "message": "refused"}))).await;
    let mut setup = Setup::new(&["send_dtmf", "end_call"]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.script = Some(LlmScript::text(Vec::<String>::new()).with_tool_call(
        "call-1",
        "send_dtmf",
        r#"{"digits":"12"}"#,
    ));
    setup.follow_up = Some(LlmScript::text(["I could not press the keys."]));
    let rig = Rig::start(setup).await;

    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    phone.say_something().await;
    // The update went out and was refused; the tool's failure is spoken on the same stream.
    carrier.wait_for(1).await;
    phone.expect_media().await;

    phone.stop();
    // Well inside the 30 s a parked session would linger.
    let ended = async {
        while rig.manager.stats().active != 0 {
            SystemClock.sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), ended)
        .await
        .unwrap_or_else(|_| panic!("the hang-up was hidden: {:?}", rig.manager.stats()));
    assert_eq!(rig.manager.stats().ended.completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twilio_without_a_resume_stream_cannot_send_dtmf() {
    // No Twilio listener in this server: the tool is not even offered.
    let carrier = Carrier::twilio().await;
    let clock = SystemClock::shared();
    let env: HashMap<String, String> = [
        ("BRISVOX_CALL_CONTROL", "twilio"),
        ("BRISVOX_TWILIO_ACCOUNT_SID", ACCOUNT),
        ("TWILIO_AUTH_TOKEN", "t"),
        (
            "BRISVOX_TWILIO_API_BASE",
            &format!("http://{}/2010-04-01", carrier.addr),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let tools = ToolSetup::from_env(&env, &clock, None).unwrap();
    let error = EngineRunner::with_tools(
        agent_file(&agent_text(&["send_dtmf"], false)),
        Mocks::new(clock.clone(), "", None, None),
        Offload::Inline(clock.clone()),
        Vad::Off,
        RunnerConfig::default(),
        tools,
        &clock,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("send_dtmf"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Warm transfer, finished through the callback listener

/// A model that says one sentence and asks for a warm transfer to sales.
fn warm_transfer_script() -> LlmScript {
    LlmScript::text(["Let me connect you with sales."]).with_tool_call(
        "call-1",
        "transfer_call",
        r#"{"destination":"sales","mode":"warm","summary":"The caller wants a refund."}"#,
    )
}

/// A call that got as far as the warm transfer's REST steps and whose stream Twilio ended.
async fn warm_transfer_started() -> (Rig, Carrier, Phone) {
    let carrier = Carrier::twilio().await;
    let mut setup = Setup::new(&["transfer_call", "end_call"]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.heard = "I need to talk to sales";
    setup.script = Some(warm_transfer_script());
    setup.follow_up = Some(LlmScript::text(["Please hold."]));
    let rig = Rig::start(setup).await;
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    phone.say_something().await;

    // 1: the caller waits in a conference; 2: the receiving party is called.
    let seen = carrier.wait_for(2).await;
    assert!(
        seen[0].path.ends_with(&format!("/Calls/{CALL_SID}.json")),
        "{}",
        seen[0].path
    );
    let twiml = seen[0].form("Twiml").unwrap();
    assert!(twiml.contains("<Conference"), "{twiml}");
    assert!(seen[1].path.ends_with("/Calls.json"), "{}", seen[1].path);
    assert_eq!(seen[1].form("To").as_deref(), Some("+15555550100"));
    // Twilio ended the bot's stream when the caller joined the conference. The session waits
    // for the receiving party's answer: it is not a hang-up.
    phone.stop();
    pause(Duration::from_millis(500)).await;
    assert_eq!(rig.manager.stats().active, 1, "the session was kept");
    (rig, carrier, phone)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_transfer_is_completed_by_the_callback_and_the_bot_leaves_the_call() {
    let (rig, carrier, _phone) = warm_transfer_started().await;

    // The callback needs credentials.
    let path = format!("/v1/calls/twilio/{CALL_SID}/warm/complete");
    let (status, _) = rig.callback("POST", &path, None, "").await;
    assert_eq!(status, 401);
    assert_eq!(
        carrier.requests().len(),
        2,
        "an unauthenticated request touched the call"
    );

    // The receiving party accepted: they join the conference, the bot leaves.
    let (status, body) = rig.callback("POST", &path, Some(&Rig::bearer()), "").await;
    assert_eq!(
        (status, body["status"].as_str()),
        (200, Some("joined")),
        "{body}"
    );
    let seen = carrier.wait_for(3).await;
    assert!(
        seen[2].path.ends_with(&format!("/Calls/{AGENT_SID}.json")),
        "{}",
        seen[2].path
    );
    let twiml = seen[2].form("Twiml").unwrap();
    assert!(twiml.contains("startConferenceOnEnter=\"true\""), "{twiml}");
    assert!(twiml.contains("The caller wants a refund."), "{twiml}");
    rig.until("the bot left the call", |m| m.stats().active == 0)
        .await;
    assert_eq!(rig.manager.stats().ended.completed, 1);
    // The caller is on the other party's line: the carrier was not asked to hang anyone up.
    assert!(
        carrier
            .requests()
            .iter()
            .all(|r| r.form("Status").as_deref() != Some("completed")),
        "{:?}",
        carrier.requests()
    );
    // The call is over: nothing left to finish.
    let (status, body) = rig.callback("POST", &path, Some(&Rig::bearer()), "").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("no_such_call")),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_warm_transfer_puts_the_caller_back_on_the_bot_who_apologizes() {
    let (rig, carrier, _phone) = warm_transfer_started().await;
    let path = format!("/v1/calls/twilio/{CALL_SID}/warm/abort");
    let (status, body) = rig.callback("POST", &path, Some(&Rig::bearer()), "").await;
    assert_eq!(
        (status, body["caller"].as_str()),
        (200, Some("resumed")),
        "{body}"
    );
    // The receiving party's leg ended, then the caller went back to the bot's stream.
    let seen = carrier.wait_for(4).await;
    assert!(
        seen[2].path.ends_with(&format!("/Calls/{AGENT_SID}.json")),
        "{}",
        seen[2].path
    );
    assert_eq!(seen[2].form("Status").as_deref(), Some("completed"));
    assert!(
        seen[3].path.ends_with(&format!("/Calls/{CALL_SID}.json")),
        "{}",
        seen[3].path
    );
    let twiml = seen[3].form("Twiml").unwrap();
    assert!(
        twiml.contains("<Parameter name=\"resume\" value=\"warm_failed\"/>"),
        "{twiml}"
    );

    // The caller comes back on a new stream and hears the bot's apology.
    let mut second = rig
        .twilio_phone(
            "MZ0000000000000000000000000000003c",
            json!({"resume": "warm_failed"}),
        )
        .await;
    second.expect_media().await;
    assert_eq!(rig.manager.stats().active, 1);
    // The transfer is over: a second abort has nothing to do.
    let (status, body) = rig.callback("POST", &path, Some(&Rig::bearer()), "").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("no_warm_transfer")),
        "{body}"
    );
    second.stop();
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

// ---------------------------------------------------------------------------------------------
// Telnyx: the stream stop of a transfer retry

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telnyx_stream_stop_of_a_transfer_retry_is_not_a_hang_up() {
    let transfers = Arc::new(AtomicU64::new(0));
    let count = Arc::clone(&transfers);
    let carrier = Carrier::start(move |r| {
        if r.path.ends_with("/actions/transfer") {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                // Telnyx refuses a transfer while media streaming is active.
                reply(
                    422,
                    json!({"errors": [{"code": "90045", "title": "Conflict", "detail": "streaming"}]}),
                )
            } else {
                // The retry takes a while: the stream's end happens meanwhile.
                reply(200, json!({"data": {"result": "ok"}})).after(Duration::from_millis(1_500))
            }
        } else {
            reply(200, json!({"data": {"result": "ok"}}))
        }
    })
    .await;
    let mut setup = Setup::new(&["transfer_call"]);
    setup.telnyx_api = Some(format!("http://{}", carrier.addr));
    setup.heard = "I need sales";
    setup.script = Some(LlmScript::text(["Transferring you."]).with_tool_call(
        "call-1",
        "transfer_call",
        r#"{"destination":"sales","mode":"cold"}"#,
    ));
    let rig = Rig::start(setup).await;
    let mut phone = rig.telnyx_phone(json!({})).await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    phone.say_something().await;

    // transfer (refused), streaming_stop; the retry is on its way.
    let seen = carrier.wait_for(2).await;
    assert!(
        seen[0].path.ends_with("/actions/transfer"),
        "{}",
        seen[0].path
    );
    assert!(
        seen[1].path.ends_with("/actions/streaming_stop"),
        "{}",
        seen[1].path
    );
    // Telnyx ends the stream: that is the host's own doing, not the caller hanging up.
    phone.stop();
    pause(Duration::from_millis(500)).await;
    assert_eq!(
        rig.manager.stats().active,
        1,
        "the stream's end was taken for a hang-up: {:?}",
        carrier.requests()
    );
    // The retry went through; the bot's work is done.
    let seen = carrier.wait_for(3).await;
    assert!(
        seen[2].path.ends_with("/actions/transfer"),
        "{}",
        seen[2].path
    );
    rig.until("the call was handed off", |m| m.stats().active == 0)
        .await;
    assert_eq!(rig.manager.stats().ended.completed, 1);
    // No hang-up request: the caller is on the destination's line.
    assert!(
        carrier
            .requests()
            .iter()
            .all(|r| !r.path.ends_with("/actions/hangup")),
        "{:?}",
        carrier.requests()
    );
}

// ---------------------------------------------------------------------------------------------
// Answering machines

/// A server for a voicemail agent over a Twilio listener with call control.
async fn voicemail_rig() -> (Rig, Carrier) {
    let carrier = Carrier::twilio().await;
    let mut setup = Setup::new(&[]);
    setup.voicemail = true;
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    (Rig::start(setup).await, carrier)
}

/// The message was spoken, then the carrier was asked to hang the call up and the stream closed.
async fn expect_message_then_hang_up(rig: &Rig, carrier: &Carrier, phone: &mut Phone) {
    phone.expect_media().await;
    let seen = carrier.wait_for(1).await;
    assert!(
        seen[0].path.ends_with(&format!("/Calls/{CALL_SID}.json")),
        "{}",
        seen[0].path
    );
    assert_eq!(seen[0].form("Status").as_deref(), Some("completed"));
    phone.expect_close().await;
    rig.until("the call ended", |m| m.stats().active == 0).await;
    assert_eq!(rig.manager.stats().ended.completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_verdict_in_the_stream_parameters_gets_the_message_and_a_hang_up() {
    let (rig, carrier) = voicemail_rig().await;
    let mut phone = rig
        .twilio_phone(
            "MZ0000000000000000000000000000001a",
            json!({"answered_by": "machine_end_beep"}),
        )
        .await;
    expect_message_then_hang_up(&rig, &carrier, &mut phone).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_verdict_callback_gets_the_message_and_a_hang_up() {
    let (rig, carrier) = voicemail_rig().await;
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    // Twilio's AsyncAmdStatusCallback, with the secret as the password of a Basic credential
    // (what `https://user:secret@host/...` becomes).
    let credentials = format!(
        "Basic {}",
        base64_encode(format!("twilio:{CALLBACK_TOKEN}").as_bytes())
    );
    let body =
        format!("CallSid={CALL_SID}&AnsweredBy=machine_end_beep&MachineDetectionDuration=4200");
    // Nothing is known of the call yet: no speech.
    pause(Duration::from_millis(300)).await;
    assert!(carrier.requests().is_empty());
    let (status, answer) = rig
        .callback("POST", "/v1/amd/twilio", Some(&credentials), &body)
        .await;
    assert_eq!(
        (status, answer["status"].as_str()),
        (200, Some("delivered")),
        "{answer}"
    );
    expect_message_then_hang_up(&rig, &carrier, &mut phone).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verdict_that_beats_the_stream_waits_for_the_call() {
    let (rig, carrier) = voicemail_rig().await;
    // The asynchronous detector answered before the media stream connected.
    let body = format!("CallSid={CALL_SID}&AnsweredBy=machine_end_beep");
    let (status, answer) = rig
        .callback("POST", "/v1/amd/twilio", Some(&Rig::bearer()), &body)
        .await;
    assert_eq!(
        (status, answer["status"].as_str()),
        (202, Some("queued")),
        "{answer}"
    );
    assert!(carrier.requests().is_empty());
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    expect_message_then_hang_up(&rig, &carrier, &mut phone).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_human_verdict_leaves_the_call_alone() {
    let (rig, carrier) = voicemail_rig().await;
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    let body = format!("CallSid={CALL_SID}&AnsweredBy=human");
    let (status, _) = rig
        .callback("POST", "/v1/amd/twilio", Some(&Rig::bearer()), &body)
        .await;
    assert_eq!(status, 200);
    pause(Duration::from_millis(1_000)).await;
    assert_eq!(rig.manager.stats().active, 1);
    assert!(carrier.requests().is_empty(), "{:?}", carrier.requests());
    phone.stop();
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_telnyx_amd_webhook_reaches_the_call() {
    let carrier = Carrier::start(|_| reply(200, json!({"data": {"result": "ok"}}))).await;
    let mut setup = Setup::new(&[]);
    setup.voicemail = true;
    setup.telnyx_api = Some(format!("http://{}", carrier.addr));
    let rig = Rig::start(setup).await;
    let mut phone = rig.telnyx_phone(json!({})).await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    let webhook = json!({"data": {
        "id": "evt-1", "event_type": "call.machine.greeting.ended",
        "payload": {"call_control_id": TELNYX_CALL, "result": "beep_detected"},
    }});
    let (status, answer) = rig
        .callback(
            "POST",
            "/v1/amd/telnyx",
            Some(&Rig::bearer()),
            &webhook.to_string(),
        )
        .await;
    assert_eq!(
        (status, answer["status"].as_str()),
        (200, Some("delivered")),
        "{answer}"
    );
    phone.expect_media().await;
    let seen = carrier.wait_for(1).await;
    assert!(
        seen[0].path.ends_with("/actions/hangup"),
        "{}",
        seen[0].path
    );
    assert_eq!(seen[0].json().get("client_state"), None);
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

// ---------------------------------------------------------------------------------------------
// The callback listener

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_callback_listener_authenticates_and_bounds_every_request() {
    let (rig, carrier) = voicemail_rig().await;
    let path = format!("/v1/calls/twilio/{CALL_SID}/warm/complete");
    let wrong = format!("Bearer {CALLBACK_TOKEN}x");
    // Credentials come before anything else, whatever the request is.
    for (method, path) in [
        ("POST", path.as_str()),
        ("GET", "/healthz"),
        ("POST", "/nope"),
    ] {
        for auth in [None, Some(wrong.as_str()), Some("Basic bm9wZQ==")] {
            let (status, _) = rig.callback(method, path, auth, "").await;
            assert_eq!(status, 401, "{method} {path} {auth:?}");
        }
    }
    let ok = Rig::bearer();
    let ok = Some(ok.as_str());
    assert_eq!(rig.callback("GET", &path, ok, "").await.0, 405);
    assert_eq!(rig.callback("POST", "/nope", ok, "").await.0, 404);
    assert_eq!(
        rig.callback("POST", "/v1/calls/vonage/x/warm/complete", ok, "")
            .await
            .0,
        404
    );
    assert_eq!(
        rig.callback("POST", "/v1/calls/twilio/a%20b/warm/complete", ok, "")
            .await
            .0,
        400
    );
    // A percent-encoded id is the id (a Telnyx `v3:...`): found or not, it is not malformed.
    let (status, body) = rig
        .callback("POST", "/v1/calls/telnyx/v3%3Aabc/warm/complete", ok, "")
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("no_such_call"))
    );
    // No such call, a call without a warm transfer, an agent with voicemail (it has).
    let (status, body) = rig.callback("POST", &path, ok, "").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("no_such_call"))
    );
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001a", json!({}))
        .await;
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    let (status, body) = rig.callback("POST", &path, ok, "").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("no_warm_transfer"))
    );
    // A body that is not an AMD callback, and one that is too large.
    assert_eq!(
        rig.callback("POST", "/v1/amd/twilio", ok, "garbage")
            .await
            .0,
        400
    );
    assert_eq!(
        rig.callback("POST", "/v1/amd/telnyx", ok, "not json")
            .await
            .0,
        400
    );
    let big = "x".repeat(brisvox_server::callbacks::MAX_BODY_BYTES + 1);
    assert_eq!(
        rig.callback("POST", "/v1/amd/twilio", ok, &big).await.0,
        413
    );
    // A verdict for a call that has not started waits for it.
    let body = "CallSid=CAffffffffffffffffffffffffffffffff&AnsweredBy=human";
    let (status, body) = rig.callback("POST", "/v1/amd/twilio", ok, body).await;
    assert_eq!((status, body["status"].as_str()), (202, Some("queued")));
    assert!(
        carrier.requests().is_empty(),
        "no request reached the carrier"
    );
    phone.stop();
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

// ---------------------------------------------------------------------------------------------
// The callback listener's resource bounds

/// A listener of its own (no server behind it) with short deadlines and room for two requests.
async fn bounded_listener() -> (CallbackServer, SocketAddr) {
    let mut settings = brisvox_server::callbacks::CallbackSettings::new(
        ([127, 0, 0, 1], 0).into(),
        CALLBACK_TOKEN,
    );
    settings.max_connections = 2;
    settings.read_timeout = Duration::from_millis(400);
    settings.handler_timeout = Duration::from_secs(5);
    let clock = SystemClock::shared();
    let hub = CallHub::with_clock(clock.clone());
    let listener = CallbackServer::bind(settings, hub, clock).await.unwrap();
    let addr = listener.local_addr();
    (listener, addr)
}

/// Sends `bytes` raw and reads what comes back until the listener closes the connection (empty
/// when it closed without an answer, also when the peer reset it).
async fn raw(addr: SocketAddr, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // A listener that already dropped the connection may reset the write: that is its answer.
    let _ = stream.write_all(bytes).await;
    let mut response = Vec::new();
    let read = tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("the callback listener neither answered nor closed");
    let _ = read;
    String::from_utf8_lossy(&response).into_owned()
}

fn status_of(response: &str) -> Option<u16> {
    response.split(' ').nth(1).and_then(|s| s.parse().ok())
}

/// Standard base64 (for a `Basic` header; the tests have no base64 crate).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[tokio::test]
async fn the_callback_listener_accepts_basic_credentials_end_to_end() {
    let (_listener, addr) = bounded_listener().await;
    let basic = |password: &str| {
        format!(
            "GET /nope HTTP/1.1\r\nHost: x\r\nAuthorization: Basic {}\r\n\r\n",
            base64(format!("twilio:{password}").as_bytes())
        )
    };
    // Authenticated (any user name, the secret as the password): the request goes on to routing.
    let answer = raw(addr, basic(CALLBACK_TOKEN).as_bytes()).await;
    assert_eq!(status_of(&answer), Some(404), "{answer}");
    let answer = raw(addr, basic("not-the-secret").as_bytes()).await;
    assert_eq!(status_of(&answer), Some(401), "{answer}");
    assert!(answer.contains("WWW-Authenticate"), "{answer}");
}

#[tokio::test]
async fn the_callback_listener_bounds_the_head_and_refuses_chunked_bodies() {
    let (_listener, addr) = bounded_listener().await;
    // A head that never ends is cut at the head limit, before anything is parsed.
    let flood = vec![b'a'; brisvox_server::callbacks::MAX_HEAD_BYTES + 2048];
    let answer = raw(addr, &flood).await;
    assert_eq!(status_of(&answer), Some(431), "{answer}");
    // Chunked bodies are not read at all.
    let request = format!(
        "POST /v1/amd/twilio HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {CALLBACK_TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
    );
    let answer = raw(addr, request.as_bytes()).await;
    assert_eq!(status_of(&answer), Some(501), "{answer}");
}

#[tokio::test]
async fn the_callback_listener_cuts_a_client_that_stalls_inside_its_head_or_body() {
    let (_listener, addr) = bounded_listener().await;
    let clock = SystemClock::shared();
    let started = clock.now();
    // Mid-head: no blank line ever comes.
    let answer = raw_stalled(addr, b"POST /v1/amd/twilio HTTP/1.1\r\nHost: x\r\n").await;
    assert_eq!(answer, "", "a stalled head gets no answer, just the close");
    // Mid-body: the head promises ten bytes, three arrive.
    let request = format!(
        "POST /v1/amd/twilio HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {CALLBACK_TOKEN}\r\nContent-Length: 10\r\n\r\nabc"
    );
    let answer = raw_stalled(addr, request.as_bytes()).await;
    assert_eq!(answer, "", "a stalled body gets no answer, just the close");
    assert!(
        clock.now() - started >= Duration::from_millis(700),
        "both stalls were waited out for the read timeout, not cut early"
    );
}

/// Sends `bytes`, then sends nothing more and keeps the connection open until the listener
/// closes it; what it answered.
async fn raw_stalled(addr: SocketAddr, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("the listener never gave up on a stalled client")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

#[tokio::test]
async fn the_callback_listener_drops_connections_beyond_its_limit_and_frees_the_slots() {
    let (_listener, addr) = bounded_listener().await;
    // Two clients hold both slots (a head that never ends) ...
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"POST /x HTTP/1.1\r\n").await.unwrap();
        held.push(stream);
    }
    // ... so a third is dropped unanswered, however good its request is.
    let good =
        format!("GET /nope HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {CALLBACK_TOKEN}\r\n\r\n");
    let answer = raw(addr, good.as_bytes()).await;
    assert_eq!(
        answer, "",
        "the third connection must be dropped, got {answer}"
    );
    // The stalled ones time out and give their slots back: the listener serves again.
    for mut stream in held {
        let mut rest = Vec::new();
        tokio::time::timeout(WAIT, stream.read_to_end(&mut rest))
            .await
            .expect("a stalled client was never cut")
            .unwrap();
    }
    let answer = raw(addr, good.as_bytes()).await;
    assert_eq!(status_of(&answer), Some(404), "{answer}");
}

// ---------------------------------------------------------------------------------------------
// The out-of-process agent: transfer through the carrier, per-call routing

async fn agent_process() -> (AgentServer, Arc<AgentLinkConfig>) {
    let server = AgentServer::bind("127.0.0.1:0".parse().unwrap(), SystemClock::shared())
        .await
        .unwrap();
    let config = Arc::new(AgentLinkConfig::new(server.url().unwrap()));
    (server, config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_process_transfers_a_carrier_call_through_the_link() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = agent_process().await;
    // The definition names no telephony tool: only the agent process transfers.
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let _phone = rig
        .twilio_phone("MZ0000000000000000000000000000001b", json!({}))
        .await;
    let mut session = tokio::time::timeout(WAIT, agent.accept())
        .await
        .unwrap()
        .unwrap();
    let start = session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(&start.body, ServerBody::SessionStart { capabilities, .. } if capabilities.transfer),
        "the carrier call can be transferred: {start:?}"
    );
    rig.until("the call runs", |m| m.stats().active == 1).await;
    session
        .send(&AgentMessage::Transfer {
            id: Some("t1".to_owned()),
            re: None,
            destination: "+15555550123".to_owned(),
            mode: None,
            summary: None,
            reason: Some("billing question".to_owned()),
        })
        .await
        .unwrap();
    let result = session
        .recv_until(|b| matches!(b, ServerBody::CommandResult { .. }))
        .await
        .unwrap();
    assert!(
        matches!(&result.body, ServerBody::CommandResult { id, ok: true, .. } if id == "t1"),
        "{result:?}"
    );
    // The carrier was asked to move the caller to the destination.
    let seen = carrier.wait_for(1).await;
    assert!(
        seen[0].path.ends_with(&format!("/Calls/{CALL_SID}.json")),
        "{}",
        seen[0].path
    );
    let twiml = seen[0].form("Twiml").unwrap();
    assert!(twiml.contains("+15555550123"), "{twiml}");
    // The caller is on the other line: the bot leaves the call.
    rig.until("the bot left the call", |m| m.stats().active == 0)
        .await;
    assert_eq!(rig.manager.stats().ended.completed, 1);
}

/// A call no carrier owns (generic websocket) cannot be transferred: the agent process is told
/// at the start (`capabilities.transfer`) and its `transfer` answers `unsupported`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_without_a_carrier_binding_cannot_be_transferred_by_the_agent_process() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.websocket = true;
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    // A generic websocket call: no carrier owns it.
    let url = format!("ws://{}/call", rig.websocket.unwrap());
    let (mut caller, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let start = brisvox_transport::websocket::ClientMessage::Start(
        brisvox_transport::websocket::Start::new(
            16_000,
            brisvox_transport::websocket::Encoding::Pcm16,
        ),
    );
    caller.send(Message::text(start.to_json())).await.unwrap();
    let mut session = tokio::time::timeout(WAIT, agent.accept())
        .await
        .unwrap()
        .unwrap();
    let start = session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(&start.body, ServerBody::SessionStart { capabilities, .. } if !capabilities.transfer),
        "{start:?}"
    );
    session
        .send(&AgentMessage::Transfer {
            id: Some("t1".to_owned()),
            re: None,
            destination: "+15555550123".to_owned(),
            mode: None,
            summary: None,
            reason: None,
        })
        .await
        .unwrap();
    let result = session
        .recv_until(|b| matches!(b, ServerBody::CommandResult { .. }))
        .await
        .unwrap();
    assert!(
        matches!(&result.body, ServerBody::CommandResult { ok: false, error: Some(e), .. } if e.code == ErrorCode::Unsupported),
        "{result:?}"
    );
    assert!(carrier.requests().is_empty(), "{:?}", carrier.requests());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_start_message_picks_the_agent_process_of_a_call() {
    let carrier = Carrier::twilio().await;
    let (default_agent, default_link) = agent_process().await;
    let (billing_agent, billing_link) = agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(default_link);
    setup.agent_routes = vec![("billing", billing_link)];
    let rig = Rig::start(setup).await;

    // A call naming a route links to that process, not to the default one.
    let _billing = rig
        .twilio_phone(
            "MZ0000000000000000000000000000001c",
            json!({"agent_route": "billing"}),
        )
        .await;
    let mut session = tokio::time::timeout(WAIT, billing_agent.accept())
        .await
        .expect("the billing agent is called")
        .unwrap();
    session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), default_agent.accept())
            .await
            .is_err(),
        "the default agent was not called"
    );
    drop(session);

    // A call without one, and a call naming a route nobody configured, get the default.
    let _plain = rig
        .twilio_phone("MZ0000000000000000000000000000001d", json!({}))
        .await;
    let mut session = tokio::time::timeout(WAIT, default_agent.accept())
        .await
        .expect("the default agent is called")
        .unwrap();
    session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    let _unknown = rig
        .twilio_phone(
            "MZ0000000000000000000000000000001e",
            json!({"agent_route": "nowhere"}),
        )
        .await;
    let mut other = tokio::time::timeout(WAIT, default_agent.accept())
        .await
        .expect("an unknown route falls back to the default agent")
        .unwrap();
    other
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    session.close().await.unwrap();
}

/// A generic websocket call names its agent process in the start message's metadata
/// (`agent_route`), as a Twilio stream does with a `<Parameter>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_websocket_start_message_picks_the_agent_process_of_a_call() {
    let (default_agent, default_link) = agent_process().await;
    let (billing_agent, billing_link) = agent_process().await;
    let carrier = Carrier::twilio().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.websocket = true;
    setup.agent_link = Some(default_link);
    setup.agent_routes = vec![("billing", billing_link)];
    let rig = Rig::start(setup).await;
    let url = format!("ws://{}/call", rig.websocket.unwrap());
    let (mut caller, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let mut start = brisvox_transport::websocket::Start::new(
        16_000,
        brisvox_transport::websocket::Encoding::Pcm16,
    );
    start
        .metadata
        .insert("agent_route".to_owned(), "billing".to_owned());
    let start = brisvox_transport::websocket::ClientMessage::Start(start);
    caller.send(Message::text(start.to_json())).await.unwrap();
    let mut session = tokio::time::timeout(WAIT, billing_agent.accept())
        .await
        .expect("the billing agent is called")
        .unwrap();
    session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), default_agent.accept())
            .await
            .is_err(),
        "the default agent was not called"
    );
}

/// The carrier lines of a call (here the apology after a failed warm transfer) still reach the
/// engine when an agent process is linked: the link owns the engine's line input, so they are
/// forwarded into it. The agent process asks for the warm transfer; the receiving party never
/// answers and the caller is put back on the bot, who apologizes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_apology_after_a_failed_warm_transfer_is_spoken_with_an_agent_process() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let mut phone = rig
        .twilio_phone("MZ0000000000000000000000000000001f", json!({}))
        .await;
    let mut session = tokio::time::timeout(WAIT, agent.accept())
        .await
        .unwrap()
        .unwrap();
    session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    rig.until("the call runs", |m| m.stats().active == 1).await;
    rig.until_registered(1).await;
    session
        .send(&AgentMessage::Transfer {
            id: Some("w1".to_owned()),
            re: None,
            destination: "+15555550100".to_owned(),
            mode: Some(WireTransferMode::Warm),
            summary: Some("The caller wants a refund.".to_owned()),
            reason: None,
        })
        .await
        .unwrap();
    // 1: the caller waits in a conference; 2: the receiving party is called.
    let seen = carrier.wait_for(2).await;
    assert!(
        seen[0].form("Twiml").unwrap().contains("<Conference"),
        "{:?}",
        seen[0]
    );
    phone.stop();
    pause(Duration::from_millis(500)).await;
    assert_eq!(rig.manager.stats().active, 1, "the session was kept");
    let path = format!("/v1/calls/twilio/{CALL_SID}/warm/abort");
    let (status, body) = rig.callback("POST", &path, Some(&Rig::bearer()), "").await;
    assert_eq!(
        (status, body["caller"].as_str()),
        (200, Some("resumed")),
        "{body}"
    );
    let mut second = rig
        .twilio_phone(
            "MZ0000000000000000000000000000003d",
            json!({"resume": "warm_failed"}),
        )
        .await;
    // The apology is spoken: it went through the link's line input.
    second.expect_media().await;
    second.stop();
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

// Caller identity (opt-in)

/// An agent process that asked for the caller identity (`[agent_link] caller_identity`).
async fn identity_agent_process() -> (AgentServer, Arc<AgentLinkConfig>) {
    let server = AgentServer::bind("127.0.0.1:0".parse().unwrap(), SystemClock::shared())
        .await
        .unwrap();
    let mut config = AgentLinkConfig::new(server.url().unwrap());
    config.caller_identity = true;
    (server, Arc::new(config))
}

/// The `call` object of the `session.start` the agent process got.
async fn session_start_call(agent: &AgentServer) -> Option<CallIdentity> {
    let mut session = tokio::time::timeout(WAIT, agent.accept())
        .await
        .unwrap()
        .unwrap();
    let start = session
        .accept_session(SessionAccept {
            version: 1,
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    let ServerBody::SessionStart { call, .. } = start.body else {
        panic!("not a session.start")
    };
    call
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_twilio_call_tells_an_opted_in_agent_process_who_is_on_the_line() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = identity_agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let _phone = rig
        .twilio_phone(
            "MZ0000000000000000000000000000004a",
            json!({"from": "+15550000001", "to": "+15550000002"}),
        )
        .await;
    assert_eq!(
        session_start_call(&agent).await,
        Some(CallIdentity {
            carrier: "twilio".to_owned(),
            carrier_call_id: CALL_SID.to_owned(),
            from: Some("+15550000001".to_owned()),
            to: Some("+15550000002".to_owned()),
            pii_redacted: false,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_telnyx_call_tells_an_opted_in_agent_process_who_is_on_the_line() {
    let carrier = Carrier::start(|_| reply(200, json!({"data": {"result": "ok"}}))).await;
    let (agent, link) = identity_agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.telnyx_api = Some(format!("http://{}", carrier.addr));
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let _phone = rig.telnyx_phone(json!({})).await;
    assert_eq!(
        session_start_call(&agent).await,
        Some(CallIdentity {
            carrier: "telnyx".to_owned(),
            carrier_call_id: TELNYX_CALL.to_owned(),
            from: Some("+13122010094".to_owned()),
            to: Some("+13122123456".to_owned()),
            pii_redacted: false,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_caller_identity_is_off_unless_the_agent_process_opted_in() {
    let carrier = Carrier::twilio().await;
    // The default configuration: no opt-in.
    let (agent, link) = agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let _phone = rig
        .twilio_phone(
            "MZ0000000000000000000000000000004b",
            json!({"from": "+15550000001", "to": "+15550000002"}),
        )
        .await;
    assert_eq!(session_start_call(&agent).await, None);
}

/// The switch is per process: a named route without `caller_identity` gets no identity, while
/// the opted-in default process of the same server does (a route may be another team's agent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_without_the_switch_gets_no_identity_while_the_opted_in_default_does() {
    let carrier = Carrier::twilio().await;
    let (default_agent, default_link) = identity_agent_process().await;
    let (billing_agent, billing_link) = agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(default_link);
    setup.agent_routes = vec![("billing", billing_link)];
    let rig = Rig::start(setup).await;
    let _billing = rig
        .twilio_phone(
            "MZ0000000000000000000000000000004d",
            json!({"from": "+15550000001", "to": "+15550000002", "agent_route": "billing"}),
        )
        .await;
    assert_eq!(session_start_call(&billing_agent).await, None);
    let _plain = rig
        .twilio_phone(
            "MZ0000000000000000000000000000004e",
            json!({"from": "+15550000001", "to": "+15550000002"}),
        )
        .await;
    assert_eq!(
        session_start_call(&default_agent).await,
        Some(CallIdentity {
            carrier: "twilio".to_owned(),
            carrier_call_id: CALL_SID.to_owned(),
            from: Some("+15550000001".to_owned()),
            to: Some("+15550000002".to_owned()),
            pii_redacted: false,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redacting_phone_numbers_withholds_them_from_an_opted_in_agent_process() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = identity_agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.agent_link = Some(link);
    setup.redact_numbers = true;
    let rig = Rig::start(setup).await;
    let _phone = rig
        .twilio_phone(
            "MZ0000000000000000000000000000004c",
            json!({"from": "+15550000001", "to": "+15550000002"}),
        )
        .await;
    assert_eq!(
        session_start_call(&agent).await,
        Some(CallIdentity {
            carrier: "twilio".to_owned(),
            carrier_call_id: CALL_SID.to_owned(),
            from: None,
            to: None,
            pii_redacted: true,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_without_a_carrier_has_no_caller_identity_even_when_opted_in() {
    let carrier = Carrier::twilio().await;
    let (agent, link) = identity_agent_process().await;
    let mut setup = Setup::new(&[]);
    setup.twilio_api = Some(format!("http://{}/2010-04-01", carrier.addr));
    setup.websocket = true;
    setup.agent_link = Some(link);
    let rig = Rig::start(setup).await;
    let url = format!("ws://{}/call", rig.websocket.unwrap());
    let (mut caller, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let start = brisvox_transport::websocket::ClientMessage::Start(
        brisvox_transport::websocket::Start::new(
            16_000,
            brisvox_transport::websocket::Encoding::Pcm16,
        ),
    );
    caller.send(Message::text(start.to_json())).await.unwrap();
    assert_eq!(session_start_call(&agent).await, None);
}
