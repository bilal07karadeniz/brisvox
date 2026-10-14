//! Early (speculative) Smart Turn analysis: the `TurnDriver` starts an analysis at the
//! VAD's `UserSpeechFalling` hint and answers the machine's request at the confirmed stop with its
//! verdict when the reuse rule holds (`turn_driver` module docs); otherwise the normal analysis
//! runs. Everything on a paused clock, with the model as a recording / gated fake.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#![allow(clippy::disallowed_methods)] // the gated-model tests wait for a real pool thread

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use brisvox_agent::TurnPolicy;
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame,
    TranscriptFrame,
};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SharedClock, SimClock, SystemClock, TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{
    EarlyAnalysisConfig, EngineConfig, Offload, TurnDriver, TurnDriverConfig, TurnDriverStats,
};
use brisvox_inference::smart_turn::{SmartTurn, TurnModel};
use brisvox_inference::{InferencePool, PoolConfig};
use common::{MS, square_frame};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Answers a fixed probability and records the features of every call.
struct RecordingModel {
    probability: f32,
    calls: Mutex<Vec<Vec<f32>>>,
}

impl RecordingModel {
    fn new(probability: f32) -> Arc<Self> {
        Arc::new(Self {
            probability,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Vec<f32>> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl TurnModel for RecordingModel {
    fn predict(&self, features: &[f32]) -> brisvox_inference::Result<f32> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(features.to_vec());
        Ok(self.probability)
    }
}

/// The FIRST call blocks on a gate (and fails if `fail_first`), later calls answer at once.
struct GatedModel {
    gate: Mutex<mpsc::Receiver<()>>,
    calls: AtomicUsize,
    fail_first: bool,
}

impl TurnModel for GatedModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
            if self.fail_first {
                return Err(brisvox_inference::Error::Model(
                    "mock: first call fails".into(),
                ));
            }
        }
        Ok(0.9)
    }
}

/// The features Smart Turn computes for exactly `frames` (what the ring must have handed over).
fn features_of(frames: &[AudioFrame]) -> Vec<f32> {
    let samples: Vec<i16> = frames
        .iter()
        .flat_map(|frame| {
            frame
                .data()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect::<Vec<_>>()
        })
        .collect();
    let recorder = RecordingModel::new(0.5);
    SmartTurn::analyze_snapshot_blocking(recorder.as_ref(), &samples).unwrap();
    recorder.calls().remove(0)
}

/// Records every downstream frame.
struct Tail {
    seen: Arc<Mutex<Vec<Frame>>>,
}

impl Processor for Tail {
    fn name(&self) -> &'static str {
        "tail"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if direction == Direction::Downstream {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(frame);
        }
        Ok(())
    }
}

const ADAPTIVE: TurnPolicy = TurnPolicy::Adaptive {
    fallback_silence: Duration::from_secs(3),
    require_final: false,
    final_grace: Duration::from_millis(800),
};

fn config(early: EarlyAnalysisConfig, policy: TurnPolicy) -> TurnDriverConfig {
    let mut engine = EngineConfig::DEFAULT;
    engine.turn = policy;
    engine.early_analysis = early;
    TurnDriverConfig::from_engine(&engine)
}

struct Rig {
    running: RunningPipeline,
    seen: Arc<Mutex<Vec<Frame>>>,
    stats: TurnDriverStats,
    clock: SimClock,
    origin: Instant,
    frames: Vec<AudioFrame>,
}

impl Rig {
    async fn new(config: TurnDriverConfig, model: Arc<dyn TurnModel>) -> Self {
        Self::with_offload(config, model, None).await
    }

    async fn with_offload(
        config: TurnDriverConfig,
        model: Arc<dyn TurnModel>,
        offload: Option<Offload>,
    ) -> Self {
        let clock = SimClock::new();
        clock.sleep(MS).await;
        let shared: SharedClock = Arc::new(clock);
        let offload = offload.unwrap_or_else(|| Offload::Inline(Arc::clone(&shared)));
        let driver = TurnDriver::new(config, Arc::clone(&shared), TimelineRecorder::new())
            .with_smart_turn(model, offload);
        let stats = driver.stats();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let running = Pipeline::new("early")
            .then(driver)
            .then(Tail {
                seen: Arc::clone(&seen),
            })
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        let rig = Self {
            running,
            seen,
            stats,
            clock,
            origin: clock.now(),
            frames: Vec::new(),
        };
        rig.settle().await;
        rig
    }

    async fn settle(&self) {
        for _ in 0..40 {
            tokio::task::yield_now().await;
        }
    }

    async fn at(&self, ms: u64) {
        brisvox_core::clock::wait_until(&self.clock, self.origin + Duration::from_millis(ms)).await;
        self.settle().await;
    }

    fn t(&self, ms: u64) -> Instant {
        self.origin + Duration::from_millis(ms)
    }

    async fn send(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
        self.settle().await;
    }

    async fn vad_start(&self, at_ms: u64) {
        self.send(SystemFrame::UserStartedSpeaking { at: self.t(at_ms) }.into())
            .await;
    }

    async fn vad_stop(&self, at_ms: u64) {
        self.send(SystemFrame::UserStoppedSpeaking { at: self.t(at_ms) }.into())
            .await;
    }

    async fn falling(&self, at_ms: u64) {
        self.send(SystemFrame::UserSpeechFalling { at: self.t(at_ms) }.into())
            .await;
    }

    /// Audio frames `first..=last` (20 ms each, stamped at `20 * index` ms), each with its own
    /// content so the features show exactly which frames the ring held.
    async fn audio(&mut self, first: u64, last: u64) {
        for i in first..=last {
            let frame = square_frame(
                self.origin,
                i,
                SampleRate::Hz16000,
                400 + 250 * i16::try_from(i % 10).unwrap(),
            );
            self.frames.push(frame.clone());
            self.send(frame.into()).await;
        }
    }

    /// The frames sent so far with indices in `first..=last`.
    fn sent(&self, first: u64, last: u64) -> Vec<AudioFrame> {
        self.frames[usize::try_from(first).unwrap()..=usize::try_from(last).unwrap()].to_vec()
    }

    async fn final_of(&self, text: &str) {
        self.send(
            DataFrame::Transcript(TranscriptFrame {
                epoch: TurnEpoch::ZERO,
                text: Arc::from(text),
                is_final: true,
                received_at: self.clock.now(),
            })
            .into(),
        )
        .await;
        self.send(
            ControlFrame {
                epoch: TurnEpoch::ZERO,
                kind: ControlKind::UtteranceFinal,
            }
            .into(),
        )
        .await;
    }

    /// Lets the pool thread run (real time) until `done`, at most 20 s.
    async fn until(&self, done: impl Fn() -> bool) {
        for _ in 0..2_000 {
            self.settle().await;
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the condition was not reached in 20 s");
    }

    fn turn_ended(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| {
                matches!(
                    frame,
                    Frame::Control(ControlFrame {
                        kind: ControlKind::UserTurnEnded,
                        ..
                    })
                )
            })
            .count()
    }

    /// Whether any downstream frame is a `UserSpeechFalling` (the driver consumes the hint).
    fn forwarded_hint(&self) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|frame| matches!(frame, Frame::System(SystemFrame::UserSpeechFalling { .. })))
    }
}

// ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_ready_early_verdict_answers_the_stop_without_another_analysis() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await; // frames stamped 0..620 ms
    rig.at(620).await;
    // The silence began at 580 ms; with a 30 ms pad the stop will be dated 610.
    rig.falling(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_hints, stats.early_started),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(model.calls().len(), 1);
    assert_eq!(
        model.calls()[0],
        features_of(&rig.sent(0, 31)),
        "the snapshot holds the audio up to the hint"
    );
    rig.audio(32, 36).await; // 100 ms of silence after the snapshot
    rig.at(700).await;
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_reused, stats.early_ready, stats.early_wasted),
        (1, 1, 0),
        "{stats:?}"
    );
    assert_eq!(
        stats.analyses_started, 0,
        "no analysis of its own: {stats:?}"
    );
    assert_eq!(stats.analyses_ok, 1);
    assert_eq!(
        model.calls().len(),
        1,
        "the model ran once for the whole pause"
    );
    assert_eq!(
        (stats.verdicts_timed, stats.verdict_latency_us),
        (1, 0),
        "the verdict was there when the stop arrived: {stats:?}"
    );
    // The machine took the complete verdict: the turn ends when the final arrives.
    assert_eq!(rig.turn_ended(), 0, "held for the final");
    rig.final_of("hello there").await;
    assert_eq!(rig.turn_ended(), 1);
    assert!(!rig.forwarded_hint(), "the hint is consumed by the driver");
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_early_verdict_keeps_the_pause_open_like_a_normal_one() {
    let model = RecordingModel::new(0.1);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.audio(32, 34).await;
    rig.vad_stop(610).await;
    rig.final_of("and then").await;
    rig.at(2000).await;
    assert_eq!(
        rig.turn_ended(),
        0,
        "incomplete: the fallback silence decides"
    );
    assert_eq!(rig.stats.snapshot().early_reused, 1);
    rig.at(3700).await;
    assert_eq!(rig.turn_ended(), 1, "3 s after the stop");
    assert_eq!(model.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_newer_hint_replaces_the_early_analysis_and_the_stop_uses_the_last_one() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(310).await; // a dip the VAD then cancelled (no event for that)
    rig.audio(21, 40).await;
    rig.falling(710).await; // the real fall
    rig.audio(41, 44).await;
    rig.vad_stop(710).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_wasted, stats.early_reused),
        (2, 1, 1),
        "{stats:?}"
    );
    assert_eq!(stats.analyses_started, 0);
    assert_eq!(model.calls().len(), 2);
    assert_eq!(model.calls()[1], features_of(&rig.sent(0, 40)));
}

#[tokio::test(start_paused = true)]
async fn a_stop_of_a_later_pause_does_not_reuse_an_earlier_hint() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(310).await; // cancelled by speech: the VAD sends nothing
    rig.audio(21, 51).await; // speech goes on, silence only from 1000 ms
    rig.at(1100).await;
    rig.vad_stop(1010).await; // the stop of a pause that had no hint of its own
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_reused, 0, "{stats:?}");
    assert_eq!(stats.early_wasted, 1, "{stats:?}");
    assert_eq!(
        stats.analyses_started, 1,
        "the normal analysis ran: {stats:?}"
    );
    assert_eq!(model.calls().len(), 2);
    assert_eq!(
        model.calls()[1],
        features_of(&rig.sent(0, 51)),
        "on the ring as it is now"
    );
}

#[tokio::test(start_paused = true)]
async fn a_speech_start_drops_a_live_early_analysis() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    assert_eq!(rig.stats.snapshot().early_started, 1);
    // The VAD lost a batch around the pause (a synthesized stop and a new start): the old
    // fall is over, whatever the stop's time says.
    rig.vad_start(700).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_wasted),
        (1, 1),
        "{stats:?}"
    );
    rig.audio(32, 40).await;
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_reused, 0, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn more_audio_than_max_tail_since_the_snapshot_is_not_reused() {
    let model = RecordingModel::new(0.9);
    let mut early = EarlyAnalysisConfig::ENABLED;
    early.max_tail = Duration::from_millis(100);
    let mut rig = Rig::new(config(early, ADAPTIVE), model.clone()).await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.audio(32, 39).await; // 160 ms after the snapshot
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.early_reused,
            stats.early_wasted,
            stats.analyses_started
        ),
        (0, 1, 1),
        "{stats:?}"
    );
    assert_eq!(model.calls().len(), 2);
    // Exactly at the bound it is still reused.
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(config(early, ADAPTIVE), model.clone()).await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.audio(32, 36).await; // 100 ms
    rig.vad_stop(610).await;
    assert_eq!(rig.stats.snapshot().early_reused, 1);
    assert_eq!(model.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_per_turn_budget_bounds_the_extra_analyses() {
    let model = RecordingModel::new(0.9);
    let mut early = EarlyAnalysisConfig::ENABLED;
    early.max_per_turn = 2;
    let mut rig = Rig::new(config(early, ADAPTIVE), model.clone()).await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(310).await; // 1: dips the VAD then forgave
    rig.audio(21, 40).await;
    rig.falling(710).await; // 2
    rig.audio(41, 60).await;
    rig.falling(1210).await; // over the budget: ignored (and the older one is stale anyway)
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_hints, stats.early_started, stats.early_capped),
        (3, 2, 1),
        "{stats:?}"
    );
    assert_eq!(
        stats.early_wasted, 2,
        "both earlier ones were superseded: {stats:?}"
    );
    rig.audio(61, 64).await;
    rig.vad_stop(1210).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_reused, 0, "{stats:?}");
    assert_eq!(
        stats.analyses_started, 1,
        "the normal analysis ran: {stats:?}"
    );
    assert_eq!(model.calls().len(), 3);
}

/// The budget is per user turn: a new turn (new epoch) starts with a full one. Without the reset
/// the first turn that spends its budget would switch the feature off for the rest of the call.
#[tokio::test(start_paused = true)]
async fn the_per_turn_budget_is_renewed_for_the_next_turn() {
    let model = RecordingModel::new(0.9);
    let mut early = EarlyAnalysisConfig::ENABLED;
    early.max_per_turn = 1;
    let mut rig = Rig::new(config(early, ADAPTIVE), model.clone()).await;
    // Turn 1 spends its whole budget on the hint of its real fall, which the stop reuses.
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.at(620).await;
    rig.falling(610).await;
    rig.audio(32, 34).await;
    rig.vad_stop(610).await;
    rig.final_of("first turn").await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_reused, stats.early_capped),
        (1, 1, 0),
        "{stats:?}"
    );
    assert_eq!(rig.turn_ended(), 1, "turn 1 is over");
    // Turn 2 (a new epoch): the first hint starts again.
    rig.at(1000).await;
    rig.vad_start(1000).await;
    rig.audio(50, 81).await; // frames stamped 1000..1620 ms
    rig.at(1620).await;
    rig.falling(1610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_hints, stats.early_started, stats.early_capped),
        (2, 2, 0),
        "turn 2's first hint must not be capped by turn 1's spending: {stats:?}"
    );
    // And the cap still holds inside turn 2.
    rig.audio(82, 91).await;
    rig.falling(1800).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_hints, stats.early_started, stats.early_capped),
        (3, 2, 1),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_hint_waits_for_the_ring_to_reach_the_silence_start() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await; // the ring holds up to 420 ms
    rig.falling(610).await; // the system lane overtook the audio lane
    assert_eq!(model.calls().len(), 0, "waiting for audio");
    assert_eq!(rig.stats.snapshot().early_started, 0);
    rig.audio(21, 29).await;
    assert_eq!(model.calls().len(), 0, "frame 29 starts at 580 ms: not yet");
    rig.audio(30, 30).await; // stamped 600 ms, but the hint is dated 610
    assert_eq!(model.calls().len(), 0);
    rig.audio(31, 31).await; // stamped 620 ms >= 610
    assert_eq!(model.calls().len(), 1);
    assert_eq!(model.calls()[0], features_of(&rig.sent(0, 31)));
    assert_eq!(rig.stats.snapshot().early_started, 1);
}

#[tokio::test(start_paused = true)]
async fn a_hint_with_no_audio_coming_starts_at_the_defer_bound() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.at(430).await;
    rig.falling(610).await;
    assert_eq!(model.calls().len(), 0);
    rig.at(430 + 150).await;
    assert_eq!(model.calls().len(), 0, "still inside the bound");
    rig.at(430 + 210).await;
    assert_eq!(
        model.calls().len(),
        1,
        "started at the bound with the audio there is"
    );
    assert_eq!(model.calls()[0], features_of(&rig.sent(0, 20)));
    // A stop that arrives while the early job waits for audio is not an early reuse.
    let model2 = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model2.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(610).await;
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_reused),
        (0, 0),
        "{stats:?}"
    );
    assert_eq!(
        stats.analyses_deferred, 1,
        "the normal path defers on its own: {stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn off_by_default_a_hint_does_nothing() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(config(EarlyAnalysisConfig::OFF, ADAPTIVE), model.clone()).await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.early_hints,
            stats.early_started,
            stats.early_reused,
            stats.early_wasted
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(stats.analyses_started, 1, "the normal analysis at the stop");
    assert_eq!(model.calls().len(), 1);
    assert!(!rig.forwarded_hint());
}

#[tokio::test(start_paused = true)]
async fn no_early_analysis_outside_an_open_adaptive_turn() {
    // Fixed silence: the machine never asks for an analysis.
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(
            EarlyAnalysisConfig::ENABLED,
            TurnPolicy::fixed(Duration::from_millis(700)),
        ),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    assert_eq!(model.calls().len(), 0);
    assert_eq!(rig.stats.snapshot().early_started, 0);
    // Adaptive, but no turn is open (the hint of a user who never started speaking).
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    assert_eq!(model.calls().len(), 0);
    assert_eq!(rig.stats.snapshot().early_started, 0);
    assert_eq!(rig.stats.snapshot().early_hints, 1);
}

#[tokio::test(start_paused = true)]
async fn a_running_early_job_is_adopted_by_the_stop_instead_of_starting_a_second_one() {
    let (release, gate) = mpsc::channel::<()>();
    let model = Arc::new(GatedModel {
        gate: Mutex::new(gate),
        calls: AtomicUsize::new(0),
        fail_first: false,
    });
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 4,
            name: "early-adopt".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let mut rig = Rig::with_offload(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
        Some(Offload::Pool(pool.handle())),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.until(|| model.calls.load(Ordering::SeqCst) == 1).await; // blocked in the model
    rig.audio(32, 34).await;
    rig.vad_stop(610).await;
    rig.final_of("waiting for the model").await;
    assert_eq!(rig.turn_ended(), 0, "the verdict is still being computed");
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.analyses_started),
        (1, 0),
        "{stats:?}"
    );
    // The job ends: its verdict answers the machine's request and the turn ends.
    release.send(()).unwrap();
    rig.until(|| rig.stats.snapshot().early_reused == 1).await;
    rig.settle().await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_reused, stats.early_ready, stats.analyses_ok),
        (1, 0, 1),
        "{stats:?}"
    );
    assert_eq!(stats.verdicts_timed, 1);
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "one model call for the pause"
    );
    assert_eq!(rig.turn_ended(), 1);
    drop(rig);
    pool.shutdown();
}

#[tokio::test(start_paused = true)]
async fn a_failed_early_job_falls_back_to_the_normal_analysis() {
    // The first model call (the early one) fails after the stop adopted it.
    let (release, gate) = mpsc::channel::<()>();
    let model = Arc::new(GatedModel {
        gate: Mutex::new(gate),
        calls: AtomicUsize::new(0),
        fail_first: true,
    });
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 4,
            name: "early-fail".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let mut rig = Rig::with_offload(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
        Some(Offload::Pool(pool.handle())),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.until(|| model.calls.load(Ordering::SeqCst) == 1).await;
    rig.audio(32, 34).await;
    rig.vad_stop(610).await;
    release.send(()).unwrap();
    // The early job fails; the driver then starts the normal analysis, which needs the pool
    // thread again.
    rig.until(|| rig.stats.snapshot().analyses_ok == 1).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_failed, 1, "{stats:?}");
    assert_eq!(stats.early_reused, 0);
    assert_eq!(
        stats.analyses_started, 1,
        "the normal analysis took over: {stats:?}"
    );
    assert_eq!(stats.analyses_ok, 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    rig.final_of("fallback").await;
    assert_eq!(rig.turn_ended(), 1, "the turn still ended on a verdict");
    drop(rig);
    pool.shutdown();
}

#[tokio::test(start_paused = true)]
async fn cancel_drops_a_pending_early_analysis() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(610).await; // waiting for audio
    rig.send(SystemFrame::Cancel.into()).await;
    rig.at(1000).await;
    assert_eq!(
        model.calls().len(),
        0,
        "the cancelled early analysis never ran"
    );
}

#[tokio::test(start_paused = true)]
async fn hangup_drops_a_pending_early_analysis() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 20).await;
    rig.falling(610).await; // waiting for audio
    rig.send(SystemFrame::Stop.into()).await;
    rig.at(1000).await;
    assert_eq!(model.calls().len(), 0, "the early analysis never ran");
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_started, 0, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn hangup_counts_a_finished_early_analysis_as_wasted() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.at(620).await;
    rig.falling(610).await;
    assert_eq!(rig.stats.snapshot().early_started, 1);
    rig.send(SystemFrame::Stop.into()).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.early_wasted, 1, "{stats:?}");
    assert_eq!(
        stats.early_started,
        stats.early_reused + stats.early_wasted + stats.early_failed,
        "every started analysis is accounted for: {stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn hangup_discards_a_running_early_job_and_its_late_verdict_changes_nothing() {
    let (release, gate) = mpsc::channel::<()>();
    let model = Arc::new(GatedModel {
        gate: Mutex::new(gate),
        calls: AtomicUsize::new(0),
        fail_first: false,
    });
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 4,
            name: "early-hangup".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let mut rig = Rig::with_offload(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
        Some(Offload::Pool(pool.handle())),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.until(|| model.calls.load(Ordering::SeqCst) == 1).await; // blocked in the model
    rig.send(SystemFrame::Stop.into()).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_wasted),
        (1, 1),
        "the running job was counted as wasted at the hangup: {stats:?}"
    );
    // The job ends after the hangup: its verdict is dropped.
    release.send(()).unwrap();
    rig.settle().await;
    std::thread::sleep(Duration::from_millis(50));
    rig.settle().await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.early_reused,
            stats.early_failed,
            stats.analyses_started
        ),
        (0, 0, 0),
        "{stats:?}"
    );
    assert_eq!(
        stats.early_started,
        stats.early_reused + stats.early_wasted + stats.early_failed,
        "{stats:?}"
    );
    assert_eq!(rig.turn_ended(), 0);
    drop(rig);
    pool.shutdown();
}

/// The turn ends on the fallback silence while the stop's adopted early job is still running:
/// the end of the turn drops it (counted as wasted), and its late verdict is ignored.
#[tokio::test(start_paused = true)]
async fn the_end_of_the_turn_discards_a_still_running_early_job() {
    let (release, gate) = mpsc::channel::<()>();
    let model = Arc::new(GatedModel {
        gate: Mutex::new(gate),
        calls: AtomicUsize::new(0),
        fail_first: false,
    });
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 4,
            name: "early-release".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    // A fallback silence shorter than the 2 s analysis job timeout, so the turn ends first.
    let quick = TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(1),
        require_final: false,
        final_grace: Duration::from_millis(800),
    };
    let mut rig = Rig::with_offload(
        config(EarlyAnalysisConfig::ENABLED, quick),
        model.clone(),
        Some(Offload::Pool(pool.handle())),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.falling(610).await;
    rig.until(|| model.calls.load(Ordering::SeqCst) == 1).await; // blocked in the model
    rig.audio(32, 34).await;
    rig.vad_stop(610).await; // adopts the running job
    rig.final_of("no verdict yet").await;
    assert_eq!(rig.turn_ended(), 0);
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_started, stats.early_wasted),
        (1, 0),
        "{stats:?}"
    );
    rig.at(1700).await; // the 1 s fallback silence after the stop
    assert_eq!(rig.turn_ended(), 1, "the fallback ended the turn");
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_wasted, stats.early_reused),
        (1, 0),
        "the end of the turn dropped the running job: {stats:?}"
    );
    release.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    rig.settle().await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.early_wasted, stats.early_reused, stats.early_failed),
        (1, 0, 0),
        "the late verdict changed nothing: {stats:?}"
    );
    assert_eq!(
        stats.early_started,
        stats.early_reused + stats.early_wasted + stats.early_failed,
        "{stats:?}"
    );
    drop(rig);
    pool.shutdown();
}

#[tokio::test(start_paused = true)]
async fn a_band_level_window_between_the_hint_and_the_stop_still_reuses() {
    // The VAD's silence is "no window at or above the threshold": audio of band level (here
    // frames with a clear signal, as the VAD's hysteresis band would pass) after the snapshot
    // neither cancels the pending stop nor changes its time, so the stop carries the hint's
    // time and the early verdict is reused. This pins the documented exception to
    // "silence-only since the snapshot": the driver trusts the VAD's dating.
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.at(620).await;
    rig.falling(610).await;
    rig.audio(32, 38).await; // 140 ms of audio after the snapshot, within max_tail
    rig.at(760).await;
    rig.vad_stop(610).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.early_reused,
            stats.early_wasted,
            stats.analyses_started
        ),
        (1, 0, 0),
        "{stats:?}"
    );
    assert_eq!(model.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancel_counts_a_started_early_analysis_as_wasted() {
    let model = RecordingModel::new(0.9);
    let mut rig = Rig::new(
        config(EarlyAnalysisConfig::ENABLED, ADAPTIVE),
        model.clone(),
    )
    .await;
    rig.vad_start(0).await;
    rig.audio(0, 31).await;
    rig.at(620).await;
    rig.falling(610).await;
    assert_eq!(rig.stats.snapshot().early_started, 1);
    rig.send(SystemFrame::Cancel.into()).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        stats.early_started,
        stats.early_reused + stats.early_wasted + stats.early_failed,
        "every started analysis is accounted for: {stats:?}"
    );
    assert_eq!(stats.early_wasted, 1, "{stats:?}");
}
