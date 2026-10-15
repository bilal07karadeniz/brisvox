//! Observability end to end: a mock call through the engine runner, the health
//! listener's `GET /metrics` on a real socket, and the turn's trace in an in-memory exporter. The
//! call runs behind the resilience layer with a dead primary STT, so the failover shows up in
//! the breaker and resilience families.
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
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_engine::Offload;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::resilience::{Resilience, ResilienceConfig};
use brisvox_providers::{Error as ProviderError, SttConfig, SttProvider, SttSession};
use brisvox_server::config::{self, AgentConfig};
use brisvox_server::hosting::Listener;
use brisvox_server::observability::exposition::{Family, parse};
use brisvox_server::observability::{MemoryExporter, Observability};
use brisvox_server::{
    CallProviders, EngineRunner, HostSettings, ManagerConfig, ResilientProviders, RunnerConfig,
    Server, ServerSettings, ShutdownPolicy, Vad,
};
use brisvox_sim::mock_llm::{LlmScript, MockLlm};
use brisvox_sim::mock_stt::MockStt;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::rtvi::RtviConfig;
use brisvox_transport::rtvi::proto::{Frame, frame};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(10);

const AGENT: &str = r#"
[agent]
name = "observability-test"
language = "en"
system_prompt = "You are a test agent."

[stt]
provider = "deepgram"
api_key_env = "OBS_TEST_STT_KEY"

[llm]
provider = "openai"
api_key_env = "OBS_TEST_LLM_KEY"

[tts]
provider = "cartesia"
api_key_env = "OBS_TEST_TTS_KEY"
voice = "voice-123"
"#;

fn agent() -> AgentConfig {
    let keys: BTreeMap<String, String> = [
        ("OBS_TEST_STT_KEY", "stt-key"),
        ("OBS_TEST_LLM_KEY", "llm-key"),
        ("OBS_TEST_TTS_KEY", "tts-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    config::load_str(AGENT, "agent.toml", &keys).unwrap()
}

/// A mock STT that can be dead: its `connect` is refused like a provider outage.
struct Stt {
    mock: MockStt,
    dead: bool,
}

impl SttProvider for Stt {
    fn name(&self) -> &'static str {
        if self.dead { "dead-stt" } else { "mock-stt" }
    }

    async fn connect(&self, config: SttConfig) -> brisvox_providers::Result<SttSession> {
        if self.dead {
            Err(ProviderError::Rejected {
                provider: "dead-stt",
                status: 503,
            })
        } else {
            self.mock.connect(config).await
        }
    }
}

// --- the RTVI client side (hand-encoded, as in tests/hosting.rs) -------------------------------

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

/// `GET path` on the health listener; the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).await.unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out.split_once("\r\n\r\n").unwrap().1.to_owned())
}

fn value(families: &[Family], family: &str, sample: &str, labels: &[(&str, &str)]) -> f64 {
    families
        .iter()
        .find(|f| f.name == family)
        .unwrap_or_else(|| panic!("no family {family}"))
        .find(sample, labels)
        .unwrap_or_else(|| panic!("no sample {sample}{labels:?}"))
        .value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mock_call_shows_up_in_metrics_and_traces() {
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 64,
            name: "obs-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    // The primary STT is down: every call fails over to the secondary.
    let llm = MockLlm::new(clock.clone());
    llm.set_fallback(LlmScript::text(["Hello from the agent."]));
    let providers = ResilientProviders::new(
        Resilience::new(ResilienceConfig::DEFAULT, clock.clone()),
        CallProviders {
            stt: Arc::new(Stt {
                mock: MockStt::new(clock.clone()),
                dead: true,
            }),
            llm: Arc::new(llm),
            tts: Arc::new(MockTts::new(clock.clone())),
        },
    )
    .with_stt_secondary(Arc::new(Stt {
        mock: MockStt::new(clock.clone()),
        dead: false,
    }));
    let exporter = MemoryExporter::new(16);
    let obs = Arc::new(Observability::new().with_exporter(exporter.clone()));
    assert!(obs.set_resilience_source(providers.resilience_handle()));
    let runner = Arc::new(
        EngineRunner::new(
            agent(),
            providers,
            Offload::Pool(pool.handle()),
            Vad::Off,
            RunnerConfig::default(),
        )
        .unwrap()
        .with_observability(Arc::clone(&obs)),
    );
    let settings = ServerSettings {
        manager: ManagerConfig::new(4).unwrap(),
        health_addr: Some(([127, 0, 0, 1], 0).into()),
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
    let server = Server::start_hosting(settings, &host, clock.clone(), &runner)
        .await
        .unwrap();
    assert!(server.manager().attach_observability(Arc::clone(&obs)));
    let health = server.health_addr().unwrap();

    // Before any call: the families exist at zero.
    let (status, body) = get(health, "/metrics").await;
    assert_eq!(status, 200, "{body}");
    let before = parse(&body).unwrap_or_else(|e| {
        panic!(
            "{e}
{body}"
        )
    });
    assert_eq!(
        value(
            &before,
            "brisvox_sessions_started_total",
            "brisvox_sessions_started_total",
            &[]
        ),
        0.0
    );

    // One call with one typed turn.
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
    ws.send(rtvi_message(
        "send-text",
        "text-1",
        &json!({"content": "hello there", "options": {"run_immediately": true}}),
    ))
    .await
    .unwrap();
    // The bot answers in text and in audio; wait for both so the TTS stages have their marks.
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

    // The scrape after the call.
    let (status, body) = get(health, "/metrics").await;
    assert_eq!(status, 200);
    let after = parse(&body).unwrap_or_else(|e| {
        panic!(
            "{e}
{body}"
        )
    });
    let turns = value(
        &after,
        "brisvox_turns_total",
        "brisvox_turns_total",
        &[("outcome", "completed")],
    ) + value(
        &after,
        "brisvox_turns_total",
        "brisvox_turns_total",
        &[("outcome", "interrupted")],
    );
    assert!(
        turns >= 1.0,
        "no turn was observed:
{body}"
    );
    for stage in ["llm_first_token", "llm_round", "tts_first_byte"] {
        let n = value(
            &after,
            "brisvox_stage_latency_seconds",
            "brisvox_stage_latency_seconds_count",
            &[("stage", stage)],
        );
        assert!(
            n >= 1.0,
            "{stage} was not observed:
{body}"
        );
    }
    assert_eq!(
        value(
            &after,
            "brisvox_sessions_started_total",
            "brisvox_sessions_started_total",
            &[]
        ),
        1.0
    );
    assert_eq!(
        value(
            &after,
            "brisvox_sessions_ended_total",
            "brisvox_sessions_ended_total",
            &[("reason", "completed")]
        ),
        1.0
    );
    assert_eq!(
        value(
            &after,
            "brisvox_calls_ended_total",
            "brisvox_calls_ended_total",
            &[]
        ),
        1.0
    );
    // The dead primary failed and was failed over from; the secondary served.
    let b = |provider: &str, event: &str| {
        value(
            &after,
            "brisvox_breaker_events_total",
            "brisvox_breaker_events_total",
            &[("stage", "stt"), ("provider", provider), ("event", event)],
        )
    };
    assert!(b("dead-stt", "failures") >= 1.0, "{body}");
    assert!(b("mock-stt", "admitted") >= 1.0, "{body}");
    assert!(
        value(
            &after,
            "brisvox_resilience_events_total",
            "brisvox_resilience_events_total",
            &[("stage", "stt"), ("event", "failovers")]
        ) >= 1.0,
        "{body}"
    );
    assert_eq!(
        value(
            &after,
            "brisvox_breaker_state",
            "brisvox_breaker_state",
            &[("stage", "stt"), ("role", "primary")]
        ),
        0.0,
        "one failure does not open the breaker"
    );
    assert!(
        value(
            &after,
            "brisvox_retry_budget_total",
            "brisvox_retry_budget_total",
            &[("outcome", "granted")]
        ) >= 1.0,
        "{body}"
    );

    // The call's usage: the agent's provider and model label every series.
    let models = agent().provider_models();
    let (llm, tts) = (models.llm.unwrap(), models.tts.unwrap());
    let stt = models.stt.unwrap();
    for kind in ["input", "output"] {
        let tokens = value(
            &after,
            "brisvox_llm_tokens_total",
            "brisvox_llm_tokens_total",
            &[
                ("provider", llm.provider),
                ("model", &llm.model),
                ("kind", kind),
            ],
        );
        assert!(tokens > 0.0, "no {kind} tokens:\n{body}");
    }
    assert!(
        value(
            &after,
            "brisvox_tts_characters_total",
            "brisvox_tts_characters_total",
            &[("provider", tts.provider), ("model", &tts.model)],
        ) >= "Hello from the agent.".len() as f64,
        "{body}"
    );
    value(
        &after,
        "brisvox_stt_audio_seconds_total",
        "brisvox_stt_audio_seconds_total",
        &[("provider", stt.provider), ("model", &stt.model)],
    );
    assert_eq!(
        value(
            &after,
            "brisvox_call_duration_seconds",
            "brisvox_call_duration_seconds_count",
            &[]
        ),
        1.0,
        "{body}"
    );

    // One trace per observed turn, with its stage spans under one root.
    let traces = exporter.take();
    assert_eq!(traces.len() as f64, turns, "one trace per turn");
    let trace = &traces[0];
    let root = trace.root().expect("a root span");
    assert_eq!(root.name, "turn");
    for stage in ["llm", "tts"] {
        let span = trace
            .span(stage)
            .unwrap_or_else(|| panic!("no {stage} span: {trace:?}"));
        assert_eq!(span.parent, Some(root.span_id));
        assert_eq!(span.trace_id, root.trace_id);
        assert!(span.start_unix_nanos >= root.start_unix_nanos);
        assert!(span.end_unix_nanos <= root.end_unix_nanos);
    }
}
