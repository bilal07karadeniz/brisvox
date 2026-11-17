//! Pause and resume of the playout through the loopback transport: silence at once, the response
//! going on from the paused point with nothing lost, positions that never count the same audio
//! twice, marks that are sent after the audio they follow, a clear that ends a pause, and the
//! full hold buffer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use brisvox_transport::{
    ClientEvent, DEFAULT_REPLAY, LoopbackClient, LoopbackConfig, LoopbackTransport, SessionConfig,
    Transport, TransportCommand, TransportEvent, TransportSession, UtteranceId,
};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// 20 ms of 16 kHz mono PCM16, every byte `fill`.
fn audio(clock: &SharedClock, utterance: u64, epoch: TurnEpoch, seq: u64) -> TransportCommand {
    let frame = AudioFrame::new(
        Bytes::from(vec![seq as u8; 640]),
        SampleRate::Hz16000,
        clock.now(),
        seq,
        epoch,
    )
    .unwrap();
    TransportCommand::Audio {
        epoch,
        utterance: UtteranceId::new(utterance),
        frame,
    }
}

/// A session that keeps audio for a resume, as a host with pause-then-decide on does.
fn cfg() -> SessionConfig {
    SessionConfig {
        replay: DEFAULT_REPLAY,
        ..SessionConfig::default()
    }
}

async fn start(
    config: SessionConfig,
) -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    start_flush(config, true).await
}

async fn start_flush(
    config: SessionConfig,
    client_flush: bool,
) -> (
    SharedClock,
    LoopbackClient,
    TransportSession,
    CancellationToken,
) {
    let clock: SharedClock = Arc::new(SimClock::new());
    let cancel = CancellationToken::new();
    let loopback = LoopbackConfig {
        latency: ms(30),
        ..LoopbackConfig::default()
    };
    let (mut transport, client) = LoopbackTransport::new(clock.clone(), loopback);
    let mut link = transport.accept(&cancel).await.unwrap().expect("one call");
    link.set_client_flush(client_flush);
    let mut session = TransportSession::open(clock.clone(), link, config, &cancel)
        .await
        .unwrap();
    let first = tokio::time::timeout(WAIT, session.recv()).await.unwrap();
    assert!(matches!(first, Some(TransportEvent::Started(_))));
    (clock, client, session, cancel)
}

/// Everything the client received up to `until` of simulated time after now.
async fn heard_for(
    clock: &SharedClock,
    client: &mut LoopbackClient,
    span: Duration,
) -> Vec<ClientEvent> {
    let end = clock.now() + span;
    let mut events = Vec::new();
    loop {
        let left = end.saturating_duration_since(clock.now());
        if left.is_zero() {
            break;
        }
        tokio::select! {
            event = client.recv() => match event {
                Some(event) => events.push(event),
                None => break,
            },
            () = clock.sleep(left) => break,
        }
    }
    events
}

fn seqs(events: &[ClientEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::Audio { frame, .. } => Some(u64::from(frame.data()[0])),
            _ => None,
        })
        .collect()
}

async fn drain(session: &mut TransportSession) -> Vec<TransportEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(ms(1), session.recv()).await {
        events.push(event);
    }
    events
}

#[tokio::test(start_paused = true)]
async fn a_pause_is_silent_at_once_and_a_resume_goes_on_from_the_paused_point() {
    let (clock, mut client, session, _cancel) = start(cfg()).await;
    let epoch = TurnEpoch::ZERO;
    for seq in 0..25 {
        session.send(audio(&clock, 1, epoch, seq)).await.unwrap();
    }
    let before = heard_for(&clock, &mut client, ms(130)).await;
    let paused_at = clock.now();
    session.send(TransportCommand::Pause).await.unwrap();
    // The client is told to clear, in the same epoch, at once.
    let during = heard_for(&clock, &mut client, ms(1000)).await;
    let clear = during
        .iter()
        .position(|event| matches!(event, ClientEvent::Clear { .. }))
        .expect("the client is told to clear");
    let ClientEvent::Clear { epoch: cleared, at } = &during[clear] else {
        panic!("not a clear");
    };
    assert_eq!(*cleared, epoch);
    assert!(*at - paused_at < ms(20), "cleared at {at:?}");
    // At most the one frame the output task was writing goes out ahead of the clear; after it,
    // silence for the whole pause.
    assert!(clear <= 1, "{during:?}");
    assert!(seqs(&during[clear..]).is_empty(), "{during:?}");

    session
        .send(TransportCommand::Resume { rewind: ms(40) })
        .await
        .unwrap();
    let after = heard_for(&clock, &mut client, ms(1000)).await;
    let before = seqs(&before);
    let after = seqs(&after);
    let last_before = *before.last().unwrap();
    // The first audio after the resume is at most the 40 ms the client may not have played
    // (two frames) before the paused point, never later: no word is skipped.
    assert!(
        after[0] <= last_before + 1 && after[0] + 3 >= last_before,
        "before {before:?} after {after:?}"
    );
    // From there on every frame arrives once, in order, to the end of the response.
    for pair in after.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "{after:?}");
    }
    assert_eq!(*after.last().unwrap(), 24);
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_resume_does_not_count_the_audio_it_sends_again() {
    let config = SessionConfig {
        position_interval: Duration::from_secs(10),
        ..cfg()
    };
    let (clock, mut client, mut session, _cancel) = start(config).await;
    let epoch = TurnEpoch::ZERO;
    for seq in 0..25 {
        session.send(audio(&clock, 1, epoch, seq)).await.unwrap();
    }
    heard_for(&clock, &mut client, ms(130)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    heard_for(&clock, &mut client, ms(500)).await;
    session
        .send(TransportCommand::Resume { rewind: ms(60) })
        .await
        .unwrap();
    heard_for(&clock, &mut client, ms(800)).await;
    session.send(TransportCommand::Stop).await.unwrap();
    let events = drain(&mut session).await;
    let last = events
        .iter()
        .filter_map(|event| match event {
            TransportEvent::PlaybackPosition { played, .. } => Some(*played),
            _ => None,
        })
        .max()
        .unwrap();
    // 25 frames of 20 ms: the audio sent again is not released audio.
    assert_eq!(last, ms(500));
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_clear_during_a_pause_drops_the_held_audio_and_ends_the_pause() {
    let (clock, mut client, session, _cancel) = start(cfg()).await;
    let old = TurnEpoch::ZERO;
    let new = TurnEpoch::new(1);
    for seq in 0..25 {
        session.send(audio(&clock, 1, old, seq)).await.unwrap();
    }
    heard_for(&clock, &mut client, ms(130)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    heard_for(&clock, &mut client, ms(300)).await;
    // The commit: the clear of the new epoch, then the new response.
    session
        .send(TransportCommand::Clear { epoch: new })
        .await
        .unwrap();
    for seq in 100..105 {
        session.send(audio(&clock, 2, new, seq)).await.unwrap();
    }
    let heard = seqs(&heard_for(&clock, &mut client, ms(500)).await);
    assert_eq!(heard, [100, 101, 102, 103, 104]);
    // A stray resume after the commit changes nothing.
    session
        .send(TransportCommand::Resume { rewind: ms(100) })
        .await
        .unwrap();
    assert!(seqs(&heard_for(&clock, &mut client, ms(200)).await).is_empty());
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_mark_is_sent_after_the_audio_it_follows_once_resumed() {
    let (clock, mut client, mut session, _cancel) = start(cfg()).await;
    let epoch = TurnEpoch::ZERO;
    for seq in 0..10 {
        session.send(audio(&clock, 1, epoch, seq)).await.unwrap();
    }
    session
        .send(TransportCommand::Mark { id: "end".into() })
        .await
        .unwrap();
    heard_for(&clock, &mut client, ms(70)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    let during = heard_for(&clock, &mut client, ms(600)).await;
    assert!(
        !during
            .iter()
            .any(|event| matches!(event, ClientEvent::Mark { .. })),
        "{during:?}"
    );
    session
        .send(TransportCommand::Resume { rewind: ms(40) })
        .await
        .unwrap();
    let after = heard_for(&clock, &mut client, ms(600)).await;
    let mark = after
        .iter()
        .position(|event| matches!(event, ClientEvent::Mark { id, .. } if id == "end"))
        .expect("the mark is sent after the resume");
    // Every frame the mark follows came before it.
    let last_audio = after
        .iter()
        .rposition(|event| matches!(event, ClientEvent::Audio { .. }))
        .unwrap();
    assert!(last_audio < mark, "{after:?}");
    assert_eq!(seqs(&after).last().copied(), Some(9));
    // The client echoes it: it ends as played, never as cleared.
    client.ack_mark("end").await.unwrap();
    let events = drain(&mut session).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TransportEvent::MarkPlayed { id } if id == "end")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TransportEvent::MarkCleared { .. })),
        "{events:?}"
    );
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_full_hold_is_reported_once_per_pause() {
    let config = SessionConfig {
        hold_max: ms(100),
        ..cfg()
    };
    let (clock, mut client, mut session, _cancel) = start(config).await;
    let epoch = TurnEpoch::ZERO;
    for seq in 0..5 {
        session.send(audio(&clock, 1, epoch, seq)).await.unwrap();
    }
    heard_for(&clock, &mut client, ms(50)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    // More audio than the hold may keep while paused.
    for seq in 5..20 {
        session.send(audio(&clock, 1, epoch, seq)).await.unwrap();
    }
    heard_for(&clock, &mut client, ms(200)).await;
    let events = drain(&mut session).await;
    let full = events
        .iter()
        .filter(|event| matches!(event, TransportEvent::HoldFull { .. }))
        .count();
    assert_eq!(full, 1, "{events:?}");
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn pause_and_resume_when_nothing_is_playing_change_nothing() {
    let (clock, mut client, session, _cancel) = start(cfg()).await;
    session.send(TransportCommand::Pause).await.unwrap();
    // A second pause is ignored.
    session.send(TransportCommand::Pause).await.unwrap();
    session
        .send(TransportCommand::Resume { rewind: ms(100) })
        .await
        .unwrap();
    let events = heard_for(&clock, &mut client, ms(100)).await;
    assert!(seqs(&events).is_empty());
    // Audio after the pair plays normally.
    for seq in 0..3 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    assert_eq!(
        seqs(&heard_for(&clock, &mut client, ms(300)).await),
        [0, 1, 2]
    );
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_session_that_keeps_no_audio_refuses_to_pause() {
    let (_clock, _client, session, _cancel) = start(SessionConfig::default()).await;
    // Nothing would be sent again on a resume, so the pause is an error (the engine then
    // interrupts as without the feature).
    assert!(session.send(TransportCommand::Pause).await.is_err());
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_rewind_beyond_what_is_kept_is_cut_and_counted() {
    let config = SessionConfig {
        replay: ms(100),
        ..cfg()
    };
    let (clock, mut client, session, _cancel) = start(config).await;
    for seq in 0..25 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    heard_for(&clock, &mut client, ms(300)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    heard_for(&clock, &mut client, ms(100)).await;
    assert_eq!(session.commands().hold_stats().rewind_clamped, 0);
    session
        .send(TransportCommand::Resume { rewind: ms(1000) })
        .await
        .unwrap();
    assert_eq!(session.commands().hold_stats().rewind_clamped, 1);
    // Only the 100 ms kept (five frames) are sent again.
    let after = seqs(&heard_for(&clock, &mut client, ms(1000)).await);
    let replayed = after
        .windows(2)
        .position(|pair| pair[1] != pair[0] + 1)
        .map_or(0, |at| at + 1);
    assert!(replayed <= 5, "{after:?}");
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn where_a_clear_does_not_flush_the_client_only_the_pulled_audio_is_sent_again() {
    let (clock, mut client, session, _cancel) = start_flush(cfg(), false).await;
    for seq in 0..25 {
        session
            .send(audio(&clock, 1, TurnEpoch::ZERO, seq))
            .await
            .unwrap();
    }
    heard_for(&clock, &mut client, ms(130)).await;
    session.send(TransportCommand::Pause).await.unwrap();
    heard_for(&clock, &mut client, ms(300)).await;
    // The engine asks for 200 ms (ten frames): the client was not flushed, so they are not
    // sent again; at most what the session itself held back.
    session
        .send(TransportCommand::Resume { rewind: ms(200) })
        .await
        .unwrap();
    let after = seqs(&heard_for(&clock, &mut client, ms(1000)).await);
    let replayed = after
        .windows(2)
        .position(|pair| pair[1] != pair[0] + 1)
        .map_or(0, |at| at + 1);
    assert!(replayed <= 2, "{after:?}");
    assert_eq!(*after.last().unwrap(), 24);
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(Duration::from_secs(1)).await.unwrap();
}
