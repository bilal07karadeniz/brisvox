//! The TTS stage against a provider and a downstream that stop taking things: a full
//! provider input channel (send timeout, lost context, the response still ends), an interrupt
//! that abandons a send waiting for room instead of waiting out `send_timeout`, a full provider
//! cancel channel, and a wedged consumer behind the reader task.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation
)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::{
    ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, TextFrame,
};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SharedClock, SimClock, TimelineRecorder, TurnEpoch,
};
use brisvox_engine::{
    CallRatesCell, EngineConfig, PlayoutBook, RoundKey, TtsStage, TtsStageConfig, TtsStats,
};
use brisvox_providers::{
    ContextId, Result as ProviderResult, TtsConfig, TtsEvent, TtsInput, TtsProvider, TtsSession,
};
use brisvox_transport::CallRates;
use bytes::Bytes;
use common::{Recorded, Recorder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn text(epoch: u32, text: &str) -> Frame {
    Frame::Data(DataFrame::LlmText(TextFrame {
        epoch: TurnEpoch::new(epoch),
        text: Arc::from(text),
    }))
}

fn control(epoch: u32, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind,
    })
}

fn interrupt(epoch: u32) -> Frame {
    Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(epoch),
    })
}

/// The far ends of the session a [`HeldTts`] handed out.
struct Held {
    input: mpsc::Receiver<TtsInput>,
    cancel: mpsc::Receiver<ContextId>,
    events: mpsc::Sender<TtsEvent>,
}

/// A provider whose channels have the capacities the test picks and whose far ends the test
/// holds: nothing reads the input or the cancel channel until the test does, and the events are
/// whatever the test sends.
#[derive(Clone)]
struct HeldTts {
    input_capacity: usize,
    cancel_capacity: usize,
    held: Arc<Mutex<Option<Held>>>,
}

impl HeldTts {
    fn new(input_capacity: usize, cancel_capacity: usize) -> Self {
        Self {
            input_capacity,
            cancel_capacity,
            held: Arc::new(Mutex::new(None)),
        }
    }

    fn with_held<R>(&self, f: impl FnOnce(&mut Held) -> R) -> R {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        f(held.as_mut().expect("the stage connected"))
    }

    /// The inputs the stage got through so far, in order (drains the channel).
    fn inputs(&self) -> Vec<TtsInput> {
        self.with_held(|held| {
            let mut out = Vec::new();
            while let Ok(input) = held.input.try_recv() {
                out.push(input);
            }
            out
        })
    }

    fn cancels(&self) -> Vec<u64> {
        self.with_held(|held| {
            let mut out = Vec::new();
            while let Ok(id) = held.cancel.try_recv() {
                out.push(id.0);
            }
            out
        })
    }

    fn events(&self) -> mpsc::Sender<TtsEvent> {
        self.with_held(|held| held.events.clone())
    }
}

impl TtsProvider for HeldTts {
    fn name(&self) -> &'static str {
        "held"
    }

    async fn connect(&self, _config: TtsConfig) -> ProviderResult<TtsSession> {
        let (input_tx, input_rx) = mpsc::channel(self.input_capacity);
        let (cancel_tx, cancel_rx) = mpsc::channel(self.cancel_capacity);
        let (events_tx, events_rx) = mpsc::channel(64);
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(Held {
            input: input_rx,
            cancel: cancel_rx,
            events: events_tx,
        });
        Ok(TtsSession {
            input: input_tx,
            cancel: cancel_tx,
            events: events_rx,
        })
    }
}

/// A stage behind which nothing ever moves again: it takes one frame and never returns.
struct Wedge;

impl Processor for Wedge {
    fn name(&self) -> &'static str {
        "wedge"
    }

    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        _frame: Frame,
        _direction: Direction,
        _ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

struct Rig {
    running: RunningPipeline,
    rec: Recorded,
    clock: SharedClock,
    tts: HeldTts,
    stats: TtsStats,
    book: Arc<PlayoutBook>,
}

impl Rig {
    /// A stage over `tts` with the given send timeout; `wedged` puts a [`Wedge`] between the
    /// stage and the recorder.
    async fn new(tts: HeldTts, send_timeout: Duration, wedged: bool) -> Self {
        let clock = SimClock::shared();
        let mut config = TtsStageConfig::from_engine(&EngineConfig::DEFAULT);
        config.send_timeout = send_timeout;
        let book = PlayoutBook::shared(16);
        let stage = TtsStage::new(
            Arc::new(tts.clone()),
            config,
            clock.clone(),
            TimelineRecorder::new(),
        )
        .with_book(Arc::clone(&book))
        .with_rates(CallRatesCell::new(CallRates::symmetric(
            SampleRate::Hz24000,
        )));
        let stats = stage.stats();
        let rec = Recorded::default();
        let pipeline = Pipeline::new("tts-faults").then(stage);
        let pipeline = if wedged {
            pipeline.then(Wedge)
        } else {
            pipeline
        };
        let running = pipeline
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        let rig = Self {
            running,
            rec,
            clock,
            tts,
            stats,
            book,
        };
        rig.settle().await;
        rig
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    async fn settle(&self) {
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    }

    async fn run_for(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    fn count_tts_ended(&self, epoch: u32) -> usize {
        self.rec
            .all()
            .iter()
            .filter(|(frame, _)| {
                matches!(
                    frame,
                    Frame::Control(ControlFrame { epoch: e, kind: ControlKind::TtsEnded })
                        if e.get() == epoch
                )
            })
            .count()
    }

    fn saw_interrupt(&self) -> bool {
        self.rec
            .all()
            .iter()
            .any(|(frame, _)| matches!(frame, Frame::System(SystemFrame::Interrupt { .. })))
    }
}

fn key(epoch: u32, round: u32) -> RoundKey {
    RoundKey::new(TurnEpoch::new(epoch), round)
}

fn is_text(input: &TtsInput, context: u64) -> bool {
    matches!(input, TtsInput::Text { context: c, .. } if c.0 == context)
}

#[tokio::test(start_paused = true)]
async fn a_send_that_never_fits_times_out_loses_the_context_and_the_response_still_ends() {
    // Input channel of one slot that nobody empties.
    let rig = Rig::new(HeldTts::new(1, 4), 500 * MS, false).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hello there.")).await; // takes the slot
    rig.feed(text(1, " And then some more.")).await; // waits for room
    rig.run_for(100 * MS).await;
    assert_eq!(rig.stats.snapshot().send_failures, 0, "still waiting");
    rig.run_for(600 * MS).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.send_failures, 1, "{stats:?}");
    assert_eq!(stats.contexts_lost, 1, "{stats:?}");
    assert_eq!(stats.text_sent, 1, "{stats:?}");
    assert!(
        rig.book.snapshot(key(1, 0)).unwrap().lost,
        "the utterance is marked lost"
    );
    // The rest of the round is dropped without waiting again, and the end of the response does
    // not wait for the idle timeout: the lost context no longer holds it back.
    rig.feed(text(1, " Even more text.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(50 * MS).await;
    assert_eq!(rig.count_tts_ended(1), 1, "{:?}", rig.rec.all().len());
    assert_eq!(rig.stats.snapshot().send_failures, 1);
    assert_eq!(rig.stats.snapshot().stalls, 0);
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_abandons_a_send_waiting_for_room_instead_of_waiting_out_the_timeout() {
    // A send timeout far longer than the test: only the interrupt can end the wait.
    let tts = HeldTts::new(1, 4);
    let rig = Rig::new(tts.clone(), Duration::from_secs(30), false).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hello there.")).await;
    rig.feed(text(1, " Blocked behind the first.")).await;
    rig.run_for(200 * MS).await;
    assert!(!rig.saw_interrupt());
    rig.feed(interrupt(2)).await;
    rig.run_for(10 * MS).await;
    // The stage was stuck in the send; the interrupt got it out and through within 10 ms.
    assert!(rig.saw_interrupt(), "the interrupt passed the stage");
    let stats = rig.stats.snapshot();
    assert_eq!(
        stats.send_failures, 0,
        "abandoned, not timed out: {stats:?}"
    );
    assert_eq!(stats.cancels, 1, "{stats:?}");
    assert_eq!(
        rig.tts.cancels(),
        [1],
        "the abandoned context is still cancelled"
    );
    assert!(
        rig.book.snapshot(key(1, 0)).unwrap().lost,
        "the unsent text must not count as heard"
    );
    // The next turn's text reaches the provider at once (the old inputs are drained as a real
    // provider would; the cancelled context's text is its business).
    let old = rig.tts.inputs();
    assert!(old.iter().all(|input| is_text(input, 1)), "{old:?}");
    rig.feed(control(2, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(2, "Okay then.")).await;
    rig.run_for(10 * MS).await;
    let fresh = rig.tts.inputs();
    assert!(
        fresh.iter().any(|input| is_text(input, 2)),
        "the new epoch's text went out within 10 ms: {fresh:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_full_cancel_channel_is_counted_and_does_not_block_the_interrupt() {
    // Room for the texts and flushes, one slot for cancels that nobody reads.
    let tts = HeldTts::new(16, 1);
    let rig = Rig::new(tts.clone(), Duration::from_secs(2), false).await;
    for round in 0..2 {
        rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
        rig.feed(text(1, &format!("Round {round} speaks."))).await;
    }
    rig.run_for(10 * MS).await;
    assert_eq!(rig.stats.snapshot().contexts, 2);
    rig.feed(interrupt(2)).await;
    rig.run_for(10 * MS).await;
    assert!(rig.saw_interrupt());
    let stats = rig.stats.snapshot();
    assert_eq!(stats.cancels, 2, "{stats:?}");
    assert_eq!(stats.cancel_overflow, 1, "{stats:?}");
    assert_eq!(tts.cancels().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_frees_the_reader_task_from_a_consumer_that_stopped() {
    let tts = HeldTts::new(16, 4);
    let rig = Rig::new(tts.clone(), Duration::from_secs(2), true).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hello there.")).await;
    rig.run_for(10 * MS).await;
    // Ten seconds of audio for context 1, in one-second events: the wedge holds the first
    // frame, the queue behind it fills up, and the reader waits for room.
    let events = tts.events();
    let second = Bytes::from(vec![7u8; 48_000]);
    for _ in 0..10 {
        events
            .send(TtsEvent::Audio {
                context: ContextId(1),
                audio: second.clone(),
            })
            .await
            .unwrap();
    }
    rig.run_for(100 * MS).await;
    let stuck = rig.stats.snapshot();
    assert_eq!(
        stuck.audio_events, 1,
        "the reader waits in the first event: {stuck:?}"
    );
    assert_eq!(stuck.unknown_events, 0);
    // Barge-in: the wait ends, the rest of the old audio is recognised as belonging to a
    // cancelled context and skipped.
    rig.feed(interrupt(2)).await;
    rig.run_for(100 * MS).await;
    let freed = rig.stats.snapshot();
    assert_eq!(freed.audio_events, 10, "{freed:?}");
    assert_eq!(freed.unknown_events, 9, "{freed:?}");
    assert!(freed.stale_dropped > 0, "{freed:?}");
}
