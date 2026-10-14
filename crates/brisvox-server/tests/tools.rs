//! The tools of the binary's calls, end to end through the [`EngineRunner`] the binary
//! uses, over mock providers (no keys): an application tool plugged in through [`AppTools`] runs
//! when the model calls it and its result reaches the model; `end_call` (feature
//! `call-control`) ends the call; a definition naming a tool nobody provides is refused; the
//! example agents start with the binary's tool setup; the Smart Turn model loads.
#![cfg(all(
    feature = "provider-deepgram",
    feature = "provider-openai",
    feature = "provider-cartesia"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)]
// test helpers

use std::collections::BTreeMap;
#[cfg(any(feature = "all-providers", feature = "smart-turn-ort"))]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::{
    RegistryError, Tool, ToolCallCtx, ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::{Clock, SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::hosting::Listener;
use brisvox_server::{
    AppTools, CallProviders, EngineRunner, HostSettings, ManagerConfig, ProviderFactory,
    RunnerConfig, Server, ServerSettings, SessionManager, ShutdownPolicy, ToolSetup, Vad,
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

/// A definition naming `tools`; its keys are fake (the mocks never use them).
fn agent_text(tools: &str) -> String {
    format!(
        r#"
[agent]
name = "tools-test"
language = "en"
system_prompt = "You are a test agent."
tools = [{tools}]

[stt]
provider = "deepgram"
api_key_env = "TOOLS_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "TOOLS_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "TOOLS_TEST_TTS_KEY"
voice = "voice-123"
"#
    )
}

fn fake_keys() -> BTreeMap<String, String> {
    [
        ("TOOLS_TEST_STT_KEY", "stt-key"),
        ("TOOLS_TEST_LLM_KEY", "llm-key"),
        ("TOOLS_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Writes `text` to a file of its own and loads it, as the binary does.
fn agent_file(text: &str) -> AgentConfig {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "brisvox-tools-{}-{}",
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

// ---------------------------------------------------------------------------------------------
// An application tool

/// `check_stock`: records its arguments, answers a fixed count.
struct CheckStock {
    calls: Arc<Mutex<Vec<Value>>>,
}

impl Tool for CheckStock {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(args.clone());
            Ok(ToolOutput::Json(
                json!({"item": args["item"], "in_stock": 3, "aisle": "B7"}),
            ))
        })
    }
}

/// The application: one tool, the calls it got.
#[derive(Default)]
struct Shop {
    calls: Arc<Mutex<Vec<Value>>>,
}

impl AppTools for Shop {
    fn register(
        &self,
        registry: &mut ToolRegistry,
        _clock: &SharedClock,
    ) -> Result<(), RegistryError> {
        registry.register(
            ToolSpec::new(
                "check_stock",
                "How many of an item the shop has.",
                json!({
                    "type": "object",
                    "properties": {"item": {"type": "string"}},
                    "required": ["item"],
                    "additionalProperties": false
                }),
                ToolKind::ReadOnly,
            ),
            CheckStock {
                calls: Arc::clone(&self.calls),
            },
        )?;
        // A tool the definition does not name: never offered to the model.
        registry.register(
            ToolSpec::new(
                "delete_everything",
                "Not for this agent.",
                json!({"type": "object", "properties": {}}),
                ToolKind::SideEffect,
            ),
            CheckStock {
                calls: Arc::clone(&self.calls),
            },
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Mock providers whose LLM calls a tool first

/// Every call's LLM: first `script` (a tool call), then `follow_up` when set, then `ANSWER`.
struct Mocks {
    clock: SharedClock,
    script: LlmScript,
    follow_up: Option<LlmScript>,
    llms: Mutex<Vec<MockLlm>>,
}

const ANSWER: &str = "We have three lamps in aisle B7.";

impl Mocks {
    fn new(clock: SharedClock, script: LlmScript) -> Self {
        Self {
            clock,
            script,
            follow_up: None,
            llms: Mutex::new(Vec::new()),
        }
    }

    /// The response after the tool round is `follow_up` (not `ANSWER`).
    #[cfg(feature = "call-control")]
    fn then(mut self, follow_up: LlmScript) -> Self {
        self.follow_up = Some(follow_up);
        self
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
        let llm = MockLlm::new(self.clock.clone());
        llm.push(self.script.clone());
        if let Some(follow_up) = &self.follow_up {
            llm.push(follow_up.clone());
        }
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

// ---------------------------------------------------------------------------------------------
// An RTVI client (typed turns: no VAD or STT needed)

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

/// `Frame{ message: MessageFrame{ data: json } }`, as the Pipecat JS client sends it.
fn rtvi_message(kind: &str, id: &str, data: &Value) -> Message {
    let json = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data}).to_string();
    let mut inner = Vec::new();
    pb_bytes(1, json.as_bytes(), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(4, &inner, &mut frame);
    Message::binary(frame)
}

/// The next RTVI message, skipping audio; `None` once the connection ended.
async fn next_message(ws: &mut Client) -> Option<Value> {
    loop {
        let next = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("nothing from the bot within the wait");
        let bytes = match next {
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        };
        if let Some(frame::Kind::Message(m)) = Frame::parse(bytes).unwrap().kind {
            return Some(serde_json::from_str(&m.data).unwrap());
        }
    }
}

/// A server with an RTVI listener on a free port; its calls on `runner`.
struct Rig {
    server: Server,
    manager: SessionManager,
    _pool: InferencePool,
}

impl Rig {
    async fn start<F: ProviderFactory>(runner: &Arc<EngineRunner<F>>, pool: InferencePool) -> Self {
        let host = HostSettings {
            rtvi: Some(Listener {
                addr: ([127, 0, 0, 1], 0).into(),
                config: RtviConfig::default(),
            }),
            ..HostSettings::none()
        };
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
        let server = Server::start_hosting(settings, &host, SystemClock::shared(), runner)
            .await
            .unwrap();
        Self {
            manager: server.manager().clone(),
            server,
            _pool: pool,
        }
    }

    /// Connects an RTVI client and completes its handshake.
    async fn call(&self) -> Client {
        let url = format!("ws://{}/rtvi", self.server.listener_addrs().rtvi.unwrap());
        let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        let ready = json!({"version": "2.1.0",
                           "about": {"library": "@pipecat-ai/client-js", "library_version": "1.13.1"}});
        ws.send(rtvi_message("client-ready", "ready-1", &ready))
            .await
            .unwrap();
        loop {
            match next_message(&mut ws).await {
                Some(m) if m["type"] == "bot-ready" => return ws,
                Some(m) if m["type"] == "error" => panic!("call failed: {m}"),
                Some(_) => {}
                None => panic!("the call ended during the handshake"),
            }
        }
    }

    async fn until(&self, what: &str, done: impl Fn(&SessionManager) -> bool) {
        let wait = async {
            while !done(&self.manager) {
                SystemClock.sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: {:?}", self.manager.stats()));
    }
}

fn pool(clock: &SharedClock) -> InferencePool {
    InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "tools-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap()
}

/// Sends a typed turn and waits for the bot's text answer.
async fn ask(ws: &mut Client, text: &str) -> String {
    ws.send(rtvi_message(
        "send-text",
        "text-1",
        &json!({"content": text, "options": {"run_immediately": true}}),
    ))
    .await
    .unwrap();
    let mut said = String::new();
    loop {
        match next_message(ws).await {
            Some(m) if m["type"] == "bot-llm-text" => {
                said.push_str(m["data"]["text"].as_str().unwrap_or_default());
            }
            Some(m) if m["type"] == "bot-llm-stopped" && !said.is_empty() => return said,
            Some(m) if m["type"] == "error" => panic!("the call reported an error: {m}"),
            Some(_) => {}
            None => return said,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_application_tool_runs_through_the_binarys_runner_and_its_result_reaches_the_model() {
    let clock = SystemClock::shared();
    let shop = Shop::default();
    let tools = ToolSetup::none().with_app(&shop, &clock).unwrap();
    let script = LlmScript::text(Vec::<String>::new()).with_tool_call(
        "call-1",
        "check_stock",
        r#"{"item":"lamp"}"#,
    );
    let pool = pool(&clock);
    let runner = Arc::new(
        EngineRunner::with_tools(
            agent_file(&agent_text("\"check_stock\"")),
            Mocks::new(clock.clone(), script),
            Offload::Pool(pool.handle()),
            Vad::Off,
            RunnerConfig::default(),
            tools,
            &clock,
        )
        .unwrap(),
    );
    let rig = Rig::start(&runner, pool).await;
    let mut ws = rig.call().await;

    let said = ask(&mut ws, "do you have lamps?").await;
    assert_eq!(said, ANSWER);
    // The tool ran once, with the model's arguments.
    let calls = shop.calls.lock().unwrap().clone();
    assert_eq!(calls, [json!({"item": "lamp"})]);
    // The model was offered only the definition's tool, and got the tool's result back.
    let asked = runner.factory().llm_requests();
    assert!(asked.contains("check_stock"), "{asked}");
    assert!(!asked.contains("delete_everything"), "{asked}");
    assert!(
        asked.contains("B7"),
        "the tool result reached the model: {asked}"
    );

    ws.send(rtvi_message("disconnect-bot", "bye-1", &json!(null)))
        .await
        .unwrap();
    rig.until("the call ended", |m| m.stats().active == 0).await;
}

#[test]
fn a_definition_naming_a_tool_nobody_provides_is_refused_with_what_would_provide_it() {
    let clock = SystemClock::shared();
    let agent = agent_file(&agent_text("\"check_stock\", \"transfer_call\""));
    let tools = ToolSetup::none()
        .with_app(&Shop::default(), &clock)
        .unwrap();
    let error = EngineRunner::with_tools(
        agent,
        Mocks::new(clock.clone(), LlmScript::text(["x"])),
        Offload::Inline(clock.clone()),
        Vad::Off,
        RunnerConfig::default(),
        tools,
        &clock,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("agent.tools[1]"), "{error}");
    assert!(error.contains("transfer_call"), "{error}");
    assert!(error.contains("BRISVOX_CALL_CONTROL"), "{error}");
    assert!(
        !error.contains("agent.tools[0]"),
        "check_stock is provided: {error}"
    );

    // `new` offers no tools at all.
    let agent = agent_file(&agent_text("\"check_stock\""));
    let error = EngineRunner::new(
        agent,
        Mocks::new(clock.clone(), LlmScript::text(["x"])),
        Offload::Inline(clock.clone()),
        Vad::Off,
        RunnerConfig::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("check_stock"), "{error}");
}

/// Every example agent with fake keys: the env map the binary would see.
#[cfg(feature = "all-providers")]
fn examples() -> Vec<(PathBuf, AgentConfig)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/agents");
    let keys: BTreeMap<String, String> = [
        "DEEPGRAM_API_KEY",
        "OPENAI_API_KEY",
        "CARTESIA_API_KEY",
        "ANTHROPIC_API_KEY",
        "ELEVENLABS_API_KEY",
        "ASSEMBLYAI_API_KEY",
        "GROQ_API_KEY",
    ]
    .into_iter()
    .map(|k| (k.to_owned(), "placeholder".to_owned()))
    .collect();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let ext = path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("toml" | "yaml")) {
            continue;
        }
        let agent = config::load_file(&path, &keys).unwrap_or_else(|e| panic!("{e}"));
        found.push((path, agent));
    }
    assert!(found.len() >= 6, "{found:?}", found = found.len());
    found
}

/// The example agents start: the binary's tool setup (demo tools, Twilio call control from
/// the environment) provides every tool they name, over their real provider adapters.
#[cfg(all(
    feature = "all-providers",
    feature = "demo-tools",
    feature = "call-control"
))]
#[tokio::test]
async fn the_example_agents_start_with_the_binarys_tool_setup() {
    let clock = SystemClock::shared();
    let env: HashMap<String, String> = [
        ("BRISVOX_CALL_CONTROL", "twilio"),
        (
            "BRISVOX_TWILIO_ACCOUNT_SID",
            "AC00000000000000000000000000000000",
        ),
        ("TWILIO_AUTH_TOKEN", "placeholder"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let shutdown = tokio_util::sync::CancellationToken::new();
    for (path, agent) in examples() {
        let providers =
            brisvox_server::ConfiguredProviders::from_agent(&agent, &clock, &shutdown).unwrap();
        let tools = ToolSetup::from_env(&env, &clock, None).unwrap();
        let runner = EngineRunner::with_tools(
            agent,
            providers,
            Offload::Inline(clock.clone()),
            Vad::Off,
            RunnerConfig::default(),
            tools,
            &clock,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!runner.has_smart_turn());
    }
    shutdown.cancel();
}

/// Without the demo tools, an example naming one is refused, and the error says which feature
/// provides it.
#[cfg(all(feature = "all-providers", not(feature = "demo-tools")))]
#[test]
fn an_example_without_its_application_tool_is_refused() {
    let clock = SystemClock::shared();
    for (path, agent) in examples() {
        if !path.to_string_lossy().contains("support-en") {
            continue;
        }
        let error = EngineRunner::with_tools(
            agent,
            Mocks::new(clock.clone(), LlmScript::text(["x"])),
            Offload::Inline(clock.clone()),
            Vad::Off,
            RunnerConfig::default(),
            ToolSetup::from_env(&HashMap::<String, String>::new(), &clock, None).unwrap(),
            &clock,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("lookup_order"), "{error}");
        assert!(error.contains("demo-tools"), "{error}");
    }
}

/// The binary's Smart Turn: loaded for an adaptive agent, not for a fixed-silence one, never
/// with `off`; a missing file stops the start.
#[cfg(feature = "smart-turn-ort")]
#[test]
fn the_smart_turn_model_loads_for_adaptive_turns() {
    use brisvox_server::runner::{ENV_MODELS_DIR, ENV_SMART_TURN_MODEL, load_smart_turn};

    let models = std::env::var("BRISVOX_MODELS_DIR")
        .unwrap_or_else(|_| format!("{}/../../models", env!("CARGO_MANIFEST_DIR")));
    let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let adaptive = agent_file(&agent_text(""));
    let model = load_smart_turn(&env(&[(ENV_MODELS_DIR, &models)]), &adaptive)
        .unwrap_or_else(|e| panic!("{e}: run scripts/models/fetch_models.py"));
    let model = model.expect("an adaptive agent gets the model");
    let clock = SystemClock::shared();
    let runner = EngineRunner::new(
        adaptive.clone(),
        Mocks::new(clock.clone(), LlmScript::text(["x"])),
        Offload::Inline(clock),
        Vad::Off,
        RunnerConfig::default(),
    )
    .unwrap()
    .with_smart_turn(model);
    assert!(runner.has_smart_turn());

    let off = env(&[(ENV_MODELS_DIR, &models), (ENV_SMART_TURN_MODEL, "off")]);
    assert!(load_smart_turn(&off, &adaptive).unwrap().is_none());
    let fixed = agent_file(&format!(
        "{}\n[turn]\npolicy = \"fixed_silence\"\nsilence_ms = 600\n",
        agent_text("")
    ));
    assert!(
        load_smart_turn(&env(&[(ENV_MODELS_DIR, "no/such/dir")]), &fixed)
            .unwrap()
            .is_none()
    );
    let error = load_smart_turn(&env(&[(ENV_MODELS_DIR, "no/such/dir")]), &adaptive)
        .err()
        .expect("a missing model stops the start")
        .to_string();
    assert!(error.contains(ENV_SMART_TURN_MODEL), "{error}");
}

// ---------------------------------------------------------------------------------------------
// end_call (feature `call-control`)

#[cfg(feature = "call-control")]
mod telephony {
    use brisvox_agent::telephony::{DtmfString, TransferRequest};
    use brisvox_server::tools::telephony::CallControls;
    use brisvox_transport::call_control::{
        CallControl, CallFuture, CallRef, CallerState, Capabilities, WarmTransfer,
    };

    use super::*;

    /// A carrier binding that is never reached: the RTVI call has none.
    #[derive(Debug)]
    struct Unreached;

    impl CallControl for Unreached {
        fn provider(&self) -> &'static str {
            "twilio"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                send_dtmf: false,
                warm_transfer: false,
                warm_briefing: false,
            }
        }

        fn send_dtmf<'a>(
            &'a self,
            _call: &'a CallRef,
            _digits: &'a DtmfString,
            _idempotency: u64,
        ) -> CallFuture<'a, ()> {
            unreachable!()
        }

        fn transfer_cold<'a>(
            &'a self,
            _call: &'a CallRef,
            _request: &'a TransferRequest,
        ) -> CallFuture<'a, ()> {
            unreachable!()
        }

        fn transfer_warm<'a>(
            &'a self,
            _call: &'a CallRef,
            _request: &'a TransferRequest,
        ) -> CallFuture<'a, WarmTransfer> {
            unreachable!()
        }

        fn complete_warm<'a>(&'a self, _transfer: &'a WarmTransfer) -> CallFuture<'a, ()> {
            unreachable!()
        }

        fn abort_warm<'a>(&'a self, _transfer: &'a WarmTransfer) -> CallFuture<'a, CallerState> {
            unreachable!()
        }

        fn hangup<'a>(&'a self, _call: &'a CallRef, _idempotency: u64) -> CallFuture<'a, ()> {
            unreachable!()
        }
    }

    /// How a call that `end_call` ended went, from the client's side.
    struct Ended {
        /// The bot's text.
        said: String,
        /// From the end of the bot's response to the connection's close.
        closed_after: Duration,
        /// Whether the bot's speech ended (`bot-stopped-speaking`) after the response did.
        farewell_heard: bool,
    }

    /// A call whose model answers a question with `script` (calling `end_call`), then with
    /// `follow_up`: what the client saw until the server closed the connection.
    async fn ended_by_end_call(script: LlmScript, follow_up: LlmScript) -> Ended {
        let clock = SystemClock::shared();
        let control: Arc<dyn CallControl> = Arc::new(Unreached);
        let tools = ToolSetup::none().with_call_control(CallControls::new().with_twilio(control));
        let pool = pool(&clock);
        let runner = Arc::new(
            EngineRunner::with_tools(
                agent_file(&agent_text("\"end_call\"")),
                Mocks::new(clock.clone(), script).then(follow_up),
                Offload::Pool(pool.handle()),
                Vad::Off,
                RunnerConfig::default(),
                tools,
                &clock,
            )
            .unwrap(),
        );
        let rig = Rig::start(&runner, pool).await;
        let mut ws = rig.call().await;
        rig.until("the call runs", |m| m.stats().active == 1).await;

        ws.send(rtvi_message(
            "send-text",
            "text-1",
            &json!({"content": "that's all, bye", "options": {"run_immediately": true}}),
        ))
        .await
        .unwrap();
        let mut said = String::new();
        let mut answered_at = None;
        let mut farewell_heard = false;
        // The client never hangs up: the server closes the connection.
        while let Some(m) = next_message(&mut ws).await {
            match m["type"].as_str().unwrap_or_default() {
                "bot-llm-text" => said.push_str(m["data"]["text"].as_str().unwrap_or_default()),
                "bot-llm-stopped" => answered_at = Some(clock.now()),
                "bot-stopped-speaking" if answered_at.is_some() => farewell_heard = true,
                "error" => panic!("the call reported an error: {m}"),
                _ => {}
            }
        }
        let closed_at = clock.now();
        let answered_at = answered_at.expect("the bot's response ended");
        rig.until("the call ended by itself", |m| m.stats().active == 0)
            .await;
        assert_eq!(rig.manager.stats().ended.completed, 1);
        let asked = runner.factory().llm_requests();
        assert!(asked.contains("end_call"), "{asked}");
        Ended {
            said,
            closed_after: closed_at - answered_at,
            farewell_heard,
        }
    }

    /// Well under `FAREWELL_TIMEOUT` (10 s): the hang-up followed the farewell, not the bound.
    const PROMPT: Duration = Duration::from_secs(5);

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_call_ends_the_call_after_the_farewell() {
        // The model ends the call, then says goodbye: the farewell is heard, then the call ends
        // without the client hanging up.
        let ended = ended_by_end_call(
            LlmScript::text(Vec::<String>::new()).with_tool_call("call-1", "end_call", "{}"),
            LlmScript::text([ANSWER]),
        )
        .await;
        assert_eq!(ended.said, ANSWER);
        assert!(ended.farewell_heard, "the call closed before the farewell");
        assert!(ended.closed_after < PROMPT, "{:?}", ended.closed_after);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_farewell_spoken_with_the_end_call_ends_the_call_once_played() {
        // The farewell and the call in one response, nothing after the tool's result.
        let ended = ended_by_end_call(
            LlmScript::text(["Goodbye, have a nice day!"])
                .with_tool_call("call-1", "end_call", "{}"),
            LlmScript::text(Vec::<String>::new()),
        )
        .await;
        assert_eq!(ended.said, "Goodbye, have a nice day!");
        assert!(ended.farewell_heard, "the call closed before the farewell");
        assert!(ended.closed_after < PROMPT, "{:?}", ended.closed_after);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_end_call_hangs_up_without_waiting_for_a_farewell() {
        // The model said goodbye before: it ends the call and says nothing more. No bot speech
        // ends after the request, so the hang-up follows the quiet after the response.
        let ended = ended_by_end_call(
            LlmScript::text(Vec::<String>::new()).with_tool_call("call-1", "end_call", "{}"),
            LlmScript::text(Vec::<String>::new()),
        )
        .await;
        assert_eq!(ended.said, "");
        assert!(ended.closed_after < PROMPT, "{:?}", ended.closed_after);
    }
}
