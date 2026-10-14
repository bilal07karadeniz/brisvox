//! The resilience layer in the server: the agent file's secondaries become the
//! providers calls fail over to, every call's providers are wrapped, and a real hosted call
//! (websocket, engine runner) keeps its idle TTS session fresh.
#![cfg(all(
    feature = "provider-deepgram",
    feature = "provider-assemblyai",
    feature = "provider-openai",
    feature = "provider-anthropic",
    feature = "provider-cartesia",
    feature = "provider-elevenlabs"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SimClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::resilience::{Resilience, ResilienceConfig};
use brisvox_providers::{
    ContextId, Error as ProviderError, LlmProvider, SttProvider, TtsConfig, TtsEvent, TtsInput,
    TtsProvider, TtsSession,
};
use brisvox_server::config;
use brisvox_server::{
    CallProviders, ConfiguredProviders, EngineRunner, HostSettings, ManagerConfig, ProviderFactory,
    ResilientProviders, RunnerConfig, Server, ServerSettings, ShutdownPolicy, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerMessage, Start};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

/// Primaries only.
const AGENT: &str = r#"
[agent]
name = "resilience-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "RES_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "RES_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "RES_TEST_TTS_KEY"
voice = "voice-123"
"#;

/// A secondary in every stage.
const AGENT_WITH_SECONDARIES: &str = r#"
[agent]
name = "resilience-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "RES_TEST_STT_KEY"

[stt.secondary]
provider = "assemblyai"
api_key_env = "RES_TEST_STT2_KEY"

[llm]
provider = "openai"
api_key_env = "RES_TEST_LLM_KEY"

[llm.secondary]
provider = "anthropic"
api_key_env = "RES_TEST_LLM2_KEY"

[tts]
provider = "cartesia"
api_key_env = "RES_TEST_TTS_KEY"
voice = "voice-123"

[tts.secondary]
provider = "elevenlabs"
api_key_env = "RES_TEST_TTS2_KEY"
voice = "voice-b"
"#;

fn agent(text: &str) -> config::AgentConfig {
    let keys: BTreeMap<String, String> = [
        ("RES_TEST_STT_KEY", "stt-key"),
        ("RES_TEST_STT2_KEY", "stt2-key"),
        ("RES_TEST_LLM_KEY", "llm-key"),
        ("RES_TEST_LLM2_KEY", "llm2-key"),
        ("RES_TEST_TTS_KEY", "tts-key"),
        ("RES_TEST_TTS2_KEY", "tts2-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    config::load_str(text, "agent.toml", &keys).unwrap()
}

/// A TTS that documents an idle limit (as Cartesia's 5 minutes, here a fraction of a second so
/// a real-time test can wait for it) over the simulator's mock.
#[derive(Debug, Clone)]
struct Limited {
    mock: MockTts,
    limit: Duration,
}

impl TtsProvider for Limited {
    fn name(&self) -> &'static str {
        "cartesia"
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        self.mock.connect(config).await
    }

    fn idle_limit(&self) -> Option<Duration> {
        Some(self.limit)
    }
}

/// The process's providers behind the resilience layer, over mocks.
fn resilient(clock: &SharedClock, tts: Limited) -> ResilientProviders<MockStt, MockLlm, Limited> {
    let llm = MockLlm::new(clock.clone());
    llm.set_fallback(LlmScript::text(["Hello."]));
    ResilientProviders::new(
        Resilience::new(ResilienceConfig::DEFAULT, clock.clone()),
        CallProviders {
            stt: Arc::new(MockStt::new(clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(tts),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_call_refreshes_its_idle_tts_session_before_the_provider_closes_it() {
    let clock = SystemClock::shared();
    let mock = MockTts::new(clock.clone());
    // 400 ms limit: the layer replaces an idle session at 80 % of it, 320 ms.
    let limited = Limited {
        mock: mock.clone(),
        limit: Duration::from_millis(400),
    };
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "resilience-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let runner = Arc::new(
        EngineRunner::new(
            agent(AGENT),
            resilient(&clock, limited),
            Offload::Pool(pool.handle()),
            Vad::Off,
            RunnerConfig::default(),
        )
        .unwrap(),
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

    let before_call = clock.now();
    let (mut caller, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
    caller.send(Message::text(start.to_json())).await.unwrap();
    let started = tokio::time::timeout(WAIT, caller.next()).await.unwrap();
    let Some(Ok(Message::Text(text))) = started else {
        panic!("expected the started message, got {started:?}");
    };
    assert!(matches!(
        ServerMessage::parse(text.as_str()),
        Ok(ServerMessage::Started { .. })
    ));

    // The engine connects the TTS session when the call starts.
    let until = |what: &'static str, done: Box<dyn Fn() -> bool + Send>| {
        let clock = clock.clone();
        async move {
            let wait = async {
                while !done() {
                    clock.sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(WAIT, wait)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        }
    };
    let m = mock.clone();
    until(
        "the call's TTS connect",
        Box::new(move || m.connects() >= 1),
    )
    .await;
    assert_eq!(mock.connects(), 1);

    // Nothing is said: the idle session is replaced by a fresh one, in the background, before
    // the provider's limit (the call is still up; nobody heard anything).
    let m = mock.clone();
    until("the idle refresh", Box::new(move || m.connects() >= 2)).await;
    let elapsed = clock.now() - before_call;
    assert!(
        elapsed >= Duration::from_millis(300),
        "refreshed after {elapsed:?}, before 80 % of the 400 ms limit"
    );
    assert_eq!(mock.connects(), 2, "one refresh per idle period");
    assert_eq!(server.manager().stats().active, 1, "the call is still up");

    // The caller hangs up: the session ends and no refresh is left running.
    caller.send(Message::Close(None)).await.unwrap();
    let manager = server.manager().clone();
    until(
        "the call to end",
        Box::new(move || manager.stats().active == 0),
    )
    .await;
    let connects = mock.connects();
    clock.sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.connects(), connects, "no refresh after the call ended");
}

/// A TTS that fails every connect with a provider error (an outage) and counts them.
#[derive(Debug, Default)]
struct Down {
    connects: Mutex<usize>,
}

impl TtsProvider for Down {
    fn name(&self) -> &'static str {
        "down"
    }

    async fn connect(&self, _config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        *self.connects.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Err(ProviderError::Rejected {
            provider: "down",
            status: 503,
        })
    }
}

/// A TTS that records the voice of every connect, over the mock.
#[derive(Debug, Clone)]
struct Recording {
    mock: MockTts,
    voices: Arc<Mutex<Vec<String>>>,
}

impl TtsProvider for Recording {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        self.voices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(config.voice.to_string());
        self.mock.connect(config).await
    }
}

/// An enum so that one factory type has a failing primary and a recording secondary.
#[derive(Debug, Clone)]
enum Either {
    Down(Arc<Down>),
    Recording(Recording),
}

impl TtsProvider for Either {
    fn name(&self) -> &'static str {
        match self {
            Self::Down(d) => d.name(),
            Self::Recording(r) => r.name(),
        }
    }

    async fn connect(&self, config: TtsConfig) -> brisvox_providers::Result<TtsSession> {
        match self {
            Self::Down(d) => d.connect(config).await,
            Self::Recording(r) => r.connect(config).await,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_fails_over_to_the_secondary_tts_which_speaks_its_own_voice() {
    let clock = SimClock::shared();
    let down = Arc::new(Down::default());
    let voices = Arc::new(Mutex::new(Vec::new()));
    let recording = Recording {
        mock: MockTts::new(clock.clone()),
        voices: Arc::clone(&voices),
    };
    let llm = MockLlm::new(clock.clone());
    let providers = ResilientProviders::new(
        Resilience::new(ResilienceConfig::DEFAULT, clock.clone()),
        CallProviders {
            stt: Arc::new(MockStt::new(clock.clone())),
            llm: Arc::new(llm),
            tts: Arc::new(Either::Down(Arc::clone(&down))),
        },
    )
    .with_tts_secondary(Arc::new(Either::Recording(recording)), "voice-b".into());
    assert_eq!(providers.has_secondaries(), (false, false, true));

    let CallProviders { tts, .. } = providers.providers();
    let config = TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: "voice-123".into(),
    };
    let mut session = tts.connect(config.clone()).await.unwrap();
    // The primary was tried (once), the secondary took over with its own voice, not the
    // engine's.
    assert_eq!(*down.connects.lock().unwrap(), 1);
    assert_eq!(*voices.lock().unwrap(), ["voice-b"]);
    // The session speaks.
    let context = ContextId(1);
    session
        .input
        .send(TtsInput::Text {
            context,
            text: "hi".into(),
        })
        .await
        .unwrap();
    session
        .input
        .send(TtsInput::Flush { context })
        .await
        .unwrap();
    let mut audio = false;
    while let Some(event) = session.events.recv().await {
        match event {
            TtsEvent::Audio { .. } => audio = true,
            TtsEvent::Flushed { .. } => break,
            _ => {}
        }
    }
    assert!(audio);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_files_secondaries_are_built_and_wrapped_into_every_calls_providers() {
    let clock = SystemClock::shared();
    let shutdown = CancellationToken::new();
    let providers =
        ConfiguredProviders::from_agent(&agent(AGENT_WITH_SECONDARIES), &clock, &shutdown).unwrap();
    assert_eq!(providers.resilient().has_secondaries(), (true, true, true));
    // No secondary where the file names none.
    let plain = ConfiguredProviders::from_agent(&agent(AGENT), &clock, &shutdown).unwrap();
    assert_eq!(plain.resilient().has_secondaries(), (false, false, false));

    // Every call gets wrappers of its own (its own retry budget and counters) over the
    // process's legs.
    let a = providers.providers();
    let b = providers.providers();
    assert!(!Arc::ptr_eq(&a.stt, &b.stt));
    assert!(!Arc::ptr_eq(&a.tts, &b.tts));
    assert_eq!(
        (a.stt.name(), a.llm.name(), a.tts.name()),
        ("deepgram", "openai", "cartesia")
    );
    shutdown.cancel();
}
