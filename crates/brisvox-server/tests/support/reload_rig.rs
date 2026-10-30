//! What the reload tests share: a server hosting the generic websocket and RTVI listeners and the
//! health endpoint on loopback, its calls on mock providers, its agent in a file the test edits,
//! and the reload hub over it. Each call is tagged with the definition (`agent.name`) it ran on.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// test helpers

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::{Clock, SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_server::config::{AgentConfig, Env};
use brisvox_server::hosting::Listener;
use brisvox_server::reload::{
    AgentSet, AgentSlot, DefinitionSource, ReloadHub, RunnerRecipe, SwapRunner, load_file_hashed,
    recording_veto,
};
use brisvox_server::{
    AdmissionPolicy, CallProviders, HostSettings, ListenerAddrs, ManagerConfig, Observability,
    ProviderFactory, RunnerConfig, Server, ServerSettings, SessionManager, ToolSetup, Vad,
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
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub const WAIT: Duration = Duration::from_secs(15);

/// The keys of the mock providers every definition names.
pub fn keys() -> BTreeMap<String, String> {
    [
        ("MCP_TEST_STT_KEY", "stt-key"),
        ("MCP_TEST_LLM_KEY", "llm-key"),
        ("MCP_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// A definition called `name` with the system prompt `prompt` and `extra` TOML after it.
pub fn agent_text(name: &str, prompt: &str, extra: &str) -> String {
    format!(
        r#"
[agent]
name = "{name}"
language = "en"
system_prompt = "{prompt}"

[stt]
provider = "deepgram"
api_key_env = "MCP_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "MCP_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "MCP_TEST_TTS_KEY"
voice = "voice-123"

{extra}
"#
    )
}

/// The LLM of every call, with the definition it ran on.
pub type Seen = Arc<Mutex<Vec<(String, MockLlm)>>>;

/// Fresh mock providers for every call of one snapshot; the LLMs are recorded with the name of
/// the definition the snapshot was built from.
pub struct Mocks {
    clock: SharedClock,
    agent: String,
    seen: Seen,
}

pub const ANSWER: &str = "Hello from the agent.";

impl ProviderFactory for Mocks {
    type Stt = MockStt;
    type Llm = MockLlm;
    type Tts = MockTts;

    fn providers(&self) -> CallProviders<MockStt, MockLlm, MockTts> {
        let llm = MockLlm::new(self.clock.clone());
        llm.set_fallback(LlmScript::text([ANSWER]));
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.agent.clone(), llm.clone()));
        CallProviders {
            stt: Arc::new(MockStt::new(self.clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(self.clock.clone())),
        }
    }
}

/// Every request the LLMs of calls that ran on definition `agent` received, in `Debug` form.
pub fn asked(seen: &Seen, agent: &str) -> String {
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    format!(
        "{:?}",
        seen.iter()
            .filter(|(name, _)| name == agent)
            .map(|(_, llm)| llm.requests())
            .collect::<Vec<_>>()
    )
}

/// How many calls ran on definition `agent`.
pub fn calls_of(seen: &Seen, agent: &str) -> usize {
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(name, _)| name == agent)
        .count()
}

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "brisvox-reload-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A definition file in a directory of its own, read like the binary reads it.
pub struct AgentFile {
    pub dir: PathBuf,
    pub path: PathBuf,
}

impl AgentFile {
    pub fn new(tag: &str, text: &str) -> Self {
        let dir = temp_dir(tag);
        let path = dir.join("agent.toml");
        std::fs::write(&path, text).unwrap();
        Self { dir, path }
    }

    pub fn write(&self, text: &str) {
        std::fs::write(&self.path, text).unwrap();
    }
}

impl Drop for AgentFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The slot of the definition in `file`: its swappable runner and the recipe for its next
/// snapshots (mock providers tagged in `seen`).
pub async fn slot(
    label: &'static str,
    file: &AgentFile,
    env: &Arc<BTreeMap<String, String>>,
    seen: &Seen,
    clock: &SharedClock,
    pool: &InferencePool,
    obs: &Arc<Observability>,
) -> (AgentSlot<Mocks>, Arc<SwapRunner<Mocks>>) {
    let (hash, agent) = load_file_hashed(&file.path, &**env);
    let agent = agent.unwrap();
    let factory_clock = clock.clone();
    let factory_seen = Arc::clone(seen);
    let recipe = RunnerRecipe::new(
        move |agent: &AgentConfig| {
            Ok(Mocks {
                clock: factory_clock.clone(),
                agent: agent.name().to_owned(),
                seen: Arc::clone(&factory_seen),
            })
        },
        Offload::Pool(pool.handle()),
        Vad::Off,
        RunnerConfig::default(),
        ToolSetup::none(),
        clock,
    )
    .with_observability(Arc::clone(obs));
    let runner = recipe.build(agent).await.unwrap();
    let swap = Arc::new(SwapRunner::new(runner));
    let slot = AgentSlot::new(
        label,
        DefinitionSource::File(file.path.clone()),
        hash.unwrap(),
        Arc::clone(&swap),
        recipe,
    )
    .with_veto(recording_veto);
    (slot, swap)
}

pub fn inference_pool(clock: &SharedClock) -> InferencePool {
    InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "reload-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap()
}

/// A server on loopback: the websocket, RTVI and health listeners, reloading `file`.
pub struct Rig {
    pub server: Option<Server>,
    pub manager: SessionManager,
    pub addrs: ListenerAddrs,
    pub health: SocketAddr,
    pub swap: Arc<SwapRunner<Mocks>>,
    pub hub: Arc<ReloadHub>,
    pub obs: Arc<Observability>,
    pub seen: Seen,
    pub file: AgentFile,
    pub clock: SharedClock,
    _pool: InferencePool,
}

impl Rig {
    pub async fn start(tag: &str, text: &str) -> Self {
        Self::start_with(tag, text, None).await
    }

    /// `admin_token`: the bearer token of `POST /admin/reload` (none: open on loopback).
    pub async fn start_with(tag: &str, text: &str, admin_token: Option<&str>) -> Self {
        let clock = SystemClock::shared();
        let file = AgentFile::new(tag, text);
        let env = Arc::new(keys());
        let seen: Seen = Arc::default();
        let obs = Arc::new(Observability::new());
        let pool = inference_pool(&clock);
        let (slot, swap) = slot("agent", &file, &env, &seen, &clock, &pool, &obs).await;
        let set = AgentSet::new(vec![slot], env as Arc<dyn Env + Send + Sync>);
        let hub = Arc::new(
            ReloadHub::new(Arc::new(set), clock.clone()).with_observability(Arc::clone(&obs)),
        );
        let loopback: SocketAddr = ([127, 0, 0, 1], 0).into();
        let host = HostSettings {
            websocket: Some(loopback),
            rtvi: Some(Listener {
                addr: loopback,
                config: RtviConfig::default(),
            }),
            ..HostSettings::none()
        };
        let settings = ServerSettings {
            manager: ManagerConfig::new(8).unwrap(),
            health_addr: Some(loopback),
            admin_token: admin_token.map(|t| brisvox_core::Secret::new(t.to_owned())),
            ..ServerSettings::default()
        };
        let server = Server::start_hosting_routed(
            settings,
            AdmissionPolicy::none(),
            &host,
            clock.clone(),
            &swap,
            None,
        )
        .await
        .unwrap();
        server.manager().attach_observability(Arc::clone(&obs));
        assert!(server.manager().attach_reload(Arc::clone(&hub)));
        Self {
            manager: server.manager().clone(),
            addrs: server.listener_addrs(),
            health: server.health_addr().unwrap(),
            server: Some(server),
            swap,
            hub,
            obs,
            seen,
            file,
            clock,
            _pool: pool,
        }
    }

    /// Waits until `done` holds of the manager.
    pub async fn until(&self, what: &str, done: impl Fn(&SessionManager) -> bool) {
        let wait = async {
            while !done(&self.manager) {
                self.clock.sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: {:?}", self.manager.stats()));
    }

    /// Connects a generic websocket call and starts it.
    pub async fn ws_call(&self) -> Client {
        let url = format!("ws://{}/call", self.addrs.websocket.unwrap());
        let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
        ws.send(Message::text(start.to_json())).await.unwrap();
        loop {
            let next = tokio::time::timeout(WAIT, ws.next())
                .await
                .expect("no answer to the start message");
            match next {
                Some(Ok(Message::Text(text))) => {
                    let message = ServerMessage::parse(text.as_str()).unwrap();
                    assert!(
                        matches!(message, ServerMessage::Started { .. }),
                        "{message:?}"
                    );
                    return ws;
                }
                Some(Ok(_)) => {}
                other => panic!("the call did not start: {other:?}"),
            }
        }
    }

    /// Connects an RTVI call and completes the handshake.
    pub async fn rtvi_call(&self) -> Client {
        let url = format!("ws://{}/rtvi", self.addrs.rtvi.unwrap());
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
            match next_from_bot(&mut ws).await {
                Some(FromBot::Rtvi(m)) if m["type"] == "bot-ready" => return ws,
                Some(FromBot::Rtvi(m)) if m["type"] == "error" => panic!("call failed: {m}"),
                Some(_) => {}
                None => panic!("the call ended during the handshake"),
            }
        }
    }

    /// A typed turn on an RTVI call: waits until the bot answered it.
    pub async fn say(&self, ws: &mut Client, text: &str) {
        ws.send(rtvi_message(
            "send-text",
            "text-1",
            &json!({"content": text, "options": {"run_immediately": true}}),
        ))
        .await
        .unwrap();
        loop {
            match next_from_bot(ws).await {
                Some(FromBot::Rtvi(m)) if m["type"] == "bot-llm-text" => return,
                Some(FromBot::Rtvi(m)) if m["type"] == "error" => panic!("call failed: {m}"),
                Some(_) => {}
                None => panic!("the call ended before the bot answered"),
            }
        }
    }

    /// `POST /admin/reload` on the health listener: (status line, body).
    pub async fn post_reload(&self, headers: &str) -> (String, String) {
        self.http(&format!(
            "POST /admin/reload HTTP/1.1\r\nHost: localhost\r\n{headers}Connection: close\r\n\r\n"
        ))
        .await
    }

    /// One HTTP exchange with the health listener: (status line, body).
    pub async fn http(&self, request: &str) -> (String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(self.health))
            .await
            .expect("connect timed out")
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        tokio::time::timeout(WAIT, stream.read_to_string(&mut out))
            .await
            .expect("no answer")
            .unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
        (
            head.lines().next().unwrap_or_default().to_owned(),
            body.to_owned(),
        )
    }

    /// Ends the server like a shutdown signal and checks the drain was clean.
    pub async fn finish(mut self) {
        let (trigger, mut signals) = brisvox_server::ShutdownSignals::manual();
        assert!(trigger.trigger(brisvox_server::ShutdownSignal::Requested));
        let report = tokio::time::timeout(WAIT, self.server.take().unwrap().run(&mut signals))
            .await
            .unwrap();
        assert_eq!(
            report.outcome(),
            brisvox_server::DrainOutcome::Drained,
            "{report:?}"
        );
    }
}

/// Polls `condition` every 20 ms for up to `secs` seconds; panics with `what` if it never holds.
pub async fn eventually(what: &str, secs: u64, mut condition: impl FnMut() -> bool) {
    let clock = SystemClock;
    let deadline = clock.now() + Duration::from_secs(secs);
    while !condition() {
        assert!(clock.now() < deadline, "timed out waiting for: {what}");
        clock.sleep(Duration::from_millis(20)).await;
    }
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// RTVI wire helpers (the Pipecat JS client's frames, encoded by hand)

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

/// `Frame{ message: MessageFrame{ data: json } }`.
pub fn rtvi_message(kind: &str, id: &str, data: &Value) -> Message {
    let json = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data}).to_string();
    let mut inner = Vec::new();
    pb_bytes(1, json.as_bytes(), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(4, &inner, &mut frame);
    Message::binary(frame)
}

/// What an RTVI client gets.
#[derive(Debug)]
pub enum FromBot {
    Rtvi(Value),
    /// Bot audio: its byte count.
    Audio(usize),
}

/// The next RTVI message or audio frame; `None` once the connection ended.
pub async fn next_from_bot(ws: &mut Client) -> Option<FromBot> {
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
