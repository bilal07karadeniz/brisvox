//! `SttStage`: connect and reconnect, the `Finalize` after every forwarded VAD stop,
//! exactly one `UtteranceFinal` per `Finalize` through provider faults, and the guarantee that a
//! `Finalize` never overtakes the speech it finalizes. The provider is the simulator's `MockStt`
//! with its fault injection, plus a small hand-driven provider where a test needs to see the
//! order of the provider's inputs.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{ControlKind, DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, QueueConfig,
    RunningPipeline, SharedClock, SimClock,
};
use brisvox_engine::{SttStage, SttStageConfig, SttStats, SttStatsSnapshot};
use brisvox_providers::{SttConfig, SttEvent, SttInput, SttProvider, SttSession};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_stt::{MockStt, SttUtterance};
use common::{MS, Recorded, Recorder, square_frame};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const SECOND: Duration = Duration::from_secs(1);

fn label(frame: &Frame) -> String {
    match frame {
        Frame::Data(DataFrame::Audio(_)) => "audio".to_owned(),
        Frame::Data(DataFrame::Transcript(t)) => {
            let kind = if t.is_final { "final" } else { "interim" };
            format!("{kind}:{}", t.text)
        }
        Frame::Control(c) if c.kind == ControlKind::UtteranceFinal => "utterance_final".to_owned(),
        Frame::System(SystemFrame::UserStoppedSpeaking { .. }) => "stop".to_owned(),
        Frame::System(SystemFrame::UserStartedSpeaking { .. }) => "speech_start".to_owned(),
        other => other.name().to_owned(),
    }
}

/// A stage in front of a recording tail.
struct Rig {
    running: RunningPipeline,
    rec: Recorded,
    clock: SimClock,
    origin: Instant,
    stats: SttStats,
    next_frame: u64,
    /// The end of the last speech sent.
    speech_end: Instant,
}

impl Rig {
    async fn new<P: SttProvider>(provider: Arc<P>, config: SttStageConfig) -> Self {
        let clock = SimClock::new();
        let shared: SharedClock = Arc::new(clock);
        let stage = SttStage::new(provider, config, shared);
        let stats = stage.stats();
        let rec = Recorded::default();
        let running = Pipeline::new("stt")
            .then_with_queue(
                stage,
                QueueConfig {
                    data_capacity: 16,
                    ..QueueConfig::DEFAULT
                },
            )
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        Self {
            running,
            rec,
            clock,
            origin: clock.now(),
            stats,
            next_frame: 0,
            speech_end: clock.now(),
        }
    }

    async fn frames(&mut self, n: u64, amplitude: i16) {
        for _ in 0..n {
            let frame = square_frame(self.origin, self.next_frame, SampleRate::Hz16000, amplitude);
            self.next_frame += 1;
            self.running.input().send(frame.into()).await.unwrap();
            self.clock.sleep(20 * MS).await;
        }
    }

    fn frame_time(&self, index: u64) -> Instant {
        self.origin + 20 * MS * u32::try_from(index).unwrap()
    }

    /// `n` frames of 20 ms speech at a 20 ms cadence.
    async fn audio(&mut self, n: u64) {
        self.frames(n, 3_000).await;
        self.speech_end = self.frame_time(self.next_frame);
    }

    /// `n` frames of 20 ms silence.
    async fn silence(&mut self, n: u64) {
        self.frames(n, 0).await;
    }

    /// A VAD stop dated to the end of the speech sent so far. A real VAD reports it only after
    /// `min_silence` of silence, so audio past the stop time always exists: call `silence` first.
    async fn stop(&self) {
        self.running
            .input()
            .send(
                SystemFrame::UserStoppedSpeaking {
                    at: self.speech_end,
                }
                .into(),
            )
            .await
            .unwrap();
    }

    async fn speech_start(&self) {
        self.running
            .input()
            .send(
                SystemFrame::UserStartedSpeaking {
                    at: self.clock.now(),
                }
                .into(),
            )
            .await
            .unwrap();
    }

    /// One utterance: `n` frames of speech, 100 ms of silence, then the stop.
    async fn utterance(&mut self, n: u64) {
        self.speech_start().await;
        self.audio(n).await;
        self.silence(5).await;
        self.stop().await;
    }

    fn labels(&self) -> Vec<String> {
        self.rec.all().iter().map(|(f, _)| label(f)).collect()
    }

    fn count(&self, what: &str) -> usize {
        self.labels().iter().filter(|l| *l == what).count()
    }
}

fn mock() -> MockStt {
    // SimClock is a thin handle on tokio's paused time: every instance reads the same clock.
    let stt = MockStt::new(Arc::new(SimClock::new()));
    stt.clone().with_latency(
        LatencyProfile::Fixed(50 * MS),
        LatencyProfile::Fixed(100 * MS),
        LatencyProfile::Fixed(150 * MS),
        0,
    )
}

fn config() -> SttStageConfig {
    SttStageConfig::default()
}

#[tokio::test(start_paused = true)]
async fn the_stop_is_forwarded_first_then_finalized_and_answered_once() {
    let stt = mock();
    stt.push(SttUtterance::words("book a table for two"));
    let mut rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(SECOND).await; // the session is prewarmed at Start
    rig.utterance(10).await;
    rig.clock.sleep(SECOND).await;

    let labels = rig.labels();
    let position = |what: &str| labels.iter().position(|l| l == what).unwrap();
    let last_audio = labels.iter().rposition(|l| l == "audio").unwrap();
    assert_eq!(rig.count("audio"), 15, "all audio is forwarded: {labels:?}");
    assert!(last_audio < position("stop"), "{labels:?}");
    assert!(
        position("stop") < position("final:book a table for two"),
        "{labels:?}"
    );
    assert!(
        position("final:book a table for two") < position("utterance_final"),
        "{labels:?}"
    );
    assert_eq!(rig.count("utterance_final"), 1, "{labels:?}");
    assert_eq!(stt.finalizes(), 1);
    assert_eq!(
        stt.audio_bytes(),
        15 * 640,
        "every frame reached the provider"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(stats.finals_real, 1, "{stats:?}");
    assert_eq!(stats.finals_synthetic, 0, "{stats:?}");
    assert_eq!(stats.connects, 1, "{stats:?}");
    assert_eq!(
        stats.audio_dropped_queue + stats.audio_dropped_down,
        0,
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_audio_sent_to_the_provider_is_counted_in_milliseconds_silence_included() {
    let mut rig = Rig::new(Arc::new(mock()), config()).await;
    rig.clock.sleep(100 * MS).await;
    // 10 frames of speech and 5 of silence, 20 ms each: the stage forwards all of it. The
    // `Finalize` of the stop is not audio.
    rig.utterance(10).await;
    rig.clock.sleep(500 * MS).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.audio_in, 15, "{stats:?}");
    assert_eq!(stats.audio_sent, 15, "{stats:?}");
    assert_eq!(stats.audio_ms_sent, 300, "{stats:?}");
    assert_eq!(stats.finalizes_sent, 1, "{stats:?}");

    // A second utterance adds to the call's total.
    rig.utterance(5).await;
    rig.clock.sleep(500 * MS).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.audio_ms_sent, 300 + 200, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn the_session_connects_at_start_before_any_audio() {
    let stt = mock();
    let rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(100 * MS).await;
    assert_eq!(stt.connects(), 1, "connected without any audio");
    assert_eq!(rig.stats.snapshot().connects, 1);
}

/// What each case does to the sessions the stage opens, in order (one entry per `connect`).
fn fault_cases() -> Vec<(&'static str, Vec<Vec<StreamFault>>)> {
    vec![
        ("clean", vec![]),
        (
            "connect rejected once",
            vec![vec![StreamFault::ConnectError { status: 503 }]],
        ),
        (
            "connect rejected twice",
            vec![
                vec![StreamFault::ConnectError { status: 503 }],
                vec![StreamFault::ConnectError { status: 503 }],
            ],
        ),
        (
            "connect stalls past the stage timeout",
            vec![vec![StreamFault::ConnectStall {
                duration: Duration::from_secs(30),
            }]],
        ),
        (
            "disconnect before the first event",
            vec![vec![StreamFault::Disconnect { after_events: 0 }]],
        ),
        (
            "disconnect after the first event",
            vec![vec![StreamFault::Disconnect { after_events: 1 }]],
        ),
        (
            "protocol error before the first event",
            vec![vec![StreamFault::ProtocolError { after_events: 0 }]],
        ),
        (
            "stall past the finalize timeout",
            vec![vec![StreamFault::Stall {
                before_event: 0,
                duration: Duration::from_secs(20),
            }]],
        ),
        (
            "several faulty sessions in a row",
            vec![
                vec![StreamFault::Disconnect { after_events: 0 }],
                vec![StreamFault::ConnectError { status: 500 }],
                vec![StreamFault::Stall {
                    before_event: 0,
                    duration: Duration::from_secs(20),
                }],
                vec![StreamFault::ProtocolError { after_events: 1 }],
            ],
        ),
    ]
}

#[tokio::test(start_paused = true)]
async fn every_stop_gets_exactly_one_final_whatever_the_provider_does() {
    for (name, sessions) in fault_cases() {
        let stt = mock();
        for faults in sessions {
            stt.push_session_faults(faults);
        }
        for _ in 0..4 {
            stt.push(SttUtterance::words("one two three"));
        }
        let mut rig = Rig::new(Arc::new(stt.clone()), config()).await;
        // Four utterances, far enough apart for a faulty session to be replaced in between.
        for _ in 0..4 {
            rig.utterance(10).await;
            rig.clock.sleep(Duration::from_secs(8)).await;
        }
        rig.clock.sleep(Duration::from_secs(30)).await;
        let stats = rig.stats.snapshot();
        assert_eq!(rig.count("stop"), 4, "{name}");
        assert_eq!(
            rig.count("utterance_final"),
            4,
            "{name}: one final per stop, however the provider behaved: {:?} {stats:?}",
            rig.labels()
        );
        assert_eq!(
            stats.finals_real + stats.finals_synthetic,
            4,
            "{name}: {stats:?}"
        );
        assert_eq!(stats.finalizes_requested, 4, "{name}: {stats:?}");
        if name == "clean" {
            assert_eq!(stats.finals_synthetic, 0, "{stats:?}");
        } else {
            assert!(
                stats.finals_synthetic >= 1,
                "{name}: a fault must show: {stats:?}"
            );
        }
        assert!(
            stats.finals_real >= 1,
            "{name}: the stage recovers and answers for real again: {stats:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_dead_provider_answers_at_once_and_the_turn_never_waits() {
    let stt = mock();
    // More refused connects than the reconnect budget: the stage degrades.
    for _ in 0..5 {
        stt.push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
    }
    let mut rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(Duration::from_secs(10)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        stats.connect_failures, 4,
        "initial attempt plus 3 retries: {stats:?}"
    );
    assert_eq!(stats.degraded, 1, "{stats:?}");
    assert_eq!(stt.connects(), 4);

    // No session: the audio is dropped (counted) but still forwarded, and the stop is answered
    // within the same instant by a synthetic final.
    rig.audio(5).await;
    rig.silence(2).await;
    rig.stop().await;
    rig.clock.sleep(MS).await;
    assert_eq!(rig.count("audio"), 7);
    assert_eq!(rig.count("utterance_final"), 1, "{:?}", rig.labels());
    let stats = rig.stats.snapshot();
    assert_eq!(stats.finals_synthetic, 1, "{stats:?}");
    assert_eq!(stats.audio_dropped_down, 7, "{stats:?}");
    assert_eq!(
        stt.connects(),
        4,
        "degraded: no further attempt without a new turn"
    );
}

#[tokio::test(start_paused = true)]
async fn the_next_turn_grants_a_degraded_stage_a_fresh_budget() {
    let stt = mock();
    for _ in 0..4 {
        stt.push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
    }
    stt.push(SttUtterance::words("hello again"));
    stt.push(SttUtterance::words("and once more"));
    let mut rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(Duration::from_secs(10)).await;
    assert_eq!(rig.stats.snapshot().degraded, 1);

    // The user starts a new turn: the stage connects (the fifth connect has no fault).
    rig.utterance(10).await;
    rig.clock.sleep(SECOND).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.connects, 1, "{stats:?}");
    assert_eq!(stt.connects(), 5);
    // Audio sent before the session was up was lost; the stop of this utterance may have been
    // answered synthetically, but a following utterance is answered for real.
    rig.utterance(10).await;
    rig.clock.sleep(SECOND).await;
    assert_eq!(rig.count("utterance_final"), 2, "{:?}", rig.labels());
    assert!(rig.stats.snapshot().finals_real >= 1);
}

#[tokio::test(start_paused = true)]
async fn the_connect_has_an_explicit_timeout_and_a_backoff_before_the_retry() {
    let stt = mock();
    stt.push_session_faults(vec![StreamFault::ConnectStall {
        duration: Duration::from_secs(60),
    }]);
    let rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(4_900 * MS).await;
    assert_eq!(
        rig.stats.snapshot().connect_failures,
        0,
        "still within the 5 s timeout"
    );
    rig.clock.sleep(200 * MS).await;
    assert_eq!(rig.stats.snapshot().connect_failures, 1, "timed out at 5 s");
    // 1 s backoff, 50 ms connect.
    rig.clock.sleep(SECOND).await;
    assert_eq!(rig.stats.snapshot().connects, 1);
    assert_eq!(stt.connects(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_late_answer_after_the_timeout_is_never_counted_twice() {
    let stt = mock();
    stt.push(SttUtterance::words("this answer arrives far too late"));
    // The session holds the answer back for 8 s: beyond the 3 s finalize timeout.
    stt.push_session_faults(vec![StreamFault::Stall {
        before_event: 0,
        duration: Duration::from_secs(8),
    }]);
    let mut rig = Rig::new(Arc::new(stt.clone()), config()).await;
    rig.clock.sleep(SECOND).await;
    rig.utterance(10).await;
    rig.clock.sleep(Duration::from_millis(3_200)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        rig.count("utterance_final"),
        1,
        "synthetic answer at the timeout"
    );
    assert_eq!(stats.finalize_timeouts, 1, "{stats:?}");
    assert_eq!(stats.finals_synthetic, 1, "{stats:?}");

    // Long after the provider would have delivered: nothing more arrives, nothing is counted
    // twice, and the late text never reaches the pipeline.
    rig.clock.sleep(Duration::from_secs(30)).await;
    let labels = rig.labels();
    assert_eq!(rig.count("utterance_final"), 1, "{labels:?}");
    assert!(
        !labels.iter().any(|l| l.contains("too late")),
        "late text must not be forwarded: {labels:?}"
    );
    assert_eq!(rig.stats.snapshot().finals_real, 0);
    // The stage went on: the session was replaced.
    assert!(
        rig.stats.snapshot().connects >= 2,
        "{:?}",
        rig.stats.snapshot()
    );
}

#[tokio::test(start_paused = true)]
async fn same_input_twice_gives_identical_frames() {
    async fn run() -> Vec<String> {
        let stt = mock();
        stt.push(SttUtterance::words("book a table for two"));
        stt.push(SttUtterance::words("thanks"));
        stt.push_session_faults(vec![]);
        let mut rig = Rig::new(Arc::new(stt), config()).await;
        rig.clock.sleep(SECOND).await;
        rig.utterance(12).await;
        rig.clock.sleep(SECOND).await;
        rig.utterance(6).await;
        rig.clock.sleep(SECOND).await;
        rig.labels()
    }
    let first = run().await;
    let second = run().await;
    assert_eq!(first, second);
    assert!(first.iter().filter(|l| *l == "utterance_final").count() == 2);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_pipeline_ends_the_session_task() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    {
        let stt = mock();
        stt.push(SttUtterance::words("hello"));
        let mut rig = Rig::new(Arc::new(stt), config()).await;
        rig.utterance(10).await;
        rig.clock.sleep(SECOND).await;
    }
    counter
        .wait_for_baseline(&clock, SECOND)
        .await
        .expect("the stt session task (or the provider session) outlived the pipeline");
}

// ---------------------------------------------------------------------------------------------
// A hand-driven provider: the test sees the provider's inputs in order and writes its events.

#[derive(Debug, PartialEq, Eq)]
enum Op {
    Audio,
    Finalize,
}

struct ManualSession {
    inputs: mpsc::Receiver<SttInput>,
    events: mpsc::Sender<SttEvent>,
}

#[derive(Clone)]
struct ManualStt {
    sessions: Arc<Mutex<Vec<ManualSession>>>,
    input_capacity: usize,
    connects: Arc<AtomicUsize>,
}

impl ManualStt {
    fn new(input_capacity: usize) -> Self {
        Self {
            sessions: Arc::default(),
            input_capacity,
            connects: Arc::default(),
        }
    }

    /// Everything the stage has sent to session `index` so far, in order.
    fn ops(&self, index: usize) -> Vec<Op> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let session = &mut sessions[index];
        let mut ops = Vec::new();
        while let Ok(input) = session.inputs.try_recv() {
            ops.push(match input {
                SttInput::Audio(_) => Op::Audio,
                SttInput::Finalize => Op::Finalize,
            });
        }
        ops
    }
}

impl SttProvider for ManualStt {
    fn name(&self) -> &'static str {
        "manual-stt"
    }

    async fn connect(&self, _config: SttConfig) -> brisvox_providers::Result<SttSession> {
        let (input_tx, input_rx) = mpsc::channel(self.input_capacity);
        let (event_tx, event_rx) = mpsc::channel(64);
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ManualSession {
                inputs: input_rx,
                events: event_tx,
            });
        Ok(SttSession {
            input: input_tx,
            events: event_rx,
        })
    }
}

/// A tail that takes `delay` per data frame: a slow consumer, so the stage in front of it falls
/// behind and its inbox fills.
struct SlowTail {
    clock: SimClock,
    delay: Duration,
}

impl Processor for SlowTail {
    fn name(&self) -> &'static str {
        "slow_tail"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        _direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if matches!(frame, Frame::Data(_)) {
            self.clock.sleep(self.delay).await;
        }
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn finalize_never_precedes_the_speech_audio_with_a_slow_inbox() {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let running = Pipeline::new("slow")
        .then_with_queue(
            stage,
            QueueConfig {
                data_capacity: 32,
                ..QueueConfig::DEFAULT
            },
        )
        .then_with_queue(
            SlowTail {
                clock,
                delay: 100 * MS,
            },
            QueueConfig {
                data_capacity: 1,
                ..QueueConfig::DEFAULT
            },
        )
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await; // connected

    // Frames 0..=9 are speech, 10 and 11 silence. They all queue in the stage's inbox (the stage
    // is stuck behind the slow tail); the VAD stop, dated to the end of frame 9, travels the
    // system lane and overtakes every frame still queued.
    for i in 0..12 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, 3_000);
        running.input().send(frame.into()).await.unwrap();
    }
    let at = origin + 200 * MS;
    running
        .input()
        .send(SystemFrame::UserStoppedSpeaking { at }.into())
        .await
        .unwrap();
    clock.sleep(Duration::from_secs(5)).await;

    let ops = provider.ops(0);
    let finalize = ops.iter().position(|op| *op == Op::Finalize).unwrap();
    // 10 speech frames plus the first silence frame (stamped at the stop time) came first.
    assert_eq!(
        finalize,
        11,
        "Finalize must follow all speech audio, got {} audio ops first",
        ops.iter()
            .take(finalize)
            .filter(|op| **op == Op::Audio)
            .count()
    );
    assert_eq!(ops.iter().filter(|op| **op == Op::Audio).count(), 12);
    let stats = stats.snapshot();
    assert_eq!(
        stats.finalizes_deferred, 1,
        "the finalize had to wait: {stats:?}"
    );
    assert_eq!(stats.finalizes_sent, 1, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn finalize_is_immediate_when_the_stage_is_not_behind() {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("fast")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    for i in 0..11 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, 3_000);
        running.input().send(frame.into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    // The stop is dated to the end of frame 9; frame 10 (silence) has been processed already.
    running
        .input()
        .send(
            SystemFrame::UserStoppedSpeaking {
                at: origin + 200 * MS,
            }
            .into(),
        )
        .await
        .unwrap();
    clock.sleep(MS).await;
    let ops = provider.ops(0);
    assert_eq!(ops.last(), Some(&Op::Finalize), "{ops:?}");
    assert_eq!(stats.snapshot().finalizes_deferred, 0);
    assert_eq!(stats.snapshot().finalizes_sent, 1);
}

#[tokio::test(start_paused = true)]
async fn a_stuck_provider_never_blocks_the_audio_and_ends_its_session() {
    let clock = SimClock::new();
    let origin = clock.now();
    // The provider's input holds two messages and nobody reads it.
    let provider = ManualStt::new(2);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("stuck")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    for i in 0..11 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, 3_000);
        running.input().send(frame.into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    running
        .input()
        .send(
            SystemFrame::UserStoppedSpeaking {
                at: origin + 200 * MS,
            }
            .into(),
        )
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    assert_eq!(
        rec.audio().len(),
        11,
        "audio is forwarded however the provider behaves"
    );
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.audio_sent, 2, "{snapshot:?}");
    // Only what the provider's input took is counted: two 20 ms frames, not the nine dropped.
    assert_eq!(snapshot.audio_ms_sent, 40, "{snapshot:?}");
    assert_eq!(snapshot.audio_dropped_provider, 9, "{snapshot:?}");
    // The Finalize found the provider's input full: that is a stuck provider. The stop is still
    // answered, once, and the session restarted.
    let finals = rec
        .all()
        .iter()
        .filter(|(f, _)| matches!(f, Frame::Control(c) if c.kind == ControlKind::UtteranceFinal))
        .count();
    assert_eq!(finals, 1);
    assert_eq!(snapshot.finals_synthetic, 1, "{snapshot:?}");
    assert_eq!(snapshot.session_deaths, 1, "{snapshot:?}");
    clock.sleep(2 * SECOND).await;
    assert_eq!(
        provider.connects.load(Ordering::SeqCst),
        2,
        "reconnected after the backoff"
    );
}

#[tokio::test(start_paused = true)]
async fn only_an_answer_to_a_finalize_closes_an_utterance() {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("endpoint")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    let events = provider.sessions.lock().unwrap()[0].events.clone();
    let transcript = |text: &str, is_final, speech_final, from_finalize| SttEvent::Transcript {
        text: Arc::from(text),
        is_final,
        speech_final,
        from_finalize,
        language: None,
    };
    // A provider-endpointed final: its text is forwarded but it closes nothing.
    events
        .send(transcript("hello there", true, true, false))
        .await
        .unwrap();
    // An empty segment is not forwarded.
    events
        .send(transcript("", false, false, false))
        .await
        .unwrap();
    // An answer to a Finalize nobody asked for: dropped.
    events
        .send(transcript("ghost", true, true, true))
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    let names: Vec<String> = rec.all().iter().map(|(f, _)| label(f)).collect();
    assert_eq!(names, ["start", "final:hello there"], "{names:?}");

    // Now a real stop and its answer: the segments of the answer come first, then the final.
    running
        .input()
        .send(square_frame(origin, 0, SampleRate::Hz16000, 3_000).into())
        .await
        .unwrap();
    running
        .input()
        .send(SystemFrame::UserStoppedSpeaking { at: origin }.into())
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    events
        .send(transcript("how are you", true, false, true))
        .await
        .unwrap();
    events.send(transcript("", true, true, true)).await.unwrap();
    clock.sleep(10 * MS).await;
    let names: Vec<String> = rec.all().iter().map(|(f, _)| label(f)).collect();
    // The stop is a system frame and may overtake the audio frame on its way to the tail.
    let mut head: Vec<&str> = names[2..4].iter().map(String::as_str).collect();
    head.sort_unstable();
    assert_eq!(head, ["audio", "stop"], "{names:?}");
    assert_eq!(
        &names[4..],
        ["final:how are you", "utterance_final"],
        "{names:?}"
    );
    let stats = stats.snapshot();
    assert_eq!(stats.endpoint_closes, 1, "{stats:?}");
    assert_eq!(stats.late_answers, 1, "{stats:?}");
    assert_eq!(stats.finals_real, 1, "{stats:?}");
}

// ---------------------------------------------------------------------------------------------
// Review fixes: the held-finalize deadline, queue overflow at stage level, the reconnect budget,
// interim words in a synthetic final, and the non-blocking upstream.

/// A burst of `n` speech frames (queued without yielding, so the stage handles them back to back
/// before its session task runs).
async fn burst(rig: &Rig, first: u64, n: u64) {
    for i in first..first + n {
        let frame = square_frame(rig.origin, i, SampleRate::Hz16000, 3_000);
        rig.running.input().send(frame.into()).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_held_finalize_is_released_by_its_own_deadline_when_no_audio_follows() {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(), // max_finalize_defer: 1 s
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("deadline")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    for i in 0..10 {
        let frame = square_frame(origin, i, SampleRate::Hz16000, 3_000);
        running.input().send(frame.into()).await.unwrap();
        clock.sleep(20 * MS).await;
    }
    // The audio stops at 200 ms (a leg that suppresses silence, or a stalled lane); the stop is
    // dated 500 ms: no audio stamped at or after it will arrive.
    running
        .input()
        .send(
            SystemFrame::UserStoppedSpeaking {
                at: origin + 500 * MS,
            }
            .into(),
        )
        .await
        .unwrap();
    clock.sleep(900 * MS).await;
    let ops = provider.ops(0);
    assert!(
        !ops.contains(&Op::Finalize),
        "still held at 0.9 s: {} ops",
        ops.len()
    );
    assert_eq!(stats.snapshot().finalizes_deferred, 1);
    clock.sleep(200 * MS).await;
    let ops = provider.ops(0);
    assert_eq!(
        ops,
        [Op::Finalize],
        "released by max_finalize_defer without any further audio"
    );
    assert_eq!(stats.snapshot().finalizes_sent, 1);
}

#[tokio::test(start_paused = true)]
async fn a_full_command_queue_drops_new_audio_and_counts_it() {
    let stt = mock();
    let config = SttStageConfig {
        audio_capacity: 2,
        ..config()
    };
    let rig = Rig::new(Arc::new(stt.clone()), config).await;
    rig.clock.sleep(SECOND).await;
    // Ten frames reach the stage before its session task gets to run.
    burst(&rig, 0, 10).await;
    rig.clock.sleep(SECOND).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.audio_in, 10, "{stats:?}");
    assert_eq!(stats.audio_dropped_queue, 8, "{stats:?}");
    assert_eq!(stats.audio_sent, 2, "{stats:?}");
    assert_eq!(rig.count("audio"), 10, "all audio is still forwarded");
    assert_eq!(stt.audio_bytes(), 2 * 640);
}

#[tokio::test(start_paused = true)]
async fn a_finalize_that_does_not_fit_is_answered_synthetically_so_the_count_still_matches() {
    let stt = mock();
    stt.push(SttUtterance::words("hello there"));
    let config = SttStageConfig {
        finalize_capacity: 1,
        ..config()
    };
    let rig = Rig::new(Arc::new(stt.clone()), config).await;
    rig.clock.sleep(SECOND).await;
    burst(&rig, 0, 8).await;
    rig.clock.sleep(100 * MS).await; // the audio is through: the stops below are not held
    // Two stops back to back, both inside the audio sent: two Finalizes for a queue of one.
    for ms in [60, 100] {
        rig.running
            .input()
            .send(
                SystemFrame::UserStoppedSpeaking {
                    at: rig.origin + ms * MS,
                }
                .into(),
            )
            .await
            .unwrap();
    }
    rig.clock.sleep(Duration::from_secs(5)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(rig.count("stop"), 2, "{:?}", rig.labels());
    assert_eq!(stats.finalizes_requested, 2, "{stats:?}");
    assert_eq!(stats.finalizes_deferred, 0, "{stats:?}");
    assert_eq!(stats.finalizes_sent, 1, "only one fit: {stats:?}");
    assert_eq!(
        rig.count("utterance_final"),
        2,
        "exactly one final per Finalize: {:?}",
        rig.labels()
    );
    assert_eq!(
        (stats.finals_real, stats.finals_synthetic),
        (1, 1),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_full_held_queue_releases_its_oldest_finalize_and_every_stop_is_still_answered() {
    let stt = mock();
    stt.push(SttUtterance::words("hello there"));
    let config = SttStageConfig {
        finalize_capacity: 2,
        ..config()
    };
    let rig = Rig::new(Arc::new(stt.clone()), config).await;
    rig.clock.sleep(SECOND).await;
    // Three stops overtake the audio of a slow inbox: more than the held queue holds, so the
    // oldest is released before its audio (the documented bound).
    for ms in [20, 60, 100] {
        rig.running
            .input()
            .send(
                SystemFrame::UserStoppedSpeaking {
                    at: rig.origin + ms * MS,
                }
                .into(),
            )
            .await
            .unwrap();
    }
    burst(&rig, 0, 8).await;
    rig.clock.sleep(Duration::from_secs(10)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.finalizes_deferred, 3, "{stats:?}");
    assert_eq!(stats.finalizes_requested, 3, "none is lost: {stats:?}");
    assert_eq!(
        rig.count("utterance_final"),
        3,
        "one final per stop: {:?} {stats:?}",
        rig.labels()
    );
}

/// A provider whose sessions open and then die at once (events closed): it accepts the
/// connection and drops it.
#[derive(Clone, Default)]
struct DyingStt {
    connects: Arc<AtomicUsize>,
}

impl SttProvider for DyingStt {
    fn name(&self) -> &'static str {
        "dying-stt"
    }

    async fn connect(&self, _config: SttConfig) -> brisvox_providers::Result<SttSession> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let (input, _input_rx) = mpsc::channel(8);
        let (_events_tx, events) = mpsc::channel(8);
        Ok(SttSession { input, events })
    }
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_connects_and_dies_exhausts_the_reconnect_budget() {
    let provider = DyingStt::default();
    let rig = Rig::new(Arc::new(provider.clone()), config()).await;
    rig.clock.sleep(Duration::from_secs(60)).await;
    let stats = rig.stats.snapshot();
    // The initial attempt plus three retries (max_reconnects), every connect succeeding: a
    // successful connect alone does not restore the budget.
    assert_eq!(provider.connects.load(Ordering::SeqCst), 4, "{stats:?}");
    assert_eq!(stats.connects, 4, "{stats:?}");
    assert_eq!(stats.session_deaths, 4, "{stats:?}");
    assert_eq!(stats.degraded, 1, "{stats:?}");
    rig.clock.sleep(Duration::from_secs(600)).await;
    assert_eq!(
        provider.connects.load(Ordering::SeqCst),
        4,
        "degraded: the provider is not hammered"
    );
}

/// A stage on a hand-driven provider with one stop finalized and not yet answered.
struct Pending {
    provider: ManualStt,
    rec: Recorded,
    stats: SttStats,
    /// The event sender of session 0.
    events: mpsc::Sender<SttEvent>,
    _running: RunningPipeline,
}

/// Feeds `segments` (text, is_final) from the provider, then a stop that the provider leaves
/// unanswered.
async fn finalize_pending_after(segments: &[(&str, bool)]) -> Pending {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("pending")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    let events = provider.sessions.lock().unwrap()[0].events.clone();
    for (text, is_final) in segments {
        events
            .send(SttEvent::Transcript {
                text: Arc::from(*text),
                is_final: *is_final,
                speech_final: false,
                from_finalize: false,
                language: None,
            })
            .await
            .unwrap();
    }
    clock.sleep(10 * MS).await;
    running
        .input()
        .send(square_frame(origin, 0, SampleRate::Hz16000, 3_000).into())
        .await
        .unwrap();
    running
        .input()
        .send(SystemFrame::UserStoppedSpeaking { at: origin }.into())
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    Pending {
        provider,
        rec,
        stats,
        events,
        _running: running,
    }
}

#[tokio::test(start_paused = true)]
async fn a_synthetic_final_keeps_the_words_the_provider_had_heard() {
    let Pending {
        provider,
        rec,
        stats,
        events,
        _running,
    } = finalize_pending_after(&[("book a tab", false), ("book a table for", false)]).await;
    // The provider dies before it answers the Finalize.
    drop(events);
    provider.sessions.lock().unwrap().clear();
    SimClock::new().sleep(10 * MS).await;
    let names: Vec<String> = rec.all().iter().map(|(f, _)| label(f)).collect();
    assert_eq!(
        names[names.len() - 2..],
        ["final:book a table for", "utterance_final"],
        "the latest interim is promoted to a final segment before the synthetic final: {names:?}"
    );
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.finals_synthetic, 1, "{snapshot:?}");
    assert_eq!(snapshot.interims_promoted, 1, "{snapshot:?}");
}

#[tokio::test(start_paused = true)]
async fn a_synthetic_final_after_a_final_segment_promotes_nothing() {
    let Pending {
        provider,
        rec,
        stats,
        events,
        _running,
    } = finalize_pending_after(&[("hello wor", false), ("hello world", true)]).await;
    drop(events);
    provider.sessions.lock().unwrap().clear();
    SimClock::new().sleep(10 * MS).await;
    assert_eq!(stats.snapshot().interims_promoted, 0);
    let names: Vec<String> = rec.all().iter().map(|(f, _)| label(f)).collect();
    assert_eq!(names.last().map(String::as_str), Some("utterance_final"));
    assert_eq!(
        names.iter().filter(|l| l.starts_with("final:")).count(),
        1,
        "{names:?}"
    );
}

/// Session 0 hears `interim` and dies mid-utterance (nothing finalized yet, so no synthetic
/// answer and no promotion); the stage reconnects, a stop arrives and session 1 answers the
/// `Finalize` with `answer`. Returns the frame labels and the stats.
async fn real_answer_after_a_mid_utterance_death(
    interim: &str,
    answer: &str,
) -> (Vec<String>, SttStatsSnapshot) {
    let clock = SimClock::new();
    let origin = clock.now();
    let provider = ManualStt::new(64);
    let stage = SttStage::new(
        Arc::new(provider.clone()),
        SttStageConfig::default(),
        Arc::new(clock),
    );
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("mid-death")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(100 * MS).await;
    let events = provider.sessions.lock().unwrap()[0].events.clone();
    events
        .send(SttEvent::Transcript {
            text: Arc::from(interim),
            is_final: false,
            speech_final: false,
            from_finalize: false,
            language: None,
        })
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    // The socket drops mid-utterance: nothing is outstanding.
    drop(events);
    provider.sessions.lock().unwrap().clear();
    clock.sleep(Duration::from_secs(2)).await;
    assert_eq!(provider.connects.load(Ordering::SeqCst), 2, "reconnected");
    running
        .input()
        .send(square_frame(origin, 0, SampleRate::Hz16000, 3_000).into())
        .await
        .unwrap();
    running
        .input()
        .send(SystemFrame::UserStoppedSpeaking { at: origin }.into())
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    let events = provider.sessions.lock().unwrap()[0].events.clone();
    // The new session heard (almost) none of the audio.
    events
        .send(SttEvent::Transcript {
            text: Arc::from(answer),
            is_final: true,
            speech_final: true,
            from_finalize: true,
            language: None,
        })
        .await
        .unwrap();
    clock.sleep(10 * MS).await;
    let names = rec.all().iter().map(|(f, _)| label(f)).collect();
    (names, stats.snapshot())
}

#[tokio::test(start_paused = true)]
async fn a_real_empty_answer_after_a_mid_utterance_death_keeps_the_interim_words() {
    let (names, stats) = real_answer_after_a_mid_utterance_death("book a table for four", "").await;
    assert_eq!(
        names[names.len() - 2..],
        ["final:book a table for four", "utterance_final"],
        "the dead session's interim survives the empty real final: {names:?}"
    );
    assert_eq!(stats.finals_real, 1, "{stats:?}");
    assert_eq!(stats.interims_promoted, 1, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn a_real_answer_with_its_own_final_text_promotes_no_stale_interim() {
    let (names, stats) =
        real_answer_after_a_mid_utterance_death("book a table", "a table for two").await;
    assert_eq!(
        names[names.len() - 2..],
        ["final:a table for two", "utterance_final"],
        "{names:?}"
    );
    assert_eq!(
        names.iter().filter(|l| l.starts_with("final:")).count(),
        1,
        "{names:?}"
    );
    assert_eq!(stats.interims_promoted, 0, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn upstream_frames_are_forwarded_without_blocking_and_drops_are_counted() {
    let stage = SttStage::new(
        Arc::new(ManualStt::new(8)),
        SttStageConfig::default(),
        Arc::new(SimClock::new()),
    );
    let stats = stage.stats();
    // The upstream neighbour never reads: its lanes hold two frames each.
    let mut neighbour = brisvox_core::Inbox::new(QueueConfig {
        system_capacity: 2,
        data_capacity: 2,
        ..QueueConfig::DEFAULT
    });
    let running = Pipeline::new("upstream")
        .then(stage)
        .start(
            Links {
                downstream: None,
                upstream: Some(neighbour.sender(Direction::Upstream)),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    for _ in 0..6 {
        running
            .upstream_input()
            .send(
                brisvox_core::frame::ControlFrame {
                    epoch: brisvox_core::TurnEpoch::ZERO,
                    kind: ControlKind::TtsStarted,
                }
                .into(),
            )
            .await
            .unwrap();
    }
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
    let mut delivered = 0;
    while neighbour.try_recv().is_ok() {
        delivered += 1;
    }
    let dropped = stats.snapshot().upstream_dropped;
    assert_eq!(
        delivered + dropped,
        6,
        "every frame is delivered or counted"
    );
    assert!(
        (1..=4).contains(&dropped),
        "the full lane dropped frames instead of blocking the stage: delivered {delivered}, dropped {dropped}"
    );
}
