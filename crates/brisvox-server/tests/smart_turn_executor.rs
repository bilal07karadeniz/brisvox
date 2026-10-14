//! The Smart Turn executor through the [`EngineRunner`] the binary uses: a call built
//! with `EngineRunner::with_smart_turn_offload` warms the model AND runs its analysis on the
//! executor's `brisvox-turn-*` threads, never on the VAD's pool. Real sockets, the production
//! Silero VAD, a probe model that records the thread of every `warm` and `predict`, and the first
//! words of `fixtures/en_two_utterances.wav` as the caller's speech (paced in real time).
#![cfg(all(
    feature = "silero-tract",
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
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock};
use brisvox_engine::Offload;
use brisvox_engine::offload::start_smart_turn_executor;
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::{
    CallProviders, DrainOutcome, EngineRunner, HostSettings, ManagerConfig, ProviderFactory,
    RunnerConfig, Server, ServerSettings, ShutdownPolicy, ShutdownSignal, ShutdownSignals, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::websocket::{ClientMessage, Encoding, ServerMessage, Start};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(20);

const AGENT: &str = r#"
[agent]
name = "smart-turn-executor-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "TURN_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "TURN_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "TURN_TEST_TTS_KEY"
voice = "voice-123"
"#;

fn agent() -> AgentConfig {
    let keys: BTreeMap<String, String> = [
        ("TURN_TEST_STT_KEY", "stt-key"),
        ("TURN_TEST_LLM_KEY", "llm-key"),
        ("TURN_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    config::load_str(AGENT, "agent.toml", &keys).unwrap()
}

/// Fresh mock providers for every call; the STT hears a scripted sentence.
struct Mocks {
    clock: SharedClock,
}

impl ProviderFactory for Mocks {
    type Stt = MockStt;
    type Llm = MockLlm;
    type Tts = MockTts;

    fn providers(&self) -> CallProviders<MockStt, MockLlm, MockTts> {
        let stt = MockStt::new(self.clock.clone());
        stt.push(SttUtterance::words("hello there how are you"));
        let llm = MockLlm::new(self.clock.clone());
        llm.set_fallback(LlmScript::text(["Fine, thanks."]));
        CallProviders {
            stt: Arc::new(stt),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(self.clock.clone())),
        }
    }
}

/// A Smart Turn model recording the thread of every warm-up and every analysis.
#[derive(Default)]
struct ProbeTurn {
    warms: Mutex<Vec<Option<String>>>,
    predicts: Mutex<Vec<Option<String>>>,
}

fn current_thread() -> Option<String> {
    std::thread::current().name().map(str::to_owned)
}

impl ProbeTurn {
    fn warms(&self) -> Vec<Option<String>> {
        self.warms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn predicts(&self) -> Vec<Option<String>> {
        self.predicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl TurnModel for ProbeTurn {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        self.predicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(current_thread());
        Ok(0.9)
    }

    fn warm(&self, _sessions: usize) -> brisvox_inference::Result<()> {
        self.warms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(current_thread());
        Ok(())
    }
}

/// The caller's audio: 1.5 s of speech (it starts at 0.5 s of the file), as 20 ms frames of
/// 16 kHz PCM16.
fn speech_frames() -> Vec<Vec<u8>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/en_two_utterances.wav");
    let bytes = std::fs::read(path).unwrap();
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("a data chunk")
        + 8;
    bytes[data + 2 * 8_000..data + 2 * (8_000 + 24_000)]
        .as_chunks::<640>()
        .0
        .iter()
        .map(|frame| frame.to_vec())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_call_warms_and_analyses_on_the_smart_turn_executor() {
    let clock = SystemClock::shared();
    let vad_pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 256,
            name: "turn-vad-pool".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let executor = start_smart_turn_executor(1, clock.clone())
        .unwrap()
        .unwrap();
    let vad = tokio::task::spawn_blocking(Vad::production)
        .await
        .unwrap()
        .unwrap();
    let probe = Arc::new(ProbeTurn::default());
    let runner = Arc::new(
        EngineRunner::new(
            agent(),
            Mocks {
                clock: clock.clone(),
            },
            Offload::Pool(vad_pool.handle()),
            vad,
            RunnerConfig::default(),
        )
        .unwrap()
        .with_smart_turn(Arc::clone(&probe) as Arc<dyn TurnModel>)
        .with_smart_turn_offload(Offload::Pool(executor.handle())),
    );
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
    let host = HostSettings {
        websocket: Some(([127, 0, 0, 1], 0).into()),
        ..HostSettings::none()
    };
    let server = Server::start_hosting(settings, &host, clock.clone(), &runner)
        .await
        .unwrap();
    let addr: SocketAddr = server.listener_addrs().websocket.unwrap();

    let (ws, _) = tokio::time::timeout(
        WAIT,
        tokio_tungstenite::connect_async(format!("ws://{addr}/call")),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(
        ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)).to_json(),
    ))
    .await
    .unwrap();
    // The bot's side (messages and audio) is read so the socket never backs up.
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = rx.next().await {
            if let Message::Text(text) = &message
                && matches!(
                    ServerMessage::parse(text.as_str()),
                    Ok(ServerMessage::Started { .. })
                )
                && let Some(started) = started.take()
            {
                let _ = started.send(());
            }
        }
    });
    tokio::time::timeout(WAIT, started_rx)
        .await
        .expect("no `started`")
        .unwrap();

    // The caller says a few words, then is silent, paced like a real call.
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    for frame in speech_frames() {
        ticker.tick().await;
        tx.send(Message::binary(frame)).await.unwrap();
    }
    let wait = async {
        while probe.predicts().is_empty() {
            ticker.tick().await;
            tx.send(Message::binary(vec![0_u8; 640])).await.unwrap();
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("no Smart Turn analysis ran");

    // The warm-up and the analysis ran on the executor's thread, none on the VAD's pool.
    let on_executor = |threads: Vec<Option<String>>| {
        !threads.is_empty()
            && threads.iter().all(|name| {
                name.as_deref()
                    .is_some_and(|n| n.starts_with(brisvox_engine::offload::SMART_TURN_THREAD_NAME))
            })
    };
    assert!(on_executor(probe.warms()), "warm-ups: {:?}", probe.warms());
    assert!(
        on_executor(probe.predicts()),
        "analyses: {:?}",
        probe.predicts()
    );
    assert!(
        executor.metrics().completed >= 2,
        "the warm-up and the analysis went through the executor: {:?}",
        executor.metrics()
    );
    assert!(
        vad_pool.metrics().completed > 0,
        "the VAD windows ran on their own pool"
    );

    // End the call and drain.
    drop(tx);
    reader.abort();
    let (trigger, mut signals) = ShutdownSignals::manual();
    assert!(trigger.trigger(ShutdownSignal::Requested));
    let report = tokio::time::timeout(WAIT, server.run(&mut signals))
        .await
        .unwrap();
    assert_eq!(report.outcome(), DrainOutcome::Drained, "{report:?}");
    drop(runner);
    clock.sleep(Duration::from_millis(50)).await;
    tokio::task::spawn_blocking(move || {
        vad_pool.shutdown();
        executor.shutdown();
    })
    .await
    .unwrap();
}
