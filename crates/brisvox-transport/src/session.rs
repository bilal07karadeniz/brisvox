//! One call's session: the engine-side handle and the tasks that move audio between the engine and
//! a [`CallLink`].
//!
//! # Structure
//! ```text
//! engine ──commands──► [commands] ──► pacer pipeline (OutputPacer) ──► sink ──► [output] ──► wire
//! engine ◄──events──── [input] ◄────────────────────────────────────────────────────── wire
//! ```
//! - **commands** (one task) accepts engine commands in order: it queues audio into the production
//!   [`OutputPacer`] and registers marks. `Clear` does not travel this queue: [`CommandSender`]
//!   applies it at once, from the caller's task (see "Clear" below).
//! - the pacer pipeline releases audio at real time; its sink is read by **output**, the only task
//!   that writes to the wire. Output also keeps the playback accounting and sends marks at their
//!   position in the audio.
//! - **input** forwards what the adapter reports (caller audio, DTMF, mark echoes) to the engine.
//! - a **supervisor** joins the three tasks and the pacer pipeline, reports the stop reason as the
//!   last event, and tells the client to hang up.
//!
//! All state shared between the tasks lives in one `Mutex`-guarded ledger; no task awaits while
//! holding it. Only the output task writes to the wire, so frames, clears and marks reach the
//! client in exactly the order the ledger describes.
//!
//! # Clear
//! A clear must not wait behind the audio it cancels, and the audio queue is deep (the command
//! queue, the pacer's input, the sink, the wire queue). So [`CommandSender::send`] applies it
//! directly: it raises the epoch in the ledger (queued audio and marks become stale), pulls the
//! stale audio out of the wire queue, and puts an `Interrupt` on the pacer's system lane, which
//! purges the pacer's data lane and abandons the frame it waits on. The output task then sends
//! `WireOut::Clear`. Audio that was still in the command queue is rejected by its epoch when the
//! command task reaches it. What can still reach the adapter ahead of the clear is the one frame
//! the output task was writing when the clear arrived.
//!
//! # Pause and resume
//! Pause-then-decide stops the bot's playout without ending the response. [`CommandSender`]
//! applies [`TransportCommand::Pause`] like a clear, around the command queue: the output task stops
//! reading the pacer's sink (so the sink fills, the pacer waits, and the pacer, command and engine queues
//! fill behind it: the hold buffer is those bounded queues, no audio is dropped), the audio and
//! marks of the wire queue are pulled back, and the adapter gets `WireOut::Clear` of the current
//! epoch, so its buffers and the client's are flushed and the caller hears silence as early as a
//! barge-in's clear gives it. The epoch is unchanged; adapters accept audio of the epoch they were
//! cleared to, so nothing else is needed. The marks sent and not yet echoed go back to waiting.
//!
//! [`TransportCommand::Resume`] re-sends the last `rewind` of the released audio (kept in a ring
//! of the most recent [`SessionConfig::replay`], never counted as released again, so positions, marks and
//! the engine's heard-position arithmetic stay those of the stream), then the marks, then the
//! pacer goes on: it finds itself behind its schedule and restarts its grid (an underrun is counted). A
//! [`TransportCommand::Clear`] during a pause (the commit) drops the held audio like any clear. If more than
//! [`SessionConfig::hold_max`] of audio waits while paused, [`TransportEvent::HoldFull`] is
//! reported once (overflow: the engine commits).
//!
//! # Marks
//! A mark waits in the ledger until the audio ahead of it is released, is then sent to the wire
//! and remembered as sent (at most `MAX_SENT_MARKS`) until the client echoes it. A clear cuts every
//! mark that has not been echoed: waiting, in the wire queue (purged with the stale audio) or in
//! the adapter's buffer. Each is reported as `MarkCleared`, and a later echo of it is ignored, so
//! a mark ends in exactly one of `MarkPlayed` and `MarkCleared`. A mark whose echo is in flight
//! when the clear arrives is reported as cleared too: the safe reading for "what the caller heard".
//!
//! # Hang-up and closed wire
//! If the adapter drops its end of the wire (so the output task cannot write), the input task
//! still gets `INPUT_DRAIN_TIMEOUT` to deliver what the adapter queued before: a hang-up that
//! follows records `PeerHangup`, and key presses and mark echoes are not lost. Only when that
//! window closes, or the adapter's event side is closed too, does the session end as
//! `PeerDisconnected`.
//!
//! # Latency added by the transport
//! Output audio waits in the pacer's input queue ([`SessionConfig::pacer_capacity`] frames, which
//! is pacing headroom, not delay), then in the sink ([`SessionConfig::sink_capacity`] frames), then
//! in the wire queue (`WIRE_OUT_CAPACITY` frames), then in the adapter's own buffer (the client
//! side, transport-specific). The first two are on the audio's schedule; the sink and the wire
//! queue add about 20 ms each when the client keeps up, and the adapter adds its playout buffer.
//!
//! # Playback position accuracy
//! [`TransportEvent::PlaybackPosition`] counts audio released to the wire, not audio the caller
//! heard: it runs ahead of the caller by the adapter's buffer (for the loopback, its configured
//! latency), and a barge-in does not subtract what the client had buffered. Exact playout comes
//! from [`TransportEvent::MarkPlayed`], which the client sends when the audio before a mark has
//! actually played (the telephony adapters wire this). A client that also reports how far it is
//! behind (Twilio, through its progress marks) gives [`TransportEvent::PlaybackLead`]: the audio
//! sent and not yet played, so `position - lead` approximates what the caller heard in the middle
//! of an utterance.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_audio::OutputPacer;
use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SystemFrame};
use brisvox_core::queue::OverflowPolicies;
use brisvox_core::{
    Direction, Error as CoreError, FrameSender, Inbox, Links, Pipeline, QueueConfig,
    RunningPipeline, SharedClock, TurnEpoch,
};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::call::{CallLink, WireEvent, WireOut};
use crate::channel::{self, ChannelStats, Overflow, Overflowing, Receiver, Sender};
use crate::error::{Error, Result};
use crate::event::{CallInfo, StopReason, TransportCommand, TransportEvent, UtteranceId};

/// Slots in the system lane of the pacer and sink inboxes (system frames are never dropped).
const SYSTEM_LANE: usize = 8;

/// The most released audio kept for a resume to send again.
pub const DEFAULT_REPLAY: Duration = Duration::from_secs(3);

/// How long the final playback report may wait for a consumer that has stopped reading.
const FINAL_REPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// Most `(epoch, utterance)` counters the output task keeps. Past it, the oldest counter is
/// reported and forgotten (an utterance id that comes back after 256 others restarts at zero).
const MAX_TRACKED_UTTERANCES: usize = 256;

/// How long the adapter may take nothing from the wire queue before the session fails. A client
/// that cannot take audio for this long has stalled; waiting longer only grows the delay.
const WIRE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the supervisor waits to hand the client its hang-up.
const HANGUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Most marks the core remembers as sent but not yet echoed. Past it, the oldest is given up and
/// reported as `MarkCleared` (a client that never echoes marks must not grow the ledger).
const MAX_SENT_MARKS: usize = 256;

/// How long the input task may keep reading the adapter's queued events after the wire to the
/// adapter has closed, before the session ends as a lost peer. A hang-up, key presses or mark
/// echoes the adapter queued before it went away are still delivered within this window.
const INPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Tuning of one session. The defaults suit a 20 ms audio path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// Seed of the pacer's per-session phase offset (see `brisvox_audio::pacer::phase_offset`),
    /// which only matters with `first_frame_phase`. Use a different seed per session.
    pub seed: u64,
    /// Whether the session's first output frame waits for the phase offset of `seed` (0-19 ms)
    /// so that sessions started in the same millisecond wake in different ones. Off by default:
    /// the first answer's first frame leaves at once.
    pub first_frame_phase: bool,
    /// Capacity of the events queue to the engine. Full: input audio is dropped oldest-first,
    /// playback reports are coalesced (the latest survives), everything else waits.
    pub events_capacity: usize,
    /// Capacity of the command queue from the engine. Full: the engine waits (`Clear` excepted:
    /// it does not use the queue).
    pub commands_capacity: usize,
    /// Capacity of the pacer's input queue, in audio frames. Full: the engine's command task waits.
    pub pacer_capacity: usize,
    /// Capacity of the pacer's sink, in frames. Keep it small: each queued frame is latency.
    pub sink_capacity: usize,
    /// Audio released per playback report (per utterance). Smaller means finer progress reports
    /// and more events.
    pub position_interval: Duration,
    /// While paused ([`TransportCommand::Pause`]): the most audio that may wait, accepted and not
    /// released, before [`TransportEvent::HoldFull`] is reported. The queues bound it anyway
    /// (a few seconds); this makes the limit explicit.
    pub hold_max: Duration,
    /// How much of the most recently released audio is kept for a resume to send again
    /// ([`TransportCommand::Resume`]). Zero (the default) keeps none and refuses
    /// [`TransportCommand::Pause`] (an error; the engine then interrupts as without
    /// pause-then-decide): hosts that enable the feature set [`DEFAULT_REPLAY`]. The ring holds
    /// reference-counted frames (no copies), about 50 per second of audio. A rewind that asks
    /// for more is cut to it and counted ([`CommandSender::hold_stats`]).
    ///
    /// What a resume cannot give back: audio the client had buffered when it was cleared beyond
    /// `rewind` (a lead estimate), and, where a clear does not flush the client
    /// ([`CallLink::client_flush`]: WebRTC, SIP), nothing but what the session pulled out of its
    /// own wire queue is sent again, so up to the adapter's own queue of audio can be lost; the
    /// one frame the output task is writing when the pause lands goes out ahead of the clear and
    /// may be heard twice (it is in the ring).
    pub replay: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            first_frame_phase: false,
            events_capacity: 64,
            commands_capacity: 64,
            pacer_capacity: 64,
            sink_capacity: 4,
            position_interval: Duration::from_millis(100),
            hold_max: Duration::from_millis(2500),
            replay: Duration::ZERO,
        }
    }
}

/// The engine-side handle of one call.
///
/// Read [`TransportEvent`]s with [`recv`](Self::recv), send [`TransportCommand`]s with
/// [`send`](Self::send) (or clone [`commands`](Self::commands) for another task). Dropping the
/// session cancels it; [`close`](Self::close) also waits for every task to finish.
pub struct TransportSession {
    events: Receiver<TransportEvent>,
    commands: CommandSender,
    shared: Arc<Shared>,
    started: CallStarted,
    cancel: CancellationToken,
    supervisor: Option<JoinHandle<()>>,
}

/// The [`CallInfo`] of a session's call, once its `Started` event passed through the session
/// ([`TransportSession::call_started`]). Cheap to clone; every clone sees the same value. A host
/// that hands the session to the engine keeps a clone to learn who the call is (a carrier's
/// call id for its REST API, the transport kind) without reading the session's events itself.
#[derive(Debug, Clone, Default)]
pub struct CallStarted(Arc<StartedCell>);

/// The shared cell of a [`CallStarted`]: the info and the wake-up of those waiting for it.
#[derive(Debug, Default)]
struct StartedCell {
    info: std::sync::OnceLock<CallInfo>,
    set: tokio::sync::Notify,
}

impl CallStarted {
    /// A handle that already holds `info` (a host that learned the call's info elsewhere, or a
    /// test).
    pub fn from_info(info: CallInfo) -> Self {
        let cell = StartedCell::default();
        let _ = cell.info.set(info);
        Self(Arc::new(cell))
    }

    /// The call's info, `None` until the adapter's `Started` event was read.
    pub fn get(&self) -> Option<&CallInfo> {
        self.0.info.get()
    }

    /// Waits until the call's info is known. Never resolves for a call that ends before its
    /// `Started` event: race it with the call's own end.
    pub async fn wait(&self) -> &CallInfo {
        loop {
            let notified = self.0.set.notified();
            tokio::pin!(notified);
            // Registered before the check, so a `set` in between is not lost.
            notified.as_mut().enable();
            if let Some(info) = self.0.info.get() {
                return info;
            }
            notified.await;
        }
    }

    /// Records `info` (the first one wins).
    fn set(&self, info: &CallInfo) {
        if self.0.info.set(info.clone()).is_ok() {
            self.0.set.notify_waiters();
        }
    }
}

/// What a command waits in the command queue with: the command and the epoch current when the
/// engine sent it.
struct Queued {
    command: TransportCommand,
    sent_in: TurnEpoch,
}

impl Overflowing for Queued {
    fn overflow(&self) -> Overflow {
        Overflow::Backpressure
    }
}

/// The path `Clear` takes around the command queue; dropped when the session ends.
struct ClearPath {
    pacer: FrameSender,
    wire: Sender<WireOut>,
}

/// State behind every [`CommandSender`] clone.
struct Control {
    shared: Arc<Shared>,
    cancel: CancellationToken,
    /// Serialises clears, so their interrupts reach the pacer in epoch order.
    gate: tokio::sync::Mutex<()>,
    path: Mutex<Option<ClearPath>>,
}

impl Control {
    fn lock_path(&self) -> MutexGuard<'_, Option<ClearPath>> {
        self.path.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the clear path. Held senders no longer keep the pacer or the wire open.
    fn close(&self) {
        *self.lock_path() = None;
    }

    /// Applies `Clear { epoch }` now. Does nothing if `epoch` is not newer than the current one.
    async fn clear(&self, epoch: TurnEpoch) -> Result<()> {
        let closed = || Error::Core(CoreError::ChannelClosed("transport session"));
        let _gate = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(closed()),
            gate = self.gate.lock() => gate,
        };
        let path = self
            .lock_path()
            .as_ref()
            .map(|path| (path.pacer.clone(), path.wire.clone()));
        let raised = {
            let mut ledger = self.shared.lock();
            if epoch > ledger.epoch {
                ledger.epoch = epoch;
                ledger.generation = ledger.generation.saturating_add(1);
                // A clear ends a pause (the commit) and drops what was held for a resume.
                if ledger.paused {
                    ledger.paused = false;
                    ledger.pause_seq = ledger.pause_seq.saturating_add(1);
                }
                ledger.pulled = Duration::ZERO;
                ledger.rewind = Duration::ZERO;
                ledger.hold_flagged = false;
                ledger.replay.clear();
                ledger.replay_dur = Duration::ZERO;
                // Everything accepted so far is older: it will never be released.
                ledger.queued.clear();
                ledger.enqueued = ledger.released;
                // Every mark that has not been echoed is cut: the ones still waiting for their
                // audio, and the ones already sent (the audio ahead of a sent mark may sit in
                // the wire queue, which is purged below, or in the adapter's buffer, which the
                // adapter drops on `WireOut::Clear`). A late echo of a cut mark is ignored, so a
                // mark ends in exactly one of `MarkPlayed` and `MarkCleared`.
                let sent = std::mem::take(&mut ledger.sent_marks);
                ledger.cleared_marks.extend(sent);
                let waiting = std::mem::take(&mut ledger.marks);
                ledger
                    .cleared_marks
                    .extend(waiting.into_iter().map(|(id, _)| id));
                // Audio and marks the output task already released are behind the wire's gate:
                // pull them back. Done under the ledger lock, so the output task cannot have put
                // a newer item there (it sends only after it has seen the new generation). The
                // one item the output task is writing right now can still land behind this.
                if let Some((_, wire)) = &path {
                    wire.purge(|out| match out {
                        WireOut::Audio(frame) => frame.epoch() < epoch,
                        WireOut::Mark { .. } => true,
                        WireOut::Clear { .. } | WireOut::Hangup => false,
                    });
                }
                true
            } else {
                false
            }
        };
        if !raised {
            return Ok(());
        }
        self.shared.wake.notify_one();
        let Some((pacer, _)) = path else {
            return Err(closed());
        };
        let interrupt = Frame::from(SystemFrame::Interrupt { new_epoch: epoch });
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(closed()),
            sent = pacer.send(interrupt) => Ok(sent?),
        }
    }
}

impl Control {
    /// Applies `Pause` now (see "Pause and resume" in the module docs).
    async fn pause(&self) -> Result<()> {
        let closed = || Error::Core(CoreError::ChannelClosed("transport session"));
        let _gate = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(closed()),
            gate = self.gate.lock() => gate,
        };
        let wire = self.lock_path().as_ref().map(|path| path.wire.clone());
        {
            let mut ledger = self.shared.lock();
            if ledger.paused {
                return Ok(());
            }
            if ledger.replay_max.is_zero() {
                return Err(Error::Config(
                    "pause needs SessionConfig::replay: nothing is kept to send again".to_owned(),
                ));
            }
            ledger.paused = true;
            ledger.pause_seq = ledger.pause_seq.saturating_add(1);
            ledger.hold_flagged = false;
            ledger.rewind = Duration::ZERO;
            // Marks already sent wait again: the client's buffer is cleared, their echoes are
            // ignored, and they are sent once more after the audio that is sent again.
            let target = ledger.released;
            let sent = std::mem::take(&mut ledger.sent_marks);
            for id in sent.into_iter().rev() {
                ledger.marks.push_front((id, target));
            }
            // Audio and marks the output task already queued for the adapter come back: the audio
            // is in the replay ring (it counts as released), the marks wait again.
            let mut pulled = Duration::ZERO;
            if let Some(wire) = &wire {
                wire.purge(|out| match out {
                    WireOut::Audio(frame) => {
                        pulled = pulled.saturating_add(frame.duration());
                        true
                    }
                    WireOut::Mark { .. } => true,
                    WireOut::Clear { .. } | WireOut::Hangup => false,
                });
            }
            ledger.pulled = pulled;
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Applies `Resume` now.
    async fn resume(&self, rewind: Duration) -> Result<()> {
        let closed = || Error::Core(CoreError::ChannelClosed("transport session"));
        let _gate = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(closed()),
            gate = self.gate.lock() => gate,
        };
        {
            let mut ledger = self.shared.lock();
            if !ledger.paused {
                return Ok(());
            }
            ledger.paused = false;
            ledger.pause_seq = ledger.pause_seq.saturating_add(1);
            let wanted = rewind.max(ledger.pulled);
            if wanted > ledger.replay_max {
                ledger.rewind_clamped = ledger.rewind_clamped.saturating_add(1);
            }
            ledger.rewind = wanted.min(ledger.replay_max);
        }
        self.shared.wake.notify_one();
        Ok(())
    }
}

/// Counters of a session's pauses ([`CommandSender::hold_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoldStats {
    /// Resumes that asked to send again more audio than [`SessionConfig::replay`] keeps: the
    /// rewind was cut, so part of what the client had buffered was not sent again.
    pub rewind_clamped: u64,
}

/// The engine's way to issue commands; cheap to clone, for tasks that act on their own (a barge-in
/// detector sends `Clear` from its own task).
///
/// Every command except [`TransportCommand::Clear`] waits in the bounded command queue, in order,
/// and `send` waits while it is full. `Clear` skips the queue: it is applied before `send`
/// returns, so it cannot wait behind audio, and audio sent before it that has not been accepted
/// yet is dropped as stale.
#[derive(Clone)]
pub struct CommandSender {
    queue: Sender<Queued>,
    control: Arc<Control>,
}

impl CommandSender {
    /// Counters of pause and resume (see [`HoldStats`]).
    pub fn hold_stats(&self) -> HoldStats {
        HoldStats {
            rewind_clamped: self.control.shared.lock().rewind_clamped,
        }
    }

    /// The number of the latest pause or resume the session applied (it grows by one with each).
    /// Read right after a [`TransportCommand::Pause`] was sent, it names that pause in
    /// [`TransportEvent::HoldFull`].
    pub fn pause_seq(&self) -> u64 {
        self.control.shared.lock().pause_seq
    }

    /// Applies a command that skips the queue (`Clear`, `Pause`, `Resume`); `None` for the rest.
    async fn out_of_band(&self, command: &TransportCommand) -> Option<Result<()>> {
        match command {
            TransportCommand::Clear { epoch } => Some(self.control.clear(*epoch).await),
            TransportCommand::Pause => Some(self.control.pause().await),
            TransportCommand::Resume { rewind } => Some(self.control.resume(*rewind).await),
            _ => None,
        }
    }

    /// Sends a command (see the type's notes for `Clear`).
    ///
    /// # Errors
    /// [`CoreError::ChannelClosed`] (wrapped) once the session has ended.
    pub async fn send(&self, command: TransportCommand) -> Result<()> {
        if let Some(done) = self.out_of_band(&command).await {
            return done;
        }
        let sent_in = self.control.shared.lock().epoch;
        self.queue.send(Queued { command, sent_in }).await
    }

    /// Sends a command that belongs to `epoch`: like [`Self::send`], but the command is
    /// judged by the epoch its sender meant, not by the transport's epoch at the moment of the
    /// call. A [`TransportCommand::Mark`] of an epoch older than the transport's current one when
    /// the command task takes it is cut (reported as [`TransportEvent::MarkCleared`], never sent
    /// to the client), even when the `Clear` that ended `epoch` was applied between the sender's
    /// own epoch check and this call. With [`Self::send`] such a mark would be stamped with the
    /// new epoch and reach the wire after the clear.
    ///
    /// Audio is judged by its frame's epoch either way; `Clear` ignores `epoch`.
    ///
    /// # Errors
    /// [`CoreError::ChannelClosed`] (wrapped) once the session has ended.
    pub async fn send_in(&self, epoch: TurnEpoch, command: TransportCommand) -> Result<()> {
        if let Some(done) = self.out_of_band(&command).await {
            return done;
        }
        self.queue
            .send(Queued {
                command,
                sent_in: epoch,
            })
            .await
    }
}

impl TransportSession {
    /// Starts a session on `link`. The session is a child of `parent`: cancelling `parent` ends it.
    ///
    /// Spawns the session's tasks and starts the output pacer. Must run inside a tokio runtime
    /// that has the time driver enabled; `clock` must be the session's clock.
    ///
    /// # Errors
    /// Errors from starting the pacer pipeline.
    pub async fn open(
        clock: SharedClock,
        link: CallLink,
        config: SessionConfig,
        parent: &CancellationToken,
    ) -> Result<Self> {
        let cancel = parent.child_token();
        let shared = Arc::new(Shared::new());
        let (events_tx, events_rx) = channel::bounded::<TransportEvent>(config.events_capacity);
        let (commands_tx, commands_rx) = channel::bounded::<Queued>(config.commands_capacity);

        let sink = Inbox::new(QueueConfig {
            system_capacity: SYSTEM_LANE,
            data_capacity: config.sink_capacity,
            policies: OverflowPolicies::DEFAULT,
        });
        let pacer_queue = QueueConfig {
            system_capacity: SYSTEM_LANE,
            data_capacity: config.pacer_capacity,
            policies: OverflowPolicies::DEFAULT,
        };
        let pipeline = Pipeline::new("transport.out")
            .with_queue_config(pacer_queue)
            .then(
                OutputPacer::new(clock, config.seed)
                    .with_first_frame_phase(config.first_frame_phase),
            );
        let links = Links {
            downstream: Some(sink.sender(Direction::Downstream)),
            upstream: None,
        };
        let pipeline = pipeline.start(links, &cancel).await?;
        let pacer_input = pipeline.input().clone();

        // A ticket still in the link is dropped here (see `CallLink::take_ticket`).
        let CallLink {
            events,
            outgoing,
            ended,
            ticket: _,
            transport: _,
            call_id: _,
            client_flush,
        } = link;
        {
            let mut ledger = shared.lock();
            ledger.replay_max = config.replay;
            ledger.client_flush = client_flush;
        }
        let control = Arc::new(Control {
            shared: Arc::clone(&shared),
            cancel: cancel.clone(),
            gate: tokio::sync::Mutex::new(()),
            path: Mutex::new(Some(ClearPath {
                pacer: pacer_input.clone(),
                wire: outgoing.clone(),
            })),
        });
        let output = Output {
            shared: Arc::clone(&shared),
            sink,
            wire: outgoing.clone(),
            events: events_tx.clone(),
            cancel: cancel.clone(),
            interval: config.position_interval,
            generation: 0,
            pause_seq: 0,
            pause_applied: false,
            hold_reported: false,
            positions: BTreeMap::new(),
            current: None,
        };

        let mut tasks = JoinSet::new();
        tasks.spawn(output.run());
        tasks.spawn(run_commands(
            commands_rx,
            pacer_input,
            Arc::clone(&shared),
            cancel.clone(),
            config.hold_max,
        ));
        let started = CallStarted::default();
        tasks.spawn(run_input(
            events,
            events_tx.clone(),
            Arc::clone(&shared),
            started.clone(),
            cancel.clone(),
        ));
        let supervisor = tokio::spawn(supervise(
            tasks,
            pipeline,
            outgoing,
            events_tx,
            Arc::clone(&control),
            ended,
            cancel.clone(),
        ));

        Ok(Self {
            events: events_rx,
            commands: CommandSender {
                queue: commands_tx,
                control,
            },
            shared,
            started,
            cancel,
            supervisor: Some(supervisor),
        })
    }

    /// The next event, waiting for one. `None` once the session has ended and the last event
    /// ([`TransportEvent::Stopped`]) was read.
    pub async fn recv(&mut self) -> Option<TransportEvent> {
        self.events.recv().await
    }

    /// Sends a command. Waits while the command queue is full; [`TransportCommand::Clear`] never
    /// waits for the queue (see [`CommandSender`]).
    ///
    /// # Errors
    /// [`CoreError::ChannelClosed`] (wrapped) once the session has ended.
    pub async fn send(&self, command: TransportCommand) -> Result<()> {
        self.commands.send(command).await
    }

    /// A clone of the command sender, for a task that issues commands on its own.
    pub fn commands(&self) -> CommandSender {
        self.commands.clone()
    }

    /// Why the session ended, once known: set when a stop is requested, the peer leaves or a task
    /// fails, and finalised (as [`StopReason::Cancelled`] if nothing else) before
    /// [`TransportEvent::Stopped`] is emitted. Unlike that event, it cannot be lost to a full
    /// event queue.
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.shared.lock().stop.clone()
    }

    /// Events lost to overflow so far (input audio dropped, or playback reports replaced, because
    /// the engine fell behind).
    pub fn event_stats(&self) -> ChannelStats {
        self.events.stats()
    }

    /// The call's [`CallInfo`] once its `Started` event passed through the session (a handle that
    /// stays valid after the session was handed to the engine).
    pub fn call_started(&self) -> CallStarted {
        self.started.clone()
    }

    /// A token that ends this session when cancelled.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Ends the session and waits for all of its tasks to finish.
    ///
    /// Stops the output and input tasks, hangs up the client, reports
    /// [`StopReason::Cancelled`] (or the reason already recorded) and waits for the pacer pipeline.
    ///
    /// # Errors
    /// [`CoreError::Timeout`] if the tasks did not finish within `timeout`; they are then aborted.
    pub async fn close(mut self, timeout: Duration) -> Result<()> {
        self.cancel.cancel();
        let Some(mut supervisor) = self.supervisor.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, &mut supervisor).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(CoreError::ProcessorPanicked("transport supervisor").into()),
            Err(_) => {
                supervisor.abort();
                Err(CoreError::Timeout {
                    what: "transport close",
                    after: timeout,
                }
                .into())
            }
        }
    }
}

impl Drop for TransportSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// ---------------------------------------------------------------------------------------------
// Shared ledger

/// Output accounting, shared by the command task (writes), the output task (reads and advances
/// the release counters) and the input task (reads the epoch).
#[derive(Default)]
struct Ledger {
    /// The transport's current epoch. Only advances, through `Clear`.
    epoch: TurnEpoch,
    /// Bumped on every epoch change. Lets the output task detect a clear it has not yet applied.
    generation: u64,
    /// One entry per audio frame accepted for the current epoch, in order.
    queued: VecDeque<UtteranceId>,
    /// Audio accepted for the current epoch, in total. Marks are positioned against this.
    enqueued: Duration,
    /// Audio released to the wire, in total. Never reset.
    released: Duration,
    /// Marks waiting for `released` to reach their position.
    marks: VecDeque<(String, Duration)>,
    /// Marks sent to the wire and not echoed yet, oldest first; at most `MAX_SENT_MARKS`. An echo
    /// of an id that is not here (cut by a clear, given up, unknown, repeated) is ignored.
    sent_marks: VecDeque<String>,
    /// Marks a clear cut; the output task reports them as `MarkCleared`.
    cleared_marks: Vec<String>,
    /// Why the session stops, first writer wins.
    stop: Option<StopReason>,
    /// The playout is paused ([`TransportCommand::Pause`]).
    paused: bool,
    /// Bumped on every pause, resume and clear of a pause; lets the output task see them.
    pause_seq: u64,
    /// The audio pulled back from the wire queue by the pause, and the rewind the resume asked
    /// for: what the output task sends again.
    pulled: Duration,
    rewind: Duration,
    /// The paused hold passed [`SessionConfig::hold_max`] and was not reported yet.
    hold_flagged: bool,
    /// [`SessionConfig::replay`], and whether a clear flushes the client ([`CallLink::client_flush`]).
    replay_max: Duration,
    client_flush: bool,
    /// Resumes whose rewind was cut to `replay_max`.
    rewind_clamped: u64,
    /// The most recently released audio, newest last, for a resume to send again.
    replay: VecDeque<AudioFrame>,
    replay_dur: Duration,
}

struct Shared {
    ledger: Mutex<Ledger>,
    /// Wakes the output task: a clear was applied or a mark was registered. A stored permit
    /// (`notify_one`) means no wake-up is lost between two loop iterations.
    wake: tokio::sync::Notify,
}

impl Shared {
    fn new() -> Self {
        Self {
            ledger: Mutex::new(Ledger::default()),
            wake: tokio::sync::Notify::new(),
        }
    }

    /// Critical sections never panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the stop reason unless one is already set.
    fn record_stop(&self, reason: StopReason) {
        let mut ledger = self.lock();
        if ledger.stop.is_none() {
            ledger.stop = Some(reason);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tasks

/// Queues one event for the engine, or ends with the cancellation.
async fn emit<T: Overflowing>(
    events: &Sender<T>,
    cancel: &CancellationToken,
    event: T,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Core(CoreError::Cancelled)),
        sent = events.send(event) => sent,
    }
}

/// Accepts engine commands in order and feeds the pacer.
async fn run_commands(
    mut commands: Receiver<Queued>,
    pacer: FrameSender,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    hold_max: Duration,
) -> Result<()> {
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            next = commands.recv() => next,
        };
        let Some(Queued { command, sent_in }) = next else {
            return Ok(());
        };
        match command {
            TransportCommand::Audio {
                epoch,
                utterance,
                frame,
            } => {
                if epoch != frame.epoch() {
                    continue;
                }
                let duration = frame.duration();
                let (accepted, hold_full) = {
                    let mut ledger = shared.lock();
                    if frame.epoch() == ledger.epoch {
                        ledger.queued.push_back(utterance);
                        ledger.enqueued = ledger.enqueued.saturating_add(duration);
                        let full = ledger.paused
                            && !ledger.hold_flagged
                            && ledger.enqueued.saturating_sub(ledger.released) > hold_max;
                        if full {
                            ledger.hold_flagged = true;
                        }
                        (true, full)
                    } else {
                        (false, false)
                    }
                };
                if !accepted {
                    continue;
                }
                if hold_full {
                    shared.wake.notify_one();
                }
                let sent = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    sent = pacer.send(Frame::from(frame)) => sent,
                };
                sent?;
            }
            TransportCommand::Clear { .. }
            | TransportCommand::Pause
            | TransportCommand::Resume { .. } => {
                // Applied by `CommandSender::send` and never queued.
            }
            TransportCommand::Mark { id } => {
                {
                    let mut ledger = shared.lock();
                    if sent_in < ledger.epoch {
                        // Sent before a clear that has since been applied: the audio it followed
                        // is gone, so the mark is cut with it.
                        ledger.cleared_marks.push(id);
                    } else {
                        let target = ledger.enqueued;
                        ledger.marks.push_back((id, target));
                    }
                }
                shared.wake.notify_one();
            }
            TransportCommand::Stop => {
                shared.record_stop(StopReason::Requested);
                cancel.cancel();
                return Ok(());
            }
        }
    }
}

/// Forwards what the adapter reports to the engine.
async fn run_input(
    mut wire: Receiver<WireEvent>,
    events: Sender<TransportEvent>,
    shared: Arc<Shared>,
    started: CallStarted,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            next = wire.recv() => next,
        };
        let Some(event) = next else {
            shared.record_stop(StopReason::PeerDisconnected);
            cancel.cancel();
            return Ok(());
        };
        let outgoing = match event {
            WireEvent::Started(info) => {
                started.set(&info);
                TransportEvent::Started(info)
            }
            WireEvent::Audio(frame) => {
                let epoch = shared.lock().epoch;
                TransportEvent::Audio(retag(frame, epoch)?)
            }
            WireEvent::Dtmf { digit, at } => TransportEvent::Dtmf { digit, at },
            WireEvent::Lead { lead } => TransportEvent::PlaybackLead { lead },
            WireEvent::MarkAck { id } => {
                let sent = {
                    let mut ledger = shared.lock();
                    ledger
                        .sent_marks
                        .iter()
                        .position(|sent| *sent == id)
                        .and_then(|index| ledger.sent_marks.remove(index))
                        .is_some()
                };
                if !sent {
                    // Cut by a clear (already reported as `MarkCleared`), given up, or never
                    // sent: the engine hears about a mark once.
                    continue;
                }
                TransportEvent::MarkPlayed { id }
            }
            WireEvent::Fault { message } => TransportEvent::Error { message },
            WireEvent::Hangup => {
                shared.record_stop(StopReason::PeerHangup);
                cancel.cancel();
                return Ok(());
            }
        };
        emit(&events, &cancel, outgoing).await?;
    }
}

/// Re-tags caller audio with the session's current epoch. The buffer is moved, not copied.
fn retag(frame: AudioFrame, epoch: TurnEpoch) -> Result<AudioFrame> {
    let (rate, timestamp, seq) = (frame.sample_rate(), frame.timestamp(), frame.seq());
    Ok(AudioFrame::new(
        frame.into_data(),
        rate,
        timestamp,
        seq,
        epoch,
    )?)
}

/// The next item of the pacer's sink, or never while the playout is paused (the sink then fills
/// and holds the pacer back).
async fn next_sink(sink: &mut Inbox, paused: bool) -> Option<(Frame, Direction)> {
    if paused {
        std::future::pending().await
    } else {
        sink.recv().await
    }
}

/// What the output task does next.
enum Step {
    /// A clear or a mark was registered: apply what is due.
    Wake,
    /// A frame came out of the pacer.
    Frame(Frame),
    /// Something other than a downstream frame (the pacer's start and interrupt echoes).
    Ignore,
}

/// Outcome of looking at a released frame against the ledger.
enum Take {
    /// A clear was applied since the last sync: sync first, then look again.
    Retry,
    /// Stale, or nothing is accounted for it: drop it.
    Drop,
    /// Send it, as this utterance.
    Send(UtteranceId),
}

/// Playback accounting of one `(epoch, utterance)`.
struct Position {
    played: Duration,
    reported: Duration,
}

/// Key of the playback counters: the same utterance id in another epoch counts on its own.
type PositionKey = (TurnEpoch, UtteranceId);

/// The output task: the only writer to the wire.
struct Output {
    shared: Arc<Shared>,
    sink: Inbox,
    wire: Sender<WireOut>,
    events: Sender<TransportEvent>,
    cancel: CancellationToken,
    interval: Duration,
    /// The clear generation this task has applied.
    generation: u64,
    /// The pause sequence this task has applied, whether the client was told to clear for the
    /// pause, and whether the hold-full event of this pause went out.
    pause_seq: u64,
    pause_applied: bool,
    hold_reported: bool,
    /// One counter per `(epoch, utterance)`, so utterances can interleave. Counters of epochs a
    /// clear has passed are reported and dropped; at most `MAX_TRACKED_UTTERANCES` are kept.
    positions: BTreeMap<PositionKey, Position>,
    /// The utterance released last.
    current: Option<PositionKey>,
}

impl Output {
    async fn run(mut self) -> Result<()> {
        let result = self.event_loop().await;
        self.finish().await;
        result
    }

    async fn event_loop(&mut self) -> Result<()> {
        loop {
            let paused = self.shared.lock().paused;
            let step = tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                () = self.shared.wake.notified() => Step::Wake,
                item = next_sink(&mut self.sink, paused) => match item {
                    Some((frame, Direction::Downstream)) => Step::Frame(frame),
                    Some(_) => Step::Ignore,
                    None => break,
                },
            };
            match step {
                Step::Wake => {
                    self.sync_generation().await?;
                    self.sync_pause().await?;
                    self.report_cleared_marks().await?;
                    self.drain_marks().await?;
                }
                Step::Frame(Frame::Data(DataFrame::Audio(audio))) => {
                    self.release(audio).await?;
                }
                Step::Frame(_) | Step::Ignore => {}
            }
        }
        Ok(())
    }

    /// Applies a pause or resume the engine made: on a pause the adapter is told to clear its
    /// buffers (the epoch stays); on a resume the audio the caller had not heard is sent again
    /// (not counted as released). Also reports a full hold once.
    async fn sync_pause(&mut self) -> Result<()> {
        let (seq, paused, epoch) = {
            let ledger = self.shared.lock();
            (ledger.pause_seq, ledger.paused, ledger.epoch)
        };
        if seq != self.pause_seq {
            self.pause_seq = seq;
            self.hold_reported = false;
            if paused {
                self.pause_applied = true;
                self.wire_send(WireOut::Clear { epoch }).await?;
            } else {
                let resend = std::mem::take(&mut self.pause_applied);
                let frames = self.take_replay(resend);
                for frame in frames {
                    self.wire_send(WireOut::Audio(frame)).await?;
                }
            }
        }
        let (flagged, pause) = {
            let ledger = self.shared.lock();
            (ledger.paused && ledger.hold_flagged, ledger.pause_seq)
        };
        if flagged && !self.hold_reported {
            self.hold_reported = true;
            emit(
                &self.events,
                &self.cancel,
                TransportEvent::HoldFull { pause },
            )
            .await?;
        }
        Ok(())
    }

    /// The frames a resume sends again: the newest ones that cover the rewind (when the client was
    /// told to clear for the pause) or only what the pause pulled back from the wire queue.
    fn take_replay(&self, client_cleared: bool) -> Vec<AudioFrame> {
        let mut ledger = self.shared.lock();
        // Without a client flush the audio the client held plays anyway: only what the session
        // pulled out of its own queue is sent again.
        let want = if client_cleared && ledger.client_flush {
            ledger.rewind
        } else {
            ledger.pulled
        };
        ledger.rewind = Duration::ZERO;
        ledger.pulled = Duration::ZERO;
        let mut covered = Duration::ZERO;
        let mut start = ledger.replay.len();
        while start > 0 && covered < want {
            start -= 1;
            if let Some(frame) = ledger.replay.get(start) {
                covered = covered.saturating_add(frame.duration());
            }
        }
        ledger.replay.iter().skip(start).cloned().collect()
    }

    /// Applies a clear the engine made: tells the wire first (so the adapter drops what it
    /// buffered), then reports the cut utterances' final positions and the cut marks.
    async fn sync_generation(&mut self) -> Result<()> {
        let (generation, epoch) = {
            let ledger = self.shared.lock();
            (ledger.generation, ledger.epoch)
        };
        if generation == self.generation {
            return Ok(());
        }
        self.generation = generation;
        self.wire_send(WireOut::Clear { epoch }).await?;
        self.flush_before(epoch).await?;
        self.report_cleared_marks().await
    }

    /// Hands one pacer-released frame to the wire, if it is still current.
    async fn release(&mut self, audio: AudioFrame) -> Result<()> {
        let utterance = loop {
            self.sync_generation().await?;
            let take = {
                let mut ledger = self.shared.lock();
                if ledger.generation != self.generation {
                    Take::Retry
                } else if audio.epoch() != ledger.epoch {
                    Take::Drop
                } else {
                    ledger.queued.pop_front().map_or(Take::Drop, Take::Send)
                }
            };
            match take {
                Take::Retry => {}
                Take::Drop => return Ok(()),
                Take::Send(utterance) => break utterance,
            }
        };
        let (epoch, duration) = (audio.epoch(), audio.duration());
        let kept = audio.clone();
        self.wire_send(WireOut::Audio(audio)).await?;
        let counted = {
            let mut ledger = self.shared.lock();
            if ledger.generation == self.generation {
                ledger.released = ledger.released.saturating_add(duration);
                if !ledger.replay_max.is_zero() {
                    ledger.replay.push_back(kept);
                    ledger.replay_dur = ledger.replay_dur.saturating_add(duration);
                }
                while ledger.replay_dur > ledger.replay_max {
                    let Some(old) = ledger.replay.pop_front() else {
                        ledger.replay_dur = Duration::ZERO;
                        break;
                    };
                    ledger.replay_dur = ledger.replay_dur.saturating_sub(old.duration());
                }
                true
            } else {
                // A clear raced with the send: this frame went out ahead of the clear, so the
                // adapter drops it. It is not released audio.
                false
            }
        };
        if !counted {
            return Ok(());
        }
        self.account((epoch, utterance), duration).await?;
        self.drain_marks().await
    }

    /// Adds released audio to its utterance and reports progress at the configured interval.
    ///
    /// The audio is counted before anything that can fail or wait, and a counter is marked as
    /// reported only after its report was queued. A cancellation in the middle therefore leaves
    /// every unreported position in `positions`, where `finish` sends it.
    async fn account(&mut self, key: PositionKey, duration: Duration) -> Result<()> {
        let previous = self.current.replace(key);
        let position = self.positions.entry(key).or_insert(Position {
            played: Duration::ZERO,
            reported: Duration::ZERO,
        });
        position.played = position.played.saturating_add(duration);
        if let Some(previous) = previous.filter(|previous| *previous != key) {
            // The previous utterance pauses: report where it stopped (its counter stays, the
            // utterance may continue later).
            self.flush_key(previous, false).await?;
        }
        if self.positions.len() > MAX_TRACKED_UTTERANCES
            && let Some(oldest) = self.positions.keys().find(|other| **other != key).copied()
        {
            self.flush_key(oldest, true).await?;
        }
        let interval = self.interval;
        let due = self
            .positions
            .get(&key)
            .is_some_and(|position| position.played.saturating_sub(position.reported) >= interval);
        if due {
            self.flush_key(key, false).await?;
        }
        Ok(())
    }

    /// Reports `played` for `key`.
    async fn report(&self, key: PositionKey, played: Duration) -> Result<()> {
        let event = TransportEvent::PlaybackPosition {
            epoch: key.0,
            utterance: key.1,
            played,
        };
        emit(&self.events, &self.cancel, event).await
    }

    /// Reports `key`'s position if it moved since the last report; with `forget`, drops its
    /// counter. The counter changes only once the report is queued: if the report fails (the
    /// session is ending), the position stays unreported and `finish` sends it.
    async fn flush_key(&mut self, key: PositionKey, forget: bool) -> Result<()> {
        let Some(position) = self.positions.get(&key) else {
            return Ok(());
        };
        let played = position.played;
        if played > position.reported {
            self.report(key, played).await?;
        }
        if forget {
            self.positions.remove(&key);
        } else if let Some(position) = self.positions.get_mut(&key) {
            position.reported = position.reported.max(played);
        }
        Ok(())
    }

    /// Reports and forgets the counters of epochs before `epoch`.
    async fn flush_before(&mut self, epoch: TurnEpoch) -> Result<()> {
        let stale: Vec<PositionKey> = self
            .positions
            .keys()
            .copied()
            .filter(|key| key.0 < epoch)
            .collect();
        for key in stale {
            self.flush_key(key, true).await?;
        }
        if self.current.is_some_and(|key| key.0 < epoch) {
            self.current = None;
        }
        Ok(())
    }

    /// Tells the engine about marks a clear cut.
    async fn report_cleared_marks(&mut self) -> Result<()> {
        let cleared = std::mem::take(&mut self.shared.lock().cleared_marks);
        for id in cleared {
            emit(
                &self.events,
                &self.cancel,
                TransportEvent::MarkCleared { id },
            )
            .await?;
        }
        Ok(())
    }

    /// Sends every mark whose position the released audio has reached, and remembers it as sent
    /// until the client echoes it (or a clear cuts it).
    async fn drain_marks(&mut self) -> Result<()> {
        loop {
            let (due, given_up) = {
                let mut ledger = self.shared.lock();
                let released = ledger.released;
                let is_due = ledger.generation == self.generation
                    && !ledger.paused
                    && ledger
                        .marks
                        .front()
                        .is_some_and(|(_, target)| *target <= released);
                // With a clear not applied yet, the marks left are for the new epoch: they wait
                // for the wake-up the clear sent, which sends `WireOut::Clear` first.
                let due = if is_due {
                    ledger.marks.pop_front().map(|(id, _)| id)
                } else {
                    None
                };
                let mut given_up = false;
                if let Some(id) = &due {
                    ledger.sent_marks.push_back(id.clone());
                    if ledger.sent_marks.len() > MAX_SENT_MARKS
                        && let Some(oldest) = ledger.sent_marks.pop_front()
                    {
                        ledger.cleared_marks.push(oldest);
                        given_up = true;
                    }
                }
                (due, given_up)
            };
            let Some(id) = due else {
                return Ok(());
            };
            if given_up {
                self.report_cleared_marks().await?;
            }
            self.wire_send(WireOut::Mark { id }).await?;
        }
    }

    /// Writes to the wire. A slow client slows the output (that is the point of the bounded wire
    /// queue), but an adapter that takes nothing for `WIRE_WRITE_TIMEOUT` has stalled: the
    /// session fails with a timeout. A closed wire means the client side is gone.
    async fn wire_send(&self, item: WireOut) -> Result<()> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(Error::Core(CoreError::Cancelled)),
            sent = tokio::time::timeout(WIRE_WRITE_TIMEOUT, self.wire.send(item)) => match sent {
                Ok(sent) => sent.map_err(|_| Error::Disconnected),
                Err(_) => Err(Error::Core(CoreError::Timeout {
                    what: "wire write",
                    after: WIRE_WRITE_TIMEOUT,
                })),
            },
        }
    }

    /// Reports every unreported position, each bounded: the engine may already be gone.
    async fn finish(&mut self) {
        let pending: Vec<(PositionKey, Duration)> = self
            .positions
            .iter()
            .filter(|(_, position)| position.played > position.reported)
            .map(|(key, position)| (*key, position.played))
            .collect();
        self.positions.clear();
        for (key, played) in pending {
            let event = TransportEvent::PlaybackPosition {
                epoch: key.0,
                utterance: key.1,
                played,
            };
            let _ = tokio::time::timeout(FINAL_REPORT_TIMEOUT, self.events.send(event)).await;
        }
    }
}

/// Joins the session's tasks and the pacer pipeline, then reports how the session ended.
async fn supervise(
    mut tasks: JoinSet<Result<()>>,
    mut pipeline: RunningPipeline,
    wire: Sender<WireOut>,
    events: Sender<TransportEvent>,
    control: Arc<Control>,
    ended: CancellationToken,
    cancel: CancellationToken,
) {
    let shared = Arc::clone(&control.shared);
    // Set once the wire to the adapter has closed: the input task then gets `INPUT_DRAIN_TIMEOUT`
    // to deliver what the adapter queued before it left (a hang-up, key presses, mark echoes).
    let mut draining = false;
    loop {
        let next = if draining {
            match tokio::time::timeout(INPUT_DRAIN_TIMEOUT, tasks.join_next()).await {
                Ok(next) => next,
                Err(_) => {
                    // The adapter keeps its event side open but reads nothing: it is gone.
                    shared.record_stop(StopReason::PeerDisconnected);
                    cancel.cancel();
                    draining = false;
                    continue;
                }
            }
        } else {
            tasks.join_next().await
        };
        let Some(joined) = next else {
            break;
        };
        if matches!(joined, Ok(Err(Error::Disconnected))) && !cancel.is_cancelled() {
            // The output task found the wire closed. That alone does not say how the call ended:
            // the adapter may have queued a hang-up before it dropped its end. The other tasks
            // keep running until the input task has read it, or the window closes.
            draining = true;
            continue;
        }
        // Any other task ending ends the call; the others follow.
        cancel.cancel();
        match joined {
            Ok(Ok(())) | Ok(Err(Error::Core(CoreError::Cancelled))) => {}
            Ok(Err(Error::Disconnected)) => shared.record_stop(StopReason::PeerDisconnected),
            Ok(Err(error)) => shared.record_stop(StopReason::Failed(error.to_string())),
            Err(join) => shared.record_stop(StopReason::Failed(format!("task failed: {join}"))),
        }
    }
    cancel.cancel();
    // Held `CommandSender`s must not keep the pacer or the wire open.
    control.close();
    // The pipeline ends with the cancellation. Errors from its tasks now are shutdown noise: the
    // sink they push into has already gone.
    let _ = pipeline.wait().await;
    drop(pipeline);
    let _ = tokio::time::timeout(HANGUP_TIMEOUT, wire.send(WireOut::Hangup)).await;
    // An adapter blocked on its client would never read the hang-up: tell it the call is over.
    ended.cancel();
    let reason = {
        let mut ledger = shared.lock();
        ledger.stop.get_or_insert(StopReason::Cancelled).clone()
    };
    let _ = tokio::time::timeout(
        HANGUP_TIMEOUT,
        events.send(TransportEvent::Stopped { reason }),
    )
    .await;
}
