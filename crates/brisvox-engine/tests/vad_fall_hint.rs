//! The opt-in `UserSpeechFalling` hint of the VAD worker: sent once per fall, before the
//! stop it belongs to and dated like that stop; never sent unless asked for.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::single_range_in_vec_init
)]

mod common;

use std::ops::Range;
use std::time::Duration;

use brisvox_core::frame::{SampleRate, SystemFrame};
use brisvox_core::{Clock, Links, Pipeline, SimClock};
use brisvox_engine::{
    AudioIn, EarlyAnalysisConfig, EnergyModel, EngineConfig, Offload, VadSetup, VadWorkerConfig,
};
use brisvox_inference::VadParams;
use common::{MS, Recorded, Recorder, square_frame};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const LOUD: i16 = 3_000;

fn worker_config() -> VadWorkerConfig {
    VadWorkerConfig {
        params: VadParams {
            threshold: 0.5,
            neg_threshold: 0.35,
            min_speech: 40 * MS,
            min_silence: 100 * MS,
            speech_pad: Duration::ZERO,
        },
        job_timeout: Duration::from_millis(250),
        max_consecutive_failures: 5,
        failure_window: Duration::ZERO,
        recovery_interval: None,
    }
}

fn setup() -> VadSetup<EnergyModel> {
    VadSetup::new(Offload::Inline(SimClock::shared()), worker_config(), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    })
}

/// What the recorder saw of the VAD, as `(label, ms after origin)`, and the hints counted.
async fn run(setup: VadSetup<EnergyModel>, speech: &[Range<u64>]) -> (Vec<(String, u64)>, u64) {
    let clock = SimClock::new();
    let origin = clock.now();
    let config = EngineConfig::DEFAULT;
    let audio_in = AudioIn::new(&config).with_vad(setup);
    let stats = audio_in.vad_stats();
    let rec = Recorded::default();
    let running = Pipeline::new("fall")
        .then_with_queue(audio_in, config.queues.input)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    for i in 0..80 {
        let loud = speech.iter().any(|r| r.contains(&i));
        let frame = square_frame(origin, i, SampleRate::Hz16000, if loud { LOUD } else { 0 });
        running.input().send(frame.into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    clock.sleep(100 * MS).await;
    let ms = |at: Instant| u64::try_from(at.saturating_duration_since(origin).as_millis()).unwrap();
    let frames = rec
        .system()
        .into_iter()
        .filter_map(|frame| match frame {
            SystemFrame::UserStartedSpeaking { at } => Some(("start".to_owned(), ms(at))),
            SystemFrame::UserSpeechFalling { at } => Some(("falling".to_owned(), ms(at))),
            SystemFrame::UserStoppedSpeaking { at } => Some(("stop".to_owned(), ms(at))),
            _ => None,
        })
        .collect();
    (frames, stats.snapshot().falls)
}

#[tokio::test(start_paused = true)]
async fn no_hint_is_sent_unless_asked_for() {
    let (frames, falls) = run(setup(), &[10..30]).await;
    assert_eq!(
        frames,
        [("start".to_owned(), 200), ("stop".to_owned(), 600)],
        "the default worker sends exactly start and stop"
    );
    assert_eq!(falls, 0);
}

#[tokio::test(start_paused = true)]
async fn the_hint_comes_once_before_the_stop_dated_like_it() {
    let (frames, falls) = run(setup().with_fall_hint(Some(40 * MS)), &[10..30]).await;
    assert_eq!(
        frames,
        [
            ("start".to_owned(), 200),
            ("falling".to_owned(), 600),
            ("stop".to_owned(), 600)
        ]
    );
    assert_eq!(falls, 1);
}

#[tokio::test(start_paused = true)]
async fn a_dip_the_vad_forgives_hints_again_at_the_real_fall() {
    // Speech 10..20, a 60 ms dip (frames 20..23) that does not become a stop, speech 23..30.
    let (frames, falls) = run(setup().with_fall_hint(Some(40 * MS)), &[10..20, 23..30]).await;
    let labels: Vec<&str> = frames.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        ["start", "falling", "falling", "stop"],
        "{frames:?}"
    );
    assert_eq!(frames[1].1, 400, "the dip's hint");
    assert_eq!(
        frames[2].1, 600,
        "the real fall's hint equals the stop's time"
    );
    assert_eq!(frames[3].1, 600);
    assert_eq!(falls, 2);
}

#[tokio::test(start_paused = true)]
async fn the_engine_config_switches_the_hint_on_with_its_fall_delay() {
    let mut engine = EngineConfig::DEFAULT;
    let (off, _) = run(setup().with_early_analysis(&engine), &[10..30]).await;
    assert_eq!(off.len(), 2, "early analysis is off by default: {off:?}");
    engine.early_analysis = EarlyAnalysisConfig {
        fall_delay: 40 * MS,
        ..EarlyAnalysisConfig::ENABLED
    };
    let (on, falls) = run(setup().with_early_analysis(&engine), &[10..30]).await;
    assert_eq!(on.len(), 3, "{on:?}");
    assert_eq!(falls, 1);
}

fn enabled_with_delay(fall_delay: Duration) -> EngineConfig {
    let mut engine = EngineConfig::DEFAULT;
    engine.early_analysis = EarlyAnalysisConfig {
        fall_delay,
        ..EarlyAnalysisConfig::ENABLED
    };
    engine
}

fn labels(frames: &[(String, u64)]) -> Vec<&str> {
    frames.iter().map(|(label, _)| label.as_str()).collect()
}

/// A zero, a constant or an ignored delay would all hint on the 60 ms dip; only the configured
/// 80 ms (four 20 ms windows of this energy model) filters it out and hints at the real fall alone.
#[tokio::test(start_paused = true)]
async fn the_configured_fall_delay_decides_which_dips_are_hinted() {
    let dip = [10..20, 23..30];
    let (short, short_falls) = run(
        setup().with_early_analysis(&enabled_with_delay(40 * MS)),
        &dip,
    )
    .await;
    assert_eq!(
        labels(&short),
        ["start", "falling", "falling", "stop"],
        "a 40 ms delay hints on the dip: {short:?}"
    );
    assert_eq!(short_falls, 2);
    let (long, long_falls) = run(
        setup().with_early_analysis(&enabled_with_delay(80 * MS)),
        &dip,
    )
    .await;
    assert_eq!(
        labels(&long),
        ["start", "falling", "stop"],
        "an 80 ms delay filters the 60 ms dip out: {long:?}"
    );
    assert_eq!(
        long[1].1, 600,
        "the hint of the real fall equals the stop's time"
    );
    assert_eq!(long_falls, 1);
}

/// A `fall_delay` the stop's confirmation reaches first (>= `min_silence`) never hints: the
/// feature is inert and says so in the statistics (docs/LATENCY.md).
#[tokio::test(start_paused = true)]
async fn a_fall_delay_not_below_min_silence_sends_no_hint() {
    let (frames, falls) = run(
        setup().with_early_analysis(&enabled_with_delay(100 * MS)),
        &[10..30],
    )
    .await;
    assert_eq!(
        frames,
        [("start".to_owned(), 200), ("stop".to_owned(), 600)],
        "{frames:?}"
    );
    assert_eq!(falls, 0);
}

/// Turning the hint on never builds a model on the caller's thread: the setup is usually made
/// inside a tokio task, and building a model can load a file (the ONNX Silero). The delay check
/// happens in the worker's first job instead, so the factory is not called here at all.
#[test]
fn turning_the_hint_on_does_not_build_a_model_on_the_callers_thread() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let built = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&built);
    let setup = VadSetup::new(
        Offload::Inline(SimClock::shared()),
        worker_config(),
        move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(EnergyModel::from_pcm_rms(500.0))
        },
    );
    let setup = setup
        .with_fall_hint(Some(40 * MS))
        .with_early_analysis(&enabled_with_delay(100 * MS));
    assert_eq!(
        built.load(Ordering::Relaxed),
        0,
        "no model is built before the worker runs"
    );
    drop(setup);
}
