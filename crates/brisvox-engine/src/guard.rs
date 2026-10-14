//! Helper-task plumbing: [`TaskGuard`] owns a spawned task, [`GuardedSender`] lets it talk to
//! the pipeline without outliving the epoch it works for.
//!
//! Both implement the helper-task rule of `docs/PIPELINE.md` section 1: a stage never leaves a
//! detached task behind (the task is aborted when its owner drops), and a helper never delivers
//! a frame of an epoch that an interrupt has already ended.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::frame::{DataFrame, Frame};
use brisvox_core::queue::TrySendError;
use brisvox_core::{Clock, EpochWatch, FrameSender, TurnEpoch};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};

/// How a [`TaskGuard::cancel_and_join`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOutcome {
    /// The task had already run to completion.
    Completed,
    /// The task was aborted and is gone.
    Aborted,
    /// The task had panicked (the panic was contained in the task; the session is unaffected).
    Panicked,
    /// The task did not end within the timeout (it is aborted and ends at its next await point).
    TimedOut,
}

/// An owned helper task: aborted when the guard is dropped.
///
/// Stages keep their helper tasks (provider readers, timers, per-epoch round tasks) in guards,
/// so dropping the processor, dropping the pipeline or cancelling the call leaves no task
/// behind. Must be created inside a tokio runtime.
#[derive(Debug)]
pub struct TaskGuard {
    name: &'static str,
    handle: JoinHandle<()>,
    /// The handle was awaited to its end (it must not be polled again).
    joined: bool,
}

impl TaskGuard {
    /// Spawns `task` on the current runtime. `name` is for logs only.
    pub fn spawn<F>(name: &'static str, task: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self {
            name,
            handle: tokio::spawn(task),
            joined: false,
        }
    }

    /// The name given at spawn time.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Whether the task has ended (completed, aborted or panicked).
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Asks the task to stop at its next await point. Dropping the guard does the same.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Aborts the task and waits for it to be gone, at most `timeout` on `clock`.
    ///
    /// Used where a stage must be sure a helper no longer touches shared state before it
    /// proceeds (for example the LLM runner before it changes the context after a barge-in).
    pub async fn cancel_and_join(mut self, clock: &dyn Clock, timeout: Duration) -> JoinOutcome {
        self.cancel_and_join_ref(clock, timeout).await
    }

    /// [`Self::cancel_and_join`] without consuming the guard, so a task that did not stop within
    /// the timeout ([`JoinOutcome::TimedOut`]) can be kept and joined again later. Calling it
    /// again after an outcome other than `TimedOut` is a no-op reporting `Completed`.
    pub async fn cancel_and_join_ref(
        &mut self,
        clock: &dyn Clock,
        timeout: Duration,
    ) -> JoinOutcome {
        if self.joined {
            return JoinOutcome::Completed;
        }
        self.handle.abort();
        let outcome = tokio::select! {
            biased;
            joined = &mut self.handle => match joined {
                Ok(()) => JoinOutcome::Completed,
                Err(error) if error.is_panic() => {
                    tracing::error!(target: "brisvox::engine", task = self.name, "helper task panicked");
                    JoinOutcome::Panicked
                }
                Err(_) => JoinOutcome::Aborted,
            },
            () = clock.sleep(timeout) => {
                tracing::warn!(target: "brisvox::engine", task = self.name, ?timeout, "helper task did not stop in time");
                return JoinOutcome::TimedOut;
            }
        };
        self.joined = true;
        outcome
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// What happened to a frame given to a [`GuardedSender`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The frame was queued in the neighbour's inbox.
    Sent,
    /// The sender's epoch is over (an interrupt to a newer epoch reached the watched inbox), so
    /// the frame was dropped and counted.
    Stale,
}

/// A helper task's sender into the pipeline, bound to the epoch the task works for.
///
/// Before every send it checks `watch.latest() <= epoch`; once an interrupt to a newer epoch has
/// been enqueued in the watched inbox (normally the owning stage's own inbox, from
/// `ProcessorContext::epoch_watch`) the frame is dropped and counted instead of delivered. A send
/// that is waiting for space in a full neighbour is abandoned the moment the epoch ends. This is
/// the same rule `ProcessorContext::push` applies to a stage's own pushes, for tasks that bypass
/// the context.
///
/// Exempt from the rule, as everywhere else: system frames (never epoch-tagged), tool results
/// (a SideEffect call may already have run and the context must learn of it) and word timestamps
/// (the played-text tracker needs the words of audio that already played).
#[derive(Debug, Clone)]
pub struct GuardedSender {
    sender: FrameSender,
    watch: EpochWatch,
    epoch: TurnEpoch,
    stale_dropped: Arc<AtomicU64>,
}

fn exempt(frame: &Frame) -> bool {
    matches!(
        frame,
        Frame::System(_) | Frame::Data(DataFrame::ToolResult(_) | DataFrame::Word(_))
    )
}

impl GuardedSender {
    /// Binds `sender` to `epoch`; `watch` observes interrupts.
    pub fn new(sender: FrameSender, watch: EpochWatch, epoch: TurnEpoch) -> Self {
        Self {
            sender,
            watch,
            epoch,
            stale_dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The epoch this sender works for.
    pub const fn epoch(&self) -> TurnEpoch {
        self.epoch
    }

    /// Whether the epoch is still the newest one the watched inbox has seen.
    pub fn is_current(&self) -> bool {
        self.watch.latest() <= self.epoch
    }

    /// Frames dropped because the epoch ended (shared by clones of this sender).
    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped.load(Ordering::Relaxed)
    }

    fn stale(&self) -> Delivery {
        self.stale_dropped.fetch_add(1, Ordering::Relaxed);
        Delivery::Stale
    }

    /// Sends `frame`, waiting for space if the neighbour's lane is full and the frame's overflow
    /// policy is backpressure. Cancel-safe.
    ///
    /// # Errors
    /// [`Error::Core`] with `ChannelClosed` if the neighbour's inbox is gone.
    pub async fn send(&self, frame: Frame) -> Result<Delivery> {
        let guarded = !exempt(&frame);
        if guarded && !self.is_current() {
            return Ok(self.stale());
        }
        let frame = match self.sender.try_send(frame) {
            Ok(()) => return Ok(Delivery::Sent),
            Err(TrySendError::Closed(_)) => {
                return Err(Error::Core(brisvox_core::Error::ChannelClosed("inbox")));
            }
            Err(TrySendError::Full(frame)) => frame,
        };
        if !guarded {
            self.sender.send(frame).await?;
            return Ok(Delivery::Sent);
        }
        tokio::select! {
            biased;
            () = self.watch.wait_beyond(self.epoch) => Ok(self.stale()),
            sent = self.sender.send(frame) => {
                sent?;
                Ok(Delivery::Sent)
            }
        }
    }

    /// Sends `frame` without waiting.
    ///
    /// # Errors
    /// [`TrySendError`] returning the frame when the lane is full (backpressure frames) or the
    /// inbox is gone. A frame of an ended epoch is not an error: it is dropped and
    /// [`Delivery::Stale`] is returned.
    pub fn try_send(&self, frame: Frame) -> std::result::Result<Delivery, TrySendError> {
        if !exempt(&frame) && !self.is_current() {
            return Ok(self.stale());
        }
        self.sender.try_send(frame).map(|()| Delivery::Sent)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use brisvox_core::frame::{ControlFrame, ControlKind, SystemFrame};
    use brisvox_core::queue::{OverflowPolicies, QueueConfig};
    use brisvox_core::{Direction, Inbox, SimClock};

    use super::*;

    fn tts_ended(epoch: u32) -> Frame {
        ControlFrame {
            epoch: TurnEpoch::new(epoch),
            kind: ControlKind::TtsEnded,
        }
        .into()
    }

    fn interrupt(epoch: u32) -> Frame {
        SystemFrame::Interrupt {
            new_epoch: TurnEpoch::new(epoch),
        }
        .into()
    }

    fn one_slot() -> QueueConfig {
        QueueConfig {
            system_capacity: 4,
            data_capacity: 1,
            policies: OverflowPolicies::DEFAULT,
        }
    }

    /// `own` is the watched inbox (the stage's own), `target` the neighbour the sender writes to.
    fn guarded(own: &Inbox, target: &Inbox, epoch: u32) -> GuardedSender {
        GuardedSender::new(
            target.sender(Direction::Downstream),
            own.epoch_watch(),
            TurnEpoch::new(epoch),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn task_guard_aborts_on_drop() {
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let guard = TaskGuard::spawn("sleeper", {
            let (started, dropped) = (Arc::clone(&started), Arc::clone(&dropped));
            async move {
                let _on_drop = SetOnDrop(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
        });
        assert_eq!(guard.name(), "sleeper");
        tokio::task::yield_now().await;
        assert!(started.load(Ordering::SeqCst));
        assert!(!guard.is_finished());
        drop(guard);
        tokio::task::yield_now().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "task future was not dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_and_join_reports_how_the_task_ended() {
        let clock = SimClock::new();
        let forever = TaskGuard::spawn("forever", std::future::pending());
        assert_eq!(
            forever
                .cancel_and_join(&clock, Duration::from_secs(1))
                .await,
            JoinOutcome::Aborted
        );

        let quick = TaskGuard::spawn("quick", async {});
        tokio::task::yield_now().await;
        assert_eq!(
            quick.cancel_and_join(&clock, Duration::from_secs(1)).await,
            JoinOutcome::Completed
        );

        let boom = TaskGuard::spawn("boom", async { panic!("helper bug") });
        tokio::task::yield_now().await;
        assert_eq!(
            boom.cancel_and_join(&clock, Duration::from_secs(1)).await,
            JoinOutcome::Panicked
        );
    }

    #[tokio::test(start_paused = true)]
    async fn guarded_sender_delivers_while_the_epoch_is_current() {
        let own = Inbox::new(QueueConfig::DEFAULT);
        let mut target = Inbox::new(QueueConfig::DEFAULT);
        let sender = guarded(&own, &target, 1);
        assert!(sender.is_current());
        assert_eq!(sender.send(tts_ended(1)).await.unwrap(), Delivery::Sent);
        assert_eq!(sender.try_send(tts_ended(1)).unwrap(), Delivery::Sent);
        assert_eq!(target.try_recv().unwrap().0, tts_ended(1));
        assert_eq!(target.len(), 1);
        assert_eq!(sender.stale_dropped(), 0);
        assert_eq!(sender.epoch(), TurnEpoch::new(1));
    }

    #[tokio::test(start_paused = true)]
    async fn guarded_sender_drops_frames_once_an_interrupt_reached_the_watched_inbox() {
        let own = Inbox::new(QueueConfig::DEFAULT);
        let own_tx = own.sender(Direction::Downstream);
        let target = Inbox::new(QueueConfig::DEFAULT);
        let sender = guarded(&own, &target, 1);
        let clone = sender.clone();

        // An interrupt to the SAME epoch does not end it; to a newer one it does.
        own_tx.send(interrupt(1)).await.unwrap();
        assert_eq!(sender.send(tts_ended(1)).await.unwrap(), Delivery::Sent);
        own_tx.send(interrupt(2)).await.unwrap();
        assert!(!sender.is_current());
        assert_eq!(sender.send(tts_ended(1)).await.unwrap(), Delivery::Stale);
        assert_eq!(clone.try_send(tts_ended(1)).unwrap(), Delivery::Stale);
        assert_eq!(
            target.len(),
            1,
            "nothing may be delivered after the interrupt"
        );
        assert_eq!(sender.stale_dropped(), 2, "clones share the counter");
    }

    #[tokio::test(start_paused = true)]
    async fn guarded_sender_exempts_system_frames_words_and_tool_results() {
        use brisvox_core::frame::{ToolResult, WordTimestamp};

        let own = Inbox::new(QueueConfig::DEFAULT);
        let own_tx = own.sender(Direction::Downstream);
        let mut target = Inbox::new(QueueConfig::DEFAULT);
        let sender = guarded(&own, &target, 1);
        own_tx.send(interrupt(5)).await.unwrap();

        let word: Frame = DataFrame::Word(Box::new(WordTimestamp {
            epoch: TurnEpoch::new(1),
            utterance: 3,
            word: Arc::from("hi"),
            start: Duration::ZERO,
        }))
        .into();
        let result: Frame = DataFrame::ToolResult(Box::new(ToolResult {
            epoch: TurnEpoch::new(1),
            call_id: Arc::from("c1"),
            content: Arc::from("{}"),
            is_error: false,
        }))
        .into();
        for frame in [word, result, SystemFrame::Start.into()] {
            assert_eq!(sender.send(frame).await.unwrap(), Delivery::Sent);
        }
        assert_eq!(target.len(), 3);
        while target.try_recv().is_ok() {}
        assert_eq!(sender.stale_dropped(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_waiting_for_space_is_abandoned_when_the_epoch_ends() {
        let own = Inbox::new(QueueConfig::DEFAULT);
        let own_tx = own.sender(Direction::Downstream);
        let mut target = Inbox::new(one_slot());
        let sender = guarded(&own, &target, 1);
        assert_eq!(sender.send(tts_ended(1)).await.unwrap(), Delivery::Sent);

        let waiting = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send(tts_ended(1)).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "lane is full: the send must wait");
        // try_send on the full lane hands the frame back.
        assert!(matches!(
            sender.try_send(tts_ended(1)),
            Err(TrySendError::Full(_))
        ));

        own_tx.send(interrupt(2)).await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("waiting send was not abandoned")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, Delivery::Stale);
        assert_eq!(target.len(), 1, "the abandoned frame was not delivered");
        let _ = target.try_recv();
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_into_a_gone_inbox_is_an_error() {
        let own = Inbox::new(QueueConfig::DEFAULT);
        let target = Inbox::new(QueueConfig::DEFAULT);
        let sender = guarded(&own, &target, 0);
        drop(target);
        assert!(matches!(
            sender.send(tts_ended(0)).await,
            Err(Error::Core(brisvox_core::Error::ChannelClosed(_)))
        ));
        assert!(matches!(
            sender.try_send(tts_ended(0)),
            Err(TrySendError::Closed(_))
        ));
    }
}
