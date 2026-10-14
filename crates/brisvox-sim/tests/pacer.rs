//! Output pacer timing (reference stage).
#![allow(clippy::unwrap_used)] // test helpers outside #[test] fns

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use brisvox_core::frame::{AudioFrame, SampleRate, SystemFrame};
use brisvox_core::{Clock, FrameSender, Links, Pipeline, SharedClock, SimClock, TurnEpoch};
use brisvox_sim::reference::{OutputPacer, PlayLog, Playout};
use brisvox_sim::scenario::SimTransportOut;
use bytes::Bytes;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

/// A clock that counts sleeps whose deadline has already come, and can run a hook on the next
/// `now()` call (to interleave an event at an exact point of a processor's work).
#[derive(Default)]
struct TestClock {
    inner: SimClock,
    due_sleeps: AtomicUsize,
    on_next_now: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for TestClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestClock").finish_non_exhaustive()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        let hook = self.on_next_now.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.now()
    }

    fn wall(&self) -> SystemTime {
        self.inner.wall()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        if deadline <= self.inner.now() {
            self.due_sleeps.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.sleep_until(deadline)
    }
}

struct Rig {
    clock: Arc<TestClock>,
    shared: SharedClock,
    played: PlayLog,
    transport: SimTransportOut,
    running: brisvox_core::RunningPipeline,
}

async fn rig() -> Rig {
    let clock = Arc::new(TestClock::default());
    let shared: SharedClock = clock.clone();
    let origin = shared.now();
    let transport = SimTransportOut::new(shared.clone(), origin, None);
    let played = PlayLog::default();
    let running = Pipeline::new("pacer")
        .then(OutputPacer::new(shared.clone(), Arc::clone(&played)))
        .then(transport.clone())
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    Rig {
        clock,
        shared,
        played,
        transport,
        running,
    }
}

fn frame(clock: &SharedClock, seq: u64, epoch: TurnEpoch) -> AudioFrame {
    AudioFrame::new(
        Bytes::from(vec![0u8; 640]),
        SampleRate::try_from(16_000).unwrap(),
        clock.now(),
        seq,
        epoch,
    )
    .unwrap()
}

fn audio_times(transport: &SimTransportOut) -> Vec<Duration> {
    transport
        .records()
        .iter()
        .filter(|r| r.kind == "audio")
        .map(|r| r.at)
        .collect()
}

/// Regression: the pacer slept until already-due deadlines. tokio rounds every sleep up
/// to the next timer tick (~15.6 ms of real time on Windows), so each turn's first frame was
/// late by a tick. Paused time cannot show the delay, so count the sleeps instead.
#[tokio::test(start_paused = true)]
async fn the_pacer_never_sleeps_on_a_due_deadline() {
    let mut rig = rig().await;
    let input = rig.running.input().clone();
    // Back-to-back frames: the first plays at once, the second one frame later (a real wait).
    input
        .send(frame(&rig.shared, 1, TurnEpoch::ZERO).into())
        .await
        .unwrap();
    input
        .send(frame(&rig.shared, 2, TurnEpoch::ZERO).into())
        .await
        .unwrap();
    // After a pause, playout has caught up: the next frame is due at once again.
    rig.shared.sleep(Duration::from_millis(500)).await;
    input
        .send(frame(&rig.shared, 3, TurnEpoch::ZERO).into())
        .await
        .unwrap();
    rig.shared.sleep(Duration::from_millis(100)).await;
    rig.running.stop(Duration::from_secs(1)).await.unwrap();
    assert_eq!(
        audio_times(&rig.transport),
        [
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(500)
        ]
    );
    assert_eq!(rig.clock.due_sleeps.load(Ordering::Relaxed), 0);
}

/// The no-sleep fast path must still honour an interrupt that reached the pacer's inbox while
/// it was handling a due frame: the frame is dropped and not logged as played (the LLM context
/// is cut to played audio).
#[tokio::test(start_paused = true)]
async fn a_due_frame_doomed_by_a_queued_interrupt_is_not_played() {
    let mut rig = rig().await;
    let input: FrameSender = rig.running.input().clone();
    let doomed = frame(&rig.shared, 1, TurnEpoch::ZERO);
    // The pacer's first `now()` happens inside `process` for this frame: enqueue the interrupt
    // exactly then (nothing else reads the clock between the send and that point).
    let injector = input.clone();
    *rig.clock.on_next_now.lock().unwrap() = Some(Box::new(move || {
        injector
            .try_send(
                SystemFrame::Interrupt {
                    new_epoch: TurnEpoch::new(1),
                }
                .into(),
            )
            .unwrap();
    }));
    input.send(doomed.into()).await.unwrap();
    // Let the pacer run without touching the clock here (that would fire the hook too early).
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(rig.clock.on_next_now.lock().unwrap().is_none(), "hook ran");
    rig.running.stop(Duration::from_secs(1)).await.unwrap();
    assert_eq!(audio_times(&rig.transport), [] as [Duration; 0]);
    assert_eq!(
        rig.played
            .lock()
            .unwrap()
            .get(&TurnEpoch::ZERO)
            .map(Playout::total),
        None,
        "stale audio logged as played"
    );
}
