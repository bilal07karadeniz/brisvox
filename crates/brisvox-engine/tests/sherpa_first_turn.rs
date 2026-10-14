//! The first turn of a fully local browser call: the real sherpa-onnx adapter against
//! a running `streaming_server.py`, through the production assembly (`run_call`: Silero VAD on
//! the inference pool, the STT stage, the turn driver), with a mock LLM and mock TTS. The
//! caller is paced in real time like a browser: a lead of low noise, one utterance cut from
//! `fixtures/en_two_utterances.wav`, then silence.
//!
//! The checker is the mock LLM: the first turn must reach it with ALL the words that were
//! spoken. Found with this test (2026-10-14): without the adapter's padding before `Done`
//! (`SherpaOnnxConfig::finalize_padding`) the server's last result lost the last word of a turn
//! cut by the VAD ("...IS ORTHODOX CHRIST", in some runs), and a very short first utterance came
//! back partial or empty. `SHERPA_PAD_MS=0` shows the difference.
//!
//! Live: runs only with `BRISVOX_LIVE_TESTS=1` and `SHERPA_ONNX_URL` (an English model).
#![cfg(feature = "silero-tract")]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{SharedClock, SystemClock, TimelineRecorder, TurnEvent};
use brisvox_engine::sim::{FixedTurnModel, LoopbackCall, open_loopback};
use brisvox_engine::{
    EngineConfig, InterruptionConfig, Offload, SessionBuilder, VadSetup, VadWorkerConfig, run_call,
};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_providers::sherpa_onnx::{SherpaOnnxConfig, SherpaOnnxStt};
use brisvox_providers::{LlmRequest, Role};
use brisvox_sim::mock_llm::LlmScript;
use brisvox_sim::session::SimSession;
use bytes::Bytes;
use tokio::time::Instant;

const FRAME_MS: u64 = 20;
const FRAME_BYTES: usize = 640;

fn fixture_pcm(from: usize, to: usize) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/en_two_utterances.wav");
    let wav = std::fs::read(path).expect("fixture");
    wav[44 + from * 2..44 + to * 2].to_vec()
}

/// Low-level noise (a browser's room tone), deterministic.
fn room_tone(ms: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(ms * 32);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..(16 * ms) {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let v = ((x >> 16) % 41) as i16 - 20;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// What the user said to the LLM in each of its requests.
fn user_texts(requests: &[LlmRequest]) -> Vec<String> {
    requests
        .iter()
        .filter_map(|r| {
            r.messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.content.to_string())
        })
        .collect()
}

fn scenario(parts: &[(&str, usize)]) -> Vec<u8> {
    let mut pcm = Vec::new();
    for (kind, ms) in parts {
        match *kind {
            "tone" => pcm.extend(room_tone(*ms)),
            "speech" => pcm.extend(fixture_pcm(130_720, 130_720 + 16 * ms)),
            other => panic!("unknown part {other}"),
        }
    }
    pcm
}

/// One call playing `pcm` in real time. Returns what the LLM was asked.
async fn call(url: &str, pcm: Vec<u8>) -> Vec<String> {
    let clock: SharedClock = SystemClock::shared();
    let sim = SimSession::new(clock.clone());
    sim.llm.push(LlmScript::text(["Sure thing."]));
    sim.llm.push(LlmScript::text(["Sure thing."]));
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 64,
            name: "sherpa-first-turn".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let vad_model = SileroTractModel::bundled().unwrap();
    let mut config = EngineConfig::DEFAULT;
    config.interruption = InterruptionConfig::OFF;
    let mut sherpa = SherpaOnnxConfig::new();
    sherpa.base_url = url.to_owned();
    if let Ok(ms) = std::env::var("SHERPA_PAD_MS") {
        sherpa.finalize_padding = Duration::from_millis(ms.parse().unwrap());
    }
    let stt = SherpaOnnxStt::new(sherpa, clock.clone());
    let LoopbackCall {
        session,
        client,
        cancel,
    } = open_loopback(&clock, 65_536).await.unwrap();
    let timeline = TimelineRecorder::new();
    let builder = SessionBuilder::new(
        config,
        clock.clone(),
        timeline.clone(),
        Offload::Pool(pool.handle()),
        Arc::new(stt),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_vad(VadSetup::new(
        Offload::Pool(pool.handle()),
        VadWorkerConfig::from_engine(&config),
        move || vad_model.stream(),
    ))
    .with_smart_turn(Arc::new(FixedTurnModel(0.9)));
    let mut running = run_call(session, builder, &cancel).await.unwrap();

    let mut client = client;
    let origin = Instant::now();
    for (n, chunk) in pcm.chunks(FRAME_BYTES).enumerate() {
        tokio::time::sleep_until(origin + Duration::from_millis(FRAME_MS * n as u64)).await;
        client
            .send_audio(Bytes::copy_from_slice(chunk), SampleRate::Hz16000)
            .await
            .unwrap();
    }
    // Let the turn finish: the VAD stop, the STT final, the LLM request.
    for _ in 0..100 {
        if !sim.llm.requests().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = user_texts(&sim.llm.requests());
    eprintln!("   stt stats: {:?}", running.handles().stt.snapshot());
    running.stop().await.unwrap();
    timeline.finish();
    for turn in timeline.history() {
        eprintln!(
            "   stt final delay: {:?}",
            turn.between(TurnEvent::UserSpeechEnd, TurnEvent::SttFinal)
        );
    }
    asked
}

fn live_url() -> Option<String> {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 and SHERPA_ONNX_URL to run");
        return None;
    }
    std::env::var("SHERPA_ONNX_URL").ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_turn_reaches_the_llm_with_all_its_words() {
    let Some(url) = live_url() else { return };
    // Sentences of 2.8 s and 3.5 s (the second is cut mid-word by the fixture's end), after
    // 0.5 s and 1.5 s of room tone, then 3 s of tone: the VAD ends the turn.
    for lead in [500_usize, 1_500] {
        for ms in [2_760_usize, 3_500] {
            let pcm = scenario(&[("tone", lead), ("speech", ms), ("tone", 3_000)]);
            let asked = call(&url, pcm).await;
            eprintln!("lead={lead} speech={ms}: LLM asked {asked:?}");
            assert_eq!(asked.len(), 1, "lead={lead} speech={ms}: {asked:?}");
            assert!(
                asked[0].ends_with("ORTHODOX CHRISTIAN"),
                "lead={lead} speech={ms}: the last word was lost: {:?}",
                asked[0]
            );
        }
    }
    // A short first utterance right after the call started still reaches the LLM.
    let asked = call(
        &url,
        scenario(&[("tone", 300), ("speech", 1_500), ("tone", 3_000)]),
    )
    .await;
    assert!(
        asked.first().is_some_and(|t| t.contains("MAJOR")),
        "short first utterance: {asked:?}"
    );
}
