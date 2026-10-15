//! The server binary's hosted transports, end to end on real sockets: an agent file,
//! the engine runner over mock providers (no keys), the listeners admitting through the session
//! manager, and a drain with a live call on it. One call runs the production call path end to
//! end: the bundled Silero VAD, the RTVI listener and its bridge, and a typed turn answered with
//! the agent's system prompt and bot audio. With the `sip` feature, a SIP call is admitted, drained
//! and refused through the same manager.
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
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::{Clock, SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::hosting::Listener;
use brisvox_server::{
    AdmissionPolicy, AdmissionSettings, CallProviders, ConfiguredProviders, DrainOutcome,
    EngineRunner, HostSettings, ListenerAddrs, LoadProbe, LoadSample, ManagerConfig,
    OverloadConfig, ProviderFactory, RunnerConfig, Server, ServerSettings, SessionManager,
    ShutdownPolicy, ShutdownSignal, ShutdownSignals, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::rtvi::RtviConfig;
use brisvox_transport::rtvi::proto::{Frame, frame};
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerMessage, Start};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(10);

/// An agent definition without tools; its keys are fake (the mocks never use them).
const AGENT: &str = r#"
[agent]
name = "hosting-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "HOSTING_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "HOSTING_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "HOSTING_TEST_TTS_KEY"
voice = "voice-123"
"#;

fn fake_keys() -> BTreeMap<String, String> {
    [
        ("HOSTING_TEST_STT_KEY", "stt-key"),
        ("HOSTING_TEST_LLM_KEY", "llm-key"),
        ("HOSTING_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Writes [`AGENT`] to a file of its own and loads it, as the binary does.
fn agent_file() -> AgentConfig {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "brisvox-hosting-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.toml");
    std::fs::write(&path, AGENT).unwrap();
    let agent = config::load_file(&path, &fake_keys()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    agent
}

/// The answer every call's mock LLM gives.
const ANSWER: &str = "Hello from the agent.";

/// Fresh mock providers for every call; the LLMs are kept to read what they were asked.
struct Mocks {
    clock: SharedClock,
    calls: AtomicU64,
    llms: Mutex<Vec<MockLlm>>,
}

impl Mocks {
    fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            calls: AtomicU64::new(0),
            llms: Mutex::new(Vec::new()),
        }
    }

    /// Every request the calls' LLMs received, in `Debug` form.
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        let llm = MockLlm::new(self.clock.clone());
        llm.set_fallback(LlmScript::text([ANSWER]));
        self.llms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(llm.clone());
        CallProviders {
            stt: Arc::new(MockStt::new(self.clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(self.clock.clone())),
        }
    }
}

/// A server hosting listeners on free ports, its calls on mock providers.
struct Rig {
    server: Option<Server>,
    manager: SessionManager,
    addrs: ListenerAddrs,
    runner: Arc<EngineRunner<Mocks>>,
    _pool: InferencePool,
}

/// Only the generic websocket listener, on a free loopback port.
fn websocket_only() -> HostSettings {
    HostSettings {
        websocket: Some(([127, 0, 0, 1], 0).into()),
        ..HostSettings::none()
    }
}

impl Rig {
    async fn start(max_sessions: usize, policy: ShutdownPolicy) -> Self {
        Self::start_with(max_sessions, policy, &websocket_only(), Vad::Off).await
    }

    async fn start_with(
        max_sessions: usize,
        policy: ShutdownPolicy,
        host: &HostSettings,
        vad: Vad,
    ) -> Self {
        Self::start_admitted(max_sessions, policy, host, vad, AdmissionPolicy::none()).await
    }

    /// [`start_with`](Self::start_with) with an admission policy.
    async fn start_admitted(
        max_sessions: usize,
        policy: ShutdownPolicy,
        host: &HostSettings,
        vad: Vad,
        admission: AdmissionPolicy,
    ) -> Self {
        Self::start_linked(max_sessions, policy, host, vad, admission, None).await
    }

    /// [`start_admitted`](Self::start_admitted) with an agent process the calls link to.
    async fn start_linked(
        max_sessions: usize,
        policy: ShutdownPolicy,
        host: &HostSettings,
        vad: Vad,
        admission: AdmissionPolicy,
        agent: Option<Arc<brisvox_agent_link::AgentLinkConfig>>,
    ) -> Self {
        let clock = SystemClock::shared();
        let pool = InferencePool::new(
            &PoolConfig {
                threads: 1,
                queue_capacity: 64,
                name: "hosting-test".to_owned(),
            },
            clock.clone(),
        )
        .unwrap();
        let mut runner = EngineRunner::new(
            agent_file(),
            Mocks::new(clock.clone()),
            Offload::Pool(pool.handle()),
            vad,
            RunnerConfig::default(),
        )
        .unwrap();
        if let Some(agent) = agent {
            runner = runner.with_agent_link(agent);
        }
        let runner = Arc::new(runner);
        let settings = ServerSettings {
            manager: ManagerConfig::new(max_sessions).unwrap(),
            health_addr: None,
            shutdown: policy,
            ..ServerSettings::default()
        };
        let server =
            Server::start_hosting_with_admission(settings, admission, host, clock, &runner)
                .await
                .unwrap();
        Self {
            manager: server.manager().clone(),
            addrs: server.listener_addrs(),
            server: Some(server),
            runner,
            _pool: pool,
        }
    }

    fn ws(&self) -> SocketAddr {
        self.addrs.websocket.unwrap()
    }

    fn url(&self) -> String {
        format!("ws://{}/call", self.ws())
    }

    /// Connects and starts a call; waits until its session runs.
    async fn call(&self) -> Client {
        let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(self.url()))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
        ws.send(Message::text(start.to_json())).await.unwrap();
        let started = next_server_message(&mut ws).await;
        assert!(
            matches!(started, Some(ServerMessage::Started { .. })),
            "{started:?}"
        );
        ws
    }

    /// Connects and expects the refusal before the upgrade: (status, has `Retry-After`).
    async fn refused(&self) -> (u16, bool) {
        let (status, retry_after, _) = self.refused_in_full().await;
        (status, retry_after.is_some())
    }

    /// The refusal before the upgrade: (status, `Retry-After` value, body).
    async fn refused_in_full(&self) -> (u16, Option<String>, String) {
        match tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(self.url()))
            .await
            .expect("connect timed out")
        {
            Err(WsError::Http(response)) => (
                response.status().as_u16(),
                response
                    .headers()
                    .get("retry-after")
                    .map(|v| v.to_str().unwrap().to_owned()),
                String::from_utf8_lossy(response.body().as_deref().unwrap_or_default())
                    .into_owned(),
            ),
            Ok(_) => panic!("the connection was upgraded instead of refused"),
            Err(e) => panic!("expected an HTTP refusal, got {e}"),
        }
    }

    async fn until(&self, what: &str, done: impl Fn(&SessionManager) -> bool) {
        let wait = async {
            while !done(&self.manager) {
                pause(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: {:?}", self.manager.stats()));
    }
}

/// The next text message, skipping audio and keepalives; `None` once the connection ended.
async fn next_server_message(ws: &mut Client) -> Option<ServerMessage> {
    loop {
        let next = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no frame within the wait");
        match next {
            Some(Ok(Message::Text(text))) => {
                return Some(ServerMessage::parse(text.as_str()).unwrap());
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => {}
        }
    }
}

/// Real time passes (on the clock, not `tokio::time`).
async fn pause(duration: Duration) {
    SystemClock.sleep(duration).await;
}

/// 20 ms of silence at 16 kHz PCM16.
fn silence() -> Message {
    Message::binary(vec![0_u8; 640])
}

fn policy(drain: Duration) -> ShutdownPolicy {
    ShutdownPolicy {
        drain_timeout: drain,
        cancel_grace: Duration::from_secs(5),
        abort_wait: Duration::from_secs(1),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_refuses_new_calls_and_waits_for_the_live_one() {
    let mut rig = Rig::start(4, policy(Duration::from_secs(30))).await;
    let mut live = rig.call().await;
    rig.until("the call's session runs", |m| m.stats().active == 1)
        .await;
    rig.until_runner_calls(1).await;

    let (trigger, mut signals) = ShutdownSignals::manual();
    let server = rig.server.take().unwrap();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(ShutdownSignal::Requested));
    rig.until("the drain began", |m| !m.is_accepting()).await;

    // Draining: a new connection is refused before its upgrade, without Retry-After.
    assert_eq!(rig.refused().await, (503, false));
    assert_eq!(rig.manager.stats().rejected_shutting_down, 1);

    // The live call keeps its connection and its session through the drain.
    for _ in 0..10 {
        live.send(silence()).await.unwrap();
    }
    pause(Duration::from_millis(100)).await;
    assert_eq!(rig.manager.stats().active, 1);
    assert!(!run.is_finished());

    // The caller hangs up: the session ends by itself and the drain finishes clean.
    live.send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
    let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(report.outcome(), DrainOutcome::Drained, "{report:?}");
    assert_eq!(report.outcome().exit_code(), 0);
    assert_eq!((report.initial, report.drained), (1, 1), "{report:?}");
    assert_eq!(rig.manager.stats().ended.completed, 1);

    // The listener is gone after the run.
    let gone = tokio::time::timeout(WAIT, TcpStream::connect(rig.ws())).await;
    assert!(
        !matches!(gone, Ok(Ok(_))),
        "the websocket listener still accepts after the drain"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_outliving_the_drain_is_cancelled_and_told() {
    let mut rig = Rig::start(4, policy(Duration::from_millis(200))).await;
    let mut live = rig.call().await;
    rig.until("the call's session runs", |m| m.stats().active == 1)
        .await;

    let (trigger, mut signals) = ShutdownSignals::manual();
    let server = rig.server.take().unwrap();
    let run = tokio::spawn(async move { server.run(&mut signals).await });
    assert!(trigger.trigger(ShutdownSignal::Requested));

    // The call does not end by itself: after the drain timeout it is cancelled, and the client
    // is told the call is over (a `stop`, then the close).
    let mut stopped = false;
    while let Some(message) = next_server_message(&mut live).await {
        if matches!(message, ServerMessage::Stop { .. }) {
            stopped = true;
        }
    }
    assert!(stopped, "the client was not told the call ended");
    let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(report.outcome(), DrainOutcome::Cancelled, "{report:?}");
    assert_eq!(report.cancelled, 1, "{report:?}");
    assert_eq!(report.leaked, 0, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_server_refuses_with_retry_after_and_admits_again_when_a_call_ends() {
    let mut rig = Rig::start(1, policy(Duration::ZERO)).await;
    let mut first = rig.call().await;
    rig.until("the call's session runs", |m| m.stats().active == 1)
        .await;

    // The manager's limit is the one that counts: 503 with Retry-After, before the upgrade.
    assert_eq!(rig.refused().await, (503, true));
    assert_eq!(rig.manager.stats().rejected_at_capacity, 1);

    // The first call hangs up; its room is free again.
    first
        .send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
    rig.until("the first call ended", |m| {
        let s = m.stats();
        s.active == 0 && s.reserved == 0
    })
    .await;
    let mut second = rig.call().await;
    rig.until("the second call runs", |m| m.stats().active == 1)
        .await;
    rig.until_runner_calls(2).await;

    let (trigger, mut signals) = ShutdownSignals::manual();
    assert!(trigger.trigger(ShutdownSignal::Requested));
    let report = rig.server.take().unwrap().run(&mut signals).await;
    // A zero drain timeout: the live call is cancelled at once.
    assert_eq!(report.outcome(), DrainOutcome::Cancelled, "{report:?}");
    while next_server_message(&mut second).await.is_some() {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_stuck_in_its_handshake_does_not_hold_up_the_shutdown() {
    let mut rig = Rig::start(
        4,
        ShutdownPolicy {
            drain_timeout: Duration::ZERO,
            cancel_grace: Duration::from_secs(1),
            abort_wait: Duration::from_millis(200),
        },
    )
    .await;
    // A TCP connection that never speaks websocket: admitted, it holds a reservation for as
    // long as its handshake may take (5 s), longer than the shutdown's stages.
    let _silent = TcpStream::connect(rig.ws()).await.unwrap();
    rig.until("the connection holds a reservation", |m| {
        m.stats().reserved == 1
    })
    .await;

    let (trigger, mut signals) = ShutdownSignals::manual();
    assert!(trigger.trigger(ShutdownSignal::Requested));
    let report = tokio::time::timeout(WAIT, rig.server.take().unwrap().run(&mut signals))
        .await
        .unwrap();
    // The cancel stage closes the transport's connections too, so the reservation comes back
    // within the grace instead of being reported as leaked.
    assert_eq!(report.leaked, 0, "{report:?}");
    assert_eq!(report.aborted, 0, "{report:?}");
    assert_eq!(report.outcome(), DrainOutcome::Cancelled, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_files_providers_build_without_touching_the_network() {
    let clock = SystemClock::shared();
    let shutdown = CancellationToken::new();
    let providers = ConfiguredProviders::from_agent(&agent_file(), &clock, &shutdown).unwrap();
    let CallProviders { stt, llm, tts } = providers.providers();
    assert_eq!(
        (stt.name(), llm.name(), tts.name()),
        ("deepgram", "openai", "cartesia")
    );
    // Every call runs behind the resilience layer: wrappers of its own over the
    // process's instances, whose breakers all calls share.
    assert!(!Arc::ptr_eq(&stt, &providers.providers().stt));
    // Cartesia documents a 5-minute idle limit; the layer refreshes an idle session before it.
    let leg = providers.resilient().tts_leg();
    assert_eq!(leg.provider().idle_limit(), Some(Duration::from_secs(300)));
    shutdown.cancel();
}

#[test]
fn an_agent_naming_tools_is_refused_until_the_binary_has_them() {
    let text = AGENT.replace(
        "system_prompt = \"You are a test agent.\"",
        "system_prompt = \"You are a test agent.\"\ntools = [\"lookup_order\"]",
    );
    let agent = config::load_str(&text, "tools.toml", &fake_keys()).unwrap();
    let err = EngineRunner::new(
        agent,
        Mocks::new(SystemClock::shared()),
        Offload::Inline(SystemClock::shared()),
        Vad::Off,
        RunnerConfig::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("lookup_order"), "{err}");
}

impl Rig {
    fn runner_calls(&self) -> u64 {
        self.runner.factory().calls.load(Ordering::Relaxed)
    }

    /// Waits until the runner was called `n` times, then checks it was not called more often. A
    /// session counts as active once it is registered, a moment before its task runs the runner.
    async fn until_runner_calls(&self, n: u64) {
        let wait = async {
            while self.runner_calls() < n {
                pause(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("runner calls: {} < {n}", self.runner_calls()));
        assert_eq!(self.runner_calls(), n);
    }
}

// ---------------------------------------------------------------------------------------------
// The production call path: Silero VAD, the RTVI listener and bridge, a turn with bot audio

/// A protobuf varint.
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

/// A length-delimited protobuf field.
fn pb_bytes(tag: u32, payload: &[u8], out: &mut Vec<u8>) {
    varint(u64::from(tag << 3 | 2), out);
    varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// `Frame{ message: MessageFrame{ data: json } }`, as the Pipecat JS client sends it (encoded
/// by hand, independent of the server's codec).
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
#[derive(Debug)]
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
            Some(Ok(Message::Text(text))) => panic!("RTVI never sends text frames: {text}"),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        };
        match Frame::parse(bytes).unwrap().kind {
            Some(frame::Kind::Message(m)) => {
                return Some(FromBot::Rtvi(serde_json::from_str(&m.data).unwrap()));
            }
            Some(frame::Kind::Audio(a)) => return Some(FromBot::Audio(a.audio.len())),
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

#[cfg(feature = "silero-tract")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rtvi_call_with_the_production_vad_answers_a_typed_turn_with_bot_audio() {
    // The binary's VAD: the bundled Silero model (its load is blocking, off the runtime).
    let vad = tokio::task::spawn_blocking(Vad::production)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(format!("{vad:?}"), "Vad::Silero");
    let host = HostSettings {
        rtvi: Some(Listener {
            addr: ([127, 0, 0, 1], 0).into(),
            config: RtviConfig::default(),
        }),
        ..HostSettings::none()
    };
    let mut rig = Rig::start_with(4, policy(Duration::from_secs(5)), &host, vad).await;
    let url = format!("ws://{}/rtvi", rig.addrs.rtvi.unwrap());
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .expect("connect timed out")
        .expect("connect failed");

    // The handshake: `client-ready` is answered `bot-ready` with its id.
    let ready = json!({"version": "2.1.0",
                       "about": {"library": "@pipecat-ai/client-js", "library_version": "1.13.1"}});
    ws.send(rtvi_message("client-ready", "ready-1", &ready))
        .await
        .unwrap();
    loop {
        match next_from_bot(&mut ws).await {
            Some(FromBot::Rtvi(m)) if m["type"] == "bot-ready" => {
                assert_eq!(m["id"], "ready-1", "{m}");
                break;
            }
            Some(FromBot::Rtvi(m)) if m["type"] == "error" => panic!("call failed: {m}"),
            Some(_) => {}
            None => panic!("the call ended during the handshake"),
        }
    }
    rig.until("the RTVI call's session runs", |m| m.stats().active == 1)
        .await;

    // Caller audio runs through the Silero VAD on the pool (silence: no user turn).
    for _ in 0..25 {
        ws.send(rtvi_audio(&[0_u8; 640])).await.unwrap();
    }

    // A typed turn through the bridge: the engine answers it with the LLM's text and bot audio.
    ws.send(rtvi_message(
        "send-text",
        "text-1",
        &json!({"content": "hello there", "options": {"run_immediately": true}}),
    ))
    .await
    .unwrap();
    let (mut user_text, mut bot_text, mut audio_bytes) = (false, false, 0_usize);
    while !(user_text && bot_text && audio_bytes > 0) {
        match next_from_bot(&mut ws).await {
            Some(FromBot::Rtvi(m)) => match m["type"].as_str() {
                Some("user-llm-text") => {
                    assert_eq!(m["data"]["text"], "hello there", "{m}");
                    user_text = true;
                }
                Some("bot-llm-text") => bot_text = true,
                Some("error") => panic!("the call reported an error: {m}"),
                _ => {}
            },
            Some(FromBot::Audio(n)) => audio_bytes += n,
            None => panic!("the call ended before the bot answered"),
        }
    }
    // The agent file's definition reached the LLM (AgentConfig::configure ran), with the turn.
    let asked = rig.runner.factory().llm_requests();
    assert!(asked.contains("You are a test agent."), "{asked}");
    assert!(asked.contains("hello there"), "{asked}");

    // The client disconnects the bot: the session ends and the server drains clean.
    ws.send(rtvi_message("disconnect-bot", "bye-1", &json!(null)))
        .await
        .unwrap();
    rig.until("the RTVI call ended", |m| m.stats().active == 0)
        .await;
    let (trigger, mut signals) = ShutdownSignals::manual();
    assert!(trigger.trigger(ShutdownSignal::Requested));
    let report = tokio::time::timeout(WAIT, rig.server.take().unwrap().run(&mut signals))
        .await
        .unwrap();
    assert_eq!(report.outcome(), DrainOutcome::Drained, "{report:?}");
    assert_eq!(rig.manager.stats().ended.completed, 1);
}

// ---------------------------------------------------------------------------------------------
// SIP (feature `sip`): a scripted carrier on raw UDP

/// The WebRTC listener (feature `webrtc`): hosted next to the manager like the others. Its
/// signaling is plain HTTP, so the tests speak raw HTTP/1.1 (one request per connection); the
/// media path has its own tests in the transport (`tests/webrtc.rs`).
#[cfg(feature = "webrtc")]
mod webrtc {
    use brisvox_core::Secret;
    use brisvox_server::hosting::WebRtcHost;
    use brisvox_transport::webrtc::WebRtcConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::*;

    fn webrtc_only(token: Option<&str>) -> HostSettings {
        HostSettings {
            webrtc: Some(Listener {
                addr: ([127, 0, 0, 1], 0).into(),
                config: WebRtcHost {
                    config: WebRtcConfig {
                        http_addr: ([127, 0, 0, 1], 0).into(),
                        udp_addr: ([127, 0, 0, 1], 0).into(),
                        bearer_token: token.map(|t| Secret::new(t.to_owned())),
                        cors_origin: Some("http://localhost:8000".to_owned()),
                        ..WebRtcConfig::default()
                    },
                    codec_pool: None,
                },
            }),
            ..HostSettings::none()
        }
    }

    /// One HTTP request, the whole response text.
    async fn http(addr: SocketAddr, request: &str) -> String {
        let exchange = async {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut out = Vec::new();
            stream.read_to_end(&mut out).await.unwrap();
            String::from_utf8_lossy(&out).into_owned()
        };
        tokio::time::timeout(WAIT, exchange).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_signaling_listener_is_hosted_and_stops_with_the_server() {
        let mut rig = Rig::start_with(
            2,
            policy(Duration::from_secs(5)),
            &webrtc_only(Some("secret-token")),
            Vad::Off,
        )
        .await;
        let addr = rig.addrs.webrtc.unwrap();
        assert!(rig.addrs.websocket.is_none());

        // The demo page is served; the signaling endpoint wants the bearer token.
        let page = http(
            addr,
            "GET /demo HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        let post = "POST /whip HTTP/1.1\r\nHost: x\r\nContent-Type: application/sdp\r\n\
                    Content-Length: 4\r\nConnection: close\r\n\r\nnope";
        let refused = http(addr, post).await;
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        // With the token, a body that is no SDP offer is a client error, not a call.
        let with_token = post.replace("Host: x", "Host: x\r\nAuthorization: Bearer secret-token");
        let bad = http(addr, &with_token).await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        // A page on the configured origin may call it.
        let preflight = http(
            addr,
            "OPTIONS /whip HTTP/1.1\r\nHost: x\r\nOrigin: http://localhost:8000\r\n\
             Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            preflight
                .to_ascii_lowercase()
                .contains("access-control-allow-origin: http://localhost:8000"),
            "{preflight}"
        );
        assert_eq!(rig.manager.stats().active, 0);

        // The server's shutdown stops the listener: the port stops answering.
        let (trigger, mut signals) = ShutdownSignals::manual();
        let server = rig.server.take().unwrap();
        let run = tokio::spawn(async move { server.run(&mut signals).await });
        assert!(trigger.trigger(ShutdownSignal::Requested));
        let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
        assert_eq!(report.outcome(), DrainOutcome::Drained, "{report:?}");
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "the listener stopped"
        );
    }

    /// A WebSocket upgrade on the signaling port reaches the engine as a call, with the
    /// signaling listener's credentials and origin rule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_websocket_fallback_on_the_signaling_port_runs_a_call_on_the_engine() {
        let rig = Rig::start_with(
            2,
            policy(Duration::from_secs(5)),
            &webrtc_only(Some("secret-token")),
            Vad::Off,
        )
        .await;
        let addr = rig.addrs.webrtc.unwrap();
        let upgrade = |extra: &str| {
            format!(
                "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extra}\r\n"
            )
        };
        // Without the token, and from a page on another origin: refused as plain HTTP.
        let refused = http(addr, &upgrade("")).await;
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        let wrong = http(
            addr,
            &upgrade("Authorization: Bearer nope\r\nOrigin: https://evil.example\r\n"),
        )
        .await;
        assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");
        assert_eq!(rig.manager.stats().active, 0);

        // With the token, from the configured page origin: a call.
        let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Authorization", "Bearer secret-token".parse().unwrap());
        request
            .headers_mut()
            .insert("Origin", "http://localhost:8000".parse().unwrap());
        let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request))
            .await
            .expect("connect timed out")
            .expect("upgrade refused");
        let start = ClientMessage::Start(Start::new(24_000, Encoding::Pcm16));
        ws.send(Message::text(start.to_json())).await.unwrap();
        rig.until("the call's session runs", |m| m.stats().active == 1)
            .await;
        for _ in 0..10 {
            ws.send(Message::binary(vec![0_u8; 960])).await.unwrap();
        }
        ws.send(Message::text(ClientMessage::Stop.to_json()))
            .await
            .unwrap();
        rig.until("the call ended", |m| m.stats().active == 0).await;
    }
}

#[cfg(feature = "sip")]
mod sip {
    use super::*;
    use brisvox_transport::sip::{Cidr, SipConfig, SipTlsConfig, SrtpPolicy};
    use tokio::net::UdpSocket;

    /// A carrier placing one call: raw SIP text over UDP, an RTP socket that only receives.
    pub struct Carrier {
        sip: std::sync::Arc<UdpSocket>,
        rtp: UdpSocket,
        server: SocketAddr,
        call_id: String,
        cseq: u32,
        branches: u32,
        to_tag: Option<String>,
    }

    /// A response: status, headers (lower-case names).
    #[derive(Debug)]
    pub struct Reply {
        pub status: u16,
        headers: Vec<(String, String)>,
    }

    impl Reply {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }

        fn to_tag(&self) -> Option<String> {
            self.header("to")?
                .split(';')
                .find_map(|p| p.trim().strip_prefix("tag="))
                .map(str::to_owned)
        }
    }

    impl Carrier {
        pub async fn new(server: SocketAddr) -> Self {
            let sip = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = sip.local_addr().unwrap().port();
            Self {
                call_id: format!("hosting-{port}@carrier.test"),
                sip,
                rtp,
                server,
                cseq: 0,
                branches: 0,
                to_tag: None,
            }
        }

        fn request(&mut self, method: &str, cseq: u32, body: Option<&str>) -> String {
            self.branches += 1;
            let me = self.sip.local_addr().unwrap();
            let mut to = "<sip:agent@hosting.test>".to_owned();
            if let Some(tag) = &self.to_tag {
                to.push_str(&format!(";tag={tag}"));
            }
            let mut text = format!(
                "{method} sip:agent@{} SIP/2.0\r\nVia: SIP/2.0/UDP {me};branch=z9hG4bK-h{}-{};rport\r\n\
                 Max-Forwards: 70\r\nFrom: <sip:carrier@hosting.test>;tag=c{}\r\nTo: {to}\r\n\
                 Call-ID: {}\r\nCSeq: {cseq} {method}\r\nContact: <sip:carrier@{me}>\r\n",
                self.server,
                me.port(),
                self.branches,
                me.port(),
                self.call_id,
            );
            match body {
                Some(body) => text.push_str(&format!(
                    "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )),
                None => text.push_str("Content-Length: 0\r\n\r\n"),
            }
            text
        }

        async fn send(&self, text: &str) {
            self.sip
                .send_to(text.as_bytes(), self.server)
                .await
                .unwrap();
        }

        /// The next SIP message: (start line, headers).
        async fn recv(&self) -> (String, Vec<(String, String)>) {
            let mut buf = vec![0_u8; 8192];
            let (n, _) = tokio::time::timeout(WAIT, self.sip.recv_from(&mut buf))
                .await
                .expect("no SIP message within the wait")
                .unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            let head = text.split("\r\n\r\n").next().unwrap_or_default();
            let mut lines = head.split("\r\n");
            let start = lines.next().unwrap_or_default().to_owned();
            let headers = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
                .collect();
            (start, headers)
        }

        /// The next final response (provisional ones skipped).
        async fn final_reply(&self) -> Reply {
            loop {
                let (start, headers) = self.recv().await;
                let Some(status) = start
                    .strip_prefix("SIP/2.0 ")
                    .and_then(|r| r.split(' ').next())
                    .and_then(|c| c.parse::<u16>().ok())
                else {
                    continue;
                };
                if status >= 200 {
                    return Reply { status, headers };
                }
            }
        }

        /// INVITE with a PCMU offer; the final answer is acknowledged.
        pub async fn invite(&mut self) -> Reply {
            let sdp = format!(
                "v=0\r\no=carrier 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                 m=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=ptime:20\r\na=sendrecv\r\n",
                self.rtp.local_addr().unwrap().port()
            );
            self.cseq += 1;
            let invite = self.request("INVITE", self.cseq, Some(&sdp));
            self.send(&invite).await;
            let reply = self.final_reply().await;
            let saved = self.to_tag.take();
            self.to_tag = reply.to_tag();
            let ack = self.request("ACK", self.cseq, None);
            self.send(&ack).await;
            if reply.status == 200 {
                // A UAC acknowledges every retransmission of the 200 (RFC 3261, 13.2.2.4); this
                // carrier does not read the socket between the calls of a test, so it repeats
                // the ACK on the stack's retransmission cadence. A stack can miss an ACK that
                // arrives within microseconds of its 200 (rsipstack: before the server
                // transaction entered its Accepted state; seen 13 in 600 under CPU load), and
                // the call would then wait for an ACK that no honest carrier fails to repeat.
                let (sip, server) = (std::sync::Arc::clone(&self.sip), self.server);
                tokio::spawn(async move {
                    for wait in [600, 1000] {
                        pause(Duration::from_millis(wait)).await;
                        let _ = sip.send_to(ack.as_bytes(), server).await;
                    }
                });
            } else {
                self.to_tag = saved;
            }
            reply
        }

        /// The carrier hangs up.
        pub async fn bye(&mut self) -> Reply {
            self.cseq += 1;
            let bye = self.request("BYE", self.cseq, None);
            self.send(&bye).await;
            self.final_reply().await
        }

        /// Waits for the server's BYE (answering it).
        pub async fn hung_up_on(&self) {
            loop {
                let (start, headers) = self.recv().await;
                if !start.starts_with("BYE ") {
                    continue;
                }
                let get = |name: &str| {
                    headers
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let ok = format!(
                    "SIP/2.0 200 OK\r\nVia: {}\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
                    get("via"),
                    get("from"),
                    get("to"),
                    get("call-id"),
                    get("cseq")
                );
                self.send(&ok).await;
                return;
            }
        }
    }

    /// The SIP listener on a free loopback port, open to 127.0.0.1 only.
    fn sip_only() -> HostSettings {
        HostSettings {
            sip: Some(Listener {
                addr: ([127, 0, 0, 1], 0).into(),
                config: SipConfig {
                    bind_addr: ([127, 0, 0, 1], 0).into(),
                    allowed_sources: vec!["127.0.0.1".parse::<Cidr>().unwrap()],
                    ack_timeout: Duration::from_secs(3),
                    hangup_timeout: Duration::from_secs(2),
                    ..SipConfig::default()
                },
            }),
            ..HostSettings::none()
        }
    }

    /// A SIP INVITE reaches an opted-in agent process as `session.start.call`: the `Call-ID`
    /// and the user parts of `From` and `To`, as the transport adapter reads them off the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sip_call_tells_an_opted_in_agent_process_who_is_on_the_line() {
        use brisvox_agent_link::AgentLinkConfig;
        use brisvox_agent_link::client::AgentServer;
        use brisvox_agent_link::protocol::{CallIdentity, ServerBody, SessionAccept};

        let agent = AgentServer::bind("127.0.0.1:0".parse().unwrap(), SystemClock::shared())
            .await
            .unwrap();
        let mut link = AgentLinkConfig::new(agent.url().unwrap());
        link.caller_identity = true;
        let rig = Rig::start_linked(
            1,
            policy(Duration::from_secs(30)),
            &sip_only(),
            Vad::Off,
            AdmissionPolicy::none(),
            Some(Arc::new(link)),
        )
        .await;
        let mut carrier = Carrier::new(rig.addrs.sip.unwrap()).await;
        let call_id = carrier.call_id.clone();
        let answer = carrier.invite().await;
        assert_eq!(answer.status, 200, "{answer:?}");
        let mut session = tokio::time::timeout(WAIT, agent.accept())
            .await
            .expect("the agent process is called")
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
        assert_eq!(
            call,
            Some(CallIdentity {
                carrier: "sip".to_owned(),
                carrier_call_id: call_id,
                from: Some("carrier".to_owned()),
                to: Some("agent".to_owned()),
                pii_redacted: false,
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sip_call_is_admitted_by_the_manager_refused_when_full_and_while_draining() {
        let mut rig =
            Rig::start_with(1, policy(Duration::from_secs(30)), &sip_only(), Vad::Off).await;
        let addr = rig.addrs.sip.unwrap();
        assert!(rig.addrs.websocket.is_none());

        // A call: answered, and its session runs in the manager.
        let mut live = Carrier::new(addr).await;
        let answer = live.invite().await;
        assert_eq!(answer.status, 200, "{answer:?}");
        rig.until("the SIP call's session runs", |m| m.stats().active == 1)
            .await;
        rig.until_runner_calls(1).await;

        // Full (the manager's limit, not the SIP server's own): 503 with Retry-After.
        let mut second = Carrier::new(addr).await;
        let busy = second.invite().await;
        assert_eq!(busy.status, 503, "{busy:?}");
        assert!(busy.header("retry-after").is_some(), "{busy:?}");
        assert_eq!(rig.manager.stats().rejected_at_capacity, 1);

        // Draining: 503 without Retry-After; the live call keeps running.
        let (trigger, mut signals) = ShutdownSignals::manual();
        let server = rig.server.take().unwrap();
        let run = tokio::spawn(async move { server.run(&mut signals).await });
        assert!(trigger.trigger(ShutdownSignal::Requested));
        rig.until("the drain began", |m| !m.is_accepting()).await;
        let mut third = Carrier::new(addr).await;
        let refused = third.invite().await;
        assert_eq!(refused.status, 503, "{refused:?}");
        assert!(refused.header("retry-after").is_none(), "{refused:?}");
        assert_eq!(rig.manager.stats().rejected_shutting_down, 1);
        assert_eq!(rig.manager.stats().active, 1);
        assert!(!run.is_finished());

        // The carrier hangs up: the drain ends clean.
        assert_eq!(live.bye().await.status, 200);
        let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
        assert_eq!(report.outcome(), DrainOutcome::Drained, "{report:?}");
        assert_eq!((report.initial, report.drained), (1, 1), "{report:?}");
        assert_eq!(rig.runner_calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_hosted_sip_server_listens_for_sips_when_asked() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let mut settings = sip_only();
        if let Some(listener) = settings.sip.as_mut() {
            listener.config.tls = Some(SipTlsConfig {
                bind_addr: ([127, 0, 0, 1], 0).into(),
                ..SipTlsConfig::new(certified.cert.pem(), certified.signing_key.serialize_pem())
            });
            listener.config.srtp = SrtpPolicy::Optional;
        }
        let rig = Rig::start_with(2, policy(Duration::from_secs(30)), &settings, Vad::Off).await;
        let tls = rig.addrs.sip_tls.expect("a SIPS address");
        assert_ne!(Some(tls), rig.addrs.sip);
        // The port is open (the TLS handshake itself is tested in the transport crate).
        tokio::net::TcpStream::connect(tls).await.unwrap();
        // The plain SIP port still serves.
        let mut carrier = Carrier::new(rig.addrs.sip.unwrap()).await;
        assert_eq!(carrier.invite().await.status, 200);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sip_call_outliving_the_drain_is_hung_up_with_a_bye() {
        let mut rig =
            Rig::start_with(4, policy(Duration::from_millis(200)), &sip_only(), Vad::Off).await;
        let mut live = Carrier::new(rig.addrs.sip.unwrap()).await;
        assert_eq!(live.invite().await.status, 200);
        rig.until("the SIP call's session runs", |m| m.stats().active == 1)
            .await;

        let (trigger, mut signals) = ShutdownSignals::manual();
        let server = rig.server.take().unwrap();
        let run = tokio::spawn(async move { server.run(&mut signals).await });
        assert!(trigger.trigger(ShutdownSignal::Requested));
        // The drain times out; the cancel stage hangs the call up: the carrier gets a BYE.
        live.hung_up_on().await;
        let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
        assert_eq!(report.outcome(), DrainOutcome::Cancelled, "{report:?}");
        assert_eq!(report.leaked, 0, "{report:?}");
        assert_eq!(report.aborted, 0, "{report:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sip_invite_is_refused_with_503_while_the_pool_is_overloaded() {
        let pool = ScriptedPool::shared();
        let rig = Rig::start_admitted(
            4,
            policy(Duration::from_secs(30)),
            &sip_only(),
            Vad::Off,
            pool_policy(&pool, &[]),
        )
        .await;
        let addr = rig.addrs.sip.unwrap();

        pool.set_depth(40);
        let mut refused = Carrier::new(addr).await;
        let reply = refused.invite().await;
        assert_eq!(reply.status, 503, "{reply:?}");
        // The overload delay, the same as the websocket's.
        assert_eq!(reply.header("retry-after"), Some("5"), "{reply:?}");
        assert_eq!(rig.manager.stats().rejected_overloaded, 1);
        assert_eq!(rig.manager.stats().reserved, 0);

        // Calm for a whole window: the next INVITE is answered.
        pool.set_depth(0);
        let _ = Carrier::new(addr).await.invite().await; // first calm sample, still refused
        pause(Duration::from_millis(150)).await;
        let mut admitted = Carrier::new(addr).await;
        assert_eq!(admitted.invite().await.status, 200);
        rig.until("the SIP call's session runs", |m| m.stats().active == 1)
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sip_invite_is_refused_with_503_when_a_provider_budget_is_spent() {
        let pool = ScriptedPool::shared();
        let rig = Rig::start_admitted(
            4,
            policy(Duration::from_secs(30)),
            &sip_only(),
            Vad::Off,
            pool_policy(&pool, &[("deepgram", 1)]),
        )
        .await;
        let addr = rig.addrs.sip.unwrap();

        let mut live = Carrier::new(addr).await;
        assert_eq!(live.invite().await.status, 200);
        rig.until("the SIP call's session runs", |m| m.stats().active == 1)
            .await;
        assert_eq!(in_use(&rig.manager, "deepgram"), 1);

        // The budget is spent: 503 with the provider budget's delay (2 s), not the overload's.
        let mut refused = Carrier::new(addr).await;
        let reply = refused.invite().await;
        assert_eq!(reply.status, 503, "{reply:?}");
        assert_eq!(reply.header("retry-after"), Some("2"), "{reply:?}");
        let stats = rig.manager.stats();
        assert_eq!((stats.rejected_provider_budget, stats.reserved), (1, 0));

        // The call hangs up: its permit is back and the next INVITE is answered.
        assert_eq!(live.bye().await.status, 200);
        rig.until("the SIP call ended", |m| {
            let s = m.stats();
            s.active == 0 && s.reserved == 0
        })
        .await;
        assert_eq!(in_use(&rig.manager, "deepgram"), 0);
        assert_eq!(Carrier::new(addr).await.invite().await.status, 200);
    }
}

// ---------------------------------------------------------------------------------------------
// Admission policy: pool pressure and provider budgets, answered per transport.

/// A pool the tests drive by hand: 2 threads, a settable queue depth.
#[derive(Default)]
struct ScriptedPool {
    depth: std::sync::atomic::AtomicUsize,
}

impl ScriptedPool {
    fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set_depth(&self, n: usize) {
        self.depth.store(n, Ordering::SeqCst);
    }
}

impl LoadProbe for ScriptedPool {
    fn sample(&self) -> LoadSample {
        LoadSample {
            threads: 2,
            queue_depth: self.depth.load(Ordering::SeqCst),
            ..LoadSample::default()
        }
    }
}

/// Shed at 4 jobs per thread, recover after 50 ms calm at 1 per thread or less; budgets as given.
fn pool_policy(pool: &Arc<ScriptedPool>, limits: &[(&str, usize)]) -> AdmissionPolicy {
    let settings = AdmissionSettings {
        overload: Some(OverloadConfig {
            shed_queue_per_thread: 4,
            resume_queue_per_thread: 1,
            depth_confirm: Duration::ZERO,
            wait_limit: Duration::ZERO,
            window: Duration::from_millis(50),
            learn_ceiling: false,
        }),
        provider_limits: limits
            .iter()
            .map(|(name, limit)| ((*name).to_owned(), *limit))
            .collect(),
    };
    let probe: Arc<dyn LoadProbe> = pool.clone();
    AdmissionPolicy::new(&settings, Some(probe))
}

fn in_use(manager: &SessionManager, provider: &str) -> usize {
    manager
        .admission_snapshot()
        .providers
        .iter()
        .find(|p| p.name == provider)
        .unwrap()
        .in_use
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overloaded_pool_refuses_with_a_longer_retry_after_and_recovers_after_a_calm_window() {
    let pool = ScriptedPool::shared();
    let rig = Rig::start_admitted(
        4,
        policy(Duration::from_secs(30)),
        &websocket_only(),
        Vad::Off,
        pool_policy(&pool, &[]),
    )
    .await;
    let mut live = rig.call().await;
    rig.until("the call's session runs", |m| m.stats().active == 1)
        .await;

    // 8 queued on 2 threads: shed at once, before the upgrade; the running call is untouched.
    pool.set_depth(8);
    let (status, retry_after, body) = rig.refused_in_full().await;
    assert_eq!(status, 503);
    assert_eq!(retry_after.as_deref(), Some("5"));
    assert_eq!(body, "server overloaded");
    assert_eq!(rig.manager.stats().rejected_overloaded, 1);
    assert_eq!(rig.manager.stats().active, 1);
    assert_eq!(rig.manager.stats().reserved, 0);

    // Back under the resume limit: refused until a whole window of calm samples has passed.
    pool.set_depth(1);
    assert_eq!(rig.refused().await, (503, true));
    pause(Duration::from_millis(120)).await;
    let mut again = rig.call().await;
    rig.until("both calls run", |m| m.stats().active == 2).await;

    for ws in [&mut live, &mut again] {
        ws.send(Message::text(ClientMessage::Stop.to_json()))
            .await
            .unwrap();
    }
    rig.until("both calls ended", |m| {
        let s = m.stats();
        s.active == 0 && s.reserved == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_provider_refuses_the_next_call_and_its_permit_comes_back_when_a_call_ends() {
    let pool = ScriptedPool::shared();
    let rig = Rig::start_admitted(
        8,
        policy(Duration::from_secs(30)),
        &websocket_only(),
        Vad::Off,
        pool_policy(&pool, &[("deepgram", 1)]),
    )
    .await;
    let mut first = rig.call().await;
    rig.until("the call's session runs", |m| m.stats().active == 1)
        .await;
    assert_eq!(in_use(&rig.manager, "deepgram"), 1);

    let (status, retry_after, body) = rig.refused_in_full().await;
    assert_eq!(status, 503);
    assert_eq!(retry_after.as_deref(), Some("2"));
    assert_eq!(body, "provider capacity reached");
    let stats = rig.manager.stats();
    assert_eq!((stats.rejected_provider_budget, stats.reserved), (1, 0));

    // The call hangs up: its permit is back and the next call is admitted.
    first
        .send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
    rig.until("the first call ended", |m| {
        let s = m.stats();
        s.active == 0 && s.reserved == 0
    })
    .await;
    assert_eq!(in_use(&rig.manager, "deepgram"), 0);
    let mut second = rig.call().await;
    rig.until("the second call runs", |m| m.stats().active == 1)
        .await;
    second
        .send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_never_becomes_a_call_gives_its_permit_back() {
    let pool = ScriptedPool::shared();
    let rig = Rig::start_admitted(
        8,
        policy(Duration::from_secs(30)),
        &websocket_only(),
        Vad::Off,
        pool_policy(&pool, &[("deepgram", 1)]),
    )
    .await;
    // Upgraded (the reservation and the permit are held), then closed before `start`.
    let (mut ws, _) = tokio_tungstenite::connect_async(rig.url()).await.unwrap();
    rig.until("the handshake holds a reservation", |m| {
        m.stats().reserved == 1
    })
    .await;
    assert_eq!(in_use(&rig.manager, "deepgram"), 1);
    assert_eq!(rig.refused().await, (503, true));
    ws.close(None).await.unwrap();
    drop(ws);
    rig.until("the reservation is released", |m| m.stats().reserved == 0)
        .await;
    assert_eq!(in_use(&rig.manager, "deepgram"), 0);
    let mut call = rig.call().await;
    call.send(Message::text(ClientMessage::Stop.to_json()))
        .await
        .unwrap();
}
