//! Routing tests: priority lanes, overflow policies, close semantics, processor chains.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use proptest::prelude::*;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::epoch::TurnEpoch;
use crate::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, MetricFrame, MetricKind, SampleRate,
    SystemFrame,
};
use crate::processor::{Direction, Processor, ProcessorContext, run_processor};

const DOWN: Direction = Direction::Downstream;
const UP: Direction = Direction::Upstream;

#[allow(clippy::disallowed_methods)] // frames need some timestamp
fn ts() -> Instant {
    Instant::now()
}

/// Data frame identified by `id` (its sequence number).
fn audio(id: u64) -> Frame {
    AudioFrame::new(Bytes::new(), SampleRate::Hz16000, ts(), id, TurnEpoch::ZERO)
        .unwrap()
        .into()
}

/// Control frame identified by `id` (its epoch).
fn control(id: u32) -> Frame {
    ControlFrame {
        epoch: TurnEpoch::new(id),
        kind: ControlKind::Flush,
    }
    .into()
}

/// System frame identified by `id` (its new epoch).
fn system(id: u32) -> Frame {
    SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(id),
    }
    .into()
}

fn metric(id: u32) -> Frame {
    DataFrame::Metric(MetricFrame {
        epoch: TurnEpoch::new(id),
        stage: "test",
        kind: MetricKind::ProcessingTime,
        value: Duration::ZERO,
    })
    .into()
}

/// The id a test frame was built with.
fn id(frame: &Frame) -> u64 {
    match frame {
        Frame::System(SystemFrame::Interrupt { new_epoch }) => u64::from(new_epoch.get()),
        Frame::Data(DataFrame::Audio(a)) => a.seq(),
        other => u64::from(other.epoch().unwrap().get()),
    }
}

fn config(system: usize, data: usize, policies: OverflowPolicies) -> QueueConfig {
    QueueConfig {
        system_capacity: system,
        data_capacity: data,
        policies,
    }
}

#[test]
fn system_frame_overtakes_1000_queued_data_frames() {
    let mut inbox = Inbox::new(config(4, 1_000, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(DOWN);
    for i in 0..1_000 {
        tx.try_send(audio(i)).unwrap();
    }
    tx.try_send(system(7)).unwrap();

    let (first, dir) = inbox.try_recv().unwrap();
    assert_eq!(dir, DOWN);
    assert!(matches!(
        first,
        Frame::System(SystemFrame::Interrupt { new_epoch }) if new_epoch == TurnEpoch::new(7)
    ));
    for i in 0..1_000 {
        let (frame, _) = inbox.try_recv().unwrap();
        assert_eq!(id(&frame), i);
    }
    assert_eq!(inbox.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[test]
fn upstream_system_frame_overtakes_downstream_data() {
    let mut inbox = Inbox::new(QueueConfig::DEFAULT);
    let down = inbox.sender(DOWN);
    let up = inbox.sender(UP);
    down.try_send(audio(1)).unwrap();
    up.try_send(system(2)).unwrap();
    let (frame, dir) = inbox.try_recv().unwrap();
    assert_eq!((frame.name(), dir), ("interrupt", UP));
}

#[test]
fn data_lanes_alternate_between_directions() {
    let mut inbox = Inbox::new(QueueConfig::DEFAULT);
    let down = inbox.sender(DOWN);
    let up = inbox.sender(UP);
    for i in 0..3 {
        down.try_send(audio(i)).unwrap();
        up.try_send(control(u32::try_from(i).unwrap())).unwrap();
    }
    let dirs: Vec<Direction> = (0..6).map(|_| inbox.try_recv().unwrap().1).collect();
    assert_eq!(dirs, [DOWN, UP, DOWN, UP, DOWN, UP]);
}

#[test]
fn full_lane_rejects_backpressure_frames_but_not_system_frames() {
    let mut inbox = Inbox::new(config(2, 2, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(DOWN);
    tx.try_send(control(0)).unwrap();
    tx.try_send(audio(1)).unwrap();
    assert!(matches!(
        tx.try_send(control(2)),
        Err(TrySendError::Full(_))
    ));
    assert!(matches!(tx.try_send(audio(3)), Err(TrySendError::Full(_))));
    // The system lane is separate: an interrupt still gets through.
    tx.try_send(system(9)).unwrap();
    assert_eq!(inbox.try_recv().unwrap().0.name(), "interrupt");
    assert_eq!(inbox.stats(), QueueStats::default());
}

#[test]
fn drop_newest_discards_metrics_when_full() {
    let mut inbox = Inbox::new(config(1, 2, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(UP);
    tx.try_send(metric(1)).unwrap();
    tx.try_send(metric(2)).unwrap();
    tx.try_send(metric(3)).unwrap(); // dropped, not an error
    assert_eq!(inbox.stats().dropped_newest, 1);
    let ids: Vec<u64> = (0..2).map(|_| id(&inbox.try_recv().unwrap().0)).collect();
    assert_eq!(ids, [1, 2]);
}

#[test]
fn drop_oldest_evicts_oldest_audio_and_keeps_control() {
    let mut inbox = Inbox::new(config(1, 3, OverflowPolicies::REALTIME_INPUT));
    let tx = inbox.sender(DOWN);
    tx.try_send(control(100)).unwrap();
    tx.try_send(audio(1)).unwrap();
    tx.try_send(audio(2)).unwrap();
    tx.try_send(audio(3)).unwrap(); // evicts audio 1
    tx.try_send(audio(4)).unwrap(); // evicts audio 2
    assert_eq!(inbox.stats().dropped_oldest, 2);
    let got: Vec<(&str, u64)> = (0..3)
        .map(|_| {
            let f = inbox.try_recv().unwrap().0;
            (f.name(), id(&f))
        })
        .collect();
    assert_eq!(got, [("flush", 100), ("audio", 3), ("audio", 4)]);
}

#[test]
fn drop_oldest_with_no_audio_queued_drops_the_new_frame() {
    let mut inbox = Inbox::new(config(1, 1, OverflowPolicies::REALTIME_INPUT));
    let tx = inbox.sender(DOWN);
    tx.try_send(control(1)).unwrap();
    tx.try_send(audio(2)).unwrap();
    assert_eq!(inbox.stats().dropped_newest, 1);
    assert_eq!(inbox.try_recv().unwrap().0.name(), "flush");
    assert_eq!(inbox.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[test]
fn close_semantics() {
    let mut inbox = Inbox::new(QueueConfig::DEFAULT);
    let tx = inbox.sender(DOWN);
    let tx2 = tx.clone();
    tx.try_send(audio(1)).unwrap();
    drop(tx);
    drop(tx2);
    // Queued frames are still delivered after the senders leave...
    assert_eq!(id(&inbox.try_recv().unwrap().0), 1);
    // ...then the inbox reports disconnection.
    assert_eq!(inbox.try_recv().unwrap_err(), TryRecvError::Disconnected);

    let inbox = Inbox::new(QueueConfig::DEFAULT);
    let tx = inbox.sender(DOWN);
    drop(inbox);
    assert!(matches!(
        tx.try_send(audio(1)),
        Err(TrySendError::Closed(_))
    ));
}

#[test]
fn zero_capacity_is_treated_as_one() {
    let mut inbox = Inbox::new(config(0, 0, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(DOWN);
    tx.try_send(audio(1)).unwrap();
    tx.try_send(system(2)).unwrap();
    assert_eq!(inbox.len(), 2);
    assert!(inbox.try_recv().is_ok());
}

#[tokio::test(start_paused = true)]
async fn full_data_queue_applies_backpressure_without_deadlock() {
    const N: u64 = 10_000;
    let mut inbox = Inbox::new(config(2, 4, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(DOWN);
    let sys = inbox.sender(DOWN);

    let producer = tokio::spawn(async move {
        for i in 0..N {
            tx.send(audio(i)).await.unwrap();
        }
    });

    // Let the producer fill the lane and block.
    tokio::task::yield_now().await;
    assert_eq!(inbox.len(), 4);
    // A blocked data producer does not block system frames.
    sys.send(system(1)).await.unwrap();
    assert_eq!(inbox.recv().await.unwrap().0.name(), "interrupt");
    drop(sys);

    // Slow consumer: everything arrives, in order, and nothing deadlocks. Under paused time a
    // deadlock would auto-advance straight into this timeout.
    let consume = async {
        let mut expected = 0;
        while let Some((frame, _)) = inbox.recv().await {
            assert_eq!(id(&frame), expected);
            expected += 1;
            if expected % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }
        expected
    };
    let received = tokio::time::timeout(Duration::from_secs(5), consume)
        .await
        .expect("deadlock: consumer stalled");
    assert_eq!(received, N);
    producer.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn blocked_sender_is_released_when_inbox_drops() {
    let inbox = Inbox::new(config(1, 1, OverflowPolicies::DEFAULT));
    let tx = inbox.sender(DOWN);
    tx.send(audio(1)).await.unwrap();
    let blocked = tokio::spawn(async move { tx.send(audio(2)).await });
    tokio::task::yield_now().await;
    drop(inbox);
    let result = tokio::time::timeout(Duration::from_secs(1), blocked)
        .await
        .expect("sender stayed blocked")
        .unwrap();
    assert_eq!(result, Err(crate::Error::ChannelClosed("inbox")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_thread_stress_preserves_per_sender_order() {
    const SENDERS: u64 = 4;
    const PER_SENDER: u64 = 5_000;
    let mut inbox = Inbox::new(config(2, 3, OverflowPolicies::DEFAULT));
    let mut producers = Vec::new();
    for s in 0..SENDERS {
        let tx = inbox.sender(if s % 2 == 0 { DOWN } else { UP });
        producers.push(tokio::spawn(async move {
            for i in 0..PER_SENDER {
                // Encode the sender in the id; interleave some system frames.
                tx.send(audio(s * 1_000_000 + i)).await.unwrap();
                if i % 500 == 0 {
                    tx.send(system(u32::try_from(s).unwrap())).await.unwrap();
                }
            }
        }));
    }
    let consume = async {
        let mut last = [None::<u64>; 4];
        let (mut data, mut sys) = (0u64, 0u64);
        while let Some((frame, _)) = inbox.recv().await {
            if frame.level() == FrameLevel::System {
                sys += 1;
                continue;
            }
            let v = id(&frame);
            let s = usize::try_from(v / 1_000_000).unwrap();
            assert!(last[s].is_none_or(|prev| v > prev), "reordered: {v}");
            last[s] = Some(v);
            data += 1;
        }
        (data, sys)
    };
    let (data, sys) = tokio::time::timeout(Duration::from_secs(30), consume)
        .await
        .expect("stress test stalled");
    for p in producers {
        p.await.unwrap();
    }
    assert_eq!(data, SENDERS * PER_SENDER);
    assert_eq!(sys, SENDERS * (PER_SENDER / 500));
}

// ---------------------------------------------------------------------------------------------
// Property tests

#[derive(Debug, Clone, Copy)]
enum Op {
    Send { up: bool, kind: u8 },
    Recv,
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            3 => (any::<bool>(), 0u8..3).prop_map(|(up, kind)| Op::Send { up, kind }),
            2 => Just(Op::Recv),
        ],
        0..400,
    )
}

/// Lane index in the model: (direction, system?) → 0..4.
fn lane(dir: Direction, is_system: bool) -> usize {
    usize::from(dir == UP) * 2 + usize::from(is_system)
}

proptest! {
    /// With backpressure everywhere, the inbox behaves like four FIFO lanes: every received
    /// frame is the head of its (direction, level) lane, system lanes always win, and nothing
    /// is lost or invented.
    #[test]
    fn random_streams_preserve_per_kind_order(ops in ops()) {
        let mut inbox = Inbox::new(config(3, 5, OverflowPolicies::DEFAULT));
        let senders = [inbox.sender(DOWN), inbox.sender(UP)];
        let mut model: [VecDeque<u64>; 4] = Default::default();
        let mut next_id = 0u64;
        for op in ops {
            match op {
                Op::Send { up, kind } => {
                    let dir = if up { UP } else { DOWN };
                    next_id += 1;
                    let small = u32::try_from(next_id).unwrap();
                    let (frame, is_system) = match kind {
                        0 => (system(small), true),
                        1 => (control(small), false),
                        _ => (audio(next_id), false),
                    };
                    let sender = &senders[usize::from(up)];
                    match sender.try_send(frame) {
                        Ok(()) => model[lane(dir, is_system)].push_back(next_id),
                        Err(TrySendError::Full(_)) => {
                            let cap = if is_system { 3 } else { 5 };
                            prop_assert_eq!(model[lane(dir, is_system)].len(), cap);
                        }
                        Err(TrySendError::Closed(_)) => prop_assert!(false, "closed"),
                    }
                }
                Op::Recv => {
                    let system_pending =
                        !model[lane(DOWN, true)].is_empty() || !model[lane(UP, true)].is_empty();
                    match inbox.try_recv() {
                        Ok((frame, dir)) => {
                            let is_system = frame.level() == FrameLevel::System;
                            prop_assert_eq!(is_system, system_pending);
                            let expected = model[lane(dir, is_system)].pop_front();
                            prop_assert_eq!(Some(id(&frame)), expected);
                        }
                        Err(e) => {
                            prop_assert_eq!(e, TryRecvError::Empty);
                            prop_assert!(model.iter().all(VecDeque::is_empty));
                        }
                    }
                }
            }
        }
        let queued: usize = model.iter().map(VecDeque::len).sum();
        prop_assert_eq!(inbox.len(), queued);
    }

    /// With drop-oldest audio, audio may be lost but never reordered, and control frames are
    /// never lost.
    #[test]
    fn drop_oldest_never_reorders_or_loses_control(ops in ops()) {
        let mut inbox = Inbox::new(config(3, 4, OverflowPolicies::REALTIME_INPUT));
        let tx = inbox.sender(DOWN);
        let (mut sent_control, mut got_control) = (Vec::new(), Vec::new());
        let mut last_audio = None;
        let mut next_id = 0u64;
        let check = |frame: Frame,
                         got_control: &mut Vec<u64>,
                         last_audio: &mut Option<u64>|
         -> Result<(), TestCaseError> {
            match frame.name() {
                "audio" => {
                    prop_assert!(last_audio.is_none_or(|last| id(&frame) > last));
                    *last_audio = Some(id(&frame));
                }
                _ => got_control.push(id(&frame)),
            }
            Ok(())
        };
        for op in ops {
            match op {
                Op::Send { kind, .. } => {
                    next_id += 1;
                    let frame = if kind == 0 {
                        control(u32::try_from(next_id).unwrap())
                    } else {
                        audio(next_id)
                    };
                    match tx.try_send(frame) {
                        Ok(()) if kind == 0 => sent_control.push(next_id),
                        Ok(()) => {}
                        // Only control (backpressure) can be refused; the test drops it.
                        Err(TrySendError::Full(f)) => prop_assert_eq!(f.name(), "flush"),
                        Err(TrySendError::Closed(_)) => prop_assert!(false, "closed"),
                    }
                }
                Op::Recv => {
                    if let Ok((frame, _)) = inbox.try_recv() {
                        check(frame, &mut got_control, &mut last_audio)?;
                    }
                }
            }
        }
        while let Ok((frame, _)) = inbox.try_recv() {
            check(frame, &mut got_control, &mut last_audio)?;
        }
        prop_assert_eq!(got_control, sent_control);
    }
}

// ---------------------------------------------------------------------------------------------
// Processor chain

/// Forwards everything; on each audio frame also reports a metric upstream.
struct Tap;

impl Processor for Tap {
    fn name(&self) -> &'static str {
        "tap"
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> crate::Result<()> {
        if direction == DOWN && frame.name() == "audio" {
            ctx.push(metric(u32::try_from(id(&frame)).unwrap()), UP)
                .await?;
        }
        ctx.forward(frame, direction).await
    }
}

#[tokio::test(start_paused = true)]
async fn three_processor_chain_routes_both_directions() {
    let cancel = CancellationToken::new();
    // source → [a] → [b] → sink, with [a]'s upstream edge observed by `up_sink`.
    let a_inbox = Inbox::new(QueueConfig::DEFAULT);
    let b_inbox = Inbox::new(QueueConfig::DEFAULT);
    let mut sink = Inbox::new(QueueConfig::DEFAULT);
    let mut up_sink = Inbox::new(QueueConfig::DEFAULT);

    let source = a_inbox.sender(DOWN);
    let a_ctx = ProcessorContext::new(Some(b_inbox.sender(DOWN)), Some(up_sink.sender(UP)));
    let b_ctx = ProcessorContext::new(Some(sink.sender(DOWN)), Some(a_inbox.sender(UP)));
    let a = tokio::spawn(run_processor(Tap, a_inbox, a_ctx, cancel.clone()));
    let b = tokio::spawn(run_processor(Tap, b_inbox, b_ctx, cancel.clone()));

    for i in 0..5 {
        source.send(audio(i)).await.unwrap();
    }
    // A non-interrupt system frame (an Interrupt would make the epoch-tagged test frames stale).
    source
        .send(SystemFrame::UserStartedSpeaking { at: ts() }.into())
        .await
        .unwrap();

    let mut down = Vec::new();
    while down.len() < 6 {
        down.push(sink.recv().await.unwrap().0);
    }
    let names: Vec<&str> = down.iter().map(Frame::name).collect();
    assert!(names.contains(&"user_started_speaking"));
    let audio_ids: Vec<u64> = down
        .iter()
        .filter(|f| f.name() == "audio")
        .map(id)
        .collect();
    assert_eq!(audio_ids, [0, 1, 2, 3, 4]);

    // Each tap reports one metric per audio frame upstream; b's metrics pass through a.
    let mut metrics = 0;
    while metrics < 10 {
        let (frame, dir) = up_sink.recv().await.unwrap();
        assert_eq!((frame.name(), dir), ("metric", UP));
        metrics += 1;
    }

    cancel.cancel();
    assert_eq!(a.await.unwrap(), Ok(()));
    assert_eq!(b.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn cancel_releases_a_processor_blocked_on_backpressure() {
    let cancel = CancellationToken::new();
    let a_inbox = Inbox::new(QueueConfig::DEFAULT);
    // Downstream neighbour that nobody drains.
    let stuck = Inbox::new(config(1, 1, OverflowPolicies::DEFAULT));
    let source = a_inbox.sender(DOWN);
    let ctx = ProcessorContext::new(Some(stuck.sender(DOWN)), None);
    let a = tokio::spawn(run_processor(Tap, a_inbox, ctx, cancel.clone()));
    for i in 0..3 {
        source.send(audio(i)).await.unwrap();
    }
    tokio::task::yield_now().await;
    // The processor is parked inside `process`, pushing frame 1 into the full neighbour.
    assert_eq!(stuck.len(), 1);
    assert!(!a.is_finished());
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), a)
        .await
        .expect("processor ignored cancellation")
        .unwrap();
    assert_eq!(result, Ok(()));
    drop(stuck);
}
