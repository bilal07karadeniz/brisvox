//! [`TransportOut`]: the tail of the pipeline. Turns frames into transport commands
//! (`docs/PIPELINE.md` sections 1, 2 and 7).
//!
//! ```text
//! TtsStage --UtteranceStarted, Audio, Word, TtsEnded--> TransportOut --TransportCommand--> TransportSession
//!                                                          |  (its OutputPacer paces the audio)
//!             TurnDriver <--TtsStarted, TtsEnded-----------+<--- TtsEnded (the pump, on the echoed end mark)
//! ```
//!
//! **This stage is not a pacer.** `TransportSession` already releases audio at real time through
//! its own `OutputPacer`; a second pacer here would only add a queue and delay. The stage
//! hands every frame to the session's command queue as it arrives and lets the queue's bounded
//! capacity push back on the TTS side. A test shows that the frames reach the client 20 ms apart
//! because of the session's pacer alone.
//!
//! # What it does
//! - **Audio.** The utterance of a frame comes from the `UtteranceStarted` control frame that
//!   precedes its audio (the TTS stage emits it in order on the data lane). The command is
//!   `Audio { epoch, utterance, frame }`. The first audio of every epoch newer than the
//!   transport's (`tx_epoch`) is preceded by `Clear { epoch }`: the transport accepts audio only
//!   of the epoch it was cleared to, and the turn driver's own `Clear` happens only on a barge-in,
//!   so without this rule the second turn's audio would be dropped silently. The clear is
//!   idempotent for an epoch the transport already reached (a barge-in), and on an idle
//!   transport it cuts nothing. The real handoff (the command was accepted) is when the stage
//!   marks `FirstAudioOut` on the timeline and sends `TtsStarted` upstream, once per epoch.
//! - **Words** go into the [`PlayoutBook`] (`push_word`), audio durations too (`add_audio`),
//!   which is what the played-text truncation reads.
//! - **`TtsEnded`** becomes a transport `Mark` with a unique id, registered in the book first.
//!   The client's echo reaches the pump (`MarkPlayed`), which sends `TtsEnded` back upstream
//!   through this stage; the stage lets it through once per epoch, and the turn driver turns it
//!   into `BotAudioEnded`. Two fallbacks keep the machine from waiting for ever: a response that
//!   sent **no audio** (empty, failed) is ended at once, upstream, without a mark; and a client
//!   that never echoes marks is given [`TransportOutConfig::mark_timeout`] after the audio is
//!   expected to have played out (first handoff plus the audio handed over), then the stage ends
//!   the response itself. A late echo after that is a duplicate and dropped. The mark is sent
//!   with the epoch it belongs to (`CommandSender::send_in`): if the turn driver's fast
//!   `Clear` lands between the stage's epoch check and the send, the transport cuts the mark
//!   (`MarkCleared`) instead of putting it on the wire after the clear.
//! - **`Interrupt`** marks `AudioOutStopped` on the interrupted epoch, sends `Clear` (idempotent,
//!   the turn driver already cleared the transport on the fast path) and forgets the pending
//!   marks of the ended epochs.
//! - **Stale frames.** The stage is [`EpochPolicy::KeepAll`] and does its own epoch filtering so
//!   that it can count what it drops: audio, utterance announcements, ends and echoes of an
//!   epoch older than the newest the stage has seen (`Interrupt`, or its own `Clear`) are dropped
//!   and counted ([`TransportOutStatsSnapshot::stale_dropped`]); the inbox of the stage holds at
//!   most a few such frames, so handling them one by one is cheap. A wait inside the stage for
//!   room in the transport's queue is abandoned the moment the interrupt is queued. After a
//!   barge-in the transport also rejects audio of older epochs by itself (the driver's `Clear`
//!   raised its epoch), so stale audio never reaches the wire even in the window before the
//!   `Interrupt` frame arrives here.
//!
//! # Timeouts and cancellation
//! A send into the session's command queue is polled once without a timer; if it must wait (the
//! queue is full, which is normal when the TTS runs ahead of real time), it waits at most
//! [`TransportOutConfig::send_timeout`] per frame and is abandoned the moment an interrupt for a
//! newer epoch is queued for the stage. A frame that could not be handed over marks its
//! utterance lost in the book (the audio has a hole). The mark timeout runs on one
//! [`DeadlineTimer`] woken through `SystemFrame::Wake`.
//!
//! # Upstream
//! Upstream frames are forwarded with a non-blocking `try_send`, never awaited from `process`.

use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, Result as CoreResult, SharedClock,
    TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_transport::{CommandSender, TransportCommand, UtteranceId};
use tokio::time::Instant;

use crate::config::EngineConfig;
use crate::event_slot::{EventSlot, SlotOverflow};
use crate::playout::PlayoutBook;
use crate::timer::DeadlineTimer;
use crate::tts::stale;

/// The `Wake` token of the mark timeout timer.
const WAKE_TOKEN: u32 = 5;

/// Settings of the [`TransportOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportOutConfig {
    /// The longest one command (an audio frame, a mark, a clear) waits for room in the session's
    /// queue. Default: [`Timeouts::send`](crate::Timeouts::send), 2 s.
    pub send_timeout: Duration,
    /// How long after the audio of a response should have played out the stage waits for the
    /// client's echo of the end mark before it ends the response itself. Default:
    /// [`Timeouts::mark`](crate::Timeouts::mark), 10 s.
    pub mark_timeout: Duration,
}

impl TransportOutConfig {
    /// Settings derived from the engine configuration.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            send_timeout: config.timeouts.send,
            mark_timeout: config.timeouts.mark,
        }
    }
}

impl Default for TransportOutConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

#[derive(Debug, Default)]
struct Counters {
    audio_frames: AtomicU64,
    audio_ms: AtomicU64,
    stale_dropped: AtomicU64,
    stale_control: AtomicU64,
    unattributed: AtomicU64,
    clears: AtomicU64,
    clear_failures: AtomicU64,
    send_failures: AtomicU64,
    send_waits: AtomicU64,
    words: AtomicU64,
    words_unknown: AtomicU64,
    started_sent: AtomicU64,
    marks_sent: AtomicU64,
    mark_timeouts: AtomicU64,
    direct_ends: AtomicU64,
    ends_upstream: AtomicU64,
    duplicate_ends: AtomicU64,
    upstream_dropped: AtomicU64,
    interrupts: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A point-in-time copy of the [`TransportOut`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportOutStatsSnapshot {
    /// Audio frames handed to the transport.
    pub audio_frames: u64,
    /// Milliseconds of audio handed to the transport (truncated per frame).
    pub audio_ms: u64,
    /// Audio frames dropped because their epoch had ended.
    pub stale_dropped: u64,
    /// Other frames (utterance announcements, ends, echoes) dropped for the same reason.
    pub stale_control: u64,
    /// Audio frames with no `UtteranceStarted` for their epoch, dropped.
    pub unattributed: u64,
    /// `Clear` commands the transport accepted.
    pub clears: u64,
    /// `Clear` commands that failed or timed out.
    pub clear_failures: u64,
    /// Audio frames or marks the transport did not accept in time (or at all).
    pub send_failures: u64,
    /// Sends that had to wait for room in the command queue.
    pub send_waits: u64,
    /// Word timestamps written to the book.
    pub words: u64,
    /// Word timestamps or audio for an utterance the book does not know.
    pub words_unknown: u64,
    /// `TtsStarted` frames sent upstream.
    pub started_sent: u64,
    /// End marks sent.
    pub marks_sent: u64,
    /// Responses ended because the client never echoed the mark.
    pub mark_timeouts: u64,
    /// Responses ended at once because they sent no audio.
    pub direct_ends: u64,
    /// `TtsEnded` frames sent upstream (all causes).
    pub ends_upstream: u64,
    /// `TtsEnded` frames dropped because the epoch had already ended.
    pub duplicate_ends: u64,
    /// Upstream frames dropped because the upstream lane was full.
    pub upstream_dropped: u64,
    /// `Interrupt` frames handled.
    pub interrupts: u64,
}

/// Shared handle on the counters of one [`TransportOut`].
#[derive(Debug, Clone)]
pub struct TransportOutStats {
    counters: Arc<Counters>,
}

impl TransportOutStats {
    /// The counters now.
    pub fn snapshot(&self) -> TransportOutStatsSnapshot {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TransportOutStatsSnapshot {
            audio_frames: get(&c.audio_frames),
            audio_ms: get(&c.audio_ms),
            stale_dropped: get(&c.stale_dropped),
            stale_control: get(&c.stale_control),
            unattributed: get(&c.unattributed),
            clears: get(&c.clears),
            clear_failures: get(&c.clear_failures),
            send_failures: get(&c.send_failures),
            send_waits: get(&c.send_waits),
            words: get(&c.words),
            words_unknown: get(&c.words_unknown),
            started_sent: get(&c.started_sent),
            marks_sent: get(&c.marks_sent),
            mark_timeouts: get(&c.mark_timeouts),
            direct_ends: get(&c.direct_ends),
            ends_upstream: get(&c.ends_upstream),
            duplicate_ends: get(&c.duplicate_ends),
            upstream_dropped: get(&c.upstream_dropped),
            interrupts: get(&c.interrupts),
        }
    }
}

/// The audio handed to the transport for one epoch.
#[derive(Debug, Clone, Copy)]
struct EpochAudio {
    epoch: TurnEpoch,
    /// When the first frame was handed over.
    first_at: Instant,
    /// Audio handed over so far.
    total: Duration,
    /// The utterance of the newest frame (the end mark belongs to it).
    utterance: u64,
}

/// An end mark waiting for the client's echo.
#[derive(Debug, Clone, Copy)]
struct PendingMark {
    epoch: TurnEpoch,
    deadline: Instant,
}

/// How a command left the stage.
enum Outcome {
    Sent,
    /// The epoch ended while the stage waited.
    Stale,
    /// The transport did not take it (closed, or the timeout passed).
    Failed,
}

/// The tail stage. See the [module docs](self).
pub struct TransportOut {
    commands: CommandSender,
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    timeline: TimelineRecorder,
    config: TransportOutConfig,
    counters: Arc<Counters>,
    /// The newest epoch the transport was cleared to by this stage (the driver's own clears
    /// are idempotent with these).
    tx_epoch: TurnEpoch,
    /// The newest epoch an `Interrupt` moved the pipeline to.
    floor: TurnEpoch,
    /// The utterance the audio now arriving belongs to.
    utterance: Option<(TurnEpoch, u64)>,
    /// Audio handed over for the newest epoch that had any.
    sent: Option<EpochAudio>,
    marks: Vec<PendingMark>,
    /// The newest epoch whose end was reported upstream.
    ended_through: Option<TurnEpoch>,
    mark_seq: u64,
    slot: EventSlot<()>,
    timer: Option<DeadlineTimer>,
}

impl std::fmt::Debug for TransportOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportOut")
            .field("tx_epoch", &self.tx_epoch)
            .field("pending_marks", &self.marks.len())
            .finish_non_exhaustive()
    }
}

impl TransportOut {
    /// A stage writing to the session behind `commands`, recording words and audio in `book`
    /// (shared with the LLM runner, the TTS stage and the pump).
    pub fn new(
        commands: CommandSender,
        book: Arc<PlayoutBook>,
        config: TransportOutConfig,
        clock: SharedClock,
        timeline: TimelineRecorder,
    ) -> Self {
        Self {
            commands,
            book,
            clock,
            timeline,
            config,
            counters: Arc::new(Counters::default()),
            tx_epoch: TurnEpoch::ZERO,
            floor: TurnEpoch::ZERO,
            utterance: None,
            sent: None,
            marks: Vec::new(),
            ended_through: None,
            mark_seq: 0,
            slot: EventSlot::new(4, SlotOverflow::DropOldest),
            timer: None,
        }
    }

    /// A handle on this stage's counters.
    pub fn stats(&self) -> TransportOutStats {
        TransportOutStats {
            counters: Arc::clone(&self.counters),
        }
    }

    // -- sending ------------------------------------------------------------------------------

    /// Hands `command` to the session. Polled once first (no timer when there is room); else
    /// waits at most the send timeout, and not past the end of `epoch` when one is given.
    async fn send_command(
        &self,
        command: TransportCommand,
        epoch: Option<TurnEpoch>,
        ctx: &ProcessorContext,
    ) -> Outcome {
        // Commands of an epoch are judged by that epoch, not by the transport's at the call
        // (a mark must not outlive a clear that landed after the stage's own check).
        let mut send = pin!(async {
            match epoch {
                Some(epoch) => self.commands.send_in(epoch, command).await,
                None => self.commands.send(command).await,
            }
        });
        let first = std::future::poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx))).await;
        if let Poll::Ready(result) = first {
            return if result.is_ok() {
                Outcome::Sent
            } else {
                Outcome::Failed
            };
        }
        bump(&self.counters.send_waits);
        let timeout = self.clock.sleep(self.config.send_timeout);
        let abandoned = async {
            match epoch {
                Some(epoch) => stale(ctx.epoch_watch(), epoch).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            () = abandoned => Outcome::Stale,
            () = timeout => {
                tracing::warn!(target: "brisvox::engine", "transport command timed out");
                Outcome::Failed
            }
            result = &mut send => if result.is_ok() { Outcome::Sent } else { Outcome::Failed },
        }
    }

    /// `Clear { epoch }`; on success the stage's transport epoch moves there.
    async fn clear(&mut self, epoch: TurnEpoch, ctx: &ProcessorContext) -> bool {
        match self
            .send_command(TransportCommand::Clear { epoch }, None, ctx)
            .await
        {
            Outcome::Sent => {
                bump(&self.counters.clears);
                self.tx_epoch = self.tx_epoch.max(epoch);
                true
            }
            Outcome::Stale | Outcome::Failed => {
                bump(&self.counters.clear_failures);
                false
            }
        }
    }

    // -- frames -------------------------------------------------------------------------------

    /// The newest epoch the stage knows of: frames of older ones are stale.
    fn current(&self) -> TurnEpoch {
        self.tx_epoch.max(self.floor)
    }

    /// Whether a control frame of `epoch` is stale (and counts it if so).
    fn stale_control(&self, epoch: TurnEpoch) -> bool {
        let stale = epoch < self.current();
        if stale {
            bump(&self.counters.stale_control);
        }
        stale
    }

    async fn audio(&mut self, audio: AudioFrame, ctx: &mut ProcessorContext) {
        let epoch = audio.epoch();
        if epoch < self.current() {
            bump(&self.counters.stale_dropped);
            return;
        }
        let Some(utterance) = self
            .utterance
            .filter(|(utterance_epoch, _)| *utterance_epoch == epoch)
            .map(|(_, id)| id)
        else {
            bump(&self.counters.unattributed);
            return;
        };
        if epoch > self.tx_epoch && !self.clear(epoch, ctx).await {
            // The transport did not move to this epoch: it would reject the frame. The next
            // frame tries again.
            return;
        }
        let duration = audio.duration();
        let command = TransportCommand::Audio {
            epoch,
            utterance: UtteranceId::new(utterance),
            frame: audio,
        };
        match self.send_command(command, Some(epoch), ctx).await {
            Outcome::Sent => {}
            Outcome::Stale => {
                bump(&self.counters.stale_dropped);
                return;
            }
            Outcome::Failed => {
                bump(&self.counters.send_failures);
                self.book.mark_lost(utterance);
                return;
            }
        }
        bump(&self.counters.audio_frames);
        self.counters.audio_ms.fetch_add(
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if !self.book.add_audio(utterance, duration) {
            bump(&self.counters.words_unknown);
        }
        self.handed_over(epoch, utterance, duration, ctx);
    }

    /// Books a frame the transport accepted; the first one of an epoch starts the response for
    /// the rest of the pipeline.
    fn handed_over(
        &mut self,
        epoch: TurnEpoch,
        utterance: u64,
        duration: Duration,
        ctx: &ProcessorContext,
    ) {
        if let Some(sent) = &mut self.sent
            && sent.epoch == epoch
        {
            sent.total = sent.total.saturating_add(duration);
            sent.utterance = utterance;
            return;
        }
        let now = self.clock.now();
        self.sent = Some(EpochAudio {
            epoch,
            first_at: now,
            total: duration,
            utterance,
        });
        self.timeline.mark(epoch, TurnEvent::FirstAudioOut, now);
        self.upstream(control(epoch, ControlKind::TtsStarted), ctx);
        bump(&self.counters.started_sent);
    }

    fn word(&self, word: &brisvox_core::frame::WordTimestamp) {
        if self
            .book
            .push_word(word.utterance, Arc::clone(&word.word), word.start)
        {
            bump(&self.counters.words);
        } else {
            bump(&self.counters.words_unknown);
        }
    }

    /// `TtsEnded` of `epoch` from the TTS stage: the response's audio is all handed over (or
    /// there is none).
    async fn tts_ended(&mut self, epoch: TurnEpoch, ctx: &mut ProcessorContext) {
        if self.stale_control(epoch) {
            return;
        }
        let Some(sent) = self.sent.filter(|sent| sent.epoch == epoch) else {
            // Nothing was sent: no echo will ever come, the response is over now.
            bump(&self.counters.direct_ends);
            self.end_upstream(epoch, ctx);
            return;
        };
        if ctx.epoch_watch().is_some_and(|w| w.latest() > epoch) {
            return; // interrupted meanwhile: the barge-in told everyone
        }
        let id = format!("brisvox-end-{}-{}", epoch.get(), self.mark_seq);
        self.mark_seq = self.mark_seq.saturating_add(1);
        // Registered before the mark is sent: the echo can come back at once.
        if !self.book.register_mark(id.as_str(), sent.utterance) {
            bump(&self.counters.words_unknown);
            self.end_upstream(epoch, ctx);
            return;
        }
        match self
            .send_command(TransportCommand::Mark { id }, Some(epoch), ctx)
            .await
        {
            Outcome::Sent => {
                bump(&self.counters.marks_sent);
                let now = self.clock.now();
                let played_out = (sent.first_at + sent.total).max(now);
                self.marks.push(PendingMark {
                    epoch,
                    deadline: brisvox_core::clock::deadline_after(
                        played_out,
                        self.config.mark_timeout,
                    ),
                });
            }
            Outcome::Stale => {}
            Outcome::Failed => {
                bump(&self.counters.send_failures);
                self.end_upstream(epoch, ctx);
            }
        }
    }

    /// Reports the end of the response of `epoch` upstream, once.
    fn end_upstream(&mut self, epoch: TurnEpoch, ctx: &ProcessorContext) {
        if self.ended_through.is_some_and(|ended| ended >= epoch) {
            bump(&self.counters.duplicate_ends);
            return;
        }
        self.ended_through = Some(epoch);
        self.marks.retain(|mark| mark.epoch != epoch);
        bump(&self.counters.ends_upstream);
        self.upstream(control(epoch, ControlKind::TtsEnded), ctx);
    }

    fn upstream(&self, frame: Frame, ctx: &ProcessorContext) {
        if let Some(upstream) = ctx.sender(Direction::Upstream)
            && upstream.try_send(frame).is_err()
        {
            bump(&self.counters.upstream_dropped);
        }
    }

    async fn interrupt(&mut self, new_epoch: TurnEpoch, ctx: &mut ProcessorContext) {
        if new_epoch <= self.floor {
            return; // a duplicate of an interrupt already handled
        }
        self.floor = new_epoch;
        bump(&self.counters.interrupts);
        if let Some(sent) = self.sent.take() {
            // The turn driver marks it first when its fast-path `Clear` returned (it is the
            // moment the transport applied the clear); a second mark would only be counted as
            // repeated.
            if !self
                .timeline
                .is_marked(sent.epoch, TurnEvent::AudioOutStopped)
            {
                self.timeline
                    .mark(sent.epoch, TurnEvent::AudioOutStopped, self.clock.now());
            }
        }
        self.utterance = None;
        self.marks.retain(|mark| mark.epoch >= new_epoch);
        // The driver cleared the transport on the fast path; this one is idempotent.
        let _ = self.clear(new_epoch, ctx).await;
    }

    /// The marks whose echo is overdue end their response.
    fn marks_due(&mut self, ctx: &ProcessorContext) {
        let _ = self.slot.drain();
        let now = self.clock.now();
        let mut due = Vec::new();
        self.marks.retain(|mark| {
            if mark.deadline <= now {
                due.push(mark.epoch);
                false
            } else {
                true
            }
        });
        for epoch in due {
            if epoch < self.tx_epoch {
                // The transport moved on (its clear cut the mark): nothing to end.
                continue;
            }
            bump(&self.counters.mark_timeouts);
            tracing::warn!(
                target: "brisvox::engine",
                epoch = epoch.get(),
                "the client did not echo the end mark; ending the response"
            );
            self.end_upstream(epoch, ctx);
        }
    }

    /// Arms the timer for the earliest overdue-check (none: disarmed). Created the first time a
    /// mark is pending.
    fn sync_timer(&mut self, ctx: &ProcessorContext) {
        let next = self.marks.iter().map(|mark| mark.deadline).min();
        if self.timer.is_none() {
            if next.is_none() {
                return;
            }
            let Some(me) = ctx.self_sender() else {
                return;
            };
            self.timer = Some(DeadlineTimer::spawn(
                Arc::clone(&self.clock),
                self.slot.clone(),
                || (),
                me,
                WAKE_TOKEN,
            ));
        }
        if let Some(timer) = self.timer.as_mut() {
            timer.set(next);
        }
    }
}

/// A control frame of `epoch`.
fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame { epoch, kind })
}

impl Processor for TransportOut {
    fn name(&self) -> &'static str {
        "transport_out"
    }

    /// The stage filters epochs itself (see the module docs).
    fn epoch_policy(&self) -> EpochPolicy {
        EpochPolicy::KeepAll
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if direction == Direction::Upstream {
            // From the pump: the client echoed an end mark. Everything else goes on upstream.
            match &frame {
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }) => {
                    let epoch = *epoch;
                    if !self.stale_control(epoch) {
                        self.end_upstream(epoch, ctx);
                    }
                }
                _ => self.upstream(frame, ctx),
            }
            self.sync_timer(ctx);
            return Ok(());
        }
        match frame {
            Frame::Data(DataFrame::Audio(audio)) => self.audio(audio, ctx).await,
            Frame::Data(DataFrame::Word(word)) => self.word(&word),
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::UtteranceStarted { utterance },
            }) => {
                if !self.stale_control(epoch) {
                    self.utterance = Some((epoch, utterance));
                }
            }
            Frame::Control(ControlFrame {
                epoch,
                kind: ControlKind::TtsEnded,
            }) => self.tts_ended(epoch, ctx).await,
            Frame::System(SystemFrame::Interrupt { new_epoch }) => {
                self.interrupt(new_epoch, ctx).await;
            }
            Frame::System(SystemFrame::Wake { token }) if token == WAKE_TOKEN => {
                self.marks_due(ctx);
            }
            // Everything else (turn boundaries, LLM and flush markers, lifecycle) ends here.
            _ => {}
        }
        self.sync_timer(ctx);
        Ok(())
    }
}
