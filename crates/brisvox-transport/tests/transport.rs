//! Transport layer tests: input audio and DTMF order, output pacing on the 20 ms grid,
//! clear, marks, playback positions, stop and cancel, and overflow policies under a slow consumer.
//! Paused time with a `SimClock`; every wait is bounded by a timeout.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] // test helpers (see clippy.toml)

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use brisvox_transport::channel::{self, ChannelStats, Overflow, Overflowing};
use brisvox_transport::{
    CallEnds, CallInfo, ClientEvent, LoopbackClient, LoopbackConfig, LoopbackTransport, MarkAcks,
    SessionConfig, StopReason, Transport, TransportCommand, TransportEvent, TransportSession,
    UtteranceId, WireEvent, WireOut, call_pair,
};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (simulated time).
const WAIT: Duration = Duration::from_secs(5);
/// The audio frame duration of the path.
const FRAME: Duration = Duration::from_millis(20);
/// Client-side buffering of the loopback used by these tests.
const LATENCY: Duration = Duration::from_millis(30);

/// 20 ms of 16 kHz mono PCM16, every sample `fill`.
fn pcm(fill: u8) -> Bytes {
    Bytes::from(vec![fill; 640])
}

/// An engine audio command: 20 ms, sequence `seq`, first byte `seq`.
fn audio(clock: &SharedClock, utterance: u64, epoch: TurnEpoch, seq: u64) -> TransportCommand {
    let frame =
        AudioFrame::new(pcm(seq as u8), SampleRate::Hz16000, clock.now(), seq, epoch).unwrap();
    TransportCommand::Audio {
        epoch,
        utterance: UtteranceId::new(utterance),
        frame,
    }
}

/// Starts a loopback call with `config` and consumes its `Started` event.
async fn start_with(
    config: SessionConfig,
) -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        ..LoopbackConfig::default()
    };
    start_full(config, loopback).await
}

/// Starts a loopback call with both configs and consumes its `Started` event.
async fn start_full(
    config: SessionConfig,
    loopback: LoopbackConfig,
) -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (mut transport, client) = LoopbackTransport::new(clock.clone(), loopback);
    let link = transport.accept(&cancel).await.unwrap().expect("one call");
    let mut session = TransportSession::open(clock.clone(), link, config, &cancel)
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        TransportEvent::Started(_)
    ));
    (clock, client, session, cancel)
}

async fn start() -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    start_with(SessionConfig::default()).await
}

async fn next_event(session: &mut TransportSession) -> TransportEvent {
    tokio::time::timeout(WAIT, session.recv())
        .await
        .expect("session event timed out")
        .expect("session ended early")
}

async fn next_client(client: &mut LoopbackClient) -> ClientEvent {
    tokio::time::timeout(WAIT, client.recv())
        .await
        .expect("client event timed out")
        .expect("transport ended early")
}

/// Lets spawned tasks run until `clock` is held only by the test and its client (`expected`
/// strong references), or gives up after a bounded number of yields.
async fn settle_clock(clock: &SharedClock, expected: usize) {
    for _ in 0..200 {
        if Arc::strong_count(clock) == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        Arc::strong_count(clock),
        expected,
        "a task still holds the clock"
    );
}

#[tokio::test(start_paused = true)]
async fn transport_accepts_one_call_then_nothing() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (mut transport, _client) = LoopbackTransport::new(clock, LoopbackConfig::default());
    assert!(transport.accept(&cancel).await.unwrap().is_some());
    assert!(transport.accept(&cancel).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn the_call_started_handle_holds_the_info_of_the_started_event() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (mut transport, _client) = LoopbackTransport::new(clock.clone(), LoopbackConfig::default());
    let link = transport.accept(&cancel).await.unwrap().expect("one call");
    let mut session = TransportSession::open(clock, link, SessionConfig::default(), &cancel)
        .await
        .unwrap();
    // Taken before the event is read, as a host does before it hands the session over.
    let started = session.call_started();
    let TransportEvent::Started(info) = next_event(&mut session).await else {
        panic!("Started expected first");
    };
    assert_eq!(started.get(), Some(&info));
    assert_eq!(session.call_started().get(), Some(&info));
}

#[tokio::test(start_paused = true)]
async fn input_audio_and_dtmf_arrive_in_order() {
    let (_clock, mut client, mut session, _cancel) = start().await;

    // Caller: A0 A1 A2 D1 A3 A4 #. Each audio frame's first sample is its index.
    let mut sent = Vec::new();
    for i in 0..5u8 {
        client
            .send_audio(pcm(i), SampleRate::Hz16000)
            .await
            .unwrap();
        sent.push(Ok(i));
        if i == 2 {
            client.send_dtmf(DtmfDigit::D1).await.unwrap();
            sent.push(Err(DtmfDigit::D1));
        }
        if i == 4 {
            client.send_dtmf(DtmfDigit::Pound).await.unwrap();
            sent.push(Err(DtmfDigit::Pound));
        }
    }

    for expected in sent {
        match (next_event(&mut session).await, expected) {
            (TransportEvent::Audio(frame), Ok(index)) => {
                assert_eq!(frame.data()[0], index);
                assert_eq!(frame.sample_rate(), SampleRate::Hz16000);
                assert_eq!(frame.duration(), FRAME);
                assert_eq!(frame.epoch(), TurnEpoch::ZERO);
            }
            (TransportEvent::Dtmf { digit, .. }, Err(want)) => assert_eq!(digit, want),
            (other, expected) => panic!("got {other:?}, expected {expected:?}"),
        }
    }
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn output_is_paced_at_real_time_on_a_20_ms_grid() {
    let (clock, mut client, session, _cancel) = start().await;
    let begin = clock.now();
    // A TTS burst: 50 frames (1 s of audio) handed over at once.
    for seq in 0..50 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }

    let mut heard = Vec::new();
    for _ in 0..50 {
        match next_client(&mut client).await {
            ClientEvent::Audio { frame, received_at } => heard.push((frame.seq(), received_at)),
            other => panic!("unexpected client event: {other:?}"),
        }
    }

    let seqs: Vec<u64> = heard.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, (0..50).collect::<Vec<u64>>());
    // One frame per 20 ms, exactly: the absolute grid of the pacer, not a burst.
    for pair in heard.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, FRAME);
    }
    // The first frame waits for the client's latency; the last one plays 49 frames later, so the
    // burst took its real duration, not zero.
    assert!(heard[0].1 - begin >= LATENCY);
    let last = heard.last().unwrap().1;
    assert!(last - begin >= Duration::from_millis(49 * 20) + LATENCY);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn clear_drops_queued_older_audio_at_once_and_reports_the_cut() {
    // The interval report never fires (10 s), so the only way the cut utterance's position reaches
    // the engine is the final report that the clear itself triggers.
    let config = SessionConfig {
        position_interval: Duration::from_secs(10),
        ..SessionConfig::default()
    };
    let (clock, mut client, mut session, _cancel) = start_with(config).await;
    let old = TurnEpoch::ZERO;
    let new = TurnEpoch::new(1);

    // 500 ms of the old answer, then the caller interrupts after 130 ms: the pacer (the first
    // frame leaves at once) has released the frames at 0, 20, 40, 60, 80, 100 and 120 ms,
    // 140 ms of audio.
    for seq in 0..25 {
        session.send(audio(&clock, 1, old, seq)).await.unwrap();
    }
    clock.sleep(Duration::from_millis(130)).await;
    let cleared_at = clock.now();
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    // The cut is reported by the clear itself, before any new audio takes over.
    assert_eq!(
        next_event(&mut session).await,
        TransportEvent::PlaybackPosition {
            epoch: old,
            utterance: UtteranceId::new(1),
            played: Duration::from_millis(140),
        }
    );
    for seq in 25..35 {
        session.send(audio(&clock, 2, new, seq)).await.unwrap();
    }

    let mut old_heard = Vec::new();
    let mut new_heard = Vec::new();
    let mut cleared = None;
    while new_heard.len() < 10 {
        match next_client(&mut client).await {
            ClientEvent::Audio { frame, received_at } if frame.epoch() == new => {
                new_heard.push(received_at);
            }
            ClientEvent::Audio { received_at, .. } => old_heard.push(received_at),
            ClientEvent::Clear { epoch, at } => cleared = Some((epoch, at)),
            other => panic!("unexpected client event: {other:?}"),
        }
    }

    let (epoch, at) = cleared.expect("the client was told to clear");
    assert_eq!(epoch, new);
    assert!(
        at >= cleared_at && at - cleared_at < FRAME,
        "cleared at {at:?}, command at {cleared_at:?}"
    );
    // Old audio the client heard was released before the clear (about one frame per 20 ms, less
    // what was still in the client's buffer). None arrives after the clear.
    assert!(
        !old_heard.is_empty() && old_heard.len() <= 7,
        "{old_heard:?}"
    );
    assert!(old_heard.iter().all(|heard| *heard <= at), "{old_heard:?}");
    assert!(new_heard.iter().all(|heard| *heard > at), "{new_heard:?}");

    // The cut utterance's final position was reported once, by the clear (see above).
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(reports_of(&events, old, 1).is_empty(), "{events:?}");
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn mark_is_sent_behind_its_audio_and_acknowledged() {
    let (clock, mut client, mut session, _cancel) = start().await;
    for seq in 0..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();

    let mut heard = Vec::new();
    let mark_at = loop {
        match next_client(&mut client).await {
            ClientEvent::Audio { received_at, .. } => heard.push(received_at),
            ClientEvent::Mark { id, received_at } => {
                assert_eq!(id, "m1");
                break received_at;
            }
            other => panic!("unexpected client event: {other:?}"),
        }
    };
    // The mark arrives right behind the audio it follows, not before it.
    assert_eq!(heard.len(), 10);
    assert_eq!(mark_at, *heard.last().unwrap());

    client.ack_mark("m1").await.unwrap();
    let acknowledged = loop {
        match next_event(&mut session).await {
            TransportEvent::MarkPlayed { id } => break id,
            TransportEvent::PlaybackPosition { .. } => continue,
            other => panic!("unexpected engine event: {other:?}"),
        }
    };
    assert_eq!(acknowledged, "m1");
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn playback_positions_are_monotonic_and_equal_the_released_audio() {
    let (clock, mut client, mut session, _cancel) = start().await;
    // Utterance 7: 30 frames (600 ms); utterance 8: 5 frames (100 ms).
    for seq in 0..30 {
        session
            .send(audio(&clock, 7, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    for seq in 30..35 {
        session
            .send(audio(&clock, 8, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    for _ in 0..35 {
        assert!(matches!(
            next_client(&mut client).await,
            ClientEvent::Audio { .. }
        ));
    }

    session.send(TransportCommand::Stop).await.unwrap();
    let mut reports = Vec::new();
    let mut stopped = None;
    while let Some(event) = tokio::time::timeout(WAIT, session.recv()).await.unwrap() {
        match event {
            TransportEvent::PlaybackPosition {
                utterance, played, ..
            } => reports.push((utterance, played)),
            TransportEvent::Stopped { reason } => stopped = Some(reason),
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
    assert_eq!(stopped, Some(StopReason::Requested));

    let of = |utterance: u64| -> Vec<Duration> {
        reports
            .iter()
            .filter(|(u, _)| *u == UtteranceId::new(utterance))
            .map(|(_, played)| *played)
            .collect()
    };
    let seven = of(7);
    let eight = of(8);
    assert!(seven.windows(2).all(|w| w[0] < w[1]), "{seven:?}");
    assert!(eight.windows(2).all(|w| w[0] < w[1]), "{eight:?}");
    assert_eq!(seven.last(), Some(&Duration::from_millis(600)));
    assert_eq!(eight.last(), Some(&Duration::from_millis(100)));
    // Utterance 7 is finished before utterance 8 reports anything.
    let first_eight = reports
        .iter()
        .position(|(u, _)| *u == UtteranceId::new(8))
        .unwrap();
    assert!(
        reports[..first_eight]
            .iter()
            .all(|(u, _)| *u == UtteranceId::new(7))
    );
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stop_ends_both_directions_and_leaves_no_task_behind() {
    let (clock, mut client, mut session, _cancel) = start().await;
    for seq in 0..3 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session.send(TransportCommand::Stop).await.unwrap();

    // The stop reason is the last engine event; then the stream closes.
    let mut last = None;
    while let Some(event) = tokio::time::timeout(WAIT, session.recv()).await.unwrap() {
        last = Some(event);
    }
    assert!(matches!(
        last,
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));

    // The client hears its audio, then the hang-up.
    loop {
        match next_client(&mut client).await {
            ClientEvent::Audio { .. } => {}
            ClientEvent::Hangup { .. } => break,
            other => panic!("unexpected client event: {other:?}"),
        }
    }

    session.close(Duration::from_secs(1)).await.unwrap();
    // The pacer pipeline and the loopback adapter have ended: only the test and its client hold
    // the clock now.
    settle_clock(&clock, 2).await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_the_owner_ends_the_session_as_cancelled() {
    let (clock, mut client, mut session, cancel) = start().await;
    for seq in 0..5 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    cancel.cancel();

    let mut last = None;
    while let Some(event) = tokio::time::timeout(WAIT, session.recv()).await.unwrap() {
        last = Some(event);
    }
    assert!(matches!(
        last,
        Some(TransportEvent::Stopped {
            reason: StopReason::Cancelled
        })
    ));
    session.close(Duration::from_secs(1)).await.unwrap();
    // The adapter saw the end of the call and hung up the client.
    loop {
        match next_client(&mut client).await {
            ClientEvent::Hangup { .. } => break,
            ClientEvent::Audio { .. } => {}
            other => panic!("unexpected client event: {other:?}"),
        }
    }
    settle_clock(&clock, 2).await;
}

#[tokio::test(start_paused = true)]
async fn slow_engine_loses_only_input_audio_and_keeps_dtmf_in_order() {
    // A tiny engine queue: 8 events. The engine reads nothing for 200 ms while the caller talks.
    let config = SessionConfig {
        events_capacity: 8,
        ..SessionConfig::default()
    };
    let (clock, mut client, mut session, _cancel) = start_with(config).await;

    for seq in 0..50u8 {
        client
            .send_audio(pcm(seq), SampleRate::Hz16000)
            .await
            .unwrap();
    }
    client.send_dtmf(DtmfDigit::D5).await.unwrap();
    for seq in 50..100u8 {
        client
            .send_audio(pcm(seq), SampleRate::Hz16000)
            .await
            .unwrap();
    }
    client.send_dtmf(DtmfDigit::Pound).await.unwrap();
    clock.sleep(Duration::from_millis(200)).await;

    let mut seqs = Vec::new();
    let mut digits = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(500), session.recv()).await {
            Ok(Some(TransportEvent::Audio(frame))) => seqs.push(frame.seq()),
            Ok(Some(TransportEvent::Dtmf { digit, .. })) => digits.push(digit),
            Ok(Some(other)) => panic!("unexpected engine event: {other:?}"),
            Ok(None) | Err(_) => break,
        }
    }

    // DTMF is lossless and ordered; audio is lossy, oldest first, so the newest frame survives
    // and the sequence stays increasing.
    assert_eq!(digits, vec![DtmfDigit::D5, DtmfDigit::Pound]);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(seqs.last(), Some(&99));
    assert!(seqs.len() < 100, "nothing was dropped: {}", seqs.len());
    assert!(session.event_stats().dropped_oldest > 0);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[derive(Debug, PartialEq, Eq)]
enum Item {
    /// Drops the oldest queued item of its class.
    Lossy(u32),
    /// Waits for space.
    Lossless(u32),
    /// Dropped when the channel is full.
    Newest(u32),
    /// Replaces the queued item with the same key, else evicts a lossy one, else waits.
    Latest(u8, u32),
}

impl Overflowing for Item {
    fn overflow(&self) -> Overflow {
        match self {
            Self::Lossy(_) => Overflow::DropOldest,
            Self::Lossless(_) => Overflow::Backpressure,
            Self::Newest(_) => Overflow::DropNewest,
            Self::Latest(..) => Overflow::Displace,
        }
    }

    fn class(&self) -> u8 {
        match self {
            Self::Lossy(_) => 1,
            _ => 0,
        }
    }

    fn supersedes(&self, queued: &Self) -> bool {
        matches!((self, queued), (Self::Latest(a, _), Self::Latest(b, _)) if a == b)
    }
}

#[tokio::test(start_paused = true)]
async fn channel_overflow_policies_hold_under_a_slow_consumer() {
    let (tx, mut rx) = channel::bounded::<Item>(3);
    for n in 1..=4 {
        tx.send(Item::Lossy(n)).await.unwrap();
    }
    // Lossy(1) was the oldest of its class: evicted, counted, never delivered.
    assert_eq!(
        tx.stats(),
        ChannelStats {
            dropped_oldest: 1,
            dropped_newest: 0
        }
    );

    // The channel is full: a lossless item waits, and nothing it waits for is lost.
    let waiting = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send(Item::Lossless(100)).await }
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "a backpressure item must wait");
    assert_eq!(rx.recv().await, Some(Item::Lossy(2)));
    waiting.await.unwrap().unwrap();

    // Queue is [3, 4, 100] again: a newest-policy item is dropped.
    tx.send(Item::Newest(7)).await.unwrap();
    assert_eq!(
        tx.stats(),
        ChannelStats {
            dropped_oldest: 1,
            dropped_newest: 1
        }
    );
    assert_eq!(rx.recv().await, Some(Item::Lossy(3)));
    assert_eq!(rx.recv().await, Some(Item::Lossy(4)));
    assert_eq!(rx.recv().await, Some(Item::Lossless(100)));

    drop(tx);
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn displace_supersedes_then_evicts_lossy_then_waits() {
    let (tx, mut rx) = channel::bounded::<Item>(3);
    tx.send(Item::Latest(1, 1)).await.unwrap();
    tx.send(Item::Lossless(10)).await.unwrap();
    tx.send(Item::Lossy(5)).await.unwrap();

    // Same key: the older one is removed and the new one queued at the back; nothing waits.
    tx.send(Item::Latest(1, 2)).await.unwrap();
    // New key: nothing to replace, so the lossy item makes room and the new one goes last.
    tx.send(Item::Latest(2, 1)).await.unwrap();
    assert_eq!(
        tx.stats(),
        ChannelStats {
            dropped_oldest: 2,
            dropped_newest: 0
        }
    );

    // The queue is [Lossless(10), Latest(1,2), Latest(2,1)]: everything left is lossless, so a
    // third key has to wait for space.
    let waiting = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send(Item::Latest(3, 1)).await }
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "nothing was evictable");
    assert_eq!(rx.recv().await, Some(Item::Lossless(10)));
    waiting.await.unwrap().unwrap();
    assert_eq!(rx.recv().await, Some(Item::Latest(1, 2)));
    assert_eq!(rx.recv().await, Some(Item::Latest(2, 1)));
    assert_eq!(rx.recv().await, Some(Item::Latest(3, 1)));

    // Purging removes queued items without counting them as overflow.
    tx.send(Item::Lossless(1)).await.unwrap();
    tx.send(Item::Lossy(2)).await.unwrap();
    assert_eq!(tx.purge(|item| matches!(item, Item::Lossy(_))), 1);
    assert_eq!(rx.recv().await, Some(Item::Lossless(1)));
    assert_eq!(tx.stats().dropped_oldest, 2);
}

/// Reads engine events until the stream closes (or two seconds of silence).
async fn drain_events(session: &mut TransportSession) -> Vec<TransportEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), session.recv()).await {
        events.push(event);
    }
    events
}

/// Reports of one `(epoch, utterance)`, in the order they arrived.
fn reports_of(events: &[TransportEvent], epoch: TurnEpoch, utterance: u64) -> Vec<Duration> {
    events
        .iter()
        .filter_map(|event| match event {
            TransportEvent::PlaybackPosition {
                epoch: e,
                utterance: u,
                played,
            } if *e == epoch && *u == UtteranceId::new(utterance) => Some(*played),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn clear_is_immediate_behind_a_deep_queue() {
    let (clock, mut client, session, _cancel) = start().await;
    let new = TurnEpoch::new(1);
    // 2 s of audio handed over at once: more than the command queue (64) holds.
    for seq in 0..100 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(50)).await;
    let cleared_at = clock.now();
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    // The clear is applied while `send` runs: it did not wait for any of the 100 frames.
    assert_eq!(clock.now(), cleared_at);
    for seq in 100..103 {
        session.send(audio(&clock, 2, new, seq)).await.unwrap();
    }

    let mut old_heard = 0;
    let mut cleared = None;
    let mut new_heard = 0;
    while new_heard < 3 {
        match next_client(&mut client).await {
            ClientEvent::Audio { frame, received_at } if frame.epoch() == new => {
                assert!(received_at > cleared.expect("new audio before the clear"));
                new_heard += 1;
            }
            ClientEvent::Audio { received_at, .. } => {
                // Only audio the client had before the clear.
                assert!(
                    cleared.is_none(),
                    "old audio after the clear: {received_at:?}"
                );
                old_heard += 1;
            }
            ClientEvent::Clear { at, .. } => cleared = Some(at),
            other => panic!("unexpected client event: {other:?}"),
        }
    }
    let at = cleared.expect("the client was told to clear");
    assert!(
        at - cleared_at < FRAME,
        "cleared {:?} late",
        at - cleared_at
    );
    assert!(old_heard <= 6, "{old_heard} old frames reached the client");
    // The old queue is not played at all: the new audio follows within a fraction of a second.
    assert!(clock.now() - cleared_at < Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn at_most_one_released_frame_follows_a_clear_on_the_wire() {
    // The adapter is a plain test endpoint that does not filter anything: what it receives is
    // exactly what the core wrote to the wire.
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, mut ends) = call_pair();
    let session = TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
        .await
        .unwrap();
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    // The adapter reads nothing: the wire queue fills with released frames and the output task
    // blocks writing the next one.
    clock.sleep(Duration::from_millis(500)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    session
        .send(audio(&clock, 2, TurnEpoch::new(1), 100))
        .await
        .unwrap();

    let mut old_before_clear = 0;
    let mut new_after_clear = 0;
    let mut cleared = false;
    while new_after_clear < 1 {
        let out = tokio::time::timeout(WAIT, ends.outgoing.recv())
            .await
            .expect("wire read timed out")
            .expect("wire closed");
        match out {
            WireOut::Audio(frame) if frame.epoch() == TurnEpoch::ZERO => {
                assert!(!cleared, "old audio after WireOut::Clear");
                old_before_clear += 1;
            }
            WireOut::Audio(_) => {
                assert!(cleared, "new audio before WireOut::Clear");
                new_after_clear += 1;
            }
            WireOut::Clear { epoch } => {
                assert_eq!(epoch, TurnEpoch::new(1));
                cleared = true;
            }
            other => panic!("unexpected wire item: {other:?}"),
        }
    }
    assert!(
        old_before_clear <= 1,
        "{old_before_clear} old frames were written after the clear command"
    );
}

#[tokio::test(start_paused = true)]
async fn playback_positions_stay_monotonic_when_utterances_interleave() {
    let (clock, mut client, mut session, _cancel) = start().await;
    for (seq, utterance) in [(0, 1), (1, 2), (2, 1)] {
        session
            .send(audio(&clock, utterance, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    for _ in 0..3 {
        assert!(matches!(
            next_client(&mut client).await,
            ClientEvent::Audio { .. }
        ));
    }
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;

    let one = reports_of(&events, TurnEpoch::ZERO, 1);
    let two = reports_of(&events, TurnEpoch::ZERO, 2);
    assert!(one.windows(2).all(|w| w[0] < w[1]), "{one:?}");
    assert_eq!(one.last(), Some(&Duration::from_millis(40)), "{one:?}");
    assert_eq!(two.last(), Some(&Duration::from_millis(20)), "{two:?}");
}

#[tokio::test(start_paused = true)]
async fn the_same_utterance_id_in_a_new_epoch_counts_on_its_own() {
    // No interval reports: the old epoch's total can only come from the clear's final report.
    let config = SessionConfig {
        position_interval: Duration::from_secs(10),
        ..SessionConfig::default()
    };
    let (clock, mut client, mut session, _cancel) = start_with(config).await;
    let new = TurnEpoch::new(1);
    for seq in 0..5 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    for _ in 0..5 {
        assert!(matches!(
            next_client(&mut client).await,
            ClientEvent::Audio { .. }
        ));
    }
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut session).await,
        TransportEvent::PlaybackPosition {
            epoch: TurnEpoch::ZERO,
            utterance: UtteranceId::new(1),
            played: Duration::from_millis(100),
        }
    );
    for seq in 5..8 {
        session.send(audio(&clock, 1, new, seq)).await.unwrap();
    }
    let mut heard = 0;
    while heard < 3 {
        if let ClientEvent::Audio { .. } = next_client(&mut client).await {
            heard += 1;
        }
    }
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;

    // The old epoch's total (100 ms) was reported by the clear, before the new epoch started.
    let before = reports_of(&events, TurnEpoch::ZERO, 1);
    let after = reports_of(&events, new, 1);
    assert!(before.is_empty(), "{before:?}");
    assert!(after.windows(2).all(|w| w[0] < w[1]), "{after:?}");
    assert_eq!(after.last(), Some(&Duration::from_millis(60)));
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_does_not_read_events_does_not_stall_the_output() {
    let (clock, mut client, mut session, _cancel) = start().await;
    let listener = tokio::spawn(async move {
        let mut heard = 0u32;
        while let Some(event) = client.recv().await {
            if matches!(event, ClientEvent::Audio { .. }) {
                heard += 1;
            }
        }
        heard
    });
    // 20 s of audio sent from one task that never reads events: a position report is produced
    // every 100 ms, far more than the 64 event slots.
    let sending = async {
        for seq in 0..1000 {
            session
                .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
                .await
                .unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(60), sending)
        .await
        .expect("sending stalled behind unread events");
    clock.sleep(Duration::from_secs(5)).await;

    // The reports were coalesced: the latest one is the whole utterance.
    assert!(session.event_stats().dropped_oldest > 0);
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    let reports = reports_of(&events, TurnEpoch::ZERO, 1);
    assert!(reports.windows(2).all(|w| w[0] < w[1]), "{reports:?}");
    assert_eq!(reports.last(), Some(&Duration::from_secs(20)));
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));
    session.close(Duration::from_secs(1)).await.unwrap();
    assert_eq!(listener.await.unwrap(), 1000);
}

#[tokio::test(start_paused = true)]
async fn stop_ends_the_session_when_the_client_never_reads() {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        client_queue: 4,
        send_timeout: Duration::from_secs(60),
        ..LoopbackConfig::default()
    };
    let (clock, client, mut session, _cancel) =
        start_full(SessionConfig::default(), loopback).await;
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    // The client holds its end but reads nothing: its 4 slots fill and the adapter blocks
    // handing it the next frame.
    clock.sleep(Duration::from_secs(2)).await;
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));
    session.close(Duration::from_secs(1)).await.unwrap();
    // The adapter left its blocked send: only the test and the client still hold the clock.
    settle_clock(&clock, 2).await;
    drop(client);
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_the_adapter_that_is_blocked_on_a_client_that_never_reads() {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        client_queue: 4,
        send_timeout: Duration::from_secs(60),
        ..LoopbackConfig::default()
    };
    let (clock, client, session, cancel) = start_full(SessionConfig::default(), loopback).await;
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_secs(2)).await;
    cancel.cancel();
    session.close(Duration::from_secs(1)).await.unwrap();
    settle_clock(&clock, 2).await;
    drop(client);
}

#[tokio::test(start_paused = true)]
async fn a_client_that_stops_reading_for_too_long_ends_the_call() {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        client_queue: 4,
        send_timeout: Duration::from_secs(1),
        ..LoopbackConfig::default()
    };
    let (clock, client, mut session, _cancel) =
        start_full(SessionConfig::default(), loopback).await;
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_secs(5)).await;
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::PeerDisconnected
        })
    ));
    session.close(Duration::from_secs(1)).await.unwrap();
    settle_clock(&clock, 2).await;
    drop(client);
}

#[tokio::test(start_paused = true)]
async fn a_mark_cut_by_a_clear_is_reported_as_cleared_and_never_sent() {
    let (clock, mut client, mut session, _cancel) = start().await;
    let new = TurnEpoch::new(1);
    for seq in 0..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(50)).await;
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    // The new answer is longer than the cut mark's position (200 ms of audio): had the clear left
    // the mark queued, it would be sent while this plays.
    for seq in 10..25 {
        session.send(audio(&clock, 2, new, seq)).await.unwrap();
    }
    // A mark registered after the clear works, and ends as played.
    session
        .send(TransportCommand::Mark {
            id: "m2".to_owned(),
        })
        .await
        .unwrap();

    // The client never gets the cut mark; it gets the new one right behind the new audio.
    let mut new_frames = 0;
    loop {
        match next_client(&mut client).await {
            ClientEvent::Audio { frame, .. } if frame.epoch() == new => new_frames += 1,
            ClientEvent::Mark { id, .. } => {
                assert_eq!(id, "m2", "cut mark {id} reached the client");
                assert_eq!(new_frames, 15);
                break;
            }
            _ => {}
        }
    }
    // Even a client that echoes the cut mark late cannot make it count as played.
    client.ack_mark("m1").await.unwrap();
    client.ack_mark("m2").await.unwrap();
    // The marker that tells the engine events are read up to the echoes.
    client.send_dtmf(DtmfDigit::D9).await.unwrap();

    let mut cleared = Vec::new();
    let mut played = Vec::new();
    loop {
        match next_event(&mut session).await {
            TransportEvent::MarkCleared { id } => cleared.push(id),
            TransportEvent::MarkPlayed { id } => played.push(id),
            TransportEvent::Dtmf { .. } => break,
            TransportEvent::PlaybackPosition { .. } => {}
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
    assert_eq!(cleared, vec!["m1".to_owned()]);
    assert_eq!(played, vec!["m2".to_owned()]);
}

#[tokio::test(start_paused = true)]
async fn a_mark_already_sent_to_the_client_is_cut_by_a_clear_and_its_echo_is_ignored() {
    // The client buffers 500 ms: when the clear arrives, the audio and the mark behind it have
    // been released and handed to the adapter, but the client has not played them.
    let loopback = LoopbackConfig {
        latency: Duration::from_millis(500),
        ..LoopbackConfig::default()
    };
    let (clock, mut client, mut session, _cancel) =
        start_full(SessionConfig::default(), loopback).await;
    for seq in 0..5 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(200)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();

    // The client was cleared before it heard anything, so it never gets the mark either.
    assert!(matches!(
        next_client(&mut client).await,
        ClientEvent::Clear { .. }
    ));
    // The engine learns the mark is gone, although it had been sent.
    loop {
        match next_event(&mut session).await {
            TransportEvent::MarkCleared { id } => {
                assert_eq!(id, "m1");
                break;
            }
            TransportEvent::PlaybackPosition { .. } => {}
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
    // A client that echoes anyway (a protocol that echoes buffered marks on a clear) cannot turn
    // it into a played mark.
    client.ack_mark("m1").await.unwrap();
    client.send_dtmf(DtmfDigit::D1).await.unwrap();
    loop {
        match next_event(&mut session).await {
            TransportEvent::Dtmf { .. } => break,
            TransportEvent::PlaybackPosition { .. } => {}
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_clear_pulls_released_marks_out_of_the_wire_queue_with_their_audio() {
    // The adapter is a plain endpoint that reads nothing until after the clear: what it then
    // receives is exactly what the core left on the wire.
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, mut ends) = call_pair();
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    for seq in 0..3 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();
    // Three frames and the mark are released into the wire queue; the adapter takes none.
    clock.sleep(Duration::from_millis(200)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();

    // The first thing the adapter sees is the clear: neither the audio nor the mark that sat
    // behind it in the queue.
    let first = tokio::time::timeout(WAIT, ends.outgoing.recv())
        .await
        .expect("wire read timed out")
        .expect("wire closed");
    assert!(
        matches!(first, WireOut::Clear { epoch } if epoch == TurnEpoch::new(1)),
        "{first:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), ends.outgoing.recv())
            .await
            .is_err(),
        "something else was left on the wire"
    );

    // The engine hears the mark was cut, and an echo does not change that.
    ends.events
        .send(WireEvent::MarkAck {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Dtmf {
            digit: DtmfDigit::D2,
            at: clock.now(),
        })
        .await
        .unwrap();
    let mut cleared = 0;
    loop {
        match next_event(&mut session).await {
            TransportEvent::MarkCleared { id } => {
                assert_eq!(id, "m1");
                cleared += 1;
            }
            TransportEvent::Dtmf { .. } => break,
            TransportEvent::PlaybackPosition { .. } => {}
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
    assert_eq!(cleared, 1);
}

#[tokio::test(start_paused = true)]
async fn a_mark_still_in_the_command_queue_when_a_clear_arrives_is_cut_too() {
    let (clock, _client, mut session, _cancel) = start().await;
    // 100 frames fill the pacer, so the mark waits in the command queue behind the rest.
    for seq in 0..100 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "late".to_owned(),
        })
        .await
        .unwrap();
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    loop {
        match next_event(&mut session).await {
            TransportEvent::MarkCleared { id } => {
                assert_eq!(id, "late");
                break;
            }
            TransportEvent::PlaybackPosition { .. } => {}
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stop_drops_the_audio_that_is_still_pending() {
    let (clock, mut client, mut session, _cancel) = start().await;
    for seq in 0..50 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(100)).await;
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));
    let mut heard = 0;
    loop {
        match next_client(&mut client).await {
            ClientEvent::Audio { .. } => heard += 1,
            ClientEvent::Hangup { .. } => break,
            other => panic!("unexpected client event: {other:?}"),
        }
    }
    // About 100 ms of the 1 s were released; the other frames were dropped.
    assert!(
        (1..=7).contains(&heard),
        "{heard} frames reached the client"
    );
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stale_audio_is_rejected_and_an_old_clear_is_ignored() {
    let (clock, mut client, session, _cancel) = start().await;
    let (one, two) = (TurnEpoch::new(1), TurnEpoch::new(2));
    session
        .send(TransportCommand::Clear { epoch: two })
        .await
        .unwrap();
    // Epoch 1 is older than the current epoch 2: its audio never plays, and a clear to it changes
    // nothing.
    for seq in 0..3 {
        session.send(audio(&clock, 1, one, seq)).await.unwrap();
    }
    session
        .send(TransportCommand::Clear { epoch: one })
        .await
        .unwrap();
    for seq in 3..5 {
        session.send(audio(&clock, 2, two, seq)).await.unwrap();
    }

    let mut clears = 0;
    let mut heard = Vec::new();
    while heard.len() < 2 {
        match next_client(&mut client).await {
            ClientEvent::Clear { epoch, .. } => {
                assert_eq!(epoch, two);
                clears += 1;
            }
            ClientEvent::Audio { frame, .. } => heard.push(frame.epoch()),
            other => panic!("unexpected client event: {other:?}"),
        }
    }
    assert_eq!(heard, vec![two, two]);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), client.recv())
            .await
            .is_err(),
        "something else reached the client"
    );
    assert_eq!(clears, 1);
}

#[tokio::test(start_paused = true)]
async fn the_client_hanging_up_ends_the_call_as_a_peer_hangup() {
    let (_clock, client, mut session, _cancel) = start().await;
    client.hangup().await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::PeerHangup
        })
    ));
    assert_eq!(session.stop_reason(), Some(StopReason::PeerHangup));
}

#[tokio::test(start_paused = true)]
async fn a_client_that_vanishes_ends_the_call_as_a_lost_peer() {
    let (_clock, client, mut session, _cancel) = start().await;
    drop(client);
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::PeerDisconnected
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn protocol_faults_reach_the_engine_as_errors() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, ends) = call_pair();
    let mut session = TransportSession::open(clock, link, SessionConfig::default(), &cancel)
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Started(CallInfo::default()))
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Fault {
            message: "bad frame".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        TransportEvent::Started(_)
    ));
    assert_eq!(
        next_event(&mut session).await,
        TransportEvent::Error {
            message: "bad frame".to_owned()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn the_stop_reason_survives_a_stopped_event_the_engine_never_made_room_for() {
    let config = SessionConfig {
        events_capacity: 2,
        ..SessionConfig::default()
    };
    let (clock, client, mut session, cancel) = start_with(config).await;
    // Key presses fill the event queue with lossless events that nothing can displace.
    for _ in 0..4 {
        client.send_dtmf(DtmfDigit::D1).await.unwrap();
    }
    clock.sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    clock.sleep(Duration::from_secs(5)).await;

    assert_eq!(session.stop_reason(), Some(StopReason::Cancelled));
    let events = drain_events(&mut session).await;
    assert!(
        events
            .iter()
            .all(|event| matches!(event, TransportEvent::Dtmf { .. })),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn caller_audio_is_tagged_with_the_current_epoch_and_keeps_its_stamps() {
    let (clock, mut client, mut session, _cancel) = start().await;
    let epoch = TurnEpoch::new(3);
    session
        .send(TransportCommand::Clear { epoch })
        .await
        .unwrap();

    // Two frames and a key press, each at its own simulated time.
    let mut stamps = Vec::new();
    for seq in 0..2u8 {
        clock.sleep(Duration::from_millis(40)).await;
        stamps.push(clock.now());
        client
            .send_audio(pcm(seq), SampleRate::Hz16000)
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(40)).await;
    let pressed_at = clock.now();
    client.send_dtmf(DtmfDigit::Star).await.unwrap();

    for (seq, stamp) in stamps.into_iter().enumerate() {
        match next_event(&mut session).await {
            TransportEvent::Audio(frame) => {
                // The client's frames are tagged epoch 0; the session re-tags them.
                assert_eq!(frame.epoch(), epoch);
                assert_eq!(frame.seq(), seq as u64);
                assert_eq!(frame.timestamp(), stamp);
                assert_eq!(frame.data()[0], seq as u8);
            }
            other => panic!("unexpected engine event: {other:?}"),
        }
    }
    match next_event(&mut session).await {
        TransportEvent::Dtmf { digit, at } => {
            assert_eq!(digit, DtmfDigit::Star);
            assert_eq!(at, pressed_at);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_report_that_cancellation_interrupts_is_still_delivered_at_the_end() {
    // A tiny event queue that key presses fill: the engine reads nothing, so the output task
    // waits to queue its position report (nothing in the queue can be displaced).
    let config = SessionConfig {
        events_capacity: 2,
        ..SessionConfig::default()
    };
    let (clock, client, mut session, cancel) = start_with(config).await;
    client.send_dtmf(DtmfDigit::D1).await.unwrap();
    client.send_dtmf(DtmfDigit::D2).await.unwrap();
    for seq in 0..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    // Five frames (100 ms) are released at 95 ms: the interval report is due and blocked.
    clock.sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    clock.sleep(Duration::from_millis(50)).await;

    // The engine starts reading within the final report's grace period.
    let events = drain_events(&mut session).await;
    assert_eq!(
        reports_of(&events, TurnEpoch::ZERO, 1),
        vec![Duration::from_millis(100)],
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Cancelled
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_hangup_the_adapter_queued_before_it_left_is_delivered_with_what_preceded_it() {
    let config = SessionConfig {
        events_capacity: 2,
        ..SessionConfig::default()
    };
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, ends) = call_pair();
    let mut session = TransportSession::open(clock.clone(), link, config, &cancel)
        .await
        .unwrap();

    // The adapter reports a call, two key presses and a hang-up, and returns: both its ends are
    // dropped. The engine has read nothing, so the input task is backed up behind the 2-slot
    // event queue.
    ends.events
        .send(WireEvent::Started(CallInfo::default()))
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Dtmf {
            digit: DtmfDigit::D1,
            at: clock.now(),
        })
        .await
        .unwrap();
    ends.events
        .send(WireEvent::Dtmf {
            digit: DtmfDigit::D2,
            at: clock.now(),
        })
        .await
        .unwrap();
    ends.events.send(WireEvent::Hangup).await.unwrap();
    drop(ends);
    // Output audio makes the output task write to the closed wire.
    session
        .send(audio(&clock, 1, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(100)).await;

    let events = drain_events(&mut session).await;
    let digits: Vec<DtmfDigit> = events
        .iter()
        .filter_map(|event| match event {
            TransportEvent::Dtmf { digit, .. } => Some(*digit),
            _ => None,
        })
        .collect();
    assert_eq!(digits, vec![DtmfDigit::D1, DtmfDigit::D2], "{events:?}");
    assert!(
        matches!(
            events.last(),
            Some(TransportEvent::Stopped {
                reason: StopReason::PeerHangup
            })
        ),
        "{events:?}"
    );
    assert_eq!(session.stop_reason(), Some(StopReason::PeerHangup));
}

#[tokio::test(start_paused = true)]
async fn a_closed_wire_without_a_hangup_ends_the_call_as_a_lost_peer() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, ends) = call_pair();
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    // The adapter drops the audio side but keeps its event side open and silent.
    let CallEnds {
        events, outgoing, ..
    } = ends;
    drop(outgoing);
    session
        .send(audio(&clock, 1, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    // Bounded: the input task is given a second, then the call ends.
    clock.sleep(Duration::from_secs(3)).await;
    assert_eq!(session.stop_reason(), Some(StopReason::PeerDisconnected));
    let seen = drain_events(&mut session).await;
    assert!(matches!(
        seen.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::PeerDisconnected
        })
    ));
    drop(events);
}

#[tokio::test(start_paused = true)]
async fn the_hangup_is_queued_on_the_wire_before_the_adapter_is_told_the_call_ended() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, mut ends) = call_pair();
    let session = TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
        .await
        .unwrap();
    // The adapter reads nothing: the wire queue (8) fills with released frames.
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(500)).await;
    session.send(TransportCommand::Stop).await.unwrap();

    // The hang-up has no room yet, and `ended` is cancelled only after it was queued.
    clock.sleep(Duration::from_millis(500)).await;
    assert_eq!(session.stop_reason(), Some(StopReason::Requested));
    assert!(!ends.ended.is_cancelled(), "ended before the hang-up");

    // The adapter reads: it gets what was queued, then the hang-up, as the last item.
    let mut last = None;
    while let Ok(Some(item)) =
        tokio::time::timeout(Duration::from_millis(200), ends.outgoing.recv()).await
    {
        let hangup = matches!(item, WireOut::Hangup);
        last = Some(item);
        if hangup {
            break;
        }
    }
    assert!(matches!(last, Some(WireOut::Hangup)), "{last:?}");
    tokio::time::timeout(WAIT, ends.ended.cancelled())
        .await
        .expect("the adapter was never told the call ended");
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_adapter_that_never_reads_the_hangup_is_told_the_call_ended_after_a_second() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, ends) = call_pair();
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(500)).await;
    session.send(TransportCommand::Stop).await.unwrap();
    clock.sleep(Duration::from_millis(500)).await;
    assert!(!ends.ended.is_cancelled());
    // The hang-up wait is bounded (1 s): then the adapter is told, and `Stopped` follows.
    clock.sleep(Duration::from_millis(700)).await;
    assert!(ends.ended.is_cancelled(), "the hang-up wait never ended");
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn an_adapter_that_takes_nothing_for_ten_seconds_fails_the_session() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, ends) = call_pair();
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    for seq in 0..40 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    // The adapter holds its ends but never reads: the wire queue fills at about 0.2 s and the
    // output task waits for room. Just short of the 10 s limit nothing has happened.
    clock.sleep(Duration::from_secs(9)).await;
    assert_eq!(session.stop_reason(), None);
    assert!(!ends.ended.is_cancelled());

    // The failure is recorded at about 10.2 s; the adapter is told the call ended after the
    // hang-up wait (1 s, nobody reads the wire).
    clock.sleep(Duration::from_secs(3)).await;
    let reason = session
        .stop_reason()
        .expect("the stalled wire ended the call");
    match &reason {
        StopReason::Failed(message) => assert!(message.contains("wire write"), "{message}"),
        other => panic!("unexpected stop reason {other:?}"),
    }
    assert!(ends.ended.is_cancelled());
    let events = drain_events(&mut session).await;
    assert_eq!(
        events.last(),
        Some(&TransportEvent::Stopped { reason }),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn drop_oldest_evicts_only_its_own_class() {
    let (tx, mut rx) = channel::bounded::<Item>(3);
    tx.send(Item::Lossless(1)).await.unwrap();
    tx.send(Item::Lossy(2)).await.unwrap();
    tx.send(Item::Lossy(3)).await.unwrap();
    // Full: the oldest *lossy* item goes, not the older lossless one in front of it.
    tx.send(Item::Lossy(4)).await.unwrap();
    assert_eq!(
        tx.stats(),
        ChannelStats {
            dropped_oldest: 1,
            dropped_newest: 0
        }
    );
    assert_eq!(rx.recv().await, Some(Item::Lossless(1)));
    assert_eq!(rx.recv().await, Some(Item::Lossy(3)));
    assert_eq!(rx.recv().await, Some(Item::Lossy(4)));

    // Full of lossless items there is nothing of its class to evict: the new item is dropped.
    for n in 10..13 {
        tx.send(Item::Lossless(n)).await.unwrap();
    }
    tx.send(Item::Lossy(5)).await.unwrap();
    assert_eq!(
        tx.stats(),
        ChannelStats {
            dropped_oldest: 1,
            dropped_newest: 1
        }
    );
    for n in 10..13 {
        assert_eq!(rx.recv().await, Some(Item::Lossless(n)));
    }
    drop(tx);
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn a_held_command_sender_fails_after_the_session_ended_and_keeps_nothing_alive() {
    let (clock, client, mut session, _cancel) = start().await;
    let held = session.commands();
    session
        .send(audio(&clock, 1, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    held.send(TransportCommand::Mark { id: "m".to_owned() })
        .await
        .unwrap();
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));

    // Nothing is accepted any more, whatever the command.
    assert!(
        held.send(audio(&clock, 1, TurnEpoch::ZERO, 1))
            .await
            .is_err()
    );
    assert!(
        held.send(TransportCommand::Clear {
            epoch: TurnEpoch::new(9)
        })
        .await
        .is_err()
    );
    assert!(held.send(TransportCommand::Stop).await.is_err());
    assert!(session.send(TransportCommand::Stop).await.is_err());

    // The clone is still alive, yet the session closes and the pacer pipeline and adapter are
    // gone (only the test and its client hold the clock).
    session.close(Duration::from_secs(1)).await.unwrap();
    settle_clock(&clock, 2).await;
    drop(held);
    drop(client);
}

#[tokio::test(start_paused = true)]
async fn clears_from_different_clones_apply_in_epoch_order() {
    let (clock, mut client, session, _cancel) = start().await;
    let (a, b) = (session.commands(), session.commands());
    for seq in 0..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(60)).await;
    // Two barge-in detectors fire at once; the later epoch is polled first.
    let (second, first) = tokio::join!(
        b.send(TransportCommand::Clear {
            epoch: TurnEpoch::new(3)
        }),
        a.send(TransportCommand::Clear {
            epoch: TurnEpoch::new(2)
        }),
    );
    second.unwrap();
    first.unwrap();
    // Audio of the final epoch plays; audio of the epoch that lost the race does not.
    session
        .send(audio(&clock, 2, TurnEpoch::new(2), 20))
        .await
        .unwrap();
    session
        .send(audio(&clock, 3, TurnEpoch::new(3), 21))
        .await
        .unwrap();

    let mut clears = Vec::new();
    let mut new_epochs = Vec::new();
    while new_epochs.is_empty() {
        match next_client(&mut client).await {
            ClientEvent::Clear { epoch, .. } => clears.push(epoch),
            ClientEvent::Audio { frame, .. } if frame.epoch() != TurnEpoch::ZERO => {
                new_epochs.push(frame.epoch());
            }
            _ => {}
        }
    }
    assert!(clears.windows(2).all(|w| w[0] < w[1]), "{clears:?}");
    assert_eq!(clears.last(), Some(&TurnEpoch::new(3)));
    assert_eq!(new_epochs, vec![TurnEpoch::new(3)]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), client.recv())
            .await
            .is_err(),
        "audio of the superseded epoch reached the client"
    );
}

#[tokio::test(start_paused = true)]
async fn the_wire_closes_for_the_adapter_although_a_command_sender_is_still_held() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, mut ends) = call_pair();
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel)
            .await
            .unwrap();
    // A barge-in task holds a sender for the whole life of the process.
    let held = session.commands();
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain_events(&mut session).await;
    assert!(matches!(
        events.last(),
        Some(TransportEvent::Stopped {
            reason: StopReason::Requested
        })
    ));
    // The adapter reads to the end of the wire: it must not wait for the clone to go away.
    loop {
        match tokio::time::timeout(WAIT, ends.outgoing.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => panic!("the wire stayed open behind a held command sender"),
        }
    }
    drop(held);
}

#[tokio::test(start_paused = true)]
async fn a_mark_registered_after_a_clear_never_overtakes_the_clear_on_the_wire() {
    // The engine queue is full of key presses, so the output task is held up queueing a position
    // report right after it released a frame. A clear and a new mark arrive meanwhile.
    let config = SessionConfig {
        events_capacity: 2,
        ..SessionConfig::default()
    };
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let (link, mut ends) = call_pair();
    let mut session = TransportSession::open(clock.clone(), link, config, &cancel)
        .await
        .unwrap();
    for digit in [DtmfDigit::D1, DtmfDigit::D2] {
        ends.events
            .send(WireEvent::Dtmf {
                digit,
                at: clock.now(),
            })
            .await
            .unwrap();
    }
    for seq in 0..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    clock.sleep(Duration::from_millis(150)).await;
    let new = TurnEpoch::new(1);
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    session
        .send(TransportCommand::Mark { id: "n".to_owned() })
        .await
        .unwrap();
    clock.sleep(Duration::from_millis(10)).await;

    // The engine reads again: the blocked report goes out and the output task carries on.
    for _ in 0..2 {
        assert!(matches!(
            next_event(&mut session).await,
            TransportEvent::Dtmf { .. }
        ));
    }
    let mut order = Vec::new();
    while let Ok(Some(item)) =
        tokio::time::timeout(Duration::from_millis(300), ends.outgoing.recv()).await
    {
        match item {
            WireOut::Clear { epoch } => order.push(format!("clear{}", epoch.get())),
            WireOut::Mark { id } => order.push(format!("mark {id}")),
            WireOut::Audio(_) => order.push("audio".to_owned()),
            WireOut::Hangup => order.push("hangup".to_owned()),
        }
    }
    assert_eq!(order, vec!["clear1".to_owned(), "mark n".to_owned()]);
}

/// Starts a call whose loopback client echoes marks as `mark_acks` says.
async fn start_acking(
    mark_acks: MarkAcks,
) -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        mark_acks,
        ..LoopbackConfig::default()
    };
    start_full(SessionConfig::default(), loopback).await
}

/// Sends `frames` frames of utterance 1 and a mark `id` behind them.
async fn send_audio_and_mark(
    clock: &SharedClock,
    session: &TransportSession,
    frames: u64,
    id: &str,
) {
    for seq in 0..frames {
        session
            .send(audio(clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark { id: id.to_owned() })
        .await
        .unwrap();
}

/// Waits for the client to receive the mark and returns when it did.
async fn mark_received_at(client: &mut LoopbackClient, expect: &str) -> tokio::time::Instant {
    loop {
        if let ClientEvent::Mark { id, received_at } = next_client(client).await {
            assert_eq!(id, expect);
            return received_at;
        }
    }
}

/// Reads engine events for at most `window`; returns the marks played (with the instant the
/// engine saw them) and cleared in it.
async fn marks_within(
    clock: &SharedClock,
    session: &mut TransportSession,
    window: Duration,
) -> (Vec<(String, tokio::time::Instant)>, Vec<String>) {
    let (mut played, mut cleared) = (Vec::new(), Vec::new());
    let end = clock.now() + window;
    while let Ok(Some(event)) = tokio::time::timeout_at(end, session.recv()).await {
        match event {
            TransportEvent::MarkPlayed { id } => played.push((id, clock.now())),
            TransportEvent::MarkCleared { id } => cleared.push(id),
            _ => {}
        }
    }
    (played, cleared)
}

#[tokio::test(start_paused = true)]
async fn auto_ack_echoes_each_mark_after_the_configured_delay() {
    let delay = Duration::from_millis(200);
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto { delay }).await;
    send_audio_and_mark(&clock, &session, 10, "m1").await;
    let received = mark_received_at(&mut client, "m1").await;
    // Not before the delay has passed: 150 ms after receipt nothing has been echoed.
    let (early, _) = marks_within(&clock, &mut session, Duration::from_millis(150)).await;
    assert!(early.is_empty(), "echoed early: {early:?}");
    // Exactly at it (paused time): the echo reaches the engine as played.
    let (late, cleared) = marks_within(&clock, &mut session, Duration::from_millis(100)).await;
    assert!(cleared.is_empty());
    assert_eq!(late.len(), 1, "{late:?}");
    assert_eq!(late[0].0, "m1");
    assert_eq!(late[0].1, received + delay);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn auto_ack_with_zero_delay_echoes_at_once_and_in_order() {
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto {
        delay: Duration::ZERO,
    })
    .await;
    send_audio_and_mark(&clock, &session, 5, "m1").await;
    for seq in 5..10 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m2".to_owned(),
        })
        .await
        .unwrap();
    // Read the engine side first: its events are stamped when we read them, so reading before the
    // client side keeps the paused clock where the adapter echoed.
    let (played, cleared) = marks_within(&clock, &mut session, Duration::from_millis(500)).await;
    assert!(cleared.is_empty());
    assert_eq!(
        played.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
        ["m1", "m2"]
    );
    let first = mark_received_at(&mut client, "m1").await;
    let second = mark_received_at(&mut client, "m2").await;
    // A zero-delay echo reaches the engine at the very instant the client received its mark.
    assert_eq!((played[0].1, played[1].1), (first, second));
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn auto_ack_is_dropped_by_a_clear_and_the_mark_ends_as_cleared() {
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto {
        delay: Duration::from_millis(500),
    })
    .await;
    send_audio_and_mark(&clock, &session, 10, "m1").await;
    mark_received_at(&mut client, "m1").await;
    // 100 ms into the 500 ms the client would have needed to play up to the mark, it is cleared.
    clock.sleep(Duration::from_millis(100)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let (played, cleared) = marks_within(&clock, &mut session, Duration::from_secs(2)).await;
    assert!(played.is_empty(), "a cleared client echoed: {played:?}");
    assert_eq!(cleared, ["m1".to_owned()]);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn never_ack_swallows_marks_even_when_the_test_echoes_them() {
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Never).await;
    send_audio_and_mark(&clock, &session, 10, "m1").await;
    mark_received_at(&mut client, "m1").await;
    client.ack_mark("m1").await.unwrap();
    let (played, _) = marks_within(&clock, &mut session, Duration::from_secs(2)).await;
    assert!(played.is_empty(), "a never-ack client echoed: {played:?}");
    // The mark is still outstanding: a clear ends it as cleared, never as played.
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let (played, cleared) = marks_within(&clock, &mut session, Duration::from_secs(1)).await;
    assert!(played.is_empty());
    assert_eq!(cleared, ["m1".to_owned()]);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn manual_mode_is_the_default_and_never_echoes_by_itself() {
    assert_eq!(LoopbackConfig::default().mark_acks, MarkAcks::Manual);
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Manual).await;
    send_audio_and_mark(&clock, &session, 10, "m1").await;
    mark_received_at(&mut client, "m1").await;
    let (played, _) = marks_within(&clock, &mut session, Duration::from_secs(1)).await;
    assert!(played.is_empty());
    client.ack_mark("m1").await.unwrap();
    let (played, _) = marks_within(&clock, &mut session, Duration::from_millis(100)).await;
    assert_eq!(played.len(), 1);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn auto_ack_and_a_manual_echo_of_the_same_mark_count_once() {
    let delay = Duration::from_millis(100);
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto { delay }).await;
    send_audio_and_mark(&clock, &session, 10, "m1").await;
    let received = mark_received_at(&mut client, "m1").await;
    client.ack_mark("m1").await.unwrap();
    // The hand-sent echo is delivered at once, not swallowed and not held to the auto delay.
    let (played, _) = marks_within(&clock, &mut session, Duration::from_millis(50)).await;
    assert_eq!(played.len(), 1, "the manual echo was lost: {played:?}");
    assert_eq!(played[0].0, "m1");
    assert_eq!(
        played[0].1, received,
        "the manual echo is not the one counted"
    );
    // The auto echo of the same mark (due at +100 ms) is a duplicate and counts no more.
    let (later, _) = marks_within(&clock, &mut session, Duration::from_secs(1)).await;
    assert!(later.is_empty(), "the duplicate echo counted: {later:?}");
    // Auto still works for a mark nobody echoed by hand.
    session
        .send(TransportCommand::Mark {
            id: "m2".to_owned(),
        })
        .await
        .unwrap();
    let received = mark_received_at(&mut client, "m2").await;
    let (played, _) = marks_within(&clock, &mut session, Duration::from_secs(1)).await;
    assert_eq!(played.len(), 1, "{played:?}");
    assert_eq!(played[0].0, "m2");
    assert_eq!(played[0].1, received + delay);
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn auto_ack_with_an_unrepresentable_delay_never_echoes_and_never_panics() {
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto {
        delay: Duration::MAX,
    })
    .await;
    send_audio_and_mark(&clock, &session, 5, "m1").await;
    mark_received_at(&mut client, "m1").await;
    let (played, _) = marks_within(&clock, &mut session, Duration::from_secs(2)).await;
    assert!(played.is_empty(), "echoed: {played:?}");
    // The adapter is alive: audio still reaches the client and the call closes normally.
    session
        .send(audio(&clock, 2, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    loop {
        if matches!(next_client(&mut client).await, ClientEvent::Audio { .. }) {
            break;
        }
    }
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_unrepresentable_latency_does_not_panic_the_adapter() {
    let loopback = LoopbackConfig {
        latency: Duration::MAX,
        ..LoopbackConfig::default()
    };
    let (clock, mut client, session, _cancel) =
        start_full(SessionConfig::default(), loopback).await;
    session
        .send(audio(&clock, 1, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    // Saturates to "heard at once"; the adapter task is alive and delivers.
    assert!(matches!(
        next_client(&mut client).await,
        ClientEvent::Audio { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn auto_ack_keeps_only_max_buffered_echoes_and_drops_the_oldest() {
    let loopback = LoopbackConfig {
        latency: LATENCY,
        max_buffered: 3,
        mark_acks: MarkAcks::Auto {
            delay: Duration::from_secs(1),
        },
        ..LoopbackConfig::default()
    };
    let (clock, mut client, mut session, _cancel) =
        start_full(SessionConfig::default(), loopback).await;
    for id in ["m1", "m2", "m3", "m4", "m5"] {
        session
            .send(TransportCommand::Mark { id: id.to_owned() })
            .await
            .unwrap();
    }
    for id in ["m1", "m2", "m3", "m4", "m5"] {
        mark_received_at(&mut client, id).await;
    }
    let (played, _) = marks_within(&clock, &mut session, Duration::from_secs(3)).await;
    assert_eq!(
        played.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
        ["m3", "m4", "m5"],
        "the two oldest echoes are dropped, the rest stay in order"
    );
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_clear_just_before_an_echo_is_due_drops_it_and_one_just_after_does_not() {
    let delay = Duration::from_millis(500);
    // 1 ms before the echo is due: the clear wins, the mark ends as cleared.
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto { delay }).await;
    send_audio_and_mark(&clock, &session, 5, "m1").await;
    mark_received_at(&mut client, "m1").await;
    clock.sleep(delay - Duration::from_millis(1)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let (played, cleared) = marks_within(&clock, &mut session, Duration::from_secs(2)).await;
    assert!(played.is_empty(), "echoed after the clear: {played:?}");
    assert_eq!(cleared, ["m1".to_owned()]);
    session.close(Duration::from_secs(1)).await.unwrap();

    // 1 ms after it was due: the mark had already been played, the clear finds nothing to clear.
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto { delay }).await;
    send_audio_and_mark(&clock, &session, 5, "m1").await;
    mark_received_at(&mut client, "m1").await;
    clock.sleep(delay + Duration::from_millis(1)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let (played, cleared) = marks_within(&clock, &mut session, Duration::from_secs(2)).await;
    assert_eq!(played.len(), 1, "{played:?}");
    assert!(cleared.is_empty(), "{cleared:?}");
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn zero_delay_echo_is_not_held_back_by_the_audio_behind_the_mark() {
    let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto {
        delay: Duration::ZERO,
    })
    .await;
    send_audio_and_mark(&clock, &session, 1, "m1").await;
    // Audio behind the mark that is due strictly later (20 ms after the mark was sent, so 20 ms
    // after the mark reaches the client): an adapter that served the delivery queue before the
    // echo would hold the echo back to that instant.
    clock.sleep(Duration::from_millis(20)).await;
    session
        .send(audio(&clock, 2, TurnEpoch::ZERO, 0))
        .await
        .unwrap();
    // Read the engine side first: the client's queue holds its events until we look.
    let (played, _) = marks_within(&clock, &mut session, Duration::from_millis(200)).await;
    assert_eq!(played.len(), 1, "{played:?}");
    let mark_at = mark_received_at(&mut client, "m1").await;
    let audio_at = loop {
        if let ClientEvent::Audio {
            received_at, frame, ..
        } = next_client(&mut client).await
            && frame.seq() == 0
            && frame.epoch() == TurnEpoch::ZERO
            && received_at > mark_at
        {
            break received_at;
        }
    };
    assert!(
        audio_at >= mark_at + Duration::from_millis(20),
        "the audio behind the mark is not due later ({mark_at:?} then {audio_at:?}): the test \
         would prove nothing"
    );
    assert_eq!(
        played[0].1, mark_at,
        "echoed at once, not held back to the later audio"
    );
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_mark_sent_in_an_epoch_a_clear_already_ended_is_cut_and_never_reaches_the_client() {
    // The engine checked its epoch (0), then the barge-in's `Clear { 1 }` was applied,
    // then the end mark of epoch 0 was sent. `send` stamps a mark with the transport's epoch at
    // the call (1), so it would go out after the clear; `send_in` keeps the epoch the mark
    // belongs to, and the mark is cut.
    for (stale_send_in, expect_stale_on_wire) in [(true, false), (false, true)] {
        let (clock, mut client, mut session, _cancel) = start_acking(MarkAcks::Auto {
            delay: Duration::ZERO,
        })
        .await;
        let commands = session.commands();
        for seq in 0..3 {
            commands
                .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
                .await
                .unwrap();
        }
        commands
            .send(TransportCommand::Clear {
                epoch: TurnEpoch::new(1),
            })
            .await
            .unwrap();
        let stale = TransportCommand::Mark {
            id: "stale".to_owned(),
        };
        if stale_send_in {
            commands.send_in(TurnEpoch::ZERO, stale).await.unwrap();
        } else {
            commands.send(stale).await.unwrap();
        }
        // A mark of the new epoch still goes out (and follows the stale one in the queue).
        commands
            .send_in(
                TurnEpoch::new(1),
                TransportCommand::Mark {
                    id: "fresh".to_owned(),
                },
            )
            .await
            .unwrap();
        let (played, cleared) = marks_within(&clock, &mut session, Duration::from_secs(1)).await;
        let mut on_wire = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::ZERO, client.recv()).await {
            if let ClientEvent::Mark { id, .. } = event {
                on_wire.push(id);
            }
        }
        let played: Vec<&str> = played.iter().map(|p| p.0.as_str()).collect();
        if expect_stale_on_wire {
            // The race this guards against, with the plain `send`.
            assert_eq!(on_wire, ["stale", "fresh"]);
            assert_eq!(played, ["stale", "fresh"]);
        } else {
            assert_eq!(on_wire, ["fresh"], "the stale mark reached the client");
            assert_eq!(played, ["fresh"]);
            assert_eq!(cleared, ["stale".to_owned()]);
        }
        session.close(Duration::from_secs(1)).await.unwrap();
    }
}
