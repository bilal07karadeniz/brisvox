//! The transport-out stage against a real `TransportSession` over the loopback
//! transport and the pump: the epoch `Clear` rule, two consecutive turns, stale audio after a
//! barge-in, end marks and their timeout, `TtsStarted`, words and audio in the book, and the
//! absence of a second pacer.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, WordTimestamp,
};
use brisvox_core::{
    Direction, EpochPolicy, Links, Pipeline, Processor, ProcessorContext, RunningPipeline,
    SharedClock, SimClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_engine::{
    EngineConfig, PlayoutBook, Pump, PumpConfig, PumpHandle, PumpLinks, RoundKey, TransportOut,
    TransportOutConfig, TransportOutStats,
};
use brisvox_transport::{
    ClientEvent, CommandSender, LoopbackClient, LoopbackConfig, LoopbackTransport, MarkAcks,
    SessionConfig, Transport, TransportCommand, TransportSession, UtteranceId,
};
use bytes::Bytes;
use common::Recorded;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn control(epoch: u32, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind,
    })
}

fn started(epoch: u32, utterance: u64) -> Frame {
    control(epoch, ControlKind::UtteranceStarted { utterance })
}

fn interrupt(epoch: u32) -> Frame {
    Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(epoch),
    })
}

fn word(epoch: u32, utterance: u64, text: &str, start_ms: u64) -> Frame {
    Frame::Data(DataFrame::Word(Box::new(WordTimestamp {
        epoch: TurnEpoch::new(epoch),
        utterance,
        word: Arc::from(text),
        start: Duration::from_millis(start_ms),
    })))
}

/// A head that forwards downstream frames and records what comes back upstream.
struct Head(Recorded);

impl Processor for Head {
    fn name(&self) -> &'static str {
        "head"
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
        if direction == Direction::Upstream {
            self.0.record(frame, direction);
            return Ok(());
        }
        ctx.forward(frame, direction).await
    }
}

struct Rig {
    running: RunningPipeline,
    client: LoopbackClient,
    pump: PumpHandle,
    commands: CommandSender,
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    timeline: TimelineRecorder,
    stats: TransportOutStats,
    /// What came back upstream out of the stage.
    upstream: Recorded,
}

struct Options {
    acks: MarkAcks,
    session: SessionConfig,
    loopback: LoopbackConfig,
    config: TransportOutConfig,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            acks: MarkAcks::Auto {
                delay: Duration::ZERO,
            },
            session: SessionConfig {
                position_interval: 20 * MS,
                ..SessionConfig::default()
            },
            loopback: LoopbackConfig {
                latency: Duration::ZERO,
                ..LoopbackConfig::default()
            },
            config: TransportOutConfig::default(),
        }
    }
}

impl Rig {
    async fn new() -> Self {
        Self::with(Options::default()).await
    }

    async fn with(options: Options) -> Self {
        let clock = SimClock::shared();
        let (mut transport, client) = LoopbackTransport::new(
            clock.clone(),
            LoopbackConfig {
                mark_acks: options.acks,
                ..options.loopback
            },
        );
        let cancel = CancellationToken::new();
        let link = transport.accept(&cancel).await.unwrap().unwrap();
        let session = TransportSession::open(clock.clone(), link, options.session, &cancel)
            .await
            .unwrap();
        let commands = session.commands();
        let book = PlayoutBook::shared(16);
        let timeline = TimelineRecorder::new();
        let upstream = Recorded::default();
        let stage = TransportOut::new(
            commands.clone(),
            Arc::clone(&book),
            options.config,
            clock.clone(),
            timeline.clone(),
        );
        let stats = stage.stats();
        let running = Pipeline::new("out")
            .then(Head(upstream.clone()))
            .then_with_queue(stage, EngineConfig::DEFAULT.queues.sink)
            .start(Links::default(), &cancel)
            .await
            .unwrap();
        let pump = Pump::spawn(
            session,
            PumpLinks {
                input: running.input().clone(),
                upstream: Some(running.upstream_input().clone()),
            },
            Arc::clone(&book),
            clock.clone(),
            PumpConfig::from_engine(&EngineConfig::DEFAULT),
            &cancel,
        );
        let rig = Self {
            running,
            client,
            pump,
            commands,
            book,
            clock,
            timeline,
            stats,
            upstream,
        };
        rig.run_for(MS).await;
        rig
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    async fn run_for(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    /// `n` 20 ms frames of epoch `epoch`, numbered from `from`.
    async fn audio(&self, epoch: u32, from: u64, n: u64) {
        for seq in from..from + n {
            self.feed(pcm(epoch, seq)).await;
        }
    }

    /// Everything the client has received so far.
    async fn client_events(&mut self) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::ZERO, self.client.recv()).await {
            events.push(event);
        }
        events
    }

    /// The upstream frames as `(kind, epoch)`.
    fn up(&self) -> Vec<(&'static str, u32)> {
        self.upstream
            .all()
            .into_iter()
            .filter_map(|(frame, direction)| match frame {
                Frame::Control(c) if direction == Direction::Upstream => {
                    Some((c.kind.name(), c.epoch.get()))
                }
                _ => None,
            })
            .collect()
    }

    fn register(&self, epoch: u32, round: u32, utterance: u64) {
        let key = RoundKey::new(TurnEpoch::new(epoch), round);
        self.book.register_round(key);
        self.book.register_utterance(key, utterance);
    }
}

fn audio_frame(epoch: u32, seq: u64) -> AudioFrame {
    AudioFrame::new(
        Bytes::from(vec![1u8; 640]),
        SampleRate::Hz16000,
        tokio::time::Instant::now(),
        seq,
        TurnEpoch::new(epoch),
    )
    .unwrap()
}

fn pcm(epoch: u32, seq: u64) -> Frame {
    audio_frame(epoch, seq).into()
}

/// `(kind, epoch)` of the audio, mark and clear events a client received.
fn kinds(events: &[ClientEvent]) -> Vec<(&'static str, u32)> {
    events
        .iter()
        .map(|event| match event {
            ClientEvent::Audio { frame, .. } => ("audio", frame.epoch().get()),
            ClientEvent::Mark { .. } => ("mark", 0),
            ClientEvent::Clear { epoch, .. } => ("clear", epoch.get()),
            ClientEvent::Hangup { .. } => ("hangup", 0),
        })
        .collect()
}

fn audio_times(events: &[ClientEvent]) -> Vec<tokio::time::Instant> {
    events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Audio { received_at, .. } => Some(*received_at),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn two_consecutive_turns_without_a_barge_in_both_reach_the_client() {
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    // Turn 1.
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 10).await;
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(Duration::from_secs(1)).await;
    assert_eq!(
        rig.up(),
        [("tts_started", 1), ("tts_ended", 1)],
        "bot audio started and ended"
    );
    // Turn 2: a higher epoch and no Interrupt (nobody barged in).
    rig.feed(started(2, 2)).await;
    rig.audio(2, 100, 10).await;
    rig.feed(control(2, ControlKind::TtsEnded)).await;
    rig.run_for(Duration::from_secs(1)).await;
    assert_eq!(
        rig.up(),
        [
            ("tts_started", 1),
            ("tts_ended", 1),
            ("tts_started", 2),
            ("tts_ended", 2)
        ]
    );
    let events = rig.client_events().await;
    let kinds = kinds(&events);
    // Each epoch is announced to the transport (Clear) before its audio, and ends in a mark.
    let mut expected = vec![("clear", 1)];
    expected.extend(std::iter::repeat_n(("audio", 1), 10));
    expected.push(("mark", 0));
    expected.push(("clear", 2));
    expected.extend(std::iter::repeat_n(("audio", 2), 10));
    expected.push(("mark", 0));
    assert_eq!(kinds, expected);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.audio_frames, 20);
    assert_eq!(stats.clears, 2);
    assert_eq!(stats.audio_ms, 400);
}

#[tokio::test(start_paused = true)]
async fn stale_audio_never_reaches_the_wire_after_a_barge_in() {
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 50).await; // a second of audio, paced out at 20 ms
    rig.run_for(210 * MS).await;
    // The turn driver's fast path: Clear straight to the transport, before the Interrupt frame
    // has travelled through the stages. Audio of epoch 1 still in the stage's queue and audio the
    // TTS produces late must not get out.
    rig.commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(2),
        })
        .await
        .unwrap();
    rig.audio(1, 50, 5).await; // late audio of the cut epoch, queued before the interrupt arrives
    rig.run_for(10 * MS).await;
    rig.feed(interrupt(2)).await;
    rig.audio(1, 55, 5).await; // after the interrupt the pipeline gate drops these
    rig.feed(started(2, 2)).await;
    rig.audio(2, 200, 5).await;
    rig.feed(control(2, ControlKind::TtsEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let events = rig.client_events().await;
    let clear = events
        .iter()
        .position(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2))
        .expect("the client was cleared to epoch 2");
    assert!(
        events[clear..].iter().all(|e| !matches!(
            e,
            ClientEvent::Audio { frame, .. } if frame.epoch().get() == 1
        )),
        "stale audio after the clear: {:?}",
        kinds(&events[clear..])
    );
    let after: Vec<_> = kinds(&events[clear..]);
    assert_eq!(after.iter().filter(|k| **k == ("audio", 2)).count(), 5);
    // Only one clear to epoch 2: the stage's own is idempotent with the driver's.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2))
            .count(),
        1
    );
    // The five frames of epoch 1 that came after the interrupt were dropped by the stage and
    // counted; the five before it were handed over (the transport rejected them: it had been
    // cleared to epoch 2) and the client got none of them.
    let stats = rig.stats.snapshot();
    assert_eq!(stats.stale_dropped, 5, "{stats:?}");
    assert!(stats.audio_frames >= 55, "{stats:?}");
    let epoch1 = events
        .iter()
        .filter(|e| matches!(e, ClientEvent::Audio { frame, .. } if frame.epoch().get() == 1))
        .count();
    assert!(
        epoch1 < 12 && epoch1 as u64 + 5 < stats.audio_frames,
        "{epoch1} of {stats:?}"
    );
    // Playout of epoch 1 stopped at the interrupt (the cut response's missing end is tested in
    // `a_cut_epoch_never_reports_an_end_however_its_end_signals_arrive`).
    rig.timeline.finish();
    assert!(
        rig.timeline
            .history()
            .iter()
            .any(|r| r.at(TurnEvent::AudioOutStopped).is_some())
    );
    assert!(rig.up().contains(&("tts_ended", 2)));
}

#[tokio::test(start_paused = true)]
async fn the_interrupt_leaves_the_audio_out_stopped_mark_the_driver_made() {
    // The turn driver marks `AudioOutStopped` when its fast-path
    // Clear returned; the stage's own mark on the Interrupt would be a repeat, so it is skipped.
    let rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 10).await;
    rig.run_for(100 * MS).await;
    let driver_mark = rig.clock.now();
    rig.timeline
        .mark(TurnEpoch::new(1), TurnEvent::AudioOutStopped, driver_mark);
    rig.run_for(30 * MS).await;
    let before = rig.timeline.stats();
    rig.feed(interrupt(2)).await;
    rig.run_for(50 * MS).await;
    assert_eq!(
        rig.timeline.stats().repeated,
        before.repeated,
        "no second mark"
    );
    rig.timeline.finish();
    let record = rig
        .timeline
        .history()
        .into_iter()
        .find(|r| r.epoch() == TurnEpoch::new(1))
        .unwrap();
    assert_eq!(record.at(TurnEvent::AudioOutStopped), Some(driver_mark));
}

#[tokio::test(start_paused = true)]
async fn only_the_sessions_pacer_spaces_the_output() {
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    let start = rig.clock.now();
    rig.audio(1, 0, 25).await;
    rig.run_for(5 * MS).await;
    // The stage handed all 25 frames (half a second) to the session at once ...
    assert_eq!(rig.stats.snapshot().audio_frames, 25);
    assert!(rig.clock.now() - start < 10 * MS);
    rig.run_for(Duration::from_secs(1)).await;
    // ... and the client still got them 20 ms apart: that is the transport's own pacer.
    let events = rig.client_events().await;
    let times = audio_times(&events);
    assert_eq!(times.len(), 25);
    let gaps: Vec<_> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.iter().all(|gap| *gap == 20 * MS), "{gaps:?}");
}

#[tokio::test(start_paused = true)]
async fn the_first_handoff_marks_first_audio_out_and_tells_the_driver_once_per_epoch() {
    let rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    rig.run_for(100 * MS).await;
    assert!(
        rig.up().is_empty(),
        "nothing is started before audio is handed over"
    );
    let before = rig.clock.now();
    rig.audio(1, 0, 5).await;
    rig.run_for(10 * MS).await;
    assert_eq!(rig.up(), [("tts_started", 1)]);
    assert_eq!(rig.stats.snapshot().started_sent, 1);
    rig.timeline.finish();
    let record = rig
        .timeline
        .history()
        .into_iter()
        .find(|r| r.at(TurnEvent::FirstAudioOut).is_some())
        .unwrap();
    let at = record.at(TurnEvent::FirstAudioOut).unwrap();
    assert!(
        at >= before && at - before < 5 * MS,
        "marked at the real handoff"
    );
}

#[tokio::test(start_paused = true)]
async fn the_end_is_a_unique_mark_whose_echo_ends_the_response_once() {
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    for (epoch, utterance) in [(1u32, 1u64), (2, 2)] {
        rig.feed(started(epoch, utterance)).await;
        rig.audio(epoch, 0, 5).await;
        rig.feed(control(epoch, ControlKind::TtsEnded)).await;
        rig.run_for(Duration::from_secs(1)).await;
    }
    let events = rig.client_events().await;
    let ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Mark { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "unique ids");
    // The echoes (auto-acked) reached the pump, which resolved them in the book and sent the
    // ends upstream; the stage let each through once.
    let ended: Vec<_> = rig
        .up()
        .into_iter()
        .filter(|(k, _)| *k == "tts_ended")
        .collect();
    assert_eq!(ended, [("tts_ended", 1), ("tts_ended", 2)]);
    assert!(
        rig.book
            .snapshot(RoundKey::new(TurnEpoch::new(1), 0))
            .unwrap()
            .marked_played
    );
    assert_eq!(rig.stats.snapshot().marks_sent, 2);
    assert_eq!(rig.stats.snapshot().mark_timeouts, 0);
    assert_eq!(rig.pump.stats().end_marks, 2);
}

#[tokio::test(start_paused = true)]
async fn a_client_that_never_acks_is_given_the_mark_timeout_then_the_response_ends() {
    let mut rig = Rig::with(Options {
        acks: MarkAcks::Manual,
        config: TransportOutConfig {
            mark_timeout: 3 * Duration::from_secs(1),
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    let t0 = rig.clock.now();
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 10).await; // 200 ms of audio
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    // The timeout counts from the end of the audio (0.2 s), so the deadline is at 3.2 s. A
    // deadline taken from the moment `TtsEnded` arrived (t0 + 3 s) would have fired by 3.1 s.
    rig.run_for(3100 * MS).await;
    assert!(
        !rig.up().contains(&("tts_ended", 1)),
        "not yet at 3.1 s: {:?}",
        rig.up()
    );
    assert_eq!(rig.stats.snapshot().mark_timeouts, 0);
    rig.run_for(200 * MS).await;
    assert!(rig.up().contains(&("tts_ended", 1)), "{:?}", rig.up());
    let at = rig.clock.now() - t0;
    assert!(at < 4 * Duration::from_secs(1));
    assert_eq!(rig.stats.snapshot().mark_timeouts, 1);
    // A late echo is a duplicate and does not end the response again.
    let events = rig.client_events().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            ClientEvent::Mark { id, .. } => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    rig.client.ack_mark(&id).await.unwrap();
    rig.run_for(100 * MS).await;
    assert_eq!(
        rig.up().iter().filter(|u| **u == ("tts_ended", 1)).count(),
        1
    );
    assert_eq!(rig.stats.snapshot().duplicate_ends, 1);
}

#[tokio::test(start_paused = true)]
async fn a_response_that_sent_no_audio_ends_at_once_without_a_mark() {
    let mut rig = Rig::new().await;
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(10 * MS).await;
    assert_eq!(rig.up(), [("tts_ended", 1)]);
    let stats = rig.stats.snapshot();
    assert_eq!((stats.direct_ends, stats.marks_sent), (1, 0));
    assert!(
        rig.client_events().await.is_empty(),
        "nothing reached the wire"
    );
    // A second end for the same epoch is dropped by the stage. (In the real chain the LLM
    // runner's own upstream `TtsEnded` for an empty turn never passes this stage: it travels
    // from the runner up to the turn driver, which therefore sees two ends for a silent turn.
    // That is harmless, the turn machine ignores `BotAudioEnded` while idle; the hybrid test
    // `a_silent_turn_ends_for_the_driver_and_the_next_turn_still_speaks` counts them.)
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(10 * MS).await;
    assert_eq!(rig.up().len(), 1);
    assert_eq!(rig.stats.snapshot().duplicate_ends, 1);
}

#[tokio::test(start_paused = true)]
async fn words_and_audio_durations_are_written_to_the_book() {
    let rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.book.push_sent(1, "Hello there");
    rig.feed(started(1, 1)).await;
    rig.feed(word(1, 1, "Hello", 0)).await;
    rig.feed(word(1, 1, "there", 200)).await;
    rig.audio(1, 0, 15).await; // 300 ms
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(Duration::from_secs(1)).await;
    let snapshot = rig
        .book
        .snapshot(RoundKey::new(TurnEpoch::new(1), 0))
        .unwrap();
    assert_eq!(snapshot.total_audio, 300 * MS);
    assert_eq!(
        snapshot.released,
        300 * MS,
        "the pump delivered the final position"
    );
    assert!(snapshot.marked_played);
    // The words made it: at 250 ms the text up to "there" was heard.
    let heard = snapshot
        .played
        .heard(250 * MS, &brisvox_agent::PlayedConfig::default());
    assert_eq!(&*heard.text, "Hello there");
    assert_eq!(rig.stats.snapshot().words, 2);
}

#[tokio::test(start_paused = true)]
async fn audio_without_an_utterance_is_dropped_and_counted() {
    let mut rig = Rig::new().await;
    rig.audio(1, 0, 3).await;
    // And audio of another epoch than the announced utterance's.
    rig.feed(started(1, 1)).await;
    rig.audio(2, 10, 3).await;
    rig.run_for(100 * MS).await;
    assert_eq!(rig.stats.snapshot().unattributed, 6);
    assert!(rig.client_events().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_burst_larger_than_the_queues_waits_for_room_and_loses_nothing() {
    // The test reads the client's events only at the end: give its buffers room for all of them.
    let mut rig = Rig::with(Options {
        loopback: LoopbackConfig {
            latency: Duration::ZERO,
            max_buffered: 4096,
            client_queue: 4096,
            ..LoopbackConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    // 300 frames (6 s of audio) fed as fast as the stage takes them.
    let feeder = async {
        for seq in 0..300 {
            rig.feed(pcm(1, seq)).await;
        }
    };
    let drain = rig.run_for(10 * Duration::from_secs(1));
    tokio::join!(feeder, drain);
    let events = rig.client_events().await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ClientEvent::Audio { .. }))
            .count(),
        300
    );
    let stats = rig.stats.snapshot();
    assert!(
        stats.send_waits > 0,
        "the command queue filled up: {stats:?}"
    );
    assert_eq!(stats.send_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_transport_that_takes_nothing_makes_the_send_time_out_and_marks_the_utterance_lost() {
    // A client buffer of one frame and a client that never reads: the whole chain backs up.
    let mut rig = Rig::with(Options {
        loopback: LoopbackConfig {
            latency: Duration::ZERO,
            max_buffered: 1,
            client_queue: 1,
            send_timeout: Duration::from_secs(60),
            ..LoopbackConfig::default()
        },
        session: SessionConfig {
            commands_capacity: 1,
            pacer_capacity: 1,
            sink_capacity: 1,
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        config: TransportOutConfig {
            send_timeout: 500 * MS,
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    for seq in 0..60 {
        rig.feed(pcm(1, seq)).await;
    }
    rig.run_for(10 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert!(stats.send_failures > 0, "{stats:?}");
    assert!(
        rig.book
            .snapshot(RoundKey::new(TurnEpoch::new(1), 0))
            .unwrap()
            .lost
    );
    let _ = rig.client_events().await;
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_abandons_a_command_send_waiting_for_room_and_the_next_epoch_gets_through() {
    // The same backed-up chain as above, with a send timeout nobody reaches: only the interrupt
    // can end the wait.
    let mut rig = Rig::with(Options {
        loopback: LoopbackConfig {
            latency: Duration::ZERO,
            max_buffered: 1,
            client_queue: 1,
            send_timeout: Duration::from_secs(600),
            ..LoopbackConfig::default()
        },
        session: SessionConfig {
            commands_capacity: 1,
            pacer_capacity: 1,
            sink_capacity: 1,
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        config: TransportOutConfig {
            send_timeout: Duration::from_secs(30),
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    rig.feed(started(1, 1)).await;
    for seq in 0..20 {
        rig.feed(pcm(1, seq)).await;
    }
    rig.run_for(Duration::from_secs(1)).await;
    let before = rig.stats.snapshot();
    assert!(
        before.send_waits > 0,
        "the stage waits for room: {before:?}"
    );
    assert_eq!(
        (before.send_failures, before.stale_dropped),
        (0, 0),
        "{before:?}"
    );
    // The client has not read for a second; barge-in. Nothing drains the chain here, so the
    // wait can only end through the epoch watch.
    rig.feed(interrupt(2)).await;
    rig.run_for(10 * MS).await;
    let after = rig.stats.snapshot();
    assert_eq!(
        after.send_failures, 0,
        "abandoned, not timed out: {after:?}"
    );
    assert!(after.stale_dropped >= 1, "{after:?}");
    // The client starts reading again; the new epoch's audio arrives within a second of the
    // interrupt, not after the 30 s send timeout.
    rig.feed(started(2, 2)).await;
    rig.audio(2, 100, 3).await;
    let interrupted_at = tokio::time::Instant::now();
    let mut first_epoch2 = None;
    for _ in 0..100 {
        rig.run_for(20 * MS).await;
        for event in rig.client_events().await {
            if let ClientEvent::Audio { frame, received_at } = &event
                && frame.epoch().get() == 2
                && first_epoch2.is_none()
            {
                first_epoch2 = Some(*received_at);
            }
        }
        if first_epoch2.is_some() {
            break;
        }
    }
    let at = first_epoch2.expect("the new epoch's audio reached the client");
    assert!(
        at.saturating_duration_since(interrupted_at) < Duration::from_secs(2),
        "{:?}",
        at.saturating_duration_since(interrupted_at)
    );
}

// -- review fixes ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_cut_epoch_never_reports_an_end_however_its_end_signals_arrive() {
    // The client echoes nothing by itself: the test plays the late echo.
    let mut rig = Rig::with(Options {
        acks: MarkAcks::Manual,
        config: TransportOutConfig {
            mark_timeout: 3 * Duration::from_secs(1),
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 10).await;
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(400 * MS).await;
    assert_eq!(
        rig.stats.snapshot().marks_sent,
        1,
        "the end mark of epoch 1 is out"
    );
    let old_mark = rig
        .client_events()
        .await
        .iter()
        .find_map(|e| match e {
            ClientEvent::Mark { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("the mark reached the client");
    // Barge-in: epoch 2 starts. Everything of epoch 1 that still arrives is stale.
    rig.feed(interrupt(2)).await;
    rig.run_for(10 * MS).await;
    let before = rig.stats.snapshot();
    // (a) The TTS stage's own TtsEnded of the cut epoch, late.
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    // (b) A late UtteranceStarted of the cut epoch.
    rig.feed(started(1, 1)).await;
    // (c) The late echo of the old mark, as the pump delivers it upstream into the stage.
    rig.running
        .upstream_input()
        .send(control(1, ControlKind::TtsEnded))
        .await
        .unwrap();
    // (d) The client's echo of the old mark itself.
    rig.client.ack_mark(&old_mark).await.unwrap();
    // The mark timeout of epoch 1 passes.
    rig.run_for(5 * Duration::from_secs(1)).await;
    let after = rig.stats.snapshot();
    assert!(
        after.stale_control - before.stale_control >= 3,
        "stale end, utterance start and echo are all recognised: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.mark_timeouts, 0,
        "the cut epoch's mark timeout is gone: {after:?}"
    );
    assert_eq!(after.ends_upstream, 0, "{after:?}");
    assert!(
        !rig.up()
            .iter()
            .any(|(kind, epoch)| *kind == "tts_ended" && *epoch == 1),
        "{:?}",
        rig.up()
    );
    // The new epoch is unaffected: it speaks and ends normally.
    rig.feed(started(2, 2)).await;
    rig.audio(2, 100, 5).await;
    rig.feed(control(2, ControlKind::TtsEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let id = rig
        .client_events()
        .await
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Mark { id, .. } => Some(id.clone()),
            _ => None,
        })
        .next_back()
        .unwrap();
    rig.client.ack_mark(&id).await.unwrap();
    rig.run_for(100 * MS).await;
    assert_eq!(
        rig.up()
            .into_iter()
            .filter(|(kind, _)| *kind == "tts_ended")
            .collect::<Vec<_>>(),
        [("tts_ended", 2)]
    );
}

#[tokio::test(start_paused = true)]
async fn the_mark_of_an_epoch_the_transport_moved_past_times_out_silently() {
    // Turn 1 ends with its mark unechoed; turn 2 starts without an interrupt (nobody barged
    // in), so the transport is cleared to epoch 2 by the first audio of epoch 2. The overdue
    // mark of epoch 1 must not end anything.
    let rig = Rig::with(Options {
        acks: MarkAcks::Manual,
        config: TransportOutConfig {
            mark_timeout: Duration::from_secs(2),
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.register(2, 0, 2);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 5).await;
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(50 * MS).await;
    rig.feed(started(2, 2)).await;
    rig.audio(2, 100, 5).await;
    rig.run_for(5 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.marks_sent, 1);
    assert_eq!(stats.mark_timeouts, 0, "{stats:?}");
    assert!(
        !rig.up().contains(&("tts_ended", 1)),
        "the overdue mark of a superseded epoch ends nothing: {:?}",
        rig.up()
    );
}

#[tokio::test(start_paused = true)]
async fn an_end_mark_racing_the_drivers_clear_never_reaches_the_wire_after_it() {
    // The turn driver clears the transport on its fast path before the `Interrupt` frame
    // reaches this stage. A `TtsEnded` of the cut epoch that arrives in that window passes the
    // stage's epoch check (the pipeline's epoch has not moved yet); its mark must be cut by the
    // transport, not sent to the client after the clear (and then echoed as played).
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 5).await;
    rig.run_for(30 * MS).await;
    rig.commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(2),
        })
        .await
        .unwrap();
    rig.feed(control(1, ControlKind::TtsEnded)).await;
    rig.run_for(Duration::from_secs(1)).await;
    let events = rig.client_events().await;
    let clear_at = events
        .iter()
        .position(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2))
        .expect("the client was cleared");
    assert!(
        !events[clear_at..]
            .iter()
            .any(|e| matches!(e, ClientEvent::Mark { .. })),
        "a stale end mark reached the wire after the clear: {:?}",
        kinds(&events)
    );
    assert_eq!(
        rig.stats.snapshot().marks_sent,
        1,
        "the mark was sent (and cut)"
    );
    assert!(
        !rig.up().contains(&("tts_ended", 1)),
        "the cut epoch was reported as played to its end: {:?}",
        rig.up()
    );
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_alone_clears_the_client() {
    // No driver Clear and no audio after the interrupt: only the stage's own clear on Interrupt
    // can tell the client to drop what it buffered.
    let mut rig = Rig::new().await;
    rig.register(1, 0, 1);
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 20).await;
    rig.run_for(100 * MS).await;
    assert_eq!(
        rig.stats.snapshot().clears,
        1,
        "the clear that started epoch 1"
    );
    let _ = rig.client_events().await;
    rig.feed(interrupt(2)).await;
    rig.run_for(100 * MS).await;
    assert_eq!(rig.stats.snapshot().clears, 2, "{:?}", rig.stats.snapshot());
    let events = rig.client_events().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::Clear { epoch, .. } if epoch.get() == 2)),
        "the client was told to clear: {:?}",
        kinds(&events)
    );
}

#[tokio::test(start_paused = true)]
async fn the_first_audio_out_waits_until_the_transport_has_taken_the_frame() {
    // A chain that is full of audio of epoch 1 that the test queued itself (a Clear to the same
    // epoch by the stage is a no-op for the transport, so that audio stays), and a client that
    // reads nothing: the stage's first frame waits for room. TtsStarted and the FirstAudioOut
    // mark describe the handoff, not the arrival.
    let mut rig = Rig::with(Options {
        loopback: LoopbackConfig {
            latency: Duration::ZERO,
            max_buffered: 1,
            client_queue: 1,
            send_timeout: Duration::from_secs(600),
            ..LoopbackConfig::default()
        },
        session: SessionConfig {
            commands_capacity: 1,
            pacer_capacity: 1,
            sink_capacity: 1,
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        config: TransportOutConfig {
            send_timeout: Duration::from_secs(60),
            ..TransportOutConfig::default()
        },
        ..Options::default()
    })
    .await;
    rig.register(1, 0, 1);
    rig.commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let filler_commands = rig.commands.clone();
    let filler = tokio::spawn(async move {
        for seq in 0..150 {
            let sent = filler_commands
                .send(TransportCommand::Audio {
                    epoch: TurnEpoch::new(1),
                    utterance: UtteranceId::new(99),
                    frame: audio_frame(1, 1000 + seq),
                })
                .await;
            if sent.is_err() {
                break;
            }
        }
    });
    // Long enough for the pacer to release more frames than the chain holds (the client buffer,
    // its queue, the sink, the pacer's own slot): the pacer releases its first frame at once, so 100 ms was one frame short of full and the test relied on the old phase.
    rig.run_for(300 * MS).await;
    assert!(!filler.is_finished(), "the chain is full");
    let before = rig.clock.now();
    rig.feed(started(1, 1)).await;
    rig.audio(1, 0, 3).await;
    rig.run_for(Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert!(
        stats.send_waits > 0,
        "the stage's first send waits: {stats:?}"
    );
    assert_eq!(stats.started_sent, 0, "{stats:?}");
    assert!(
        rig.up().is_empty(),
        "nothing is started yet: {:?}",
        rig.up()
    );
    // The client starts reading: the chain drains, the filler finishes, the stage's frame goes in.
    let mut seen = false;
    for _ in 0..400 {
        rig.run_for(20 * MS).await;
        let _ = rig.client_events().await;
        if rig.up() == [("tts_started", 1)] {
            seen = true;
            break;
        }
    }
    assert!(seen, "the handoff happened once there was room");
    let handed_at = rig.clock.now();
    rig.timeline.finish();
    let at = rig
        .timeline
        .history()
        .into_iter()
        .find_map(|r| r.at(TurnEvent::FirstAudioOut))
        .expect("FirstAudioOut is marked");
    assert!(
        at.duration_since(before) >= Duration::from_secs(1),
        "marked at the handoff, a second after the frame arrived: {:?}",
        at.duration_since(before)
    );
    assert!(at <= handed_at);
    filler.abort();
}
