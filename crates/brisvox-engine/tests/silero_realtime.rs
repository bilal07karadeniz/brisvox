//! Real time, real model: the VAD worker runs Silero (tract) on the inference pool and
//! finds the speech segments of the fixtures. Paused-time simulations cannot cover this path:
//! the pool's threads live outside the runtime (D7 in `docs/PIPELINE.md`).
#![cfg(feature = "silero-tract")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{Clock, Links, Pipeline, SharedClock, SystemClock, TurnEpoch};
use brisvox_engine::{AudioIn, EngineConfig, Offload, VadSetup, VadWorkerConfig};
use brisvox_inference::silero_tract::SileroTractModel;
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use common::{MS, Recorded, Recorder};
use tokio_util::sync::CancellationToken;

const FIXTURES: [&str; 3] = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"];

/// Feeds the fixture as 20 ms frames stamped with their position, never letting the worker fall
/// more than a few frames behind (this is a correctness test of detection, not of overload: the
/// worker tests cover the shedding).
async fn detect(
    fixture: &Fixture,
    pool: &InferencePool,
    model: &std::sync::Arc<SileroTractModel>,
) -> (Vec<(bool, Duration)>, u64) {
    let clock = SystemClock;
    let config = EngineConfig::DEFAULT;
    let model = std::sync::Arc::clone(model);
    let setup = VadSetup::new(
        Offload::Pool(pool.handle()),
        VadWorkerConfig::from_engine(&config),
        move || model.stream(),
    );
    let audio_in = AudioIn::new(&config).with_vad(setup);
    let stats = audio_in.vad_stats();
    let rec = Recorded::default();
    let running = Pipeline::new("silero")
        .then_with_queue(audio_in, config.queues.input)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let origin = clock.now();
    let frames = fixture.pcm.len().div_ceil(640);
    for i in 0..frames {
        let range = i * 640..((i + 1) * 640).min(fixture.pcm.len());
        let frame = AudioFrame::new(
            fixture.pcm.slice(range),
            SampleRate::Hz16000,
            origin + 20 * MS * u32::try_from(i).unwrap(),
            i as u64,
            TurnEpoch::ZERO,
        )
        .unwrap();
        running.input().send(frame.into()).await.unwrap();
        let mut spins = 0u32;
        while (i as u64 + 1).saturating_sub(stats.snapshot().frames) > 3 {
            spins += 1;
            if spins < 200 {
                tokio::task::yield_now().await;
            } else {
                clock.sleep(MS).await;
            }
        }
    }
    // Let the last batch finish.
    let deadline = clock.now() + Duration::from_secs(5);
    while stats.snapshot().frames < frames as u64 && clock.now() < deadline {
        clock.sleep(2 * MS).await;
    }
    clock.sleep(20 * MS).await;
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.frames, frames as u64, "{snapshot:?}");
    assert!(!snapshot.failed_open, "{snapshot:?}");
    assert_eq!(rec.audio().len(), frames, "all audio was forwarded");
    let events = rec
        .vad()
        .into_iter()
        .map(|(start, at)| (start, at - origin))
        .collect();
    (events, snapshot.tee_dropped)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silero_on_the_pool_detects_the_speech_segments_of_the_fixtures() {
    let clock: SharedClock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 64,
            name: "silero-rt".to_owned(),
        },
        clock,
    )
    .unwrap();
    let model = SileroTractModel::bundled().unwrap();
    let mut worst = Duration::ZERO;
    for name in FIXTURES {
        let fixture = Fixture::load(&fixtures_dir(), name).unwrap();
        let (events, dropped) = detect(&fixture, &pool, &model).await;
        let truth: Vec<Duration> = fixture
            .speech_segments()
            .into_iter()
            .flat_map(|(start, end)| [start, end])
            .collect();
        println!("{name}: truth {truth:?}\n  found {events:?} (tee dropped {dropped})");
        assert_eq!(events.len(), truth.len(), "{name}: {events:?}");
        for (i, ((is_start, found), truth)) in events.iter().zip(&truth).enumerate() {
            assert_eq!(*is_start, i % 2 == 0, "{name}: events alternate start/stop");
            let error = found.abs_diff(*truth);
            worst = worst.max(error);
            // Window 32 ms + padding 30 ms + hangover: boundaries within 100 ms of the truth.
            assert!(
                error <= Duration::from_millis(100),
                "{name} boundary {i}: {found:?} vs truth {truth:?}"
            );
        }
        assert_eq!(dropped, 0, "{name}: the paced feed must not shed frames");
    }
    println!("worst boundary error {worst:?}");
    let metrics = pool.metrics();
    assert!(
        metrics.completed > 100,
        "the inference ran on the pool: {metrics:?}"
    );
    assert_eq!(
        metrics.panicked + metrics.rejected + metrics.timed_out,
        0,
        "{metrics:?}"
    );
    pool.shutdown();
}
