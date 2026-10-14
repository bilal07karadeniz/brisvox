//! The server binary's agent link hook, end to end on real sockets: a websocket call
//! of the engine runner over mock providers, linked to the reference agent process. The agent
//! takes the call (its greeting is spoken), then ends it with a farewell: the runner stops the
//! call and the link says `session.end`.
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent_link::AgentLinkConfig;
use brisvox_agent_link::client::AgentServer;
use brisvox_agent_link::protocol::{AgentMessage, ServerBody, SessionAccept};
use brisvox_core::{SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::{
    CallProviders, EngineRunner, HostSettings, ManagerConfig, ProviderFactory, RunnerConfig,
    Server, ServerSettings, ShutdownPolicy, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerMessage, Start};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(10);

const AGENT: &str = r#"
[agent]
name = "linked-agent"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "AGENT_LINK_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "AGENT_LINK_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "AGENT_LINK_TEST_TTS_KEY"
voice = "voice-123"
"#;

fn agent_file() -> AgentConfig {
    let keys: BTreeMap<String, String> = [
        ("AGENT_LINK_TEST_STT_KEY", "stt-key"),
        ("AGENT_LINK_TEST_LLM_KEY", "llm-key"),
        ("AGENT_LINK_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let dir: PathBuf =
        std::env::temp_dir().join(format!("brisvox-agent-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.toml");
    std::fs::write(&path, AGENT).unwrap();
    let agent = config::load_file(&path, &keys).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    agent
}

struct Mocks {
    clock: SharedClock,
}

impl ProviderFactory for Mocks {
    type Stt = MockStt;
    type Llm = MockLlm;
    type Tts = MockTts;

    fn providers(&self) -> CallProviders<MockStt, MockLlm, MockTts> {
        let llm = MockLlm::new(self.clock.clone());
        llm.set_fallback(LlmScript::text(["Hello."]));
        CallProviders {
            stt: Arc::new(MockStt::new(self.clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(self.clock.clone())),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_greets_and_ends_a_hosted_call() {
    let clock = SystemClock::shared();
    let agent = AgentServer::bind("127.0.0.1:0".parse().unwrap(), clock.clone())
        .await
        .unwrap();
    let link = Arc::new(AgentLinkConfig::new(agent.url().unwrap()));
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "agent-link-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let runner = Arc::new(
        EngineRunner::new(
            agent_file(),
            Mocks {
                clock: clock.clone(),
            },
            Offload::Pool(pool.handle()),
            Vad::Off,
            RunnerConfig::default(),
        )
        .unwrap()
        .with_agent_link(link),
    );
    let host = HostSettings {
        websocket: Some(([127, 0, 0, 1], 0).into()),
        ..HostSettings::none()
    };
    let settings = ServerSettings {
        manager: ManagerConfig::new(2).unwrap(),
        health_addr: None,
        shutdown: ShutdownPolicy {
            drain_timeout: Duration::from_secs(5),
            cancel_grace: Duration::from_secs(5),
            abort_wait: Duration::from_secs(1),
        },
        ..ServerSettings::default()
    };
    let server = Server::start_hosting(settings, &host, clock.clone(), &runner)
        .await
        .unwrap();
    let url = format!("ws://{}/call", server.listener_addrs().websocket.unwrap());

    // The caller connects; the runner links the call to the agent, which greets.
    let (mut caller, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
    caller.send(Message::text(start.to_json())).await.unwrap();
    // The caller's client plays everything at once: it echoes every mark (the end of a
    // response's playout) until the call ends.
    let caller = tokio::spawn(async move {
        loop {
            match caller.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ServerMessage::Mark { id }) = ServerMessage::parse(text.as_str()) {
                        let echo = ClientMessage::Mark { id }.to_json();
                        if caller.send(Message::text(echo)).await.is_err() {
                            return;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            }
        }
    });
    let mut session = agent.accept().await.unwrap();
    let started = session
        .accept_session(SessionAccept {
            version: 1,
            greeting: Some("Welcome!".to_owned()),
            ..SessionAccept::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(&started.body, ServerBody::SessionStart { agent, .. } if agent == "linked-agent"),
        "{started:?}"
    );
    session
        .recv_until(|b| matches!(b, ServerBody::TurnBotEnded { .. }))
        .await
        .unwrap();
    assert_eq!(server.manager().stats().active, 1);

    // The agent ends the call with a farewell: the session ends, the link says why.
    session
        .send(&AgentMessage::End {
            id: Some("bye".to_owned()),
            re: None,
            say: Some("Goodbye!".to_owned()),
            reason: None,
        })
        .await
        .unwrap();
    let result = session
        .recv_until(|b| matches!(b, ServerBody::CommandResult { .. }))
        .await
        .unwrap();
    assert!(
        matches!(&result.body, ServerBody::CommandResult { id, ok: true, .. } if id == "bye"),
        "{result:?}"
    );
    let end = session
        .recv_until(|b| matches!(b, ServerBody::SessionEnd { .. }))
        .await
        .unwrap();
    assert_eq!(
        end.body,
        ServerBody::SessionEnd {
            reason: "ended_by_agent".to_owned()
        }
    );
    let manager = server.manager().clone();
    let ended = async {
        while manager.stats().active != 0 {
            clock.sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WAIT, ended).await.unwrap();
    // The caller's connection ends too.
    tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
}
