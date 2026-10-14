//! The VAD worker behind `AudioIn`: backdated events, batching, never delaying audio,
//! gap handling, and every way a job can fail.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::panic
)]
#![allow(clippy::disallowed_methods)] // real-time tests: blocking models sleep their pool thread

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use brisvox_core::frame::{DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{Clock, Links, Pipeline, RunningPipeline, SharedClock, SimClock, SystemClock};
use brisvox_engine::{AudioIn, EnergyModel, EngineConfig, Offload, VadSetup, VadWorkerConfig};
use brisvox_inference::{
    Error as InferenceError, InferencePool, PoolConfig, Priority, SpeechModel, VadParams,
};
use brisvox_sim::leak::TaskCounter;
use common::{MS, Recorded, Recorder, square_frame};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const LOUD: i16 = 3_000;

fn params() -> VadParams {
    VadParams {
        threshold: 0.5,
        neg_threshold: 0.35,
        min_speech: 40 * MS,
        min_silence: 100 * MS,
        speech_pad: Duration::ZERO,
    }
}

fn worker_config(failures: u32) -> VadWorkerConfig {
    VadWorkerConfig {
        params: params(),
        job_timeout: Duration::from_millis(250),
        max_consecutive_failures: failures,
        failure_window: Duration::ZERO,
        // Failing open is permanent here unless a test turns the probes on.
        recovery_interval: None,
    }
}

fn energy_setup(offload: Offload) -> VadSetup<EnergyModel> {
    VadSetup::new(offload, worker_config(5), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    })
}

struct Chain {
    running: RunningPipeline,
    rec: Recorded,
    stats: brisvox_engine::VadStats,
    audio_stats: brisvox_engine::AudioInStats,
}

async fn start<M: SpeechModel + 'static>(setup: VadSetup<M>) -> Chain {
    let config = EngineConfig::DEFAULT;
    let audio_in = AudioIn::new(&config).with_vad(setup);
    let stats = audio_in.vad_stats();
    let audio_stats = audio_in.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("input")
        .then_with_queue(audio_in, config.queues.input)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    Chain {
        running,
        rec,
        stats,
        audio_stats,
    }
}

/// Sends frame `i` (loud when `speech`), then lets 20 ms pass on the clock.
async fn feed(chain: &Chain, clock: &SimClock, origin: Instant, i: u64, speech: bool) {
    let frame = square_frame(
        origin,
        i,
        SampleRate::Hz16000,
        if speech { LOUD } else { 0 },
    );
    chain.running.input().send(frame.into()).await.unwrap();
    clock.sleep(20 * MS).await;
}

#[tokio::test(start_paused = true)]
async fn speech_boundaries_are_backdated_to_the_audio_and_all_audio_is_forwarded() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start(energy_setup(Offload::Inline(SimClock::shared()))).await;
    for i in 0..60 {
        feed(&chain, &clock, origin, i, (10..30).contains(&i)).await;
    }
    clock.sleep(100 * MS).await;

    // Speech is frames 10..30: it started at frame 10's timestamp and ended with frame 29's end,
    // and both events were sent long after, backdated by the worker.
    assert_eq!(
        chain.rec.vad(),
        [(true, origin + 200 * MS), (false, origin + 600 * MS)]
    );
    let audio = chain.rec.audio();
    assert_eq!(audio.len(), 60, "every frame is forwarded");
    assert!(audio.windows(2).all(|w| w[1].seq() == w[0].seq() + 1));
    let stats = chain.stats.snapshot();
    assert_eq!((stats.starts, stats.stops), (1, 1));
    assert_eq!(stats.frames, 60);
    assert_eq!(stats.tee_dropped + stats.discarded_failed_open, 0);
    assert!(!stats.failed_open);
    assert_eq!(chain.audio_stats.snapshot().passthrough, 60);
}

#[tokio::test(start_paused = true)]
async fn frames_that_queue_up_while_the_worker_is_busy_become_one_job() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start(energy_setup(Offload::Inline(SimClock::shared()))).await;
    // Five frames arrive before the worker task ever runs: it takes them as one batch.
    for i in 0..5 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().try_send(frame.into()).unwrap();
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let stats = chain.stats.snapshot();
    assert_eq!((stats.batches, stats.frames, stats.jobs_ok), (1, 5, 1));
    assert_eq!(chain.rec.audio().len(), 5);
    // Speech from the very first window was found inside that one job.
    assert_eq!(chain.rec.vad(), [(true, origin)]);
}

#[tokio::test(start_paused = true)]
async fn a_jump_in_the_sequence_closes_the_stream_instead_of_running_windows_across_the_hole() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start(energy_setup(Offload::Inline(SimClock::shared()))).await;
    for i in 0..10 {
        feed(&chain, &clock, origin, i, true).await;
    }
    // Frames 10..15 never arrive (dropped by the first-stage inbox); speech goes on at 15.
    for i in 15..25 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 25..40 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let kinds: Vec<bool> = chain.rec.vad().iter().map(|(start, _)| *start).collect();
    assert_eq!(
        kinds,
        [true, false, true, false],
        "stop at the hole, then a new start"
    );
    let events = chain.rec.vad();
    // The stop at the hole is dated to the end of the last audio before it (frame 9).
    assert_eq!(events[1].1, origin + 200 * MS);
    assert_eq!(
        events[2].1,
        origin + 300 * MS,
        "the new start is the first frame after the hole"
    );
    assert_eq!(chain.stats.snapshot().gap_resets, 1);
    assert_eq!(chain.audio_stats.snapshot().gaps, 1);
    assert_eq!(
        chain.rec.audio().len(),
        10 + 10 + 15,
        "the audio itself is not touched"
    );
}

/// A 10 ms frame at 8 kHz (80 samples; half a resampler block) with sequence number `i`.
fn frame_10ms_8k(origin: Instant, i: u64, speech: bool) -> brisvox_core::frame::AudioFrame {
    let full = square_frame(origin, 0, SampleRate::Hz8000, if speech { LOUD } else { 0 });
    brisvox_core::frame::AudioFrame::new(
        full.data().slice(..160),
        SampleRate::Hz8000,
        origin + 10 * MS * u32::try_from(i).unwrap(),
        i,
        brisvox_core::TurnEpoch::ZERO,
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_hole_before_a_frame_that_yields_no_resampled_output_still_closes_the_stream() {
    let clock = SimClock::new();
    let origin = clock.now();
    let chain = start(energy_setup(Offload::Inline(SimClock::shared()))).await;
    let send = |i: u64, speech: bool| {
        let input = chain.running.input().clone();
        let frame = frame_10ms_8k(origin, i, speech);
        async move {
            input.send(frame.into()).await.unwrap();
            clock.sleep(10 * MS).await;
        }
    };
    for i in 0..20 {
        send(i, true).await;
    }
    // Frames 20..25 never arrive. Frame 25 is the FIRST half of a resampler block (80 of 160
    // samples), so it produces no output: the flag for the hole has to wait for frame 26.
    for i in 25..45 {
        send(i, true).await;
    }
    for i in 45..90 {
        send(i, false).await;
    }
    clock.sleep(200 * MS).await;

    let events = chain.rec.vad();
    let kinds: Vec<bool> = events.iter().map(|(start, _)| *start).collect();
    assert_eq!(
        kinds,
        [true, false, true, false],
        "stop at the hole, then a new start: {events:?}"
    );
    // The stop is dated within the audio before the hole (it ends at 200 ms), the new start is
    // at the first audio after it (250 ms, minus the filter delay it was compensated for).
    assert!(events[1].1 <= origin + 200 * MS, "{events:?}");
    assert!(
        events[2].1 >= origin + 230 * MS && events[2].1 <= origin + 250 * MS,
        "{events:?}"
    );
    assert_eq!(chain.stats.snapshot().gap_resets, 1);
    assert_eq!(chain.audio_stats.snapshot().gaps, 1);
}

/// A model that fails in a scripted way: `panic_at` panics in that (0-based) call, `error_at`
/// returns an error there; otherwise it is an energy model. `builds` counts how many were made.
struct Scripted {
    inner: EnergyModel,
    calls: usize,
    panic_at: Option<usize>,
    error_at: Option<usize>,
}

impl SpeechModel for Scripted {
    fn window(&self) -> usize {
        self.inner.window()
    }
    fn context(&self) -> usize {
        0
    }
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        let call = self.calls;
        self.calls += 1;
        if self.panic_at == Some(call) {
            panic!("scripted model panic");
        }
        if self.error_at == Some(call) {
            return Err(InferenceError::Model("scripted model error".into()));
        }
        self.inner.infer(input)
    }
    fn reset(&mut self) {}
}

fn scripted_setup(
    first: (Option<usize>, Option<usize>),
    failures: u32,
    builds: Arc<AtomicUsize>,
) -> VadSetup<Scripted> {
    VadSetup::new(
        Offload::Inline(SimClock::shared()),
        worker_config(failures),
        move || {
            let n = builds.fetch_add(1, Ordering::SeqCst);
            // Only the first model is scripted to fail; rebuilt ones are healthy.
            let (panic_at, error_at) = if n == 0 { first } else { (None, None) };
            Ok(Scripted {
                inner: EnergyModel::from_pcm_rms(500.0),
                calls: 0,
                panic_at,
                error_at,
            })
        },
    )
}

#[tokio::test(start_paused = true)]
async fn a_panicking_model_loses_one_batch_and_the_stream_is_rebuilt() {
    let clock = SimClock::new();
    let origin = clock.now();
    let builds = Arc::new(AtomicUsize::new(0));
    let chain = start(scripted_setup((Some(8), None), 5, Arc::clone(&builds))).await;
    // Speech from frame 2 on; the first model panics in its 9th window (frame 8).
    for i in 0..2 {
        feed(&chain, &clock, origin, i, false).await;
    }
    for i in 2..20 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 20..40 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;

    let events = chain.rec.vad();
    let kinds: Vec<bool> = events.iter().map(|(start, _)| *start).collect();
    // start, the stop the worker synthesized when the stream that knew about the speech was lost
    // (dated to the end of the last audio that went through: frame 7), a new start from the
    // rebuilt stream, and the real stop.
    assert_eq!(kinds, [true, false, true, false], "{events:?}");
    assert_eq!(events[0].1, origin + 40 * MS);
    assert_eq!(events[1].1, origin + 160 * MS, "end of frame 7");
    let stats = chain.stats.snapshot();
    assert_eq!(
        (stats.panics, stats.rebuilds, stats.synthesized_stops),
        (1, 1, 1)
    );
    assert!(!stats.failed_open, "one failure is not a degradation");
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(
        chain.rec.audio().len(),
        40,
        "the panic never touched the audio path"
    );
    assert!(
        chain
            .rec
            .system()
            .iter()
            .all(|s| !matches!(s, SystemFrame::Error(_))),
        "no error frame below the failure limit"
    );
}

#[tokio::test(start_paused = true)]
async fn a_model_error_on_one_window_keeps_the_stream_and_the_events() {
    let clock = SimClock::new();
    let origin = clock.now();
    let builds = Arc::new(AtomicUsize::new(0));
    let chain = start(scripted_setup((None, Some(5)), 5, Arc::clone(&builds))).await;
    for i in 0..20 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 20..40 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let kinds: Vec<bool> = chain.rec.vad().iter().map(|(start, _)| *start).collect();
    assert_eq!(kinds, [true, false]);
    let stats = chain.stats.snapshot();
    assert_eq!(
        (stats.model_errors, stats.rebuilds, stats.panics),
        (1, 0, 0)
    );
    assert_eq!(builds.load(Ordering::SeqCst), 1, "the stream survived");
}

#[tokio::test(start_paused = true)]
async fn consecutive_failures_fail_open_with_one_error_frame_and_audio_keeps_flowing() {
    let clock = SimClock::new();
    let origin = clock.now();
    // Every model panics on its first window: each batch fails and the rebuilt model fails again.
    let setup = VadSetup::new(
        Offload::Inline(SimClock::shared()),
        worker_config(3),
        || {
            Ok(Scripted {
                inner: EnergyModel::from_pcm_rms(500.0),
                calls: 0,
                panic_at: Some(0),
                error_at: None,
            })
        },
    );
    let chain = start(setup).await;
    for i in 0..12 {
        feed(&chain, &clock, origin, i, true).await;
    }
    clock.sleep(100 * MS).await;

    let errors: Vec<_> = chain
        .rec
        .system()
        .into_iter()
        .filter_map(|s| match s {
            SystemFrame::Error(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "exactly one error frame");
    assert_eq!(errors[0].stage, "vad");
    assert!(!errors[0].fatal, "audio keeps flowing: not fatal");
    let stats = chain.stats.snapshot();
    assert!(stats.failed_open);
    assert_eq!(stats.panics, 3);
    assert_eq!(
        stats.discarded_failed_open,
        12 - 3,
        "later frames are only drained"
    );
    assert!(
        chain.rec.vad().is_empty(),
        "no VAD event from a failed detector"
    );
    assert_eq!(chain.rec.audio().len(), 12, "every frame was forwarded");
}

#[tokio::test(start_paused = true)]
async fn the_worker_is_aborted_with_the_stage() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    let chain = start(energy_setup(Offload::Inline(SimClock::shared()))).await;
    clock.sleep(20 * MS).await;
    // pipeline: audio_in, recorder + the vad worker
    assert_eq!(counter.alive(), counter.baseline() + 3);
    drop(chain);
    counter
        .wait_for_baseline(&clock, Duration::from_millis(100))
        .await
        .expect("a vad worker outlived its pipeline");
}

/// Every model built before the `healthy_after`-th (0-based) panics on its first window.
fn failing_then_healthy(config: VadWorkerConfig, healthy_after: usize) -> VadSetup<Scripted> {
    let builds = Arc::new(AtomicUsize::new(0));
    VadSetup::new(Offload::Inline(SimClock::shared()), config, move || {
        let n = builds.fetch_add(1, Ordering::SeqCst);
        Ok(Scripted {
            inner: EnergyModel::from_pcm_rms(500.0),
            calls: 0,
            panic_at: (n < healthy_after).then_some(0),
            error_at: None,
        })
    })
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_failures_longer_than_the_count_but_shorter_than_the_window_recovers() {
    let clock = SimClock::new();
    let origin = clock.now();
    // 8 failed batches in a row (160 ms of audio), above the count limit of 5 but far below the
    // 1 s window: a saturation burst, not a dead detector.
    let config = VadWorkerConfig {
        failure_window: Duration::from_secs(1),
        ..worker_config(5)
    };
    let chain = start(failing_then_healthy(config, 8)).await;
    for i in 0..10 {
        feed(&chain, &clock, origin, i, false).await;
    }
    for i in 10..40 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 40..60 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.panics, 8, "{stats:?}");
    assert!(
        !stats.failed_open,
        "a short burst must not end VAD: {stats:?}"
    );
    assert_eq!(stats.discarded_failed_open, 0);
    assert!(
        chain
            .rec
            .system()
            .iter()
            .all(|s| !matches!(s, SystemFrame::Error(_))),
        "no degradation frame for a burst that healed"
    );
    // And detection works again afterwards.
    let kinds: Vec<bool> = chain.rec.vad().iter().map(|(start, _)| *start).collect();
    assert_eq!(kinds, [true, false], "{:?}", chain.rec.vad());
    assert_eq!(chain.rec.audio().len(), 60);
}

#[tokio::test(start_paused = true)]
async fn failures_that_last_the_whole_window_fail_open() {
    let clock = SimClock::new();
    let origin = clock.now();
    let config = VadWorkerConfig {
        failure_window: 200 * MS,
        ..worker_config(3)
    };
    let chain = start(failing_then_healthy(config, usize::MAX)).await;
    for i in 0..30 {
        feed(&chain, &clock, origin, i, true).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert!(stats.failed_open, "{stats:?}");
    // The streak starts at the end of the first failed batch (20 ms) and has covered 200 ms of
    // audio at the end of the 11th (220 ms): the count limit alone (3) did not do it.
    assert_eq!(stats.panics, 11, "{stats:?}");
    assert_eq!(chain.rec.audio().len(), 30);
    let errors = chain
        .rec
        .system()
        .into_iter()
        .filter(|s| matches!(s, SystemFrame::Error(_)))
        .count();
    assert_eq!(errors, 1);
}

/// An energy model whose health the test controls through `ok_budget`: `usize::MAX` is healthy,
/// `0` fails every window, `n` lets `n` more windows succeed and then fails.
struct Controlled {
    inner: EnergyModel,
    ok_budget: Arc<AtomicUsize>,
}

impl SpeechModel for Controlled {
    fn window(&self) -> usize {
        self.inner.window()
    }
    fn context(&self) -> usize {
        0
    }
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        let left = self.ok_budget.load(Ordering::SeqCst);
        if left == 0 {
            return Err(InferenceError::Model("controlled failure".into()));
        }
        if left != usize::MAX {
            self.ok_budget.store(left - 1, Ordering::SeqCst);
        }
        self.inner.infer(input)
    }
    fn reset(&mut self) {}
}

fn controlled_setup(config: VadWorkerConfig, ok_budget: &Arc<AtomicUsize>) -> VadSetup<Controlled> {
    let ok_budget = Arc::clone(ok_budget);
    VadSetup::new(Offload::Inline(SimClock::shared()), config, move || {
        Ok(Controlled {
            inner: EnergyModel::from_pcm_rms(500.0),
            ok_budget: Arc::clone(&ok_budget),
        })
    })
}

fn error_frames(chain: &Chain) -> usize {
    chain
        .rec
        .system()
        .into_iter()
        .filter(|s| matches!(s, SystemFrame::Error(_)))
        .count()
}

/// Failures scripted by the index of the window (one window per 20 ms frame, as long as the
/// detector is not failed open).
struct Scheduled {
    inner: EnergyModel,
    calls: Arc<AtomicUsize>,
    fails: fn(usize) -> bool,
}

impl SpeechModel for Scheduled {
    fn window(&self) -> usize {
        self.inner.window()
    }
    fn context(&self) -> usize {
        0
    }
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn infer(&mut self, input: &[f32]) -> brisvox_inference::Result<f32> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if (self.fails)(n) {
            return Err(InferenceError::Model("scheduled failure".into()));
        }
        self.inner.infer(input)
    }
    fn reset(&mut self) {}
}

fn scheduled_setup(config: VadWorkerConfig, fails: fn(usize) -> bool) -> VadSetup<Scheduled> {
    let calls = Arc::new(AtomicUsize::new(0));
    VadSetup::new(Offload::Inline(SimClock::shared()), config, move || {
        Ok(Scheduled {
            inner: EnergyModel::from_pcm_rms(500.0),
            calls: Arc::clone(&calls),
            fails,
        })
    })
}

/// The streak resets with a successful job: four failures, one success, four failures is never
/// five in a row, even with no window to require (`failure_window` zero).
#[tokio::test(start_paused = true)]
async fn a_successful_job_resets_the_failure_streak() {
    let clock = SimClock::new();
    let origin = clock.now();
    // Windows 0..4 fail, 4 succeeds, 5..9 fail, 9 succeeds, ... (four failures, one success).
    let chain = start(scheduled_setup(worker_config(5), |n| n % 5 < 4)).await;
    for i in 0..60 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.model_errors, 48, "{stats:?}");
    assert!(
        !stats.failed_open,
        "separate bursts of four are not five in a row: {stats:?}"
    );
    assert_eq!(stats.discarded_failed_open, 0);
    assert_eq!(error_frames(&chain), 0);
}

/// Two bursts of three failures 2 s apart, with a count limit of three and a window of 1 s: each
/// burst is short and the success between them clears it, so the detector does not fail open.
#[tokio::test(start_paused = true)]
async fn two_short_bursts_far_apart_do_not_add_up() {
    let clock = SimClock::new();
    let origin = clock.now();
    let config = VadWorkerConfig {
        failure_window: Duration::from_secs(1),
        ..worker_config(3)
    };
    let chain = start(scheduled_setup(config, |n| {
        (5..8).contains(&n) || (105..108).contains(&n)
    }))
    .await;
    for i in 0..130 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.model_errors, 6, "{stats:?}");
    assert!(!stats.failed_open, "{stats:?}");
    assert_eq!(error_frames(&chain), 0);
}

/// A model factory that cannot build a model (the first call, or every call) is a failure like
/// any other: counted, and a permanently broken one fails the detector open with one `Error`
/// frame instead of retrying (and logging) forever.
#[tokio::test(start_paused = true)]
async fn a_factory_that_always_fails_fails_the_detector_open() {
    let clock = SimClock::new();
    let origin = clock.now();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let setup: VadSetup<EnergyModel> = VadSetup::new(
        Offload::Inline(SimClock::shared()),
        worker_config(3),
        move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(InferenceError::Model("cannot load the model".into()))
        },
    );
    let chain = start(setup).await;
    for i in 0..12 {
        feed(&chain, &clock, origin, i, true).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.build_failures, 3, "{stats:?}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "no retry once failed open"
    );
    assert!(stats.failed_open, "{stats:?}");
    assert_eq!(stats.discarded_failed_open, 12 - 3);
    assert_eq!((stats.jobs_ok, stats.rebuilds), (0, 0));
    assert_eq!(error_frames(&chain), 1, "exactly one error frame");
    assert_eq!(chain.rec.audio().len(), 12, "audio keeps flowing");
    assert!(chain.rec.vad().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_factory_that_fails_once_costs_one_batch_and_detection_goes_on() {
    let clock = SimClock::new();
    let origin = clock.now();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let setup: VadSetup<EnergyModel> = VadSetup::new(
        Offload::Inline(SimClock::shared()),
        worker_config(3),
        move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(InferenceError::Model("transient".into()))
            } else {
                Ok(EnergyModel::from_pcm_rms(500.0))
            }
        },
    );
    let chain = start(setup).await;
    for i in 0..5 {
        feed(&chain, &clock, origin, i, false).await;
    }
    for i in 5..25 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 25..45 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!((stats.build_failures, stats.jobs_ok), (1, 44), "{stats:?}");
    assert!(!stats.failed_open);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(error_frames(&chain), 0);
    let kinds: Vec<bool> = chain.rec.vad().iter().map(|(start, _)| *start).collect();
    assert_eq!(kinds, [true, false], "{:?}", chain.rec.vad());
}

fn recovery_config(interval: Option<Duration>) -> VadWorkerConfig {
    VadWorkerConfig {
        recovery_interval: interval,
        ..worker_config(3)
    }
}

/// Failing open is not for the rest of the call: every `recovery_interval` of audio the worker
/// probes with a fresh stream; a probe that fails changes nothing, the first that succeeds
/// brings the events back.
#[tokio::test(start_paused = true)]
async fn a_failed_open_detector_recovers_when_a_probe_succeeds() {
    let clock = SimClock::new();
    let origin = clock.now();
    let budget = Arc::new(AtomicUsize::new(0)); // every window fails
    let chain = start(controlled_setup(
        recovery_config(Some(Duration::from_secs(1))),
        &budget,
    ))
    .await;
    for i in 0..60 {
        feed(&chain, &clock, origin, i, true).await;
    }
    clock.sleep(10 * MS).await;
    // Failed open after three batches (60 ms); the first probe is the first batch ending a
    // second later (frame 52), and it fails too.
    let stats = chain.stats.snapshot();
    assert!(stats.failed_open, "{stats:?}");
    assert_eq!(
        stats.model_errors, 4,
        "three, and the failed probe: {stats:?}"
    );
    assert_eq!(stats.discarded_failed_open, 60 - 4, "{stats:?}");
    assert_eq!(stats.recoveries, 0);
    assert_eq!(error_frames(&chain), 1);
    assert!(chain.rec.vad().is_empty());

    // The model heals. The next probe is a second after the last one (frame 102): everything
    // before it is drained, and speech after it is detected again.
    budget.store(usize::MAX, Ordering::SeqCst);
    for i in 60..110 {
        feed(&chain, &clock, origin, i, false).await;
    }
    let stats = chain.stats.snapshot();
    assert_eq!(
        (stats.recoveries, stats.failed_open),
        (1, false),
        "{stats:?}"
    );
    for i in 110..130 {
        feed(&chain, &clock, origin, i, true).await;
    }
    for i in 130..150 {
        feed(&chain, &clock, origin, i, false).await;
    }
    clock.sleep(100 * MS).await;
    assert_eq!(
        chain.rec.vad(),
        [
            (true, origin + 110 * 20 * MS),
            (false, origin + 130 * 20 * MS)
        ]
    );
    let stats = chain.stats.snapshot();
    assert!(!stats.failed_open);
    assert_eq!(stats.recoveries, 1);
    assert_eq!(
        error_frames(&chain),
        1,
        "no second error frame for the same outage"
    );
    assert_eq!(chain.rec.audio().len(), 150, "audio never stopped");
}

/// With `recovery_interval: None` (the old behaviour) the detector stays failed open however
/// healthy the model gets.
#[tokio::test(start_paused = true)]
async fn without_a_recovery_interval_failing_open_is_permanent() {
    let clock = SimClock::new();
    let origin = clock.now();
    let budget = Arc::new(AtomicUsize::new(0));
    let chain = start(controlled_setup(recovery_config(None), &budget)).await;
    for i in 0..10 {
        feed(&chain, &clock, origin, i, true).await;
    }
    assert!(chain.stats.snapshot().failed_open);
    budget.store(usize::MAX, Ordering::SeqCst);
    for i in 10..300 {
        feed(&chain, &clock, origin, i, i % 100 < 50).await;
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert!(stats.failed_open, "{stats:?}");
    assert_eq!(stats.recoveries, 0);
    assert!(chain.rec.vad().is_empty());
}

/// A probe that starts speech and then fails must not leave that speech open: a start whose
/// stop never comes would hold a turn for the rest of the call.
#[tokio::test(start_paused = true)]
async fn a_failed_probe_closes_the_speech_it_started() {
    let clock = SimClock::new();
    let origin = clock.now();
    let budget = Arc::new(AtomicUsize::new(0));
    let chain = start(controlled_setup(
        recovery_config(Some(Duration::from_secs(1))),
        &budget,
    ))
    .await;
    for i in 0..52 {
        feed(&chain, &clock, origin, i, true).await;
    }
    assert!(chain.stats.snapshot().failed_open);
    // Five frames at once make the probe batch (it ends past the interval): three windows are
    // healthy, so speech starts, and then the model fails.
    budget.store(3, Ordering::SeqCst);
    for i in 52..57 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().try_send(frame.into()).unwrap();
    }
    clock.sleep(100 * MS).await;
    let stats = chain.stats.snapshot();
    assert!(stats.failed_open, "the probe failed: {stats:?}");
    assert_eq!(stats.recoveries, 0);
    let kinds: Vec<bool> = chain.rec.vad().iter().map(|(start, _)| *start).collect();
    assert_eq!(kinds, [true, false], "closed again: {:?}", chain.rec.vad());
}

// ---------------------------------------------------------------------------------------------
// Real time, on the inference pool

/// A model that takes `delay` per window, on its pool thread, and says "speech".
struct Slow {
    delay: Duration,
    first_delay: Option<Duration>,
    calls: usize,
}

impl SpeechModel for Slow {
    fn window(&self) -> usize {
        320
    }
    fn context(&self) -> usize {
        0
    }
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn infer(&mut self, _input: &[f32]) -> brisvox_inference::Result<f32> {
        let delay = if self.calls == 0 {
            self.first_delay.unwrap_or(self.delay)
        } else {
            self.delay
        };
        self.calls += 1;
        std::thread::sleep(delay);
        Ok(1.0)
    }
    fn reset(&mut self) {}
}

fn pool(threads: usize, queue_capacity: usize) -> InferencePool {
    let clock: SharedClock = SystemClock::shared();
    InferencePool::new(
        &PoolConfig {
            threads,
            queue_capacity,
            name: "vad-test".to_owned(),
        },
        clock,
    )
    .unwrap()
}

/// Waits (real time) until `condition` holds, at most 5 s.
async fn eventually(what: &str, condition: impl Fn() -> bool) {
    let clock = SystemClock;
    let deadline = clock.now() + Duration::from_secs(5);
    while !condition() {
        assert!(clock.now() < deadline, "timed out waiting for {what}");
        clock.sleep(2 * MS).await;
    }
}

#[tokio::test]
async fn a_slow_model_never_delays_audio_it_only_drops_the_oldest_vad_frames() {
    let clock = SystemClock;
    let pool = pool(1, 16);
    let setup = VadSetup::new(
        Offload::Pool(pool.handle()),
        VadWorkerConfig {
            job_timeout: Duration::from_secs(5),
            ..worker_config(5)
        },
        || {
            Ok(Slow {
                delay: Duration::from_millis(25),
                first_delay: None,
                calls: 0,
            })
        },
    );
    let chain = start(setup).await;
    let origin = clock.now();
    let started = clock.now();
    for i in 0..100 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        tokio::task::yield_now().await;
    }
    eventually("all audio to be forwarded", || {
        chain.rec.audio().len() == 100
    })
    .await;
    let forwarding = clock.now() - started;
    // The model needs 25 ms per 20 ms window, i.e. about 2.5 s for the 100 windows. The audio is
    // forwarded without waiting for it.
    assert!(
        forwarding < Duration::from_millis(1_500),
        "audio was delayed by the model: {forwarding:?}"
    );
    eventually("the worker to finish the batch behind the hole", || {
        chain.stats.snapshot().jobs_ok >= 2
    })
    .await;
    let stats = chain.stats.snapshot();
    assert!(
        stats.tee_dropped > 0,
        "the tee shed the oldest frames: {stats:?}"
    );
    assert!(
        stats.gap_resets > 0,
        "and the worker closed its stream at the holes: {stats:?}"
    );
    assert!(stats.batches < 100, "frames were batched: {stats:?}");
    assert!(stats.jobs_ok >= 1);
    drop(chain);
    pool.shutdown();
}

/// A one-thread pool whose thread (and queue slots) are held by blocking jobs on demand, so the
/// next job is refused (or waits in the queue). `release` frees every blocker.
struct FullPool {
    pool: InferencePool,
    release: mpsc::Sender<()>,
    release_rx: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    blockers: Vec<tokio::task::JoinHandle<()>>,
}

impl FullPool {
    /// A pool with `queue_capacity` slots and nothing blocking it yet.
    fn open(queue_capacity: usize) -> Self {
        let (release, release_rx) = mpsc::channel::<()>();
        Self {
            pool: pool(1, queue_capacity),
            release,
            release_rx: Arc::new(std::sync::Mutex::new(release_rx)),
            blockers: Vec::new(),
        }
    }

    /// Adds one blocking job (the first runs on the only thread, later ones fill the queue).
    async fn block(&mut self) {
        let handle = self.pool.handle();
        let rx = Arc::clone(&self.release_rx);
        self.blockers.push(tokio::spawn(async move {
            let _ = handle
                .run_with(Priority::High, Duration::from_secs(20), move || {
                    let _ = rx.lock().unwrap().recv();
                })
                .await;
        }));
        SystemClock.sleep(100 * MS).await;
    }

    /// The thread and the only queue slot are both held: the next job is refused.
    async fn new() -> Self {
        let mut full = Self::open(1);
        full.block().await;
        full.block().await;
        full
    }

    async fn release(self) -> InferencePool {
        for _ in &self.blockers {
            self.release.send(()).unwrap();
        }
        for blocker in self.blockers {
            let _ = blocker.await;
        }
        self.pool
    }
}

#[tokio::test]
async fn a_saturated_pool_delays_the_batch_and_keeps_the_stream_and_the_turn() {
    let clock = SystemClock;
    let full = FullPool::new().await;
    let setup = VadSetup::new(Offload::Pool(full.pool.handle()), worker_config(50), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    });
    let chain = start(setup).await;
    let origin = clock.now();
    for i in 0..3 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(30 * MS).await;
    }
    eventually("a rejected job", || chain.stats.snapshot().saturated >= 1).await;
    assert_eq!(
        chain.rec.audio().len(),
        3,
        "audio is unaffected by the rejection"
    );
    assert!(chain.rec.vad().is_empty());

    // The pool drains: the batch that was refused runs, on the SAME stream, together with the
    // audio that arrived meanwhile.
    let pool = full.release().await;
    for i in 3..13 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(25 * MS).await;
    }
    eventually("speech to be detected after the recovery", || {
        !chain.rec.vad().is_empty()
    })
    .await;
    let stats = chain.stats.snapshot();
    assert!(stats.saturated >= 1 && stats.retries >= 1, "{stats:?}");
    assert_eq!(stats.rebuilds, 0, "the stream was kept: {stats:?}");
    assert_eq!(stats.synthesized_stops + stats.tee_dropped, 0, "{stats:?}");
    assert!(!stats.failed_open);
    assert_eq!(stats.frames, 13, "frames are counted once: {stats:?}");
    // The refused audio was not lost: speech starts at the very first frame.
    assert_eq!(chain.rec.vad().first(), Some(&(true, origin)));
    drop(chain);
    pool.shutdown();
}

#[tokio::test]
async fn audio_kept_for_a_retry_is_bounded_and_shed_like_a_tee_overflow() {
    let clock = SystemClock;
    let full = FullPool::new().await;
    let setup = VadSetup::new(
        Offload::Pool(full.pool.handle()),
        worker_config(100_000),
        || Ok(EnergyModel::from_pcm_rms(500.0)),
    );
    let chain = start(setup).await;
    let origin = clock.now();
    // Far more than a second of audio while the pool refuses everything.
    for i in 0..120 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(5 * MS).await;
    }
    eventually("the kept audio to be shed", || {
        chain.stats.snapshot().tee_dropped >= 50
    })
    .await;
    let stats = chain.stats.snapshot();
    assert!(stats.retries >= 1 && stats.rebuilds == 0, "{stats:?}");
    assert!(!stats.failed_open);

    // After the drain the stream closes at the hole and goes on.
    let pool = full.release().await;
    for i in 120..140 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(25 * MS).await;
    }
    eventually("a job after the hole", || {
        chain.stats.snapshot().jobs_ok >= 1
    })
    .await;
    assert!(chain.stats.snapshot().gap_resets >= 1);
    drop(chain);
    pool.shutdown();
}

#[tokio::test]
async fn a_job_that_times_out_is_counted_and_the_stream_is_rebuilt() {
    let clock = SystemClock;
    let pool = pool(2, 16);
    let builds = Arc::new(AtomicUsize::new(0));
    let builds_in = Arc::clone(&builds);
    let setup = VadSetup::new(
        Offload::Pool(pool.handle()),
        VadWorkerConfig {
            job_timeout: Duration::from_millis(60),
            ..worker_config(5)
        },
        move || {
            // The first model's first window takes far longer than the job timeout.
            let n = builds_in.fetch_add(1, Ordering::SeqCst);
            Ok(Slow {
                delay: Duration::ZERO,
                first_delay: (n == 0).then_some(Duration::from_millis(400)),
                calls: 0,
            })
        },
    );
    let chain = start(setup).await;
    let origin = clock.now();
    let frame = square_frame(origin, 0, SampleRate::Hz16000, LOUD);
    chain.running.input().send(frame.into()).await.unwrap();
    eventually("the timeout", || chain.stats.snapshot().timeouts >= 1).await;
    for i in 1..8 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(25 * MS).await;
    }
    eventually("a job on the rebuilt stream", || {
        chain.stats.snapshot().jobs_ok >= 1
    })
    .await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.timeouts, 1);
    assert!(stats.rebuilds >= 1 && builds.load(Ordering::SeqCst) >= 2);
    assert!(!stats.failed_open);
    drop(chain);
    pool.shutdown();
}

#[tokio::test]
async fn a_closed_pool_fails_open_without_stopping_the_audio() {
    let clock = SystemClock;
    let pool = pool(1, 4);
    let handle = pool.handle();
    pool.shutdown(); // the handle now refuses jobs: ChannelClosed
    let setup = VadSetup::new(Offload::Pool(handle), worker_config(3), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    });
    let chain = start(setup).await;
    let origin = clock.now();
    for i in 0..10 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, LOUD);
        chain.running.input().send(frame.into()).await.unwrap();
        clock.sleep(25 * MS).await;
    }
    eventually("fail open", || chain.stats.snapshot().failed_open).await;
    assert_eq!(chain.stats.snapshot().other_failures, 3);
    assert_eq!(chain.rec.audio().len(), 10);
    assert!(
        chain
            .rec
            .all()
            .iter()
            .any(|(f, _)| matches!(f, Frame::System(SystemFrame::Error(e)) if !e.fatal))
    );
    // Not a data frame of any other kind slipped in.
    assert!(
        chain
            .rec
            .all()
            .iter()
            .all(|(f, _)| matches!(f, Frame::Data(DataFrame::Audio(_)) | Frame::System(_)))
    );
}

/// A model that notes in `log` that it ran a window.
struct Logged(Arc<std::sync::Mutex<Vec<&'static str>>>);

impl SpeechModel for Logged {
    fn window(&self) -> usize {
        320
    }
    fn context(&self) -> usize {
        0
    }
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn infer(&mut self, _input: &[f32]) -> brisvox_inference::Result<f32> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("vad");
        Ok(0.0)
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn a_vad_batch_runs_before_the_normal_jobs_queued_ahead_of_it() {
    // Smart Turn analyses (about 1.5 ms of features each, 25 ms with the model) queued
    // ahead of the VAD windows of hundreds of calls overflowed the shared queue and lost the
    // streams; the VAD batch goes in the high-priority line.
    let clock = SystemClock;
    let pool = pool(1, 16);
    let handle = pool.handle();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let blocker = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let _ = handle
                .run(Duration::from_secs(20), move || {
                    let _ = release_rx.recv();
                })
                .await;
        })
    };
    clock.sleep(100 * MS).await; // the blocker runs on the only thread
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model_log = Arc::clone(&log);
    let setup = VadSetup::new(Offload::Pool(handle.clone()), worker_config(5), move || {
        Ok(Logged(Arc::clone(&model_log)))
    });
    let chain = start(setup).await;
    // Three normal jobs wait ahead of the VAD batch.
    let mut normals = Vec::new();
    for _ in 0..3 {
        let (handle, log) = (handle.clone(), Arc::clone(&log));
        normals.push(tokio::spawn(async move {
            handle
                .run(Duration::from_secs(20), move || {
                    log.lock().unwrap().push("normal");
                })
                .await
        }));
    }
    eventually("three normal jobs to queue", || handle.queue_depth() == 3).await;
    let frame = square_frame(clock.now(), 0, SampleRate::Hz16000, LOUD);
    chain.running.input().send(frame.into()).await.unwrap();
    eventually("the VAD batch to queue", || handle.queue_depth() == 4).await;
    release_tx.send(()).unwrap();
    for normal in normals {
        normal.await.unwrap().unwrap();
    }
    blocker.await.unwrap();
    let order = log.lock().unwrap().clone();
    assert_eq!(order, ["vad", "normal", "normal", "normal"]);
    drop(chain);
    pool.shutdown();
}

/// Sends loud (`speech`) or silent frames `range` at `gap` spacing.
async fn feed_range(
    chain: &Chain,
    origin: Instant,
    range: std::ops::Range<u64>,
    speech: bool,
    gap: Duration,
) {
    for i in range {
        let frame = square_frame(
            origin,
            i,
            SampleRate::Hz16000,
            if speech { LOUD } else { 0 },
        );
        chain.running.input().send(frame.into()).await.unwrap();
        SystemClock.sleep(gap).await;
    }
}

#[tokio::test]
async fn a_refusal_in_the_middle_of_speech_keeps_the_built_stream_and_the_stop_comes_once() {
    let clock = SystemClock;
    let mut full = FullPool::open(1);
    let setup = VadSetup::new(Offload::Pool(full.pool.handle()), worker_config(50), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    });
    let chain = start(setup).await;
    let origin = clock.now();
    // Speech runs on a stream that is already built.
    feed_range(&chain, origin, 0..10, true, 25 * MS).await;
    eventually("the start of speech", || {
        chain.rec.vad().first().is_some_and(|e| e.0)
    })
    .await;
    let before = chain.stats.snapshot();
    assert_eq!(before.rebuilds, 0, "{before:?}");
    assert!(before.jobs_ok >= 1 && before.saturated == 0, "{before:?}");

    // The pool fills up while the speaker goes on and then falls silent; those batches are refused.
    full.block().await;
    full.block().await;
    feed_range(&chain, origin, 10..16, true, 25 * MS).await;
    feed_range(&chain, origin, 16..22, false, 25 * MS).await;
    eventually("a refusal", || chain.stats.snapshot().saturated >= 1).await;
    assert_eq!(chain.rec.vad().len(), 1, "no stop while nothing ran");

    // The pool drains: the kept stream is told about the silence and closes the turn once, at the
    // time the audio says.
    let pool = full.release().await;
    feed_range(&chain, origin, 22..40, false, 25 * MS).await;
    eventually("the stop", || chain.rec.vad().len() >= 2).await;
    let stats = chain.stats.snapshot();
    assert!(stats.retries >= 1, "{stats:?}");
    assert_eq!(stats.rebuilds, 0, "the stream was kept: {stats:?}");
    assert_eq!(stats.synthesized_stops, 0, "{stats:?}");
    assert_eq!(stats.tee_dropped + stats.gap_resets, 0, "{stats:?}");
    assert!(!stats.failed_open);
    let events = chain.rec.vad();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], (true, origin));
    assert!(!events[1].0);
    // Speech ended at frame 16 (320 ms); the stop is dated within a model window of it.
    let stop = events[1].1 - origin;
    assert!(
        stop >= 280 * MS && stop <= 360 * MS,
        "the stop is dated to the end of the speech: {stop:?}"
    );
    drop(chain);
    pool.shutdown();
}

#[tokio::test]
async fn a_job_that_times_out_in_the_queue_is_retried_on_the_same_stream() {
    let clock = SystemClock;
    // The only thread is blocked; the queue has room, so the job is accepted but never starts.
    let mut full = FullPool::open(4);
    full.block().await;
    let setup = VadSetup::new(
        Offload::Pool(full.pool.handle()),
        VadWorkerConfig {
            job_timeout: Duration::from_millis(60),
            ..worker_config(100_000)
        },
        || Ok(EnergyModel::from_pcm_rms(500.0)),
    );
    let chain = start(setup).await;
    let origin = clock.now();
    feed_range(&chain, origin, 0..4, true, 25 * MS).await;
    eventually("a queue timeout", || chain.stats.snapshot().timeouts >= 1).await;
    let stats = chain.stats.snapshot();
    assert!(
        stats.retries >= 1,
        "the unstarted job is retried: {stats:?}"
    );
    assert_eq!(stats.rebuilds + stats.panics, 0, "{stats:?}");

    let pool = full.release().await;
    feed_range(&chain, origin, 4..14, true, 25 * MS).await;
    eventually("speech after the drain", || !chain.rec.vad().is_empty()).await;
    let stats = chain.stats.snapshot();
    assert_eq!(stats.rebuilds, 0, "the stream was kept: {stats:?}");
    assert_eq!(stats.synthesized_stops + stats.tee_dropped, 0, "{stats:?}");
    // The audio that waited in the queue was not lost: speech starts at the very first frame.
    assert_eq!(chain.rec.vad().first(), Some(&(true, origin)));
    drop(chain);
    pool.shutdown();
}

#[tokio::test]
async fn failing_open_drops_the_audio_kept_for_a_retry() {
    let clock = SystemClock;
    let full = FullPool::new().await;
    // Eight refusals in a row fail the detector open (the retries are 10 ms apart).
    let setup = VadSetup::new(Offload::Pool(full.pool.handle()), worker_config(8), || {
        Ok(EnergyModel::from_pcm_rms(500.0))
    });
    let chain = start(setup).await;
    let origin = clock.now();
    // Twenty frames are kept for the retry before the detector fails open.
    feed_range(&chain, origin, 0..20, true, MS).await;
    eventually("the detector to fail open", || {
        chain.stats.snapshot().failed_open
    })
    .await;
    clock.sleep(150 * MS).await;
    let after_open = chain.stats.snapshot();
    assert_eq!(after_open.jobs_ok, 0, "{after_open:?}");

    // Later audio is drained; the kept batch is not run, and not discarded a second time.
    let pool = full.release().await;
    feed_range(&chain, origin, 20..25, true, 25 * MS).await;
    clock.sleep(150 * MS).await;
    let stats = chain.stats.snapshot();
    assert_eq!(
        stats.jobs_ok, 0,
        "the kept audio was not run late: {stats:?}"
    );
    assert_eq!(
        stats.discarded_failed_open,
        after_open.discarded_failed_open + 5,
        "only the new audio is drained: {stats:?} vs {after_open:?}"
    );
    assert!(chain.rec.vad().is_empty());
    drop(chain);
    pool.shutdown();
}
