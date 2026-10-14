//! The transport pump: events to frames, marks and positions to the book, hangup, and
//! never stopping to drain the transport even when the pipeline does.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, DtmfDigit, Frame, SampleRate, SystemFrame,
};
use brisvox_core::{Direction, Inbox, QueueConfig, SharedClock, SimClock, TurnEpoch};
use brisvox_engine::{
    EngineConfig, EngineRates, PlayoutBook, Pump, PumpConfig, PumpEnd, PumpHandle, PumpLinks,
    RoundKey,
};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::{
    CallEnds, CallInfo, CallRates, LoopbackClient, LoopbackConfig, LoopbackTransport, MarkAcks,
    SessionConfig, StopReason, Transport, TransportCommand, TransportSession, UtteranceId,
    WireEvent, call_pair,
};
use bytes::Bytes;
use common::MS;
use tokio_util::sync::CancellationToken;

fn input_queue() -> QueueConfig {
    EngineConfig::DEFAULT.queues.input
}

struct Rig {
    handle: PumpHandle,
    input: Inbox,
    upstream: Inbox,
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    cancel: CancellationToken,
}

impl Rig {
    /// Frames queued in the pipeline input, oldest first, system lane first.
    fn input_frames(&mut self) -> Vec<Frame> {
        std::iter::from_fn(|| self.input.try_recv().ok())
            .map(|(f, _)| f)
            .collect()
    }

    fn upstream_frames(&mut self) -> Vec<(Frame, Direction)> {
        std::iter::from_fn(|| self.upstream.try_recv().ok()).collect()
    }
}

async fn settle() {
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
}

fn rig_from(
    session: TransportSession,
    config: PumpConfig,
    queue: QueueConfig,
    clock: SharedClock,
    cancel: CancellationToken,
) -> Rig {
    let input = Inbox::new(queue);
    let upstream = Inbox::new(QueueConfig::DEFAULT);
    let book = PlayoutBook::shared(16);
    let handle = Pump::spawn(
        session,
        PumpLinks {
            input: input.sender(Direction::Downstream),
            upstream: Some(upstream.sender(Direction::Upstream)),
        },
        Arc::clone(&book),
        clock.clone(),
        config,
        &cancel,
    );
    Rig {
        handle,
        input,
        upstream,
        book,
        clock,
        cancel,
    }
}

/// A pump on a loopback call (latency 0, marks echoed at once).
async fn loopback_rig(config: PumpConfig, queue: QueueConfig) -> (Rig, LoopbackClient) {
    let clock = SimClock::shared();
    let (mut transport, client) = LoopbackTransport::new(
        clock.clone(),
        LoopbackConfig {
            latency: Duration::ZERO,
            mark_acks: MarkAcks::Auto {
                delay: Duration::ZERO,
            },
            ..LoopbackConfig::default()
        },
    );
    let cancel = CancellationToken::new();
    let link = transport.accept(&cancel).await.unwrap().unwrap();
    let session = TransportSession::open(
        clock.clone(),
        link,
        SessionConfig {
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        &cancel,
    )
    .await
    .unwrap();
    (rig_from(session, config, queue, clock, cancel), client)
}

/// A pump on a call whose adapter the test plays (to send `Started` metadata and faults).
async fn adapter_rig(config: PumpConfig) -> (Rig, CallEnds) {
    let clock = SimClock::shared();
    let (link, ends) = call_pair();
    let cancel = CancellationToken::new();
    let session = TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
        .await
        .unwrap();
    (
        rig_from(session, config, input_queue(), clock, cancel),
        ends,
    )
}

fn pcm(origin: tokio::time::Instant, seq: u64, epoch: u32) -> AudioFrame {
    AudioFrame::new(
        Bytes::from(vec![1u8; 640]),
        SampleRate::Hz16000,
        origin,
        seq,
        TurnEpoch::new(epoch),
    )
    .unwrap()
}

fn started(rate: Option<&str>) -> WireEvent {
    let mut info = CallInfo {
        call_id: "test".into(),
        ..CallInfo::default()
    };
    if let Some(rate) = rate {
        info.metadata
            .insert(CallInfo::SAMPLE_RATE_KEY.to_owned(), rate.to_owned());
    }
    WireEvent::Started(info)
}

fn audio_frames(frames: &[Frame]) -> Vec<&AudioFrame> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Data(DataFrame::Audio(a)) => Some(a),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn caller_audio_becomes_frames_in_epoch_zero() {
    let roomy = QueueConfig {
        data_capacity: 32,
        ..input_queue()
    };
    let (mut rig, mut client) = loopback_rig(PumpConfig::default(), roomy).await;
    // Move the transport to epoch 3 first: its input events are tagged with it, the pump
    // retags them to the user side's epoch zero.
    rig.handle
        .commands()
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(3),
        })
        .await
        .unwrap();
    for _ in 0..10 {
        client
            .send_audio(Bytes::from(vec![2u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        settle().await;
    }
    let frames = rig.input_frames();
    let audio = audio_frames(&frames);
    assert_eq!(audio.len(), 10);
    assert!(audio.iter().all(|a| a.epoch() == TurnEpoch::ZERO));
    assert!(audio.windows(2).all(|w| w[1].seq() == w[0].seq() + 1));
    assert_eq!(rig.handle.stats().audio, 10);
}

#[tokio::test(start_paused = true)]
async fn started_rates_are_parsed_published_and_defaulted() {
    // The loopback reports no rate: the configured fallback applies.
    let (rig, _client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    settle().await;
    assert!(rig.handle.started());
    assert_eq!(
        rig.handle.rates(),
        CallRates::symmetric(SampleRate::Hz16000)
    );

    // An adapter that reports 8 kHz (telephony).
    let (mut rig, ends) = adapter_rig(PumpConfig::default()).await;
    assert!(!rig.handle.started());
    assert_eq!(
        rig.handle.rates(),
        PumpConfig::default().rates.fallback_call
    );
    ends.events.send(started(Some("8000"))).await.unwrap();
    let rates = rig.handle.wait_started().await.unwrap();
    assert_eq!(rates, CallRates::symmetric(SampleRate::Hz8000));
    assert_eq!(rig.handle.rates(), rates);
    assert!(
        rig.input_frames()
            .iter()
            .all(|f| !matches!(f, Frame::System(SystemFrame::Error(_))))
    );
    assert_eq!(rig.handle.end(), None);
}

#[tokio::test(start_paused = true)]
async fn rates_the_audio_path_cannot_use_are_a_fatal_error_and_a_hangup() {
    for (metadata, why) in [("11025", "unsupported rate"), ("fast", "not a number")] {
        let (mut rig, ends) = adapter_rig(PumpConfig::default()).await;
        ends.events.send(started(Some(metadata))).await.unwrap();
        let end = rig.handle.wait_end().await;
        assert!(matches!(end, PumpEnd::BadRates(_)), "{why}: {end:?}");
        let frames = rig.input_frames();
        let error = frames
            .iter()
            .find_map(|f| match f {
                Frame::System(SystemFrame::Error(e)) => Some(e),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{why}: no error frame in {frames:?}"));
        assert!(error.fatal, "{why}");
        assert_eq!(error.stage, "transport.in");
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, Frame::System(SystemFrame::Stop))),
            "{why}: no Stop"
        );
        assert!(
            !rig.handle.started(),
            "{why}: unusable rates are not published"
        );
    }

    // A frame length that is not whole samples at the reported rate (15 ms at 22.05 kHz).
    let config = PumpConfig {
        rates: EngineRates {
            frame: 15 * MS,
            fallback_call: CallRates::symmetric(SampleRate::Hz16000),
            ..EngineRates::DEFAULT
        },
        ..PumpConfig::default()
    };
    let (mut rig, ends) = adapter_rig(config).await;
    ends.events.send(started(Some("22050"))).await.unwrap();
    assert!(matches!(rig.handle.wait_end().await, PumpEnd::BadRates(_)));
}

#[tokio::test(start_paused = true)]
async fn dtmf_becomes_a_data_frame_in_epoch_zero() {
    let (mut rig, client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    client.send_dtmf(DtmfDigit::D7).await.unwrap();
    settle().await;
    let frames = rig.input_frames();
    let dtmf: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            Frame::Data(DataFrame::Dtmf(d)) => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(dtmf.len(), 1);
    assert_eq!(
        (dtmf[0].digit, dtmf[0].epoch),
        (DtmfDigit::D7, TurnEpoch::ZERO)
    );
    assert_eq!(rig.handle.stats().dtmf, 1);
}

#[tokio::test(start_paused = true)]
async fn the_caller_hanging_up_stops_the_pipeline_input_and_says_why() {
    let (mut rig, mut client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    client
        .send_audio(Bytes::from(vec![0u8; 640]), SampleRate::Hz16000)
        .await
        .unwrap_or(());
    client.hangup().await.unwrap();
    let end = rig.handle.wait_end().await;
    assert_eq!(end, PumpEnd::Stopped(StopReason::PeerHangup));
    let frames = rig.input_frames();
    assert!(
        frames
            .iter()
            .any(|f| matches!(f, Frame::System(SystemFrame::Stop)))
    );
    assert_eq!(rig.handle.stats().hangups, 1);
    // What arrives after the hangup is covered by
    // `after_a_hangup_nothing_reaches_the_pipeline_but_marks_still_reach_the_book`.
}

/// Hangs up by a transport fault (the session itself stays open, so the adapter can keep
/// sending), then sends audio, a key press and the echo of a registered end mark.
#[tokio::test(start_paused = true)]
async fn after_a_hangup_nothing_reaches_the_pipeline_but_marks_still_reach_the_book() {
    let config = PumpConfig {
        hangup_on_error: true,
        ..PumpConfig::default()
    };
    let (mut rig, mut ends) = adapter_rig(config).await;
    ends.events.send(started(None)).await.unwrap();
    // An end mark the pump is waiting for: registered in the book, sent to the adapter.
    let key = RoundKey::new(TurnEpoch::new(1), 0);
    rig.book.register_round(key);
    rig.book.register_utterance(key, 5);
    rig.book.register_mark("end-5", 5);
    rig.handle
        .commands()
        .send(TransportCommand::Mark { id: "end-5".into() })
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        ends.outgoing.recv().await.unwrap(),
        brisvox_transport::WireOut::Mark { id: "end-5".into() }
    );

    ends.events
        .send(WireEvent::Fault {
            message: "boom".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        rig.handle.wait_end().await,
        PumpEnd::TransportError("boom".into())
    );
    settle().await;
    assert!(
        rig.input_frames()
            .iter()
            .any(|f| matches!(f, Frame::System(SystemFrame::Stop)))
    );

    for seq in 0..3 {
        ends.events
            .send(WireEvent::Audio(pcm(rig.clock.now(), seq, 0)))
            .await
            .unwrap();
    }
    ends.events
        .send(WireEvent::Dtmf {
            digit: DtmfDigit::D9,
            at: rig.clock.now(),
        })
        .await
        .unwrap();
    ends.events
        .send(WireEvent::MarkAck { id: "end-5".into() })
        .await
        .unwrap();
    settle().await;

    // Nothing new reached the (stopped) pipeline: audio and the key press are discarded, the
    // end mark's TtsEnded goes nowhere.
    assert_eq!(rig.input_frames(), []);
    assert_eq!(rig.upstream_frames(), []);
    let stats = rig.handle.stats();
    assert_eq!(
        (stats.audio, stats.audio_discarded),
        (3, 3),
        "audio after the hangup: {stats:?}"
    );
    assert_eq!(stats.dtmf, 0, "{stats:?}");
    assert_eq!(stats.dtmf_dropped, 1, "{stats:?}");
    // The echo was still read: the book knows the utterance finished playing, so the
    // barge-in accounting does not stall.
    assert_eq!(stats.end_marks, 1, "{stats:?}");
    assert_eq!(stats.upstream_dropped, 1, "{stats:?}");
    assert!(rig.book.snapshot(key).unwrap().marked_played);
    assert_eq!(stats.hangups, 1);
}

/// The session's events end (here: `Stopped`, then no more), but the `Stop` is still stuck
/// behind a full system lane: the pump keeps trying until the lane has room.
#[tokio::test(start_paused = true)]
async fn the_pending_stop_is_flushed_after_the_session_closed() {
    let queue = QueueConfig {
        system_capacity: 1,
        ..input_queue()
    };
    let (mut rig, client) = loopback_rig(PumpConfig::default(), queue).await;
    let filler = rig.input.sender(Direction::Downstream);
    filler
        .try_send(Frame::System(SystemFrame::Wake { token: 0 }))
        .unwrap();
    client.hangup().await.unwrap();
    // The hangup is read, the session ends and its event stream closes; the Stop waits.
    rig.clock.sleep(100 * MS).await;
    settle().await;
    assert_eq!(rig.handle.stats().pending_dropped_at_hangup, 0);
    let first = rig.input_frames();
    assert!(
        first
            .iter()
            .all(|f| !matches!(f, Frame::System(SystemFrame::Stop))),
        "the Stop had no room yet: {first:?}"
    );
    // Room is made after the session closed: the Stop is delivered, not lost.
    settle().await;
    let later = rig.input_frames();
    assert!(
        later
            .iter()
            .any(|f| matches!(f, Frame::System(SystemFrame::Stop))),
        "{later:?}"
    );
    assert_eq!(rig.handle.stats().pending_dropped_at_hangup, 0);
}

#[tokio::test(start_paused = true)]
async fn a_transport_error_is_only_counted_unless_configured_to_hang_up() {
    let strict = PumpConfig {
        hangup_on_error: true,
        ..PumpConfig::default()
    };
    let (mut rig, ends) = adapter_rig(strict).await;
    ends.events.send(started(None)).await.unwrap();
    ends.events
        .send(WireEvent::Fault {
            message: "bad frame".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        rig.handle.wait_end().await,
        PumpEnd::TransportError("bad frame".into())
    );
    assert!(
        rig.input_frames()
            .iter()
            .any(|f| matches!(f, Frame::System(SystemFrame::Stop)))
    );

    // By default (hangup_on_error off) it is only counted, and the call goes on: the
    // transport's Error is non-fatal and it ends the call itself when its fault budget is spent.
    assert!(!PumpConfig::default().hangup_on_error);
    let (mut rig, ends) = adapter_rig(PumpConfig::default()).await;
    ends.events.send(started(None)).await.unwrap();
    ends.events
        .send(WireEvent::Fault {
            message: "bad frame".into(),
        })
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Dtmf {
            digit: DtmfDigit::D1,
            at: rig.clock.now(),
        })
        .await
        .unwrap();
    settle().await;
    assert_eq!(rig.handle.end(), None);
    let stats = rig.handle.stats();
    assert_eq!(
        (stats.transport_errors, stats.hangups, stats.dtmf),
        (1, 0, 1)
    );
    assert!(
        rig.input_frames()
            .iter()
            .all(|f| !matches!(f, Frame::System(SystemFrame::Stop)))
    );
}

fn utterance_frame(at: tokio::time::Instant, seq: u64, epoch: u32) -> AudioFrame {
    // 20 ms at 16 kHz.
    pcm(at, seq, epoch)
}

#[tokio::test(start_paused = true)]
async fn marks_and_positions_reach_the_book_and_the_end_mark_ends_the_turn_upstream() {
    let (mut rig, _client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    let commands = rig.handle.commands();
    let key = RoundKey::new(TurnEpoch::new(1), 0);
    rig.book.register_round(key);
    rig.book.register_utterance(key, 5);
    rig.book.register_mark("end-5", 5);

    commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 0..10 {
        commands
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::new(1),
                utterance: UtteranceId::new(5),
                frame: utterance_frame(rig.clock.now(), seq, 1),
            })
            .await
            .unwrap();
    }
    commands
        .send(TransportCommand::Mark { id: "end-5".into() })
        .await
        .unwrap();
    // 10 frames of 20 ms are paced out in real (here: simulated) time, then the mark is echoed.
    rig.clock.sleep(500 * MS).await;
    settle().await;

    let snapshot = rig.book.snapshot(key).unwrap();
    assert!(snapshot.marked_played, "the echo reached the book");
    assert_eq!(
        snapshot.released,
        200 * MS,
        "the final position is the audio released"
    );
    let stats = rig.handle.stats();
    assert!(stats.positions >= 2, "{stats:?}");
    assert_eq!(stats.end_marks, 1);
    // The end of the utterance's audio travels upstream through the last stage, with the epoch
    // of the utterance.
    let up = rig.upstream_frames();
    assert_eq!(
        up,
        [(
            Frame::Control(ControlFrame {
                epoch: TurnEpoch::new(1),
                kind: ControlKind::TtsEnded
            }),
            Direction::Upstream
        )]
    );
    // An echo of a mark nobody registered is counted, not forwarded.
    commands
        .send(TransportCommand::Mark { id: "stray".into() })
        .await
        .unwrap();
    settle().await;
    rig.clock.sleep(50 * MS).await;
    assert_eq!(rig.handle.stats().unknown_marks, 1);
    assert!(rig.upstream_frames().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_mark_cut_by_a_clear_reaches_the_book_but_ends_nothing_upstream() {
    let (mut rig, _client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    let commands = rig.handle.commands();
    let key = RoundKey::new(TurnEpoch::new(1), 0);
    rig.book.register_round(key);
    rig.book.register_utterance(key, 6);
    rig.book.register_mark("end-6", 6);
    commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 0..50 {
        commands
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::new(1),
                utterance: UtteranceId::new(6),
                frame: utterance_frame(rig.clock.now(), seq, 1),
            })
            .await
            .unwrap();
    }
    commands
        .send(TransportCommand::Mark { id: "end-6".into() })
        .await
        .unwrap();
    // The caller barges in 100 ms into the answer.
    rig.clock.sleep(100 * MS).await;
    rig.book.note_clear(TurnEpoch::new(2));
    commands
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(2),
        })
        .await
        .unwrap();
    rig.clock.sleep(100 * MS).await;
    settle().await;

    let snapshot = rig.book.snapshot(key).unwrap();
    assert!(snapshot.mark_cleared && !snapshot.marked_played);
    assert!(snapshot.settled, "the cut mark settles the utterance");
    assert!(snapshot.released > Duration::ZERO && snapshot.released < 1_000 * MS);
    let stats = rig.handle.stats();
    assert_eq!((stats.marks_cleared, stats.end_marks), (1, 0));
    assert!(rig.upstream_frames().is_empty());
}

#[tokio::test(start_paused = true)]
async fn it_keeps_draining_the_transport_while_the_pipeline_is_stuck() {
    // Nobody reads the pipeline's input: its audio lane (8 frames) is full from the start.
    let (mut rig, mut client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    for _ in 0..300 {
        client
            .send_audio(Bytes::from(vec![3u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }
    settle().await;
    let stats = rig.handle.stats();
    assert_eq!(
        stats.audio, 300,
        "every audio event was read from the transport: {stats:?}"
    );
    assert_eq!(
        rig.input.stats().dropped_oldest,
        292,
        "the inbox shed the oldest audio"
    );
    assert_eq!(rig.input.len(), 8);

    // The key press cannot be delivered (the data lane is full) and waits; the hangup behind it
    // does not: its own system lane has room, and the reading goes on.
    client.send_dtmf(DtmfDigit::D9).await.unwrap();
    client.hangup().await.unwrap();
    settle().await;
    assert_eq!(
        rig.handle.end(),
        Some(PumpEnd::Stopped(StopReason::PeerHangup))
    );
    let first = rig.input.try_recv().unwrap().0;
    assert!(
        matches!(first, Frame::System(SystemFrame::Stop)),
        "Stop must not wait behind the key press stuck on the data lane: {first:?}"
    );
    // The reader drains the data lane: room appears and the key press is delivered after all.
    let _audio = rig.input_frames();
    settle().await;
    let rest = rig.input_frames();
    assert!(
        rest.iter()
            .any(|f| matches!(f, Frame::Data(DataFrame::Dtmf(d)) if d.digit == DtmfDigit::D9)),
        "{rest:?}"
    );
    assert_eq!(rig.handle.stats().dtmf_dropped, 0);
}

#[tokio::test(start_paused = true)]
async fn a_hangup_is_delivered_while_a_key_press_is_stuck_and_the_press_is_given_up_later() {
    let config = PumpConfig {
        flush_timeout: 500 * MS,
        ..PumpConfig::default()
    };
    let (mut rig, mut client) = loopback_rig(config, input_queue()).await;
    // Nobody reads the input: its data lane fills with audio and stays full.
    for _ in 0..20 {
        client
            .send_audio(Bytes::from(vec![3u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        settle().await;
    }
    client.send_dtmf(DtmfDigit::D4).await.unwrap();
    settle().await;
    client.hangup().await.unwrap();
    settle().await;
    // The system lane is empty, so the Stop is in the inbox at once even though the key press
    // ahead of it cannot be delivered.
    let stop_now = rig.input.try_recv().unwrap().0;
    assert!(
        matches!(stop_now, Frame::System(SystemFrame::Stop)),
        "{stop_now:?}"
    );
    assert_eq!(rig.handle.stats().pending_dropped_at_hangup, 0);
    // The key press is still pending; the flush gives up on it, and only on it.
    rig.clock.sleep(600 * MS).await;
    settle().await;
    let stats = rig.handle.stats();
    assert_eq!(stats.pending_dropped_at_hangup, 1, "{stats:?}");
    assert_eq!(stats.control_dropped, 0);
}

#[tokio::test(start_paused = true)]
async fn the_end_of_an_utterance_goes_upstream_while_a_key_press_is_stuck_on_the_input() {
    let (mut rig, mut client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    // Fill the input's data lane (nobody reads it) and add a key press that cannot be delivered.
    for _ in 0..20 {
        client
            .send_audio(Bytes::from(vec![3u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        settle().await;
    }
    client.send_dtmf(DtmfDigit::D2).await.unwrap();
    settle().await;

    let commands = rig.handle.commands();
    let key = RoundKey::new(TurnEpoch::new(1), 0);
    rig.book.register_round(key);
    rig.book.register_utterance(key, 5);
    rig.book.register_mark("end-5", 5);
    for seq in 0..3 {
        commands
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::new(1),
                utterance: UtteranceId::new(5),
                frame: utterance_frame(rig.clock.now(), seq, 1),
            })
            .await
            .unwrap();
    }
    commands
        .send(TransportCommand::Mark { id: "end-5".into() })
        .await
        .unwrap();
    rig.clock.sleep(300 * MS).await;
    settle().await;
    let up = rig.upstream_frames();
    assert_eq!(
        up,
        [(
            Frame::Control(ControlFrame {
                epoch: TurnEpoch::new(1),
                kind: ControlKind::TtsEnded
            }),
            Direction::Upstream
        )],
        "the end of the utterance must not wait behind the stuck key press"
    );
    assert_eq!(rig.handle.stats().dtmf_dropped, 0);
}

#[tokio::test(start_paused = true)]
async fn the_pending_queue_is_bounded_and_counts_what_it_drops() {
    let config = PumpConfig {
        pending_capacity: 2,
        ..PumpConfig::default()
    };
    let (mut rig, mut client) = loopback_rig(config, input_queue()).await;
    for _ in 0..10 {
        client
            .send_audio(Bytes::from(vec![3u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        settle().await;
    }
    for digit in [
        DtmfDigit::D1,
        DtmfDigit::D2,
        DtmfDigit::D3,
        DtmfDigit::D4,
        DtmfDigit::D5,
    ] {
        client.send_dtmf(digit).await.unwrap();
        settle().await;
    }
    let stats = rig.handle.stats();
    assert_eq!(stats.dtmf, 5);
    assert_eq!(
        stats.dtmf_dropped, 3,
        "two wait, three were dropped: {stats:?}"
    );
    // Making room delivers the two oldest, in order.
    let _ = rig.input_frames();
    settle().await;
    let digits: Vec<DtmfDigit> = rig
        .input_frames()
        .iter()
        .filter_map(|f| match f {
            Frame::Data(DataFrame::Dtmf(d)) => Some(d.digit),
            _ => None,
        })
        .collect();
    assert_eq!(digits, [DtmfDigit::D1, DtmfDigit::D2]);
}

#[tokio::test(start_paused = true)]
async fn undeliverable_frames_are_given_up_after_the_flush_timeout() {
    // A system lane of one slot, already taken: the Stop of the hangup cannot be delivered.
    let queue = QueueConfig {
        system_capacity: 1,
        ..input_queue()
    };
    let config = PumpConfig {
        flush_timeout: 500 * MS,
        ..PumpConfig::default()
    };
    let (rig, client) = loopback_rig(config, queue).await;
    // Fill the system lane: a Wake sent into the inbox's own lane by the test.
    let filler = rig.input.sender(Direction::Downstream);
    filler
        .try_send(Frame::System(SystemFrame::Wake { token: 0 }))
        .unwrap();
    client.hangup().await.unwrap();
    settle().await;
    assert_eq!(rig.handle.stats().pending_dropped_at_hangup, 0);
    rig.clock.sleep(600 * MS).await;
    settle().await;
    assert_eq!(
        rig.handle.stats().pending_dropped_at_hangup,
        1,
        "the Stop was given up"
    );
    assert_eq!(
        rig.handle.end(),
        Some(PumpEnd::Stopped(StopReason::PeerHangup))
    );
}

#[tokio::test(start_paused = true)]
async fn a_closed_pipeline_does_not_stop_the_draining_either() {
    let (rig, mut client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    let Rig {
        handle,
        input,
        upstream,
        ..
    } = rig;
    drop(input); // the pipeline ended
    for _ in 0..5 {
        client
            .send_audio(Bytes::from(vec![3u8; 640]), SampleRate::Hz16000)
            .await
            .unwrap();
        settle().await;
    }
    client.send_dtmf(DtmfDigit::D1).await.unwrap();
    settle().await;
    let stats = handle.stats();
    assert_eq!(stats.audio, 5);
    assert_eq!(stats.audio_discarded, 5);
    assert_eq!(stats.dtmf_dropped, 1);
    assert_eq!(handle.end(), None, "a closed pipeline is not a hangup");
    drop(upstream);
}

#[tokio::test(start_paused = true)]
async fn cancelling_or_dropping_the_pump_leaves_no_task_behind() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    let (mut rig, _client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    settle().await;
    assert!(counter.alive() > counter.baseline());
    rig.cancel.cancel();
    assert_eq!(rig.handle.wait_end().await, PumpEnd::Cancelled);
    drop(rig);
    counter
        .wait_for_baseline(&clock, Duration::from_millis(500))
        .await
        .expect("the pump, the transport session or the loopback outlived the call");

    // Dropping the handle alone also ends the pump (and with it the session).
    let (rig, client) = loopback_rig(PumpConfig::default(), input_queue()).await;
    settle().await;
    let Rig { handle, cancel, .. } = rig;
    drop(handle);
    cancel.cancel();
    drop(client);
    counter
        .wait_for_baseline(&clock, Duration::from_millis(500))
        .await
        .expect("a task outlived the dropped pump handle");
}
