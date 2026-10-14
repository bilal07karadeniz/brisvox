//! `TurnDriver`: the turn machine behind real frames on a paused clock. Fixed-silence
//! and adaptive policies, the finals gate, barge-in with the transport clear and the
//! interruption guards, hang-up, lost finals, races (a verdict after a new speech start, a
//! `BotAudioEnded` after an interrupt), the response backstop, the Smart Turn ring (exact audio
//! content, clearing, a stop that overtakes queued audio), failing and hanging transport clears
//! and analyses, determinism (frames and the exact timer deadlines), and a table that replays
//! scenarios through the driver and requires the exact action sequence the bare `TurnMachine`
//! produces.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::{
    BackchannelSet, InterruptionPolicy, StateKind, TurnAction, TurnInput, TurnMachine, TurnPolicy,
    VetoReason,
};
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, DtmfDigit, DtmfFrame, Frame, SampleRate,
    SystemFrame, TranscriptFrame,
};
use brisvox_core::{
    Clock, Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SharedClock, SimClock, SystemClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_engine::{
    ClearHook, EngineConfig, EngineEvent, EngineEvents, EventStream, InterruptionConfig, Offload,
    TextTurn, TextTurnReceiver, TextTurnSender, TurnDriver, TurnDriverConfig, TurnDriverStats,
    TurnProbe, text_turns,
};
use brisvox_inference::smart_turn::{SmartTurn, TurnModel};
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_sim::leak::TaskCounter;
use common::{MS, square_frame};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn ep(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

/// A Smart Turn model with a fixed answer.
struct FixedModel(f32);

impl TurnModel for FixedModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Ok(self.0)
    }
}

/// A model that always fails.
struct FailingModel;

impl TurnModel for FailingModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        Err(brisvox_inference::Error::Model("mock: broken".into()))
    }
}

/// A model that records the features it is given and answers a fixed probability.
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

/// A model that blocks its pool thread until the test drops the release sender.
struct HangingModel {
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl TurnModel for HangingModel {
    fn predict(&self, _features: &[f32]) -> brisvox_inference::Result<f32> {
        let _ = self
            .release
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv();
        Err(brisvox_inference::Error::Model("mock: released".into()))
    }
}

/// The features Smart Turn computes for exactly `samples` (what the ring must have handed over).
fn features_of(samples: &[i16]) -> Vec<f32> {
    let recorder = RecordingModel::new(0.5);
    SmartTurn::analyze_snapshot_blocking(recorder.as_ref(), samples).unwrap();
    recorder.calls().remove(0)
}

/// The PCM16 samples of `frames`, in order.
fn samples_of(frames: &[AudioFrame]) -> Vec<i16> {
    frames
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
        .collect()
}

/// `count` 16 kHz frames from index `first`, each with its own amplitude (so that order and
/// content are visible in the features).
fn frames_from(origin: Instant, first: u64, count: u64) -> Vec<AudioFrame> {
    (first..first + count)
        .map(|i| {
            square_frame(
                origin,
                i,
                SampleRate::Hz16000,
                400 + 250 * i16::try_from(i % 10).unwrap(),
            )
        })
        .collect()
}

/// What the tail saw.
#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    frame: Frame,
}

/// Records every downstream frame with its time and passes upstream frames back to the driver.
struct Tail {
    seen: Arc<Mutex<Vec<Seen>>>,
    clock: SimClock,
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
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        match direction {
            Direction::Upstream => {
                if let Some(up) = ctx.sender(Direction::Upstream) {
                    let _ = up.try_send(frame);
                }
            }
            Direction::Downstream => self
                .seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Seen {
                    at: self.clock.now(),
                    frame,
                }),
        }
        Ok(())
    }
}

fn label(frame: &Frame) -> String {
    match frame {
        Frame::Control(ControlFrame { epoch, kind }) => format!("{}:{}", kind.name(), epoch.get()),
        Frame::Data(DataFrame::Transcript(t)) => {
            format!("transcript:{}:{}:{}", t.epoch.get(), t.is_final, t.text)
        }
        Frame::Data(DataFrame::Dtmf(d)) => format!("dtmf:{}:{:?}", d.epoch.get(), d.digit),
        Frame::System(SystemFrame::Interrupt { new_epoch }) => {
            format!("interrupt:{}", new_epoch.get())
        }
        other => other.name().to_owned(),
    }
}

struct Rig {
    running: RunningPipeline,
    seen: Arc<Mutex<Vec<Seen>>>,
    probe: TurnProbe,
    stats: TurnDriverStats,
    timeline: TimelineRecorder,
    clock: SimClock,
    origin: Instant,
}

fn config(turn: TurnPolicy, interruption: InterruptionConfig) -> TurnDriverConfig {
    let mut engine = EngineConfig::DEFAULT;
    engine.turn = turn;
    engine.interruption = interruption;
    TurnDriverConfig::from_engine(&engine)
}

const FIXED: TurnPolicy = TurnPolicy::fixed(Duration::from_millis(700));

fn adaptive(require_final: bool) -> TurnPolicy {
    TurnPolicy::Adaptive {
        fallback_silence: Duration::from_secs(3),
        require_final,
        final_grace: 800 * MS,
    }
}

impl Rig {
    async fn new(config: TurnDriverConfig, model: Option<Arc<dyn TurnModel>>) -> Self {
        Self::with(config, model, None).await
    }

    async fn with(
        config: TurnDriverConfig,
        model: Option<Arc<dyn TurnModel>>,
        commands: Option<brisvox_transport::CommandSender>,
    ) -> Self {
        Self::build(config, model, commands, None, None).await
    }

    /// A rig whose driver judges barge-ins with `policy` (`TurnDriver::with_interruption`).
    async fn with_policy(config: TurnDriverConfig, policy: InterruptionPolicy) -> Self {
        Self::build_with(config, None, None, None, None, Some(policy), None).await
    }

    /// A rig with every optional part: a transport `commands` or a clear `hook`, and an
    /// `offload` other than the inline one.
    async fn build(
        config: TurnDriverConfig,
        model: Option<Arc<dyn TurnModel>>,
        commands: Option<brisvox_transport::CommandSender>,
        hook: Option<ClearHook>,
        offload: Option<Offload>,
    ) -> Self {
        Self::build_with(config, model, commands, hook, offload, None, None).await
    }

    async fn build_with(
        config: TurnDriverConfig,
        model: Option<Arc<dyn TurnModel>>,
        commands: Option<brisvox_transport::CommandSender>,
        hook: Option<ClearHook>,
        offload: Option<Offload>,
        policy: Option<InterruptionPolicy>,
        observed: Option<(EngineEvents, TextTurnReceiver)>,
    ) -> Self {
        let clock = SimClock::new();
        // Land on a millisecond boundary of the timer wheel, so that deadlines at whole
        // milliseconds from the origin fire exactly.
        clock.sleep(MS).await;
        let shared: SharedClock = Arc::new(clock);
        let timeline = TimelineRecorder::new();
        let probe = TurnProbe::new(512);
        let mut driver = TurnDriver::new(config, Arc::clone(&shared), timeline.clone())
            .with_probe(probe.clone());
        if let Some(probability) = model {
            let offload = offload.unwrap_or_else(|| Offload::Inline(Arc::clone(&shared)));
            driver = driver.with_smart_turn(probability, offload);
        }
        if let Some(policy) = policy {
            driver = driver.with_interruption(policy);
        }
        if let Some(commands) = commands {
            driver = driver.with_commands(commands);
        }
        if let Some(hook) = hook {
            driver = driver.with_clear_hook(hook);
        }
        if let Some((events, input)) = observed {
            driver = driver.with_events(events).with_text_input(input);
        }
        let stats = driver.stats();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let running = Pipeline::new("turn")
            .then(driver)
            .then(Tail {
                seen: Arc::clone(&seen),
                clock,
            })
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        let rig = Self {
            running,
            seen,
            probe,
            stats,
            timeline,
            clock,
            origin: clock.now(),
        };
        rig.settle().await;
        rig
    }

    /// Lets every ready task run without advancing time.
    async fn settle(&self) {
        for _ in 0..40 {
            tokio::task::yield_now().await;
        }
    }

    /// Advances to `ms` after the origin and lets the pipeline settle.
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

    async fn interim(&self, text: &str) {
        self.send(self.transcript(text, false)).await;
    }

    fn transcript(&self, text: &str, is_final: bool) -> Frame {
        DataFrame::Transcript(TranscriptFrame {
            epoch: TurnEpoch::ZERO,
            text: Arc::from(text),
            is_final,
            received_at: self.clock.now(),
        })
        .into()
    }

    /// The final segment(s) `texts` followed by the `UtteranceFinal` that closes them.
    async fn final_of(&self, texts: &[&str]) {
        for text in texts {
            self.send(self.transcript(text, true)).await;
        }
        self.send(
            ControlFrame {
                epoch: TurnEpoch::ZERO,
                kind: ControlKind::UtteranceFinal,
            }
            .into(),
        )
        .await;
    }

    async fn up(&self, kind: ControlKind, epoch: u32) {
        self.running
            .upstream_input()
            .send(
                ControlFrame {
                    epoch: ep(epoch),
                    kind,
                }
                .into(),
            )
            .await
            .unwrap();
        self.settle().await;
    }

    /// `(milliseconds after the origin, label)` of every downstream frame, skipping the
    /// pipeline's own `Start`.
    fn frames(&self) -> Vec<(u64, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| !matches!(s.frame, Frame::System(SystemFrame::Start)))
            .map(|s| {
                let ms = s.at.saturating_duration_since(self.origin).as_millis();
                (u64::try_from(ms).unwrap(), label(&s.frame))
            })
            .collect()
    }

    fn labels(&self) -> Vec<String> {
        self.frames().into_iter().map(|(_, l)| l).collect()
    }

    fn count(&self, prefix: &str) -> usize {
        self.labels()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    fn machine(&self) -> brisvox_engine::MachineSnapshot {
        self.probe.machine().unwrap()
    }
}

fn model(p: f32) -> Option<Arc<dyn TurnModel>> {
    Some(Arc::new(FixedModel(p)))
}

// ---------------------------------------------------------------------------------------------
// Fixed silence and the finals gate

#[tokio::test(start_paused = true)]
async fn fixed_silence_ends_the_turn_at_the_deadline_once_the_final_is_in() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(100).await;
    rig.interim("book").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await; // VAD reports the stop 100 ms late, dated to the speech end
    rig.at(1250).await;
    rig.final_of(&["book a table"]).await;
    rig.at(3000).await;

    assert_eq!(
        rig.frames(),
        vec![
            (0, "user_turn_started:0".to_owned()),
            (1700, "transcript:0:true:book a table".to_owned()),
            (1700, "user_turn_ended:0".to_owned()),
        ]
    );
    // The timeline is complete.
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.at(TurnEvent::UserSpeechStart), Some(rig.t(0)));
    assert_eq!(turn.at(TurnEvent::SttInterim), Some(rig.t(100)));
    assert_eq!(turn.at(TurnEvent::UserSpeechEnd), Some(rig.t(1000)));
    assert_eq!(turn.at(TurnEvent::SttFinal), Some(rig.t(1250)));
    assert_eq!(turn.at(TurnEvent::TurnEnd), Some(rig.t(1700)));
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.turns_started, stats.turns_ended),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(
        stats.ends_held, 0,
        "the final came before the deadline: {stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_end_is_held_until_the_final_arrives() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(2000).await; // the deadline (1700) passed: the end is held for the final
    assert_eq!(rig.labels(), ["user_turn_started:0"]);
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    rig.final_of(&["a late answer"]).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (2000, "transcript:0:true:a late answer".to_owned()),
            (2000, "user_turn_ended:0".to_owned())
        ]
    );
    assert_eq!(rig.stats.snapshot().gate_timeouts, 0);
    // The latency metrics count the STT wait: the turn ended when the gate released it (the
    // machine's own end was at 1700), and the final is stamped when it arrived.
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.at(TurnEvent::TurnEnd), Some(rig.t(2000)));
    assert_eq!(turn.at(TurnEvent::SttFinal), Some(rig.t(2000)));
}

#[tokio::test(start_paused = true)]
async fn one_joined_final_per_stop_and_one_transcript_per_turn() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(600).await;
    rig.vad_stop(500).await;
    rig.at(800).await;
    // Three segments close with one UtteranceFinal.
    rig.final_of(&["I want", "to book", "a table"]).await;
    rig.at(900).await;
    rig.vad_start(900).await; // the user resumes: the same turn
    rig.at(1600).await;
    rig.vad_stop(1500).await;
    rig.at(1900).await;
    rig.final_of(&["for four"]).await;
    rig.at(4000).await;
    assert_eq!(
        rig.frames(),
        vec![
            (0, "user_turn_started:0".to_owned()),
            (
                2200,
                "transcript:0:true:I want to book a table for four".to_owned()
            ),
            (2200, "user_turn_ended:0".to_owned()),
        ]
    );
    // The machine saw exactly one final per stop: none was ignored.
    let machine = rig.machine();
    assert_eq!(machine.ignored.total_ignored(), 0, "{:?}", machine.ignored);
    assert_eq!(rig.stats.snapshot().stray_finals, 0);
}

#[tokio::test(start_paused = true)]
async fn a_lost_final_ends_the_turn_at_final_wait_with_the_latest_interim() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("hello wor").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    // The machine ends the turn at 1700; the final never comes; the gate gives up 1.5 s later.
    rig.at(3100).await;
    assert_eq!(rig.labels(), ["user_turn_started:0"], "still held at 3100");
    rig.at(3300).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (3200, "transcript:0:true:hello wor".to_owned()),
            (3200, "user_turn_ended:0".to_owned())
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.gate_timeouts, stats.interim_fallbacks),
        (1, 1),
        "{stats:?}"
    );
    // The turn ended when the gate gave up, not when the machine wanted to (1700), and no
    // final was ever seen.
    let turn = rig.timeline.current().unwrap();
    assert_eq!(turn.at(TurnEvent::TurnEnd), Some(rig.t(3200)));
    assert_eq!(turn.at(TurnEvent::SttFinal), None);

    // The final turns up very late: its text is dropped, nothing else happens.
    rig.at(9000).await;
    rig.final_of(&["too late now"]).await;
    assert_eq!(rig.count("transcript"), 1);
    assert_eq!(rig.stats.snapshot().late_finals, 1);
    assert_eq!(rig.machine().ignored.total_ignored(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_empty_transcript_ends_no_turn_and_the_machine_returns_to_idle() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&[]).await; // an UtteranceFinal without any text
    rig.at(2000).await;
    assert_eq!(rig.labels(), ["user_turn_started:0"], "no UserTurnEnded");
    let machine = rig.machine();
    assert_eq!(
        machine.state,
        StateKind::Idle,
        "BotAudioEnded was synthesized"
    );
    assert_eq!(rig.stats.snapshot().empty_turns, 1);

    // The next speech is an ordinary new turn: a new epoch, no interrupt.
    rig.vad_start(2100).await;
    assert_eq!(
        rig.labels(),
        ["user_turn_started:0", "user_turn_started:1"],
        "{:?}",
        rig.labels()
    );
}

#[tokio::test(start_paused = true)]
async fn a_held_end_is_superseded_when_the_user_keeps_talking() {
    let cfg = config(TurnPolicy::fixed(100 * MS), InterruptionConfig::OFF);
    let rig = Rig::new(cfg, None).await;
    rig.vad_start(0).await;
    rig.at(1050).await;
    rig.vad_stop(1000).await; // the machine ends the turn at 1100, the final is still owed
    rig.at(1200).await;
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    rig.vad_start(1200).await; // the user resumes while the end is held
    rig.at(1500).await;
    rig.final_of(&["tell me"]).await; // the final of the FIRST stop
    rig.at(2100).await;
    rig.vad_stop(2000).await;
    rig.at(2200).await;
    rig.final_of(&["a story"]).await;
    rig.at(4000).await;
    assert_eq!(
        rig.frames(),
        vec![
            (0, "user_turn_started:0".to_owned()),
            (1200, "user_turn_started:1".to_owned()),
            (2200, "transcript:1:true:tell me a story".to_owned()),
            (2200, "user_turn_ended:1".to_owned()),
        ],
        "one answer, after the whole utterance, and no Interrupt: nothing was playing"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.ends_superseded, stats.interrupts),
        (1, 0),
        "{stats:?}"
    );
}

/// The user resumes (`speech_ms` of speech, opening with `opening`) while the end of the turn is
/// held for a final that is late, under the RECOMMENDED guards (minimum speech, backchannel
/// veto) and with a transport clear that would be counted.
async fn resume_while_held(speech_ms: u64, opening: &str) -> Rig {
    let mut guards = InterruptionConfig::RECOMMENDED;
    guards.vad_hangover = Duration::ZERO;
    let hook: ClearHook = Arc::new(|_| Box::pin(async { Ok(()) }));
    let rig = Rig::build(config(FIXED, guards), None, None, Some(hook), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(2000).await; // the machine ended the turn at 1700: held, the final is owed
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    rig.vad_start(2000).await;
    rig.at(2000 + speech_ms / 2).await;
    rig.interim(opening).await;
    rig.at(2000 + speech_ms + 50).await;
    rig.vad_stop(2000 + speech_ms).await;
    rig.final_of(&["book a table"]).await; // the final of the FIRST stop
    rig.final_of(&[&format!("{opening} and also on Friday")])
        .await;
    rig.at(6000).await;
    rig
}

#[tokio::test(start_paused = true)]
async fn a_short_or_backchannel_resume_while_the_end_is_held_is_not_vetoed_and_loses_nothing() {
    // 200 ms is under the 300 ms minimum speech, and "yeah" is a backchannel: as a barge-in
    // candidate it would be vetoed and its words dropped. No bot audio exists: it is a resume.
    let rig = resume_while_held(200, "yeah").await;
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "user_turn_started:1",
            "transcript:1:true:book a table yeah and also on Friday",
            "user_turn_ended:1",
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.ends_superseded, stats.late_finals, stats.interrupts),
        (1, 0, 0),
        "{stats:?}"
    );
    let machine = rig.machine();
    assert_eq!(machine.interruption.candidates(), 0, "never a candidate");
    assert_eq!(machine.interruption.total_vetoed(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_long_resume_while_the_end_is_held_is_no_interruption_and_sends_no_clear() {
    // 500 ms satisfies the minimum speech: as a candidate it would be CONFIRMED, with a spurious
    // transport Clear, a downstream Interrupt and an `Interrupted` on the timeline.
    let rig = resume_while_held(500, "wait").await;
    assert_eq!(rig.count("interrupt"), 0, "{:?}", rig.labels());
    assert_eq!(
        rig.labels(),
        [
            "user_turn_started:0",
            "user_turn_started:1",
            "transcript:1:true:book a table wait and also on Friday",
            "user_turn_ended:1",
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.interrupts, stats.clears_sent, stats.ends_superseded),
        (0, 0, 1),
        "{stats:?}"
    );
    let history = rig.timeline.history();
    let first = history.iter().find(|t| t.epoch() == ep(0)).unwrap();
    assert_eq!(first.at(TurnEvent::Interrupted), None);
}

#[tokio::test(start_paused = true)]
async fn a_final_that_is_a_little_late_is_waited_for_once_when_the_turn_has_no_text() {
    // finalize (3 s) - final_wait (1.5 s): the STT stage answers by then, really or not.
    assert_eq!(
        TurnDriverConfig::default().late_final_wait,
        Duration::from_millis(1_500)
    );
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await; // the machine ends the turn at 1700, the gate would give up at 3200
    rig.at(3300).await;
    assert_eq!(
        rig.labels(),
        ["user_turn_started:0"],
        "the gate was extended once instead of ending the turn empty"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.gate_extensions,
            stats.gate_timeouts,
            stats.empty_turns
        ),
        (1, 0, 0),
        "{stats:?}"
    );
    // The STT stage answers at 3.5 s, after the driver's final_wait but before its own timeout.
    rig.at(3500).await;
    rig.final_of(&["a slow answer"]).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (3500, "transcript:0:true:a slow answer".to_owned()),
            (3500, "user_turn_ended:0".to_owned())
        ]
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.gate_timeouts, stats.late_finals),
        (0, 0),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_final_that_never_comes_ends_the_extended_gate_empty_after_one_extension() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(4600).await;
    assert_eq!(rig.stats.snapshot().gate_timeouts, 0, "extended to 4700");
    rig.at(4800).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.gate_extensions,
            stats.gate_timeouts,
            stats.empty_turns
        ),
        (1, 1, 1),
        "{stats:?}"
    );
    assert_eq!(rig.labels(), ["user_turn_started:0"], "no UserTurnEnded");
    assert_eq!(rig.machine().state, StateKind::Idle);
}

#[tokio::test(start_paused = true)]
async fn the_gate_is_not_extended_when_an_interim_can_answer() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("only the interim").await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(3300).await;
    assert_eq!(rig.count("transcript:0:true:only the interim"), 1);
    assert_eq!(rig.stats.snapshot().gate_extensions, 0);
}

#[tokio::test(start_paused = true)]
async fn the_interim_never_replaces_the_confirmed_text_of_an_earlier_stop() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(600).await;
    rig.vad_stop(500).await;
    rig.at(700).await;
    rig.final_of(&["I want a table"]).await; // stop 1 answered
    rig.at(900).await;
    rig.vad_start(900).await; // the user resumes: the same turn
    rig.at(1000).await;
    rig.interim("for fo").await;
    rig.at(1200).await;
    rig.vad_stop(1100).await; // stop 2: its final is lost
    // The machine ends the turn at 1800; the gate gives up at 3300.
    rig.at(3400).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (3300, "transcript:0:true:I want a table".to_owned()),
            (3300, "user_turn_ended:0".to_owned())
        ],
        "the confirmed words, not the unconfirmed fragment"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (
            stats.gate_timeouts,
            stats.interim_fallbacks,
            stats.gate_extensions
        ),
        (1, 0, 0),
        "{stats:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Smart Turn

#[tokio::test(start_paused = true)]
async fn a_complete_verdict_ends_the_turn_as_soon_as_the_final_is_in() {
    let rig = Rig::new(config(adaptive(false), InterruptionConfig::OFF), model(0.9)).await;
    rig.vad_start(0).await;
    // 8 s of audio: the ring is fed (the fixed model ignores its content).
    for i in 0..10 {
        rig.send(square_frame(rig.origin, i, SampleRate::Hz16000, 3_000).into())
            .await;
    }
    rig.at(1100).await;
    // The audio stops at 180 ms and the stop is dated 1000: the analysis waits for audio that
    // never comes and starts at the deferral bound (200 ms), well before the final.
    rig.vad_stop(1000).await;
    rig.at(1400).await;
    rig.final_of(&["yes please"]).await;
    assert_eq!(
        rig.frames(),
        vec![
            (0, "user_turn_started:0".to_owned()),
            (1400, "transcript:0:true:yes please".to_owned()),
            (1400, "user_turn_ended:0".to_owned()),
        ],
        "the end waited for the final, not for a deadline"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.analyses_started, stats.analyses_ok),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(stats.audio_frames, 10);
    assert_eq!(stats.analyses_deferred, 1, "{stats:?}");
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_verdict_waits_for_the_fallback_silence() {
    let rig = Rig::new(config(adaptive(false), InterruptionConfig::OFF), model(0.1)).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1300).await;
    rig.final_of(&["and then"]).await; // no sentence-final punctuation
    rig.at(3900).await;
    assert_eq!(
        rig.count("user_turn_ended"),
        0,
        "still inside the 3 s fallback"
    );
    rig.at(4100).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (4000, "transcript:0:true:and then".to_owned()),
            (4000, "user_turn_ended:0".to_owned())
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_analysis_or_no_model_falls_back_to_the_fallback_silence() {
    for (name, model, failed, unavailable) in [
        (
            "failing model",
            Some(Arc::new(FailingModel) as Arc<dyn TurnModel>),
            1,
            0,
        ),
        ("no model", None, 0, 1),
    ] {
        let rig = Rig::new(config(adaptive(false), InterruptionConfig::OFF), model).await;
        rig.vad_start(0).await;
        rig.at(1100).await;
        rig.vad_stop(1000).await;
        rig.at(1300).await;
        rig.final_of(&["something"]).await;
        rig.at(5000).await;
        let frames = rig.frames();
        assert_eq!(
            frames[1..],
            [
                (4000, "transcript:0:true:something".to_owned()),
                (4000, "user_turn_ended:0".to_owned())
            ],
            "{name}"
        );
        let stats = rig.stats.snapshot();
        assert_eq!(stats.analyses_failed, failed, "{name}: {stats:?}");
        assert_eq!(stats.analyses_unavailable, unavailable, "{name}: {stats:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_verdict_after_a_new_speech_start_is_ignored() {
    let rig = Rig::new(config(adaptive(false), InterruptionConfig::OFF), model(0.9)).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    // The stop and a new speech start are queued together: the analysis job spawned by the
    // stop only runs after the start was handled, so its verdict belongs to a pause that is
    // already over.
    let input = rig.running.input();
    input
        .try_send(SystemFrame::UserStoppedSpeaking { at: rig.t(1000) }.into())
        .unwrap();
    input
        .try_send(SystemFrame::UserStartedSpeaking { at: rig.t(1100) }.into())
        .unwrap();
    rig.settle().await;
    assert_eq!(
        rig.count("user_turn_ended"),
        0,
        "the stale verdict ended nothing"
    );
    let machine = rig.machine();
    assert_eq!(machine.state, StateKind::UserSpeaking);
    assert_eq!(
        machine.ignored.ignored(brisvox_agent::InputKind::Verdict),
        1,
        "the machine counted the stale verdict"
    );
    assert_eq!(
        rig.stats.snapshot().analyses_ok,
        1,
        "the job itself finished"
    );

    // The turn goes on normally: its own stop gets a fresh verdict.
    rig.at(2200).await;
    rig.vad_stop(2000).await;
    rig.at(2300).await;
    // Both stops are answered, in order; the turn ends when the last answer is in.
    rig.final_of(&["first part"]).await;
    assert_eq!(
        rig.count("user_turn_ended"),
        0,
        "the second stop's final is still owed"
    );
    rig.final_of(&["and the second part"]).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (
                2300,
                "transcript:0:true:first part and the second part".to_owned()
            ),
            (2300, "user_turn_ended:0".to_owned())
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Barge-in

/// A completed turn at epoch 0 with the bot speaking. Returns the time it is at (3000 ms).
async fn bot_speaking(rig: &Rig) {
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1300).await;
    rig.final_of(&["tell me a story"]).await;
    rig.at(2000).await;
    assert_eq!(rig.count("user_turn_ended:0"), 1);
    rig.up(ControlKind::TtsStarted, 0).await;
    rig.at(3000).await;
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_clears_the_transport_then_interrupts_then_starts_the_turn() {
    let config_engine = EngineConfig::DEFAULT;
    let shared: SharedClock = Arc::new(SimClock::new());
    let (input, commands) = common::loopback::open(&shared, &config_engine)
        .await
        .unwrap();
    let rig = Rig::with(config(FIXED, InterruptionConfig::OFF), None, Some(commands)).await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await;
    rig.at(3200).await;
    let frames = rig.frames();
    assert_eq!(
        frames[3..],
        [
            (3000, "interrupt:1".to_owned()),
            (3000, "user_turn_started:1".to_owned())
        ],
        "Interrupt precedes UserTurnStarted: {frames:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!((stats.interrupts, stats.clears_sent), (1, 1), "{stats:?}");
    assert_eq!((stats.clear_errors, stats.clear_timeouts), (0, 0));
    // The transport was cleared for the new epoch, at the moment of the decision.
    let event = input.recv_client(Duration::from_secs(1)).await;
    match event {
        Some(brisvox_transport::ClientEvent::Clear { epoch, at }) => {
            assert_eq!(epoch, ep(1));
            assert_eq!(at, rig.t(3000));
        }
        other => panic!("expected the clear, got {other:?}"),
    }
    // The timeline recorded the interruption of epoch 0 and the speech start of epoch 1.
    let history = rig.timeline.history();
    let interrupted = history.iter().find(|t| t.epoch() == ep(0)).unwrap();
    assert_eq!(interrupted.at(TurnEvent::Interrupted), Some(rig.t(3000)));
    let current = rig.timeline.current().unwrap();
    assert_eq!(current.epoch(), ep(1));
    assert_eq!(current.at(TurnEvent::UserSpeechStart), Some(rig.t(3000)));

    // A stale BotAudioEnded of the interrupted epoch changes nothing.
    rig.up(ControlKind::TtsEnded, 0).await;
    assert_eq!(rig.machine().state, StateKind::UserSpeaking);
    assert_eq!(
        rig.machine()
            .ignored
            .ignored(brisvox_agent::InputKind::BotAudioEnded),
        1
    );
    // The new turn runs to its end.
    rig.at(4100).await;
    rig.vad_stop(4000).await;
    rig.at(4200).await;
    rig.final_of(&["wait stop"]).await;
    rig.at(5000).await;
    assert_eq!(
        rig.frames()[5..],
        [
            (4700, "transcript:1:true:wait stop".to_owned()),
            (4700, "user_turn_ended:1".to_owned())
        ]
    );
}

/// Only the echo guard: no minimum speech, no backchannels, a 400 ms window needing one STT word.
fn echo_guard_only() -> InterruptionConfig {
    let mut guards = InterruptionConfig::OFF;
    guards.echo_guard = 400 * MS;
    guards.echo_min_words = 1;
    guards
}

#[tokio::test(start_paused = true)]
async fn the_echo_window_needs_stt_words_before_a_barge_in_counts() {
    let rig = Rig::new(config(FIXED, echo_guard_only()), None).await;
    bot_speaking(&rig).await; // the bot's audio started at 2000 ms
    // 3000 ms is past the first 400 ms window; a second bot audio burst reopens it (3000-3400).
    rig.up(ControlKind::TtsStarted, 0).await;
    rig.vad_start(3000).await;
    rig.at(3300).await;
    assert_eq!(
        rig.count("interrupt"),
        0,
        "VAD alone cannot interrupt inside the echo window (no other guard is on)"
    );
    assert_eq!(rig.machine().interruption.confirmed(), 0);
    // One STT word inside the window confirms the barge-in at once.
    rig.interim("wait a second").await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
    assert_eq!(rig.machine().interruption.confirmed(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_echo_window_ends_by_the_clock_and_vad_alone_then_interrupts() {
    let rig = Rig::new(config(FIXED, echo_guard_only()), None).await;
    bot_speaking(&rig).await;
    rig.up(ControlKind::TtsStarted, 0).await; // reopens the window at 3000
    rig.vad_start(3000).await;
    rig.at(3399).await;
    assert_eq!(rig.count("interrupt"), 0, "{:?}", rig.labels());
    rig.at(3450).await;
    assert_eq!(
        rig.count("interrupt:1"),
        1,
        "the window is over: {:?}",
        rig.labels()
    );
}

#[tokio::test(start_paused = true)]
async fn control_run_without_the_second_bot_audio_the_same_vad_interrupts_at_once() {
    // The same timeline as above without the second `TtsStarted`: the first window ended at
    // 2400, so VAD alone interrupts. This proves the window, not another guard, held it back.
    let rig = Rig::new(config(FIXED, echo_guard_only()), None).await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
}

#[tokio::test(start_paused = true)]
async fn a_backchannel_never_interrupts_and_its_stop_is_still_answered() {
    let mut guards = InterruptionConfig::RECOMMENDED;
    guards.vad_hangover = Duration::ZERO;
    let rig = Rig::new(config(FIXED, guards), None).await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await;
    rig.at(3100).await;
    rig.interim("mm-hmm").await;
    rig.at(3300).await;
    rig.vad_stop(3200).await;
    rig.at(3400).await;
    rig.final_of(&["mm-hmm"]).await;
    rig.at(6000).await;
    assert_eq!(rig.count("interrupt"), 0, "{:?}", rig.labels());
    assert_eq!(
        rig.count("user_turn_started"),
        1,
        "no new turn for a backchannel"
    );
    let machine = rig.machine();
    assert_eq!(machine.interruption.vetoed(VetoReason::Backchannel), 1);
    assert_eq!(machine.state, StateKind::BotSpeaking, "the bot goes on");
    // The vetoed stop's final was consumed: nothing is owed and the machine is in step.
    assert_eq!(rig.stats.snapshot().stray_finals, 0);
    assert_eq!(machine.ignored.total_ignored(), 0, "{:?}", machine.ignored);
}

// ---------------------------------------------------------------------------------------------
// Hangup, backstop, DTMF, lifecycle

#[tokio::test(start_paused = true)]
async fn hangup_mid_speech_ends_the_turn_without_a_response() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.interim("I would like").await;
    rig.at(600).await;
    rig.send(SystemFrame::Stop.into()).await;
    rig.at(30_000).await;
    assert_eq!(
        rig.labels(),
        ["user_turn_started:0", "stop"],
        "{:?}",
        rig.labels()
    );
    assert_eq!(rig.machine().state, StateKind::Closed);
    // The stage ended with the hang-up: nothing is accepted any more, nothing was emitted.
    assert!(
        rig.running
            .input()
            .send(SystemFrame::Start.into())
            .await
            .is_err()
    );
    assert_eq!(rig.count("transcript"), 0);
}

#[tokio::test(start_paused = true)]
async fn hangup_drops_a_held_end() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1800).await; // ended by the machine, held for the final
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    rig.send(SystemFrame::Stop.into()).await;
    rig.at(10_000).await;
    assert_eq!(
        rig.labels(),
        ["user_turn_started:0", "stop"],
        "the held end was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn the_response_deadline_closes_a_turn_that_gets_no_bot_audio() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&["hello"]).await;
    rig.at(1750).await;
    assert_eq!(rig.count("user_turn_ended"), 1);
    assert_eq!(rig.machine().state, StateKind::AwaitingResponse);
    // The turn ended at 1700 ms: the 20 s backstop expires at 21.7 s.
    rig.at(21_000).await;
    assert_eq!(rig.stats.snapshot().response_timeouts, 0, "not yet");
    assert_eq!(rig.machine().state, StateKind::AwaitingResponse);
    rig.at(21_800).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.response_timeouts, 1, "{stats:?}");
    assert_eq!(
        rig.machine().state,
        StateKind::Idle,
        "a synthetic BotAudioEnded"
    );
}

#[tokio::test(start_paused = true)]
async fn a_response_that_outlasts_the_deadline_is_never_cut_off() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&["hello"]).await;
    rig.at(16_000).await;
    // The bot's audio starts (sent once, at the first handoff) and plays far longer than the
    // 20 s backstop: a long answer.
    rig.up(ControlKind::TtsStarted, 0).await;
    assert_eq!(rig.machine().state, StateKind::BotSpeaking);
    rig.at(90_000).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.response_timeouts, 0, "{stats:?}");
    assert_eq!(
        rig.machine().state,
        StateKind::BotSpeaking,
        "the backstop did not end the bot's turn mid-speech"
    );
    // So the user's speech is still a barge-in, with an Interrupt.
    rig.vad_start(90_000).await;
    rig.at(90_100).await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
    assert_eq!(rig.count("user_turn_started:1"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_response_deadline_does_not_fire_when_the_bot_finishes() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&["hello"]).await;
    rig.at(2000).await;
    rig.up(ControlKind::TtsStarted, 0).await;
    rig.at(4000).await;
    rig.up(ControlKind::TtsEnded, 0).await;
    assert_eq!(rig.machine().state, StateKind::Idle);
    rig.at(60_000).await;
    assert_eq!(rig.stats.snapshot().response_timeouts, 0);
}

#[tokio::test(start_paused = true)]
async fn tts_ended_alone_cancels_the_response_backstop() {
    // The bot's turn ends without any audio (`TtsStarted` is never sent: nothing was
    // synthesized): `TtsEnded` is the only report, and it must disarm the backstop.
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&["hello"]).await;
    rig.at(2000).await;
    assert_eq!(rig.count("user_turn_ended:0"), 1);
    rig.up(ControlKind::TtsEnded, 0).await;
    assert_eq!(rig.machine().state, StateKind::Idle);
    rig.at(40_000).await;
    assert_eq!(
        rig.stats.snapshot().response_timeouts,
        0,
        "the backstop must not fire for a turn that already finished"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_is_forwarded_ends_the_stage_and_no_deferred_analysis_runs_after_it() {
    let model = RecordingModel::new(0.9);
    let rig = Rig::new(
        config(adaptive(false), InterruptionConfig::OFF),
        Some(Arc::clone(&model) as Arc<dyn TurnModel>),
    )
    .await;
    rig.vad_start(0).await;
    for frame in &frames_from(rig.origin, 0, 5) {
        rig.send(frame.clone().into()).await;
    }
    rig.at(1100).await;
    rig.vad_stop(1000).await; // no audio after the stop: the analysis is deferred
    assert_eq!(rig.stats.snapshot().analyses_deferred, 1);
    rig.send(SystemFrame::Cancel.into()).await;
    rig.at(1500).await; // past the 200 ms bound
    assert_eq!(model.calls().len(), 0, "the cancelled analysis never ran");
    assert_eq!(rig.stats.snapshot().analyses_started, 0);
    assert_eq!(rig.count("cancel"), 1, "{:?}", rig.labels());
    // `Cancel` is terminal for the stage (the pipeline ends it after `process`): nothing is
    // accepted any more.
    assert!(
        rig.running
            .input()
            .send(SystemFrame::Start.into())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn the_owed_ledger_is_bounded_and_overflow_is_counted() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    // 65 stops, none answered: the clock never advances, so no turn ends.
    for i in 0..65 {
        rig.vad_start(i * 20).await;
        rig.vad_stop(i * 20 + 10).await;
    }
    let stats = rig.stats.snapshot();
    assert_eq!((stats.vad_stops, stats.owed_overflow), (65, 1), "{stats:?}");
    // 64 stops are still remembered: 64 finals are consumed, the 65th is a stray.
    for _ in 0..65 {
        rig.final_of(&[]).await;
    }
    assert_eq!(rig.stats.snapshot().stray_finals, 1);
}

#[tokio::test(start_paused = true)]
async fn a_final_over_the_text_bound_is_dropped_and_counted() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    rig.vad_start(0).await;
    rig.at(600).await;
    rig.vad_stop(500).await;
    rig.at(700).await;
    rig.final_of(&[&"x".repeat(64 * 1024 + 1)]).await;
    rig.at(3000).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.text_dropped, stats.empty_turns, stats.turns_ended),
        (1, 1, 0),
        "{stats:?}"
    );
    assert_eq!(rig.count("transcript"), 0);
}

#[tokio::test(start_paused = true)]
async fn the_text_of_one_turn_is_bounded_and_the_overflowing_segment_is_dropped() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    let part = "y".repeat(40_000);
    rig.vad_start(0).await;
    rig.at(600).await;
    rig.vad_stop(500).await;
    rig.at(700).await;
    rig.final_of(&[&part]).await;
    rig.at(900).await;
    rig.vad_start(900).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&[&part]).await; // 80 000 bytes would exceed 64 KiB: this segment is dropped
    rig.at(5000).await;
    assert_eq!(rig.stats.snapshot().text_dropped, 1);
    assert_eq!(
        rig.labels()[1..],
        [
            format!("transcript:0:true:{part}"),
            "user_turn_ended:0".to_owned()
        ],
        "the turn is answered with the text that fits"
    );
}

#[tokio::test(start_paused = true)]
async fn other_upstream_frames_are_forwarded_without_blocking_and_drops_are_counted() {
    let clock = SimClock::new();
    let driver = TurnDriver::new(
        config(FIXED, InterruptionConfig::OFF),
        Arc::new(clock),
        TimelineRecorder::new(),
    );
    let stats = driver.stats();
    // The upstream neighbour never reads: its lanes hold two frames each.
    let mut neighbour = brisvox_core::Inbox::new(brisvox_core::QueueConfig {
        system_capacity: 2,
        data_capacity: 2,
        ..brisvox_core::QueueConfig::DEFAULT
    });
    let running = Pipeline::new("upstream")
        .then(driver)
        .then(Tail {
            seen: Arc::default(),
            clock,
        })
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
                ControlFrame {
                    epoch: TurnEpoch::ZERO,
                    kind: ControlKind::UserTurnStarted,
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
        "the full lane dropped frames instead of blocking the driver: delivered {delivered}, dropped {dropped}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_custom_interruption_policy_replaces_the_configured_one() {
    // The configuration has every guard OFF (a barge-in would interrupt at once); the policy
    // given to `with_interruption` vetoes the phrase "okey dokey" as a backchannel.
    let mut policy = InterruptionPolicy::disabled();
    policy.backchannels = vec![BackchannelSet::new(
        brisvox_agent::Language::En,
        ["okey dokey"],
    )];
    let rig = Rig::with_policy(config(FIXED, InterruptionConfig::OFF), policy).await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await;
    rig.at(3100).await;
    rig.interim("okey dokey").await;
    rig.at(3300).await;
    rig.vad_stop(3200).await;
    rig.at(3400).await;
    rig.final_of(&["okey dokey"]).await;
    rig.at(6000).await;
    assert_eq!(rig.count("interrupt"), 0, "{:?}", rig.labels());
    let machine = rig.machine();
    assert_eq!(machine.interruption.vetoed(VetoReason::Backchannel), 1);
    assert_eq!(machine.state, StateKind::BotSpeaking, "the bot goes on");
}

#[tokio::test(start_paused = true)]
async fn dtmf_is_retagged_with_the_current_epoch() {
    let rig = Rig::new(config(FIXED, InterruptionConfig::OFF), None).await;
    let press = |digit| {
        DataFrame::Dtmf(DtmfFrame {
            epoch: TurnEpoch::ZERO,
            digit,
            at: rig.clock.now(),
        })
        .into()
    };
    rig.send(press(DtmfDigit::D5)).await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await; // epoch 1 (barge-in)
    rig.send(press(DtmfDigit::Pound)).await;
    let dtmf: Vec<String> = rig
        .labels()
        .into_iter()
        .filter(|l| l.starts_with("dtmf"))
        .collect();
    assert_eq!(dtmf, ["dtmf:0:D5", "dtmf:1:Pound"]);
    assert_eq!(rig.stats.snapshot().dtmf, 2);
}

#[tokio::test(start_paused = true)]
async fn nothing_outlives_the_pipeline() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    {
        let rig = Rig::new(config(adaptive(false), InterruptionConfig::OFF), model(0.9)).await;
        rig.vad_start(0).await;
        rig.at(1100).await;
        rig.vad_stop(1000).await;
        rig.at(1200).await;
        // Dropped with a pending analysis result, the gate and the timer armed.
    }
    counter
        .wait_for_baseline(&clock, Duration::from_secs(1))
        .await
        .expect("the timer or an analysis task outlived the driver");
}

// ---------------------------------------------------------------------------------------------
// The Smart Turn ring, transport clear failures, analysis timeouts

#[tokio::test(start_paused = true)]
async fn the_ring_holds_exactly_the_turns_audio_and_is_cleared_when_the_turn_ends() {
    let model = RecordingModel::new(0.9);
    let rig = Rig::new(
        config(adaptive(false), InterruptionConfig::OFF),
        Some(Arc::clone(&model) as Arc<dyn TurnModel>),
    )
    .await;
    rig.vad_start(0).await;
    // Audio at another rate never reaches the ring (counted), the rest does, in order.
    for i in 0..3 {
        rig.send(square_frame(rig.origin, i, SampleRate::Hz8000, 9_000).into())
            .await;
    }
    let first = frames_from(rig.origin, 0, 10);
    for frame in &first {
        rig.send(frame.clone().into()).await;
    }
    rig.at(300).await;
    rig.vad_stop(100).await; // dated inside the audio: the analysis starts at once
    assert_eq!(model.calls().len(), 1);
    let expected = features_of(&samples_of(&first));
    assert_eq!(
        model.calls()[0],
        expected,
        "the snapshot is exactly the 16 kHz audio, oldest first"
    );
    // The check discriminates: another order or a missing frame gives other features.
    let mut reversed = first.clone();
    reversed.reverse();
    assert_ne!(features_of(&samples_of(&reversed)), expected);
    assert_ne!(features_of(&samples_of(&first[1..])), expected);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.audio_frames, 13, "{stats:?}");
    assert_eq!(stats.audio_wrong_rate, 3, "{stats:?}");

    // The turn ends (verdict complete + the final): the ring is cleared.
    rig.final_of(&["one"]).await;
    assert_eq!(rig.count("user_turn_ended:0"), 1);

    // The next turn's pause sees only the next turn's audio.
    rig.at(2000).await;
    rig.vad_start(2000).await;
    let second = frames_from(rig.origin, 100, 6);
    for frame in &second {
        rig.send(frame.clone().into()).await;
    }
    rig.at(2300).await;
    rig.vad_stop(2050).await;
    assert_eq!(model.calls().len(), 2);
    assert_eq!(
        model.calls()[1],
        features_of(&samples_of(&second)),
        "nothing of the previous turn's tail is analysed again"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stop_that_overtakes_queued_audio_waits_for_the_ring_to_catch_up() {
    let model = RecordingModel::new(0.9);
    let rig = Rig::new(
        config(adaptive(false), InterruptionConfig::OFF),
        Some(Arc::clone(&model) as Arc<dyn TurnModel>),
    )
    .await;
    rig.vad_start(0).await;
    let all = frames_from(rig.origin, 0, 10);
    for frame in &all[..5] {
        rig.send(frame.clone().into()).await;
    }
    rig.at(300).await;
    // The driver's inbox is behind: the second half of the audio is queued (data lane) when the
    // stop (system lane) arrives, and the stop overtakes it.
    let input = rig.running.input();
    for frame in &all[5..] {
        input.try_send(frame.clone().into()).unwrap();
    }
    input
        .try_send(SystemFrame::UserStoppedSpeaking { at: rig.t(100) }.into())
        .unwrap();
    rig.settle().await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        stats.analyses_deferred, 1,
        "the stop overtook the audio: {stats:?}"
    );
    assert_eq!(model.calls().len(), 1, "one analysis, after the audio");
    // It starts with the first frame stamped at the stop (index 5 = 100 ms): the ring holds all
    // the audio up to the stop, not the truncated tail the stop alone would have seen.
    assert_eq!(
        model.calls()[0],
        features_of(&samples_of(&all[..6])),
        "the model saw the audio up to the stop"
    );
    assert_ne!(
        model.calls()[0],
        features_of(&samples_of(&all[..5])),
        "the truncated tail is different"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deferred_analysis_starts_at_its_bound_and_is_dropped_when_the_user_resumes() {
    let model = RecordingModel::new(0.9);
    let rig = Rig::new(
        config(adaptive(false), InterruptionConfig::OFF),
        Some(Arc::clone(&model) as Arc<dyn TurnModel>),
    )
    .await;
    rig.vad_start(0).await;
    let frames = frames_from(rig.origin, 0, 5); // up to 80 ms
    for frame in &frames {
        rig.send(frame.clone().into()).await;
    }
    rig.at(1100).await;
    rig.vad_stop(1000).await; // no audio after the stop (a leg that suppresses silence)
    assert_eq!(model.calls().len(), 0, "deferred");
    rig.at(1250).await;
    assert_eq!(model.calls().len(), 0, "still inside the 200 ms bound");
    rig.at(1350).await;
    assert_eq!(model.calls().len(), 1, "the bound released it");
    assert_eq!(model.calls()[0], features_of(&samples_of(&frames)));

    // A second pause whose analysis is deferred, then the user resumes: it never runs.
    rig.vad_start(1400).await;
    rig.at(1500).await;
    rig.vad_stop(1450).await;
    assert_eq!(rig.stats.snapshot().analyses_deferred, 2);
    rig.vad_start(1500).await;
    rig.at(1900).await;
    assert_eq!(model.calls().len(), 1, "the stale analysis was dropped");
}

/// A barge-in with a transport clear built by `hook`; returns the rig after the new turn
/// started.
async fn barge_in_with(hook: ClearHook) -> Rig {
    let rig = Rig::build(
        config(FIXED, InterruptionConfig::OFF),
        None,
        None,
        Some(hook),
        None,
    )
    .await;
    bot_speaking(&rig).await;
    rig.vad_start(3000).await;
    rig
}

#[tokio::test(start_paused = true)]
async fn a_clear_that_never_completes_times_out_and_the_interrupt_still_follows() {
    let hook: ClearHook = Arc::new(|_| Box::pin(std::future::pending()));
    let rig = barge_in_with(hook).await;
    rig.at(4900).await;
    assert_eq!(
        rig.count("interrupt"),
        0,
        "the driver waits for the clear, bounded by send_timeout (2 s)"
    );
    rig.at(5100).await;
    let frames = rig.frames();
    assert_eq!(
        frames[3..],
        [
            (5000, "interrupt:1".to_owned()),
            (5000, "user_turn_started:1".to_owned())
        ],
        "{frames:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.clear_timeouts, stats.clears_sent, stats.clear_errors),
        (1, 0, 0),
        "{stats:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_clear_that_fails_is_counted_and_the_interrupt_still_follows() {
    let hook: ClearHook = Arc::new(|_| Box::pin(async { Err("transport closed".to_owned()) }));
    let rig = barge_in_with(hook).await;
    rig.at(3100).await;
    let frames = rig.frames();
    assert_eq!(
        frames[3..],
        [
            (3000, "interrupt:1".to_owned()),
            (3000, "user_turn_started:1".to_owned())
        ],
        "{frames:?}"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.clear_errors, stats.clears_sent, stats.clear_timeouts),
        (1, 0, 0),
        "{stats:?}"
    );
}

/// `AudioOutStopped` of the interrupted epoch 0, if the timeline has it.
fn audio_out_stopped(rig: &Rig) -> Option<Instant> {
    rig.timeline
        .history()
        .iter()
        .find(|t| t.epoch() == ep(0))
        .and_then(|t| t.at(TurnEvent::AudioOutStopped))
}

#[tokio::test(start_paused = true)]
async fn the_driver_marks_the_audio_stopped_when_its_clear_is_applied() {
    // The clear takes 20 ms: the mark is when it returned, not when the barge-in was decided.
    // No stage after the driver marks the timeline here, so the mark is the driver's own.
    let hook: ClearHook = Arc::new(|_| {
        Box::pin(async {
            // The rig's paused tokio clock: any `SimClock` sleeps on it.
            SimClock::new().sleep(20 * MS).await;
            Ok(())
        })
    });
    let rig = barge_in_with(hook).await;
    rig.at(3100).await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
    let interrupted = rig.timeline.history();
    let interrupted = interrupted.iter().find(|t| t.epoch() == ep(0)).unwrap();
    assert_eq!(interrupted.at(TurnEvent::Interrupted), Some(rig.t(3000)));
    assert_eq!(
        audio_out_stopped(&rig),
        Some(rig.t(3020)),
        "{interrupted:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_clear_that_fails_marks_no_audio_stopped() {
    let hook: ClearHook = Arc::new(|_| Box::pin(async { Err("transport closed".to_owned()) }));
    let rig = barge_in_with(hook).await;
    rig.at(3100).await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
    assert_eq!(
        audio_out_stopped(&rig),
        None,
        "a clear that was not applied stopped nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_before_any_bot_audio_marks_no_audio_stopped() {
    // The same barge-in as `barge_in_with`, but the bot's answer never reached the transport
    // (`TtsStarted` is never sent): the clear is applied, yet no audio of epoch 0 was playing.
    let hook: ClearHook = Arc::new(|_| Box::pin(async { Ok(()) }));
    let rig = Rig::build(
        config(FIXED, InterruptionConfig::OFF),
        None,
        None,
        Some(hook),
        None,
    )
    .await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1300).await;
    rig.final_of(&["tell me a story"]).await;
    rig.at(2000).await;
    assert_eq!(rig.count("user_turn_ended:0"), 1);
    rig.vad_start(2500).await;
    rig.at(2600).await;
    assert_eq!(rig.count("interrupt:1"), 1, "{:?}", rig.labels());
    assert_eq!(rig.stats.snapshot().clears_sent, 1);
    assert_eq!(audio_out_stopped(&rig), None);
}

#[tokio::test(start_paused = true)]
async fn a_hanging_analysis_times_out_and_the_fallback_silence_ends_the_turn() {
    let (release, hold) = std::sync::mpsc::channel::<()>();
    let model: Arc<dyn TurnModel> = Arc::new(HangingModel {
        release: Mutex::new(hold),
    });
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 4,
            name: "turn-driver-test".to_owned(),
        },
        SystemClock::shared(),
    )
    .unwrap();
    let mut cfg = config(adaptive(false), InterruptionConfig::OFF);
    cfg.analysis_timeout = Duration::from_secs(1);
    let rig = Rig::build(
        cfg,
        Some(model),
        None,
        None,
        Some(Offload::Pool(pool.handle())),
    )
    .await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1300).await;
    rig.final_of(&["something"]).await;
    rig.at(2000).await;
    assert_eq!(rig.stats.snapshot().analyses_failed, 0, "still waiting");
    rig.at(2300).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.analyses_failed, 1, "timed out after 1 s: {stats:?}");
    assert_eq!(rig.count("user_turn_ended"), 0, "no verdict ended the turn");
    assert!(
        rig.machine().state != StateKind::Idle,
        "the pause is still open for the fallback"
    );
    rig.at(5000).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (4000, "transcript:0:true:something".to_owned()),
            (4000, "user_turn_ended:0".to_owned())
        ],
        "the fallback silence ended the turn"
    );
    drop(rig);
    drop(release);
    pool.shutdown();
}

/// The Smart Turn executor with every thread busy and its queue full: the analysis is
/// refused at once (`PoolSaturated`), counts as a failed analysis and the turn ends at its
/// fallback deadline; it never waits for the executor and never hangs.
#[tokio::test(start_paused = true)]
#[allow(clippy::disallowed_methods)] // real time: the executor's threads are not on the paused clock
async fn an_analysis_refused_by_a_saturated_executor_fails_and_the_fallback_ends_the_turn() {
    let executor = brisvox_engine::offload::start_smart_turn_executor(1, SystemClock::shared())
        .unwrap()
        .unwrap();
    let handle = executor.handle();
    let capacity = brisvox_engine::offload::SMART_TURN_QUEUE_PER_THREAD;
    // One job occupies the thread, `capacity` more fill the queue; all wait for the gate. The
    // first one is let start before the others are submitted: submitted together, the others
    // can fill the queue before the executor's thread has taken the first job out of it (a
    // slow thread start, as on a loaded CI runner), and the last one is refused for the wrong
    // reason, leaving the queue one short of full for good.
    let (release, hold) = std::sync::mpsc::channel::<()>();
    let hold = Arc::new(Mutex::new(hold));
    let (started, running) = std::sync::mpsc::channel::<()>();
    let mut fillers = Vec::new();
    let spawn_filler = |signal: Option<std::sync::mpsc::Sender<()>>| {
        let (handle, hold) = (handle.clone(), Arc::clone(&hold));
        tokio::spawn(async move {
            handle
                .run(Duration::from_secs(3600), move || {
                    if let Some(signal) = signal {
                        let _ = signal.send(());
                    }
                    let _ = hold.lock().unwrap_or_else(PoisonError::into_inner).recv();
                })
                .await
        })
    };
    fillers.push(spawn_filler(Some(started)));
    let mut spins = 0;
    while running.try_recv().is_err() {
        spins += 1;
        assert!(
            spins < 10_000,
            "the executor never started the first job: {:?}",
            executor.metrics()
        );
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(200));
    }
    for _ in 0..capacity {
        fillers.push(spawn_filler(None));
    }
    let mut spins = 0;
    while executor.metrics().queue_depth < capacity {
        spins += 1;
        assert!(
            spins < 10_000,
            "the executor never filled: {:?}",
            executor.metrics()
        );
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(200));
    }
    let before = executor.metrics().rejected;

    let rig = Rig::build(
        config(adaptive(false), InterruptionConfig::OFF),
        model(0.9),
        None,
        None,
        Some(Offload::Pool(handle.clone())),
    )
    .await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1300).await;
    rig.final_of(&["something"]).await;
    rig.at(1400).await;
    let mut stats = rig.stats.snapshot();
    for _ in 0..100 {
        if stats.analyses_failed > 0 {
            break;
        }
        tokio::task::yield_now().await;
        stats = rig.stats.snapshot();
    }
    assert_eq!(stats.analyses_failed, 1, "refused at once: {stats:?}");
    assert!(
        executor.metrics().rejected > before,
        "the executor refused the analysis: {:?}",
        executor.metrics()
    );
    assert_eq!(rig.count("user_turn_ended"), 0, "no verdict ended the turn");
    rig.at(5000).await;
    assert_eq!(
        rig.frames()[1..],
        [
            (4000, "transcript:0:true:something".to_owned()),
            (4000, "user_turn_ended:0".to_owned())
        ],
        "the fallback silence ended the turn"
    );
    drop(rig);
    drop(release);
    for filler in fillers {
        let _ = filler.await;
    }
    executor.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Determinism and the table of scenarios

async fn mixed_run() -> (Vec<(u64, String)>, Vec<TurnAction>, Instant) {
    let rig = Rig::new(
        config(adaptive(true), InterruptionConfig::RECOMMENDED),
        model(0.9),
    )
    .await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1500).await;
    rig.final_of(&["tell me a story"]).await;
    rig.at(2500).await;
    rig.up(ControlKind::TtsStarted, 0).await;
    rig.at(3000).await;
    rig.vad_start(3000).await;
    rig.at(3200).await;
    rig.interim("no wait please").await;
    rig.at(4100).await;
    rig.vad_stop(4000).await;
    rig.at(4400).await;
    rig.final_of(&["no wait please stop"]).await;
    rig.at(9000).await;
    (rig.frames(), rig.probe.actions(), rig.origin)
}

#[tokio::test(start_paused = true)]
async fn the_same_input_twice_gives_identical_frames_and_actions() {
    let first = mixed_run().await;
    let second = mixed_run().await;
    assert_eq!(first.0, second.0);
    assert!(!first.0.is_empty());
    // The actions carry instants of different runs: compare them relative to each run's origin,
    // so a different timer deadline fails.
    let shape = |actions: &[TurnAction], origin: Instant| -> Vec<String> {
        actions
            .iter()
            .map(|a| match a {
                TurnAction::ArmTimer { deadline } => {
                    format!("arm+{:?}", deadline.saturating_duration_since(origin))
                }
                other => format!("{other:?}"),
            })
            .collect()
    };
    let (a, b) = (shape(&first.1, first.2), shape(&second.1, second.2));
    assert!(a.iter().any(|s| s.starts_with("arm+")), "{a:?}");
    assert_eq!(a, b);
}

/// One step of a scenario: when it is delivered (ms after the origin) and what it is.
#[derive(Clone, Debug)]
enum Step {
    Start { at: u64 },
    Stop { at: u64 },
    Interim(&'static str),
    Final(&'static str),
    BotStarted(u32),
    BotEnded(u32),
    Hangup,
}

struct Scenario {
    name: &'static str,
    turn: TurnPolicy,
    interruption: InterruptionConfig,
    /// The Smart Turn model's probability, if there is one.
    model: Option<f32>,
    steps: Vec<(u64, Step)>,
    /// Run until this time (ms) so that every armed deadline fires.
    until: u64,
}

/// The bare machine with the caller's duties: it keeps the one timer slot, delivers a `Tick` at
/// every armed deadline, and answers every analysis request with the model's verdict at once.
struct Simulation {
    machine: TurnMachine,
    armed: Option<Instant>,
    actions: Vec<TurnAction>,
    model: Option<f32>,
}

impl Simulation {
    fn new(scenario: &Scenario) -> Self {
        Self {
            machine: TurnMachine::with_interruption(scenario.turn, scenario.interruption.policy()),
            armed: None,
            actions: Vec::new(),
            model: scenario.model,
        }
    }

    /// Applies `input`, tracks the timer slot, and returns the verdicts to feed next.
    fn feed(&mut self, input: TurnInput, now: Instant) -> Vec<TurnInput> {
        let mut out = Vec::new();
        self.machine.handle(input, &mut out);
        let mut verdicts = Vec::new();
        for action in &out {
            match action {
                TurnAction::ArmTimer { deadline } => self.armed = Some(*deadline),
                TurnAction::CancelTimer => self.armed = None,
                TurnAction::RequestAnalysis { id } => {
                    if let Some(p) = self.model {
                        verdicts.push(TurnInput::Verdict {
                            request: *id,
                            complete: p > 0.5,
                            probability: p,
                            at: now,
                        });
                    }
                }
                _ => {}
            }
        }
        self.actions.extend(out);
        verdicts
    }

    /// Fires every timer armed up to and including `limit`, each at its own deadline.
    fn tick_until(&mut self, limit: Instant) {
        while let Some(deadline) = self.armed {
            if deadline > limit {
                break;
            }
            self.armed = None;
            let _ = self.feed(TurnInput::Tick { now: deadline }, deadline);
        }
    }
}

/// The steps grouped by delivery time: steps with the same time are delivered together (queued
/// back to back, no yield in between), the way a stop and a new speech start arrive together.
fn groups(steps: &[(u64, Step)]) -> Vec<(u64, &[(u64, Step)])> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some((ms, _)) = steps.get(from) {
        let len = steps[from..].iter().take_while(|(t, _)| t == ms).count();
        out.push((*ms, &steps[from..from + len]));
        from += len;
    }
    out
}

/// The bare machine's actions for `scenario`: the delivery of each step at its time, a `Tick`
/// at every armed deadline, and a verdict for every analysis request, fed after the inputs
/// delivered together with the request (the analysis job only runs once the driver has handled
/// what was already queued).
fn expected_actions(scenario: &Scenario, origin: Instant) -> Vec<TurnAction> {
    let mut sim = Simulation::new(scenario);
    for (ms, group) in groups(&scenario.steps) {
        let now = origin + Duration::from_millis(ms);
        sim.tick_until(now - Duration::from_nanos(1));
        let mut verdicts = Vec::new();
        for (_, step) in group {
            let input = sim_input(step, origin, now);
            verdicts.extend(sim.feed(input, now));
        }
        for verdict in verdicts {
            let _ = sim.feed(verdict, now);
        }
    }
    sim.tick_until(origin + Duration::from_millis(scenario.until));
    sim.actions
}

fn sim_input(step: &Step, origin: Instant, now: Instant) -> TurnInput {
    {
        match step {
            Step::Start { at } => TurnInput::VadStart {
                at: origin + Duration::from_millis(*at),
            },
            Step::Stop { at } => TurnInput::VadStop {
                at: origin + Duration::from_millis(*at),
            },
            Step::Interim(text) => TurnInput::SttInterim {
                text: (*text).to_owned(),
                at: now,
            },
            Step::Final(text) => TurnInput::SttFinal {
                text: (*text).to_owned(),
                at: now,
            },
            Step::BotStarted(epoch) => TurnInput::BotAudioStarted {
                epoch: ep(*epoch),
                at: now,
            },
            Step::BotEnded(epoch) => TurnInput::BotAudioEnded {
                epoch: ep(*epoch),
                at: now,
            },
            Step::Hangup => TurnInput::Hangup { at: now },
        }
    }
}

/// Plays `scenario` through the driver and returns the actions it carried out.
async fn driven_actions(scenario: &Scenario) -> (Vec<TurnAction>, Instant) {
    let probability = scenario.model;
    let rig = Rig::new(
        config(scenario.turn, scenario.interruption),
        probability.map(|p| Arc::new(FixedModel(p)) as Arc<dyn TurnModel>),
    )
    .await;
    for (ms, group) in groups(&scenario.steps) {
        rig.at(ms).await;
        for (_, step) in group {
            match step {
                // Queued back to back; the settle after the group lets the driver run.
                Step::Start { at } => rig
                    .running
                    .input()
                    .try_send(SystemFrame::UserStartedSpeaking { at: rig.t(*at) }.into())
                    .unwrap(),
                Step::Stop { at } => rig
                    .running
                    .input()
                    .try_send(SystemFrame::UserStoppedSpeaking { at: rig.t(*at) }.into())
                    .unwrap(),
                Step::Interim(text) => rig.interim(text).await,
                Step::Final(text) => rig.final_of(&[text]).await,
                Step::BotStarted(epoch) => rig.up(ControlKind::TtsStarted, *epoch).await,
                Step::BotEnded(epoch) => rig.up(ControlKind::TtsEnded, *epoch).await,
                Step::Hangup => rig.send(SystemFrame::Stop.into()).await,
            }
        }
        rig.settle().await;
    }
    rig.at(scenario.until).await;
    (rig.probe.actions(), rig.origin)
}

fn scenarios() -> Vec<Scenario> {
    use Step::{BotEnded, BotStarted, Final, Hangup, Interim, Start, Stop};
    let guards = {
        let mut guards = InterruptionConfig::RECOMMENDED;
        guards.vad_hangover = Duration::ZERO;
        guards
    };
    let story = vec![
        (0, Start { at: 0 }),
        (1_100, Stop { at: 1_000 }),
        (1_300, Final("tell me a story")),
    ];
    let with = |extra: Vec<(u64, Step)>| -> Vec<(u64, Step)> {
        story.iter().cloned().chain(extra).collect()
    };
    vec![
        Scenario {
            name: "fixed silence ends the turn",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: vec![
                (0, Start { at: 0 }),
                (100, Interim("hello")),
                (1_100, Stop { at: 1_000 }),
                (1_300, Final("hello there")),
            ],
            until: 4_000,
        },
        Scenario {
            name: "fixed silence, the user resumes inside it",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: vec![
                (0, Start { at: 0 }),
                (600, Stop { at: 500 }),
                (800, Final("I want")),
                (900, Start { at: 900 }),
                (1_600, Stop { at: 1_500 }),
                (1_900, Final("to book")),
            ],
            until: 5_000,
        },
        Scenario {
            name: "adaptive, complete verdict",
            turn: adaptive(false),
            interruption: InterruptionConfig::OFF,
            model: Some(0.9),
            steps: story.clone(),
            until: 6_000,
        },
        Scenario {
            name: "adaptive, complete verdict that waits for the final",
            turn: adaptive(true),
            interruption: InterruptionConfig::OFF,
            model: Some(0.9),
            steps: story.clone(),
            until: 6_000,
        },
        Scenario {
            name: "adaptive, incomplete verdict, punctuated final shortens the wait",
            turn: adaptive(true),
            interruption: InterruptionConfig::OFF,
            model: Some(0.1),
            steps: vec![
                (0, Start { at: 0 }),
                (1_100, Stop { at: 1_000 }),
                (1_500, Final("Is that right?")),
            ],
            until: 7_000,
        },
        Scenario {
            name: "adaptive, incomplete verdict falls back",
            turn: adaptive(false),
            interruption: InterruptionConfig::OFF,
            model: Some(0.1),
            steps: story.clone(),
            until: 7_000,
        },
        Scenario {
            name: "adaptive without a model falls back",
            turn: adaptive(false),
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: story.clone(),
            until: 7_000,
        },
        Scenario {
            name: "barge-in with every guard off",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: with(vec![
                (2_000, BotStarted(0)),
                (2_500, Start { at: 2_500 }),
                (3_600, Stop { at: 3_500 }),
                (3_800, Final("stop")),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "backchannel candidate vetoed",
            turn: FIXED,
            interruption: guards,
            model: None,
            steps: with(vec![
                (2_000, BotStarted(0)),
                (3_000, Start { at: 3_000 }),
                (3_100, Interim("mm-hmm")),
                (3_400, Stop { at: 3_300 }),
                (3_500, Final("mm-hmm")),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "barge-in confirmed by words inside the echo window",
            turn: FIXED,
            interruption: guards,
            model: None,
            steps: with(vec![
                (2_000, BotStarted(0)),
                (2_100, Start { at: 2_100 }),
                (2_200, Interim("wait a second")),
                (2_900, Stop { at: 2_800 }),
                (3_100, Final("wait a second please")),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "adaptive, the user resumes while the analysis is pending (stale verdict)",
            turn: adaptive(false),
            interruption: InterruptionConfig::OFF,
            model: Some(0.9),
            steps: vec![
                (0, Start { at: 0 }),
                (1_100, Stop { at: 1_000 }),
                (1_100, Start { at: 1_100 }),
                (2_100, Stop { at: 2_000 }),
                (2_300, Final("part one")),
                (2_400, Final("part two")),
            ],
            until: 8_000,
        },
        Scenario {
            name: "adaptive, complete verdict, the final never comes (final_grace expires)",
            turn: adaptive(true),
            interruption: InterruptionConfig::OFF,
            model: Some(0.9),
            steps: vec![(0, Start { at: 0 }), (1_100, Stop { at: 1_000 })],
            until: 8_000,
        },
        Scenario {
            name: "barge-in while awaiting the response",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: with(vec![
                (2_000, Start { at: 2_000 }),
                (3_100, Stop { at: 3_000 }),
                (3_300, Final("actually")),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "stale bot audio started and ended after an interrupt",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: with(vec![
                (2_000, BotStarted(0)),
                (2_500, Start { at: 2_500 }),
                (2_600, BotStarted(0)),
                (2_700, BotEnded(0)),
                (3_600, Stop { at: 3_500 }),
                (3_800, Final("stop")),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "repeated bot audio started",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: with(vec![
                (2_000, BotStarted(0)),
                (2_100, BotStarted(0)),
                (3_000, BotEnded(0)),
            ]),
            until: 8_000,
        },
        Scenario {
            name: "a second final in the same pause",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: with(vec![(1_400, Final("again"))]),
            until: 8_000,
        },
        Scenario {
            name: "an empty turn, then a new turn",
            turn: FIXED,
            interruption: InterruptionConfig::OFF,
            model: None,
            steps: vec![
                (0, Start { at: 0 }),
                (1_100, Stop { at: 1_000 }),
                (1_300, Final("")),
                (1_701, BotEnded(0)), // what the driver feeds itself when the empty turn is released at 1700
                (3_000, Start { at: 3_000 }),
                (4_100, Stop { at: 4_000 }),
                (4_300, Final("hello")),
            ],
            until: 8_000,
        },
        Scenario {
            name: "hangup in a pause",
            turn: adaptive(false),
            interruption: InterruptionConfig::OFF,
            model: Some(0.1),
            steps: vec![
                (0, Start { at: 0 }),
                (1_100, Stop { at: 1_000 }),
                (1_200, Hangup),
            ],
            until: 6_000,
        },
    ]
}

#[tokio::test(start_paused = true)]
async fn scenarios_replay_through_the_driver_with_the_machines_exact_actions() {
    for scenario in scenarios() {
        let (driven, origin) = driven_actions(&scenario).await;
        let expected = expected_actions(&scenario, origin);
        assert!(!expected.is_empty(), "{}", scenario.name);
        assert_eq!(driven, expected, "{}", scenario.name);
    }
}

// ---------------------------------------------------------------------------------------------
// Conversation events

/// A rig whose driver publishes its events and takes typed messages.
async fn observed(config: TurnDriverConfig) -> (Rig, EventStream, TextTurnSender) {
    let events = EngineEvents::new(64);
    let stream = events.subscribe();
    let (text, input) = text_turns(8);
    let rig = Rig::build_with(config, None, None, None, None, None, Some((events, input))).await;
    (rig, stream, text)
}

/// The events published so far.
fn drain(stream: &mut EventStream) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    while let Some(Some(event)) = futures::FutureExt::now_or_never(stream.recv()) {
        out.push(event);
    }
    out
}

fn started(n: u32) -> EngineEvent {
    EngineEvent::UserStartedSpeaking { epoch: ep(n) }
}

fn stopped(n: u32) -> EngineEvent {
    EngineEvent::UserStoppedSpeaking { epoch: ep(n) }
}

fn user_text(n: u32, text: &str) -> EngineEvent {
    EngineEvent::UserText {
        epoch: ep(n),
        text: Arc::from(text),
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_turn_still_stops_the_user_speaking() {
    let (rig, mut events, _text) = observed(config(FIXED, InterruptionConfig::OFF)).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.final_of(&[]).await; // a cough: no words
    rig.at(2000).await;
    assert_eq!(rig.stats.snapshot().empty_turns, 1);
    assert_eq!(drain(&mut events), [started(0), stopped(0)]);
    rig.vad_start(2100).await;
    assert_eq!(drain(&mut events), [started(1)]);
}

#[tokio::test(start_paused = true)]
async fn a_superseded_held_end_stops_before_the_new_turn_starts() {
    let cfg = config(TurnPolicy::fixed(100 * MS), InterruptionConfig::OFF);
    let (rig, mut events, _text) = observed(cfg).await;
    rig.vad_start(0).await;
    rig.at(1050).await;
    rig.vad_stop(1000).await;
    rig.at(1200).await;
    rig.vad_start(1200).await; // resumes while the end is held
    rig.at(1500).await;
    rig.final_of(&["tell me"]).await;
    rig.at(2100).await;
    rig.vad_stop(2000).await;
    rig.at(2200).await;
    rig.final_of(&["a story"]).await;
    rig.at(4000).await;
    assert_eq!(rig.stats.snapshot().ends_superseded, 1);
    assert_eq!(
        drain(&mut events),
        [
            started(0),
            stopped(0),
            started(1),
            user_text(1, "tell me a story"),
            stopped(1),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_hang_up_stops_an_open_or_held_turn() {
    // Mid-speech.
    let (rig, mut events, _text) = observed(config(FIXED, InterruptionConfig::OFF)).await;
    rig.vad_start(0).await;
    rig.at(500).await;
    rig.send(SystemFrame::Stop.into()).await;
    assert_eq!(drain(&mut events), [started(0), stopped(0)]);

    // Held for its final.
    let (rig, mut events, _text) = observed(config(FIXED, InterruptionConfig::OFF)).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1800).await;
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    rig.send(SystemFrame::Stop.into()).await;
    assert_eq!(drain(&mut events), [started(0), stopped(0)]);
}

#[tokio::test(start_paused = true)]
async fn a_typed_turn_raises_no_speaking_events() {
    let (rig, mut events, text) = observed(config(FIXED, InterruptionConfig::OFF)).await;
    // To an idle bot: only the text.
    text.try_send(TextTurn::now("hello")).unwrap();
    rig.at(100).await;
    assert_eq!(rig.stats.snapshot().text_turns, 1);
    assert_eq!(drain(&mut events), [user_text(0, "hello")]);

    // While the bot speaks: the barge-in, then the text.
    rig.up(ControlKind::TtsStarted, 0).await;
    rig.at(500).await;
    text.try_send(TextTurn::now("stop")).unwrap();
    rig.at(600).await;
    assert_eq!(
        drain(&mut events),
        [
            EngineEvent::BotInterrupted { epoch: ep(1) },
            user_text(1, "stop"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_typed_message_that_takes_over_a_held_spoken_turn_stops_it() {
    let (rig, mut events, text) = observed(config(FIXED, InterruptionConfig::OFF)).await;
    rig.vad_start(0).await;
    rig.at(1100).await;
    rig.vad_stop(1000).await;
    rig.at(1800).await; // ended by the machine, held for the final
    assert_eq!(rig.stats.snapshot().ends_held, 1);
    text.try_send(TextTurn::now("and quickly")).unwrap();
    rig.at(1900).await;
    rig.final_of(&["book a table"]).await;
    rig.at(4000).await;
    let got = drain(&mut events);
    // Whatever the machine made of it: the spoken turn stopped once, before anything else,
    // and the typed text started no speaking.
    assert_eq!(got.first(), Some(&started(0)), "{got:?}");
    assert_eq!(got.get(1), Some(&stopped(0)), "{got:?}");
    assert!(
        !got[2..].iter().any(|event| matches!(
            event,
            EngineEvent::UserStartedSpeaking { .. } | EngineEvent::UserStoppedSpeaking { .. }
        )),
        "{got:?}"
    );
}
