//! The Smart Turn executor: with `SessionBuilder::with_smart_turn_offload` the model's
//! warm-up (and with it every analysis, which uses the same offload) runs on the executor's own
//! threads and sizes itself on them, while the VAD's pool is never asked for it. Real time, real
//! threads, a probe model that records where it ran.
#![allow(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods // real time
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock, TimelineRecorder};
use brisvox_engine::offload::start_smart_turn_executor;
use brisvox_engine::sim::open_loopback;
use brisvox_engine::{EngineConfig, Offload, SessionBuilder, run_call};
use brisvox_inference::smart_turn::TurnModel;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_sim::session::SimSession;

/// A Smart Turn model recording the sessions it was asked for and the thread that was asked.
struct ProbeTurn {
    asked: Mutex<Vec<(usize, Option<String>)>>,
}

impl TurnModel for ProbeTurn {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(0.9)
    }

    fn warm(&self, sessions: usize) -> brisvox_inference::Result<()> {
        self.asked
            .lock()
            .unwrap()
            .push((sessions, std::thread::current().name().map(str::to_owned)));
        Ok(())
    }
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_smart_turn_model_warms_on_the_executor_and_not_on_the_vad_pool() {
    let clock: SharedClock = SystemClock::shared();
    let vad_pool = InferencePool::new(
        &PoolConfig {
            threads: 3,
            queue_capacity: 64,
            name: "vad-pool-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let executor = start_smart_turn_executor(1, clock.clone())
        .unwrap()
        .unwrap();
    let sim = SimSession::new(clock.clone());
    let turn = Arc::new(ProbeTurn {
        asked: Mutex::new(Vec::new()),
    });
    let call = open_loopback(&clock, 1_024).await.unwrap();
    let builder = SessionBuilder::new(
        EngineConfig::DEFAULT,
        clock.clone(),
        TimelineRecorder::new(),
        Offload::Pool(vad_pool.handle()),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_smart_turn(Arc::clone(&turn) as Arc<dyn TurnModel>)
    .with_smart_turn_offload(Offload::Pool(executor.handle()));
    let cancel = call.cancel.clone();
    let mut running = run_call(call.session, builder, &cancel).await.unwrap();
    let handles = running.handles().clone();
    eventually("the warm-up", || handles.driver.snapshot().warms == 1).await;

    // One executor thread: one session (the pool's three threads would have asked for three),
    // asked on the executor's thread.
    let asked = turn.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].0, 1, "sessions follow the executor's threads");
    assert!(
        asked[0]
            .1
            .as_deref()
            .is_some_and(|n| n.starts_with("brisvox-turn")),
        "{asked:?}"
    );
    assert_eq!(executor.metrics().completed, 1);
    assert_eq!(
        vad_pool.metrics().completed,
        0,
        "the VAD's pool ran nothing for Smart Turn"
    );

    running.stop().await.unwrap();
    drop(running);
    tokio::task::spawn_blocking(move || {
        vad_pool.shutdown();
        executor.shutdown();
    })
    .await
    .unwrap();
}

/// The golden cases of the real Smart Turn model through the executor's threads give the
/// bit-identical probabilities of the same analysis on the test thread (on every machine), and
/// on a machine running the reference's kernels also the reference's within the goldens'
/// rounding: where the analysis runs changes nothing it computes.
#[cfg(feature = "smart-turn-ort")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_cases_through_the_executor_are_bit_identical() {
    use brisvox_inference::smart_turn::SmartTurn;
    use brisvox_inference::smart_turn::ort_backend::SmartTurnOrt;
    use brisvox_sim::fixture::{Fixture, fixtures_dir};

    let clock: SharedClock = SystemClock::shared();
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../models")
        .join(brisvox_inference::smart_turn::MODEL_FILE);
    let model = SmartTurnOrt::load(&path)
        .unwrap_or_else(|e| panic!("{e}: run scripts/models/fetch_models.py"));
    model.warm(2).unwrap();
    let executor = start_smart_turn_executor(2, clock).unwrap().unwrap();
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("smart_turn_golden.json")).unwrap(),
    )
    .unwrap();
    let cases = golden["cases"].as_object().unwrap();
    assert_eq!(cases.len(), 12);
    let reference_kernels = brisvox_sim::platform::smart_turn_goldens_apply_here();
    for (key, case) in cases {
        let fixture = Fixture::load(&fixtures_dir(), case["fixture"].as_str().unwrap()).unwrap();
        let pcm: Vec<i16> = fixture
            .pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let (start, end) = (
            usize::try_from(case["start"].as_u64().unwrap()).unwrap(),
            usize::try_from(case["end"].as_u64().unwrap()).unwrap(),
        );
        let snapshot = pcm[start..end].to_vec();
        #[allow(clippy::cast_possible_truncation)]
        let expected = case["probability"].as_f64().unwrap() as f32;
        let direct = SmartTurn::analyze_snapshot_blocking(model.as_ref(), &snapshot).unwrap();
        let job_model = Arc::clone(&model);
        let done = executor
            .handle()
            .run(Duration::from_secs(30), move || {
                let thread = std::thread::current().name().map(str::to_owned);
                (
                    thread,
                    SmartTurn::analyze_snapshot_blocking(job_model.as_ref(), &snapshot),
                )
            })
            .await
            .unwrap();
        let (thread, via_executor) = done.value;
        assert!(
            thread
                .as_deref()
                .is_some_and(|n| n.starts_with("brisvox-turn")),
            "{key}: ran on {thread:?}"
        );
        let via_executor = via_executor.unwrap();
        assert_eq!(
            via_executor.to_bits(),
            direct.to_bits(),
            "{key}: executor {via_executor} vs test thread {direct}"
        );
        // The reference's goldens hold only where their ONNX Runtime kernels run; the
        // exactness above (executor == this thread, bit for bit) holds everywhere.
        if reference_kernels {
            assert!(
                (via_executor - expected).abs() < 1e-6,
                "{key}: {via_executor} vs reference {expected}"
            );
        }
    }
    tokio::task::spawn_blocking(move || executor.shutdown())
        .await
        .unwrap();
}
