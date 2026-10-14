//! The engine target with the real models: Silero VAD and Smart Turn v3.2 instead of the
//! energy VAD and the Smart Turn stub, over the real-speech fixtures.
//!
//! The run needs the `models` feature and the Smart Turn model file (`scripts/models/
//! fetch_models.py`: `BRISVOX_MODELS_DIR`, default `<workspace>/models`); without either the
//! test says so and passes, like the live tests do without keys.
#![cfg(feature = "models")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods // a test server on real instants
)]

use std::path::PathBuf;
use std::time::Duration;

use brisvox_bench::capacity::report::Verdict;
use brisvox_bench::capacity::{CapacityConfig, run_capacity};
use brisvox_bench::v2v::engine_target::{EngineServer, EngineTargetConfig};
use brisvox_bench::v2v::real_models::RealModels;
use brisvox_core::SystemClock;
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn models_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("BRISVOX_MODELS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"),
        PathBuf::from,
    );
    dir.join(brisvox_inference::smart_turn::MODEL_FILE)
        .is_file()
        .then_some(dir)
}

#[test]
fn a_missing_model_file_is_a_config_error_naming_the_file() {
    let dir = std::env::temp_dir().join(format!("brisvox-no-models-{}", std::process::id()));
    let error = RealModels::load(&dir, SystemClock::shared()).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains(brisvox_inference::smart_turn::MODEL_FILE),
        "{message}"
    );
    assert!(RealModels::available());
}

/// Two concurrent calls over the real English fixture: Silero hears the speech, the engine
/// ends each turn through the real Smart Turn model, and every turn is answered within the SLO
/// (no lost turn, no failed call).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_with_the_real_models_answer_every_turn() {
    let Some(dir) = models_dir() else {
        eprintln!("skipped: models/smart-turn-v3.2-cpu.onnx is missing (fetch_models.py)");
        return;
    };
    let models = RealModels::load(&dir, SystemClock::shared()).unwrap();
    let fixtures = fixtures_dir();
    let fixture = Fixture::load(&fixtures, "en_two_utterances").unwrap();
    let engine = EngineTargetConfig {
        max_calls: 8,
        pool_threads: 2,
        ..EngineTargetConfig::default()
    };
    let settings = engine.settings();
    let cancel = CancellationToken::new();
    let server = EngineServer::start_with(
        engine,
        ([127, 0, 0, 1], 0).into(),
        Some(models),
        SystemClock::shared(),
        &cancel,
    )
    .await
    .unwrap();
    server.set_default_script(&fixture);

    let mut config = CapacityConfig::new(server.url(), Duration::from_millis(settings.floor_ms));
    config.fixture = "en_two_utterances".to_owned();
    config.fixtures_dir = fixtures;
    config.steps = vec![2];
    config.rounds = 1;
    config.warmup_calls = 1;
    config.spread = 300 * MS;
    config.min_gap = 4_000 * MS;
    config.pause = 50 * MS;
    config.cooldown = 200 * MS;
    config.sample_every = 50 * MS;
    config.client.response_timeout = Duration::from_secs(6);
    // A test machine is loaded and the debug build slow: a generous allowance, and no verdict
    // from a late generator.
    config.slo.over_floor = 1_500 * MS;
    config.lateness_limit = Duration::from_secs(5);
    config.engine = Some(settings);

    let report = run_capacity(SystemClock::shared(), &config, &CancellationToken::new())
        .await
        .unwrap();
    let stats = server.models().unwrap().turn_stats();
    server.shutdown(Duration::from_secs(5)).await.unwrap();

    let step = &report.steps[0];
    assert_eq!(step.verdict, Verdict::Pass, "{:?}", step.reasons);
    assert_eq!((step.calls, step.calls_failed), (2, 0));
    // Two utterances per call: every one of the four turns got its answer.
    assert_eq!((step.turns_expected, step.turns_answered), (4, 4));
    // The real Smart Turn model analysed the audio (the stub never runs a model).
    assert!(stats.analyses >= 4, "{stats:?}");
    assert!(stats.run_total > Duration::ZERO, "{stats:?}");
}
