//! End-to-end check of the foundation pieces on a real pipeline: a stage wakes itself through an
//! `EventSlot` from a `TaskGuard` helper, and nothing outlives the pipeline (`TaskCounter`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{Frame, SystemFrame};
use brisvox_core::{
    Clock, Direction, Links, Pipeline, Processor, ProcessorContext, SimClock, TurnEpoch,
};
use brisvox_engine::{Delivery, EventSlot, GuardedSender, SlotOverflow, TaskGuard, wake_frame};
use brisvox_sim::leak::TaskCounter;
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

/// A stage with two helper tasks: a ticker that wakes it every 5 ms with a typed event, and a
/// guarded sender used to push an "ended" frame downstream.
struct Stage {
    slot: EventSlot<u32>,
    clock: SimClock,
    seen: Arc<Mutex<Vec<u32>>>,
    /// What the guarded helper's send returned, with its stale-drop count (set from the helper
    /// task, asserted from the test body: a panic inside a task would not fail the test).
    guarded_outcome: Arc<Mutex<Option<(Delivery, u64)>>>,
    /// Tokens of the guarded wakes the stage received.
    guarded_wakes: Arc<Mutex<Vec<u32>>>,
    helpers: Vec<TaskGuard>,
}

impl Processor for Stage {
    fn name(&self) -> &'static str {
        "stage"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        match &frame {
            Frame::System(SystemFrame::Start) => {
                let me = ctx.self_sender().expect("attached to a task");
                let (slot, clock) = (self.slot.clone(), self.clock);
                self.helpers.push(TaskGuard::spawn("ticker", async move {
                    let mut n = 0;
                    loop {
                        clock.sleep(5 * MS).await;
                        n += 1;
                        if slot.push_and_wake(n, &me, 1).await.is_err() {
                            return;
                        }
                    }
                }));
                // A second helper that holds a GuardedSender into the stage's own inbox and
                // never finishes: it must die with the pipeline too.
                let watch = ctx.epoch_watch().expect("attached").clone();
                let guarded = GuardedSender::new(
                    ctx.self_sender().expect("attached"),
                    watch,
                    TurnEpoch::ZERO,
                );
                let outcome = Arc::clone(&self.guarded_outcome);
                self.helpers.push(TaskGuard::spawn("idle", async move {
                    let delivery = guarded.send(wake_frame(99)).await.unwrap();
                    *outcome.lock().unwrap() = Some((delivery, guarded.stale_dropped()));
                    std::future::pending::<()>().await;
                }));
            }
            Frame::System(SystemFrame::Wake { token: 1 }) => {
                self.seen.lock().unwrap().extend(self.slot.drain());
                return Ok(());
            }
            Frame::System(SystemFrame::Wake { token }) => {
                self.guarded_wakes.lock().unwrap().push(*token);
                return Ok(());
            }
            _ => {}
        }
        ctx.forward(frame, dir).await
    }
}

#[tokio::test(start_paused = true)]
async fn helpers_wake_the_stage_and_die_with_the_pipeline() {
    let clock = SimClock::new();
    let counter = TaskCounter::start_settled(&clock, 20 * MS).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let guarded_outcome = Arc::new(Mutex::new(None));
    let guarded_wakes = Arc::new(Mutex::new(Vec::new()));
    let running = Pipeline::new("p")
        .then(Stage {
            slot: EventSlot::new(8, SlotOverflow::DropOldest),
            clock,
            seen: Arc::clone(&seen),
            guarded_outcome: Arc::clone(&guarded_outcome),
            guarded_wakes: Arc::clone(&guarded_wakes),
            helpers: Vec::new(),
        })
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    clock.sleep(22 * MS).await;
    assert_eq!(*seen.lock().unwrap(), [1, 2, 3, 4]);
    assert_eq!(
        *guarded_outcome.lock().unwrap(),
        Some((Delivery::Sent, 0)),
        "the guarded send into the stage's own inbox was delivered, nothing stale"
    );
    assert_eq!(
        *guarded_wakes.lock().unwrap(),
        [99],
        "the stage received the guarded wake"
    );
    assert!(
        counter.alive() > counter.baseline(),
        "stage and helpers run"
    );

    drop(running);
    counter
        .wait_for_baseline(&clock, Duration::from_millis(50))
        .await
        .expect("a helper task outlived the pipeline");
    let after = seen.lock().unwrap().len();
    clock.sleep(50 * MS).await;
    assert_eq!(seen.lock().unwrap().len(), after, "ticks after the drop");
}

#[tokio::test(start_paused = true)]
async fn guarded_sender_stops_a_helper_of_an_ended_epoch() {
    use brisvox_core::frame::{ControlFrame, ControlKind};
    use brisvox_core::{Inbox, QueueConfig};

    let own = Inbox::new(QueueConfig::DEFAULT);
    let own_tx = own.sender(Direction::Downstream);
    let mut target = Inbox::new(QueueConfig::DEFAULT);
    let guarded = GuardedSender::new(
        target.sender(Direction::Downstream),
        own.epoch_watch(),
        TurnEpoch::new(1),
    );
    let ended: Frame = ControlFrame {
        epoch: TurnEpoch::new(1),
        kind: ControlKind::TtsEnded,
    }
    .into();
    assert_eq!(guarded.send(ended.clone()).await.unwrap(), Delivery::Sent);
    own_tx
        .send(
            SystemFrame::Interrupt {
                new_epoch: TurnEpoch::new(2),
            }
            .into(),
        )
        .await
        .unwrap();
    assert_eq!(guarded.send(ended.clone()).await.unwrap(), Delivery::Stale);
    assert_eq!(target.try_recv().unwrap().0, ended);
    assert!(target.try_recv().is_err());
}
