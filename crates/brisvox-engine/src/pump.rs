//! The transport pump: the one task that reads a [`TransportSession`] and turns its events into
//! frames (`docs/PIPELINE.md` sections 1 and 7).
//!
//! | Transport event | What the pump does |
//! |---|---|
//! | `Audio` | retags the frame to epoch zero and `try_send`s it into the first stage (a `REALTIME_INPUT` inbox: full means the OLDEST audio is dropped and counted there) |
//! | `Started` | parses `metadata["sample_rate"]` into [`CallRates`] and publishes them ([`PumpHandle::rates`]); a rate the audio path cannot work with is a fatal `Error` frame and a hangup |
//! | `Dtmf` | a `DataFrame::Dtmf` in epoch zero (the turn driver re-tags it with the current epoch) |
//! | `MarkPlayed` | resolves the mark in the [`PlayoutBook`]; if it was the end mark of an utterance, pushes `TtsEnded` of that utterance's epoch UPSTREAM through the last stage |
//! | `MarkCleared`, `PlaybackPosition` | into the [`PlayoutBook`] only |
//! | `Stopped` | hangup: `SystemFrame::Stop` into the first stage and [`PumpHandle::end`] |
//! | `Error` | counted and logged; a hangup only when [`PumpConfig::hangup_on_error`] is set (default off) |
//!
//! # It never stops draining
//! The transport's event queue is bounded and several of its events are lossless, so a reader
//! that stops reading eventually stalls the whole session (mark echoes included). The pump
//! therefore never awaits a send into the pipeline inside its read loop. Audio is lossy by
//! design (`try_send` into a drop-oldest inbox). The rare lossless frames (DTMF, `Stop`, the
//! upstream `TtsEnded`, a fatal `Error`) wait, when their target has no room, in small FIFOs
//! that separate `select!` branches deliver, one frame at a time per FIFO, whenever the target
//! has room; the read loop keeps running meanwhile. There is one FIFO per target lane, so a
//! frame stuck on a full lane never holds back one that has room (the system lane overtaking
//! the data lane is the pipeline's rule, and the pump keeps it): DTMF (the first stage's data
//! lane), `Stop` and the fatal `Error` (its system lane, in order), and `TtsEnded` (the last
//! stage's upstream inbox). The DTMF and `TtsEnded` FIFOs are bounded
//! ([`PumpConfig::pending_capacity`] each): when one is full a new DTMF or `TtsEnded` is
//! dropped and counted (a lost `TtsEnded` is recovered by the turn driver's response
//! deadline). The system FIFO holds at most a fatal `Error` and a `Stop` and is always queued.
//!
//! # Hangup
//! Hangup means `SystemFrame::Stop` into the first stage: a graceful stop that every stage
//! forwards after draining what it has, ending the pipeline in order. The turn driver reads the
//! `Stop` as the caller leaving. After a hangup the pump stops forwarding audio and DTMF but keeps
//! reading until the session ends, so marks and positions still reach the book. The `Stop` does
//! not wait for a key press that is stuck on a full data lane: the hangup matters more than the
//! last digit, and the digit is still offered until it is delivered or the flush gives up. If
//! frames are still undelivered [`PumpConfig::flush_timeout`] after the hangup they are dropped,
//! after one last `try_send` of each queued `Stop` or `Error`.
//! The pump does not end the transport session itself: on [`PumpEnd`] the owner of the call
//! closes it (through [`PumpHandle::commands`]).
//!
//! # Decision: `TransportEvent::Error` is a counted warning by default
//! The transport documents `Error` as a NON-FATAL fault and keeps its own fault budget (the
//! Twilio connection tolerates a number of recoverable faults, such as one undecodable media
//! payload, and ends the call itself with `Stopped` when the budget is spent). A pump that hung
//! up on the first `Error` would defeat that budget and kill a live call over one glitch. So
//! [`PumpConfig::hangup_on_error`] defaults to `false`: the pump counts the fault, logs a
//! warning and carries on, and only `Stopped` (the transport's verdict that the call is over)
//! hangs up. Setting it to `true` restores the strict reading where the first `Error` is a
//! hangup.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, DtmfFrame, ErrorFrame, Frame, SystemFrame,
};
use brisvox_core::queue::TrySendError;
use brisvox_core::{Clock, FrameSender, SharedClock, TurnEpoch};
use brisvox_transport::{CallRates, CommandSender, StopReason, TransportEvent, TransportSession};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{EngineConfig, EngineRates};
use crate::guard::TaskGuard;
use crate::playout::PlayoutBook;
use crate::rates::CallRatesCell;

/// Why the pump stopped forwarding: the first hangup, or how the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PumpEnd {
    /// The transport reported `Stopped` with this reason.
    Stopped(StopReason),
    /// The transport reported an `Error` and [`PumpConfig::hangup_on_error`] is set.
    TransportError(String),
    /// The call's rates (from `Started`) cannot be used; the message names why.
    BadRates(String),
    /// The session's event stream ended without a `Stopped`.
    SessionClosed,
    /// The pump was cancelled.
    Cancelled,
}

/// Settings of the pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpConfig {
    /// Audio rates and frame length: the fallback call rates when the transport reports none,
    /// and the frame length every reported rate must divide into whole samples.
    pub rates: EngineRates,
    /// Whether `TransportEvent::Error` triggers a hangup (see the module docs). Default `false`:
    /// the transport's `Error` is non-fatal and it ends the call itself (`Stopped`) when its
    /// fault budget is spent.
    pub hangup_on_error: bool,
    /// Most undelivered DTMF frames, and most undelivered upstream `TtsEnded` frames, the pump
    /// holds (a FIFO each; see the module docs). Default 32.
    pub pending_capacity: usize,
    /// How long after a hangup the pump keeps trying to deliver pending frames. Default 5 s
    /// ([`Timeouts::stop`](crate::Timeouts::stop)).
    pub flush_timeout: Duration,
}

impl PumpConfig {
    /// Settings derived from the engine configuration.
    pub fn from_engine(config: &EngineConfig) -> Self {
        Self {
            rates: config.rates,
            hangup_on_error: false,
            pending_capacity: 32,
            flush_timeout: config.timeouts.stop,
        }
    }
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self::from_engine(&EngineConfig::DEFAULT)
    }
}

/// Where the pump delivers frames.
#[derive(Debug, Clone)]
pub struct PumpLinks {
    /// The first stage of the pipeline (`RunningPipeline::input`).
    pub input: FrameSender,
    /// The last stage, for frames travelling upstream (`RunningPipeline::upstream_input`).
    /// `None` when the pipeline has no stage that writes marks (the end-mark `TtsEnded` is then
    /// counted as unrouted).
    pub upstream: Option<FrameSender>,
}

#[derive(Debug, Default)]
struct Counters {
    audio: AtomicU64,
    audio_discarded: AtomicU64,
    rate_mismatch: AtomicU64,
    dtmf: AtomicU64,
    dtmf_dropped: AtomicU64,
    end_marks: AtomicU64,
    marks_cleared: AtomicU64,
    unknown_marks: AtomicU64,
    positions: AtomicU64,
    leads: AtomicU64,
    positions_rejected: AtomicU64,
    upstream_dropped: AtomicU64,
    unrouted: AtomicU64,
    transport_errors: AtomicU64,
    hangups: AtomicU64,
    pending_dropped_at_hangup: AtomicU64,
    control_dropped: AtomicU64,
}

/// A point-in-time copy of the pump's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PumpStats {
    /// Audio events read from the transport.
    pub audio: u64,
    /// Audio events not forwarded because the call hung up or the pipeline is gone.
    pub audio_discarded: u64,
    /// Audio frames whose rate differed from the call's input rate (still forwarded;
    /// `AudioIn` converts by the frame's own rate).
    pub rate_mismatch: u64,
    /// DTMF events forwarded (queued or delivered).
    pub dtmf: u64,
    /// DTMF events dropped because the pending queue was full or the call had hung up.
    pub dtmf_dropped: u64,
    /// End-of-utterance marks echoed by the client (each produced an upstream `TtsEnded`).
    pub end_marks: u64,
    /// End-of-utterance marks cut by a clear before the client echoed them.
    pub marks_cleared: u64,
    /// Mark events the book did not know (not registered, evicted, already resolved).
    pub unknown_marks: u64,
    /// Playback positions accepted by the book.
    pub positions: u64,
    /// Client playback-lead reports recorded in the book (Twilio).
    pub leads: u64,
    /// Playback positions the book ignored (unknown utterance or another epoch).
    pub positions_rejected: u64,
    /// Upstream `TtsEnded` frames dropped (pending queue full, upstream gone, or hangup).
    pub upstream_dropped: u64,
    /// Upstream frames with nowhere to go ([`PumpLinks::upstream`] is `None`).
    pub unrouted: u64,
    /// `TransportEvent::Error` events seen.
    pub transport_errors: u64,
    /// Hangups issued (0 or 1).
    pub hangups: u64,
    /// Pending frames discarded when the flush timeout after a hangup expired.
    pub pending_dropped_at_hangup: u64,
    /// `Stop` or fatal `Error` frames that could not be delivered because the pipeline's input
    /// was already closed (the pipeline had ended).
    pub control_dropped: u64,
}

/// The owner's handle on a running pump. Dropping it aborts the pump task (which drops the
/// transport session, ending the call).
pub struct PumpHandle {
    _task: TaskGuard,
    counters: Arc<Counters>,
    commands: CommandSender,
    fallback: CallRates,
    rates: watch::Receiver<Option<CallRates>>,
    end: watch::Receiver<Option<PumpEnd>>,
}

impl std::fmt::Debug for PumpHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpHandle")
            .field("rates", &self.rates())
            .field("end", &self.end())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl PumpHandle {
    /// A sender for transport commands (audio out, clear, mark, stop), valid while the session
    /// lives. The pump owns the session itself.
    pub fn commands(&self) -> CommandSender {
        self.commands.clone()
    }

    /// The call's rates: what `Started` reported, or the configured fallback before that and
    /// when the transport reports none.
    pub fn rates(&self) -> CallRates {
        (*self.rates.borrow()).unwrap_or(self.fallback)
    }

    /// Whether `Started` was read.
    pub fn started(&self) -> bool {
        self.rates.borrow().is_some()
    }

    /// Waits until `Started` was read and returns the call's rates; `None` if the pump ended
    /// first.
    pub async fn wait_started(&mut self) -> Option<CallRates> {
        loop {
            if let Some(rates) = *self.rates.borrow_and_update() {
                return Some(rates);
            }
            self.rates.changed().await.ok()?;
        }
    }

    /// Why the pump stopped forwarding, once it did.
    pub fn end(&self) -> Option<PumpEnd> {
        self.end.borrow().clone()
    }

    /// Waits for the first hangup or the end of the session, and returns why.
    pub async fn wait_end(&mut self) -> PumpEnd {
        loop {
            if let Some(end) = self.end.borrow_and_update().clone() {
                return end;
            }
            if self.end.changed().await.is_err() {
                // The task is gone without publishing (aborted).
                return self.end().unwrap_or(PumpEnd::Cancelled);
            }
        }
    }

    /// The counters now.
    pub fn stats(&self) -> PumpStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        PumpStats {
            audio: get(&c.audio),
            audio_discarded: get(&c.audio_discarded),
            rate_mismatch: get(&c.rate_mismatch),
            dtmf: get(&c.dtmf),
            dtmf_dropped: get(&c.dtmf_dropped),
            end_marks: get(&c.end_marks),
            marks_cleared: get(&c.marks_cleared),
            unknown_marks: get(&c.unknown_marks),
            positions: get(&c.positions),
            leads: get(&c.leads),
            positions_rejected: get(&c.positions_rejected),
            upstream_dropped: get(&c.upstream_dropped),
            unrouted: get(&c.unrouted),
            transport_errors: get(&c.transport_errors),
            hangups: get(&c.hangups),
            pending_dropped_at_hangup: get(&c.pending_dropped_at_hangup),
            control_dropped: get(&c.control_dropped),
        }
    }
}

/// The FIFO of lossless frames waiting for one target lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fifo {
    /// DTMF into the first stage's data lane.
    Data,
    /// `Stop` and the fatal `Error` into the first stage's system lane.
    System,
    /// `TtsEnded` into the last stage's upstream inbox.
    Upstream,
}

/// Spawns the pump.
#[derive(Debug, Clone, Copy)]
pub struct Pump;

impl Pump {
    /// Starts the pump task for `session`. Must run inside a tokio runtime.
    ///
    /// `book` receives marks and positions; `clock` times the post-hangup flush. The task ends
    /// when the session's events end, when `cancel` fires, or when the returned handle is
    /// dropped.
    pub fn spawn(
        session: TransportSession,
        links: PumpLinks,
        book: Arc<PlayoutBook>,
        clock: SharedClock,
        config: PumpConfig,
        cancel: &CancellationToken,
    ) -> PumpHandle {
        let rates = CallRatesCell::new(config.rates.fallback_call);
        Self::spawn_publishing(session, links, book, clock, config, cancel, rates)
    }

    /// Like [`Pump::spawn`], publishing the call's rates into `rates` as well, so a stage built
    /// before the pump (the TTS stage resamples to the output rate) can read them. The cell's
    /// fallback should be [`EngineRates::fallback_call`].
    pub fn spawn_publishing(
        session: TransportSession,
        links: PumpLinks,
        book: Arc<PlayoutBook>,
        clock: SharedClock,
        config: PumpConfig,
        cancel: &CancellationToken,
        rates: CallRatesCell,
    ) -> PumpHandle {
        let counters = Arc::new(Counters::default());
        let commands = session.commands();
        let rates_rx = rates.subscribe();
        let rates_tx = rates;
        let (end_tx, end_rx) = watch::channel(None);
        let fallback = config.rates.fallback_call;
        let state = State {
            session,
            links,
            book,
            clock,
            config,
            counters: Arc::clone(&counters),
            rates_tx,
            end_tx,
            call_rates: fallback,
            data: VecDeque::with_capacity(config.pending_capacity.saturating_add(2)),
            system: VecDeque::with_capacity(4),
            upstream: VecDeque::with_capacity(config.pending_capacity.saturating_add(2)),
            pipeline_gone: false,
            hung_up: false,
            hangup_deadline: None,
        };
        let task = TaskGuard::spawn("transport_pump", state.run(cancel.clone()));
        PumpHandle {
            _task: task,
            counters,
            commands,
            fallback,
            rates: rates_rx,
            end: end_rx,
        }
    }
}

struct State {
    session: TransportSession,
    links: PumpLinks,
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    config: PumpConfig,
    counters: Arc<Counters>,
    rates_tx: CallRatesCell,
    end_tx: watch::Sender<Option<PumpEnd>>,
    call_rates: CallRates,
    /// DTMF waiting for room in the first stage's data lane.
    data: VecDeque<Frame>,
    /// `Stop` / fatal `Error` waiting for room in the first stage's system lane (a few at most:
    /// always queued).
    system: VecDeque<Frame>,
    /// `TtsEnded` waiting for room in the last stage's upstream inbox.
    upstream: VecDeque<Frame>,
    /// The first stage's inbox is closed: the pipeline ended.
    pipeline_gone: bool,
    /// A hangup was issued.
    hung_up: bool,
    /// When the post-hangup flush gives up (`None` before a hangup and after giving up).
    hangup_deadline: Option<Instant>,
}

/// Which counter a frame that could not be queued is charged to.
#[derive(Debug, Clone, Copy)]
enum Lane {
    Dtmf,
    TtsEnded,
    Control,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl State {
    fn charge(&self, lane: Lane) {
        bump(match lane {
            Lane::Dtmf => &self.counters.dtmf_dropped,
            Lane::TtsEnded => &self.counters.upstream_dropped,
            Lane::Control => &self.counters.control_dropped,
        });
    }

    async fn run(mut self, cancel: CancellationToken) {
        let mut session_open = true;
        // One waiter for the task's whole life: registering a new `cancelled()` waiter per event
        // would take the token's lock, which every task of the call shares.
        let cancelled = cancel.cancelled();
        tokio::pin!(cancelled);
        let end = loop {
            // The session's events ended: what is still pending gets its chance (a hangup's
            // `Stop` is queued right behind the last event), then the pump is done.
            if !session_open && self.nothing_pending() {
                break PumpEnd::SessionClosed;
            }
            // The head of each FIFO, delivered by its own branch so the read loop never waits
            // for a full neighbour and a stuck lane never holds back another one.
            let head_system = self.system.front().cloned();
            let head_data = self.data.front().cloned();
            let head_upstream = self.upstream.front().cloned();
            let deadline = self.hangup_deadline;
            tokio::select! {
                biased;
                () = &mut cancelled => break PumpEnd::Cancelled,
                delivered = Self::deliver(&self.links, Fifo::System, head_system), if !self.system.is_empty() => {
                    self.delivered(Fifo::System, delivered);
                }
                delivered = Self::deliver(&self.links, Fifo::Data, head_data), if !self.data.is_empty() => {
                    self.delivered(Fifo::Data, delivered);
                }
                delivered = Self::deliver(&self.links, Fifo::Upstream, head_upstream), if !self.upstream.is_empty() => {
                    self.delivered(Fifo::Upstream, delivered);
                }
                () = wait_deadline(&*self.clock, deadline), if deadline.is_some() && !self.nothing_pending() => {
                    self.give_up();
                }
                event = self.session.recv(), if session_open => match event {
                    Some(event) => self.handle(event),
                    None => {
                        session_open = false;
                        // Ended without a `Stopped` (it can be lost to a full queue): still a hangup.
                        self.hangup(PumpEnd::SessionClosed);
                    }
                },
            }
        };
        // Publish how it ended unless a hangup already said why.
        self.end_tx.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(end);
                true
            } else {
                false
            }
        });
    }

    fn nothing_pending(&self) -> bool {
        self.data.is_empty() && self.system.is_empty() && self.upstream.is_empty()
    }

    fn fifo_mut(&mut self, fifo: Fifo) -> &mut VecDeque<Frame> {
        match fifo {
            Fifo::Data => &mut self.data,
            Fifo::System => &mut self.system,
            Fifo::Upstream => &mut self.upstream,
        }
    }

    fn target(links: &PumpLinks, fifo: Fifo) -> Option<&FrameSender> {
        match fifo {
            Fifo::Upstream => links.upstream.as_ref(),
            Fifo::Data | Fifo::System => Some(&links.input),
        }
    }

    /// Delivers the head of a FIFO (awaiting room). Cancel-safe: the frame is a clone and stays
    /// queued until the delivery is reported.
    async fn deliver(links: &PumpLinks, fifo: Fifo, head: Option<Frame>) -> Delivered {
        let Some(frame) = head else {
            return std::future::pending().await;
        };
        let Some(target) = Self::target(links, fifo) else {
            return Delivered::Unrouted;
        };
        match target.send(frame).await {
            Ok(()) => Delivered::Sent,
            Err(_) => Delivered::Closed,
        }
    }

    fn delivered(&mut self, fifo: Fifo, outcome: Delivered) {
        // The head is the frame `deliver` was given: a FIFO only grows at the back while its
        // delivery is in flight.
        let Some(head) = self.fifo_mut(fifo).pop_front() else {
            return;
        };
        match outcome {
            Delivered::Sent => {}
            Delivered::Unrouted => bump(&self.counters.unrouted),
            Delivered::Closed if fifo == Fifo::Upstream => self.charge(Lane::TtsEnded),
            Delivered::Closed => {
                self.pipeline_closed();
                self.charge(match head {
                    Frame::Data(_) => Lane::Dtmf,
                    _ => Lane::Control,
                });
            }
        }
    }

    /// The flush after a hangup ran out of time: one last `try_send` of each queued `Stop` or
    /// fatal `Error` (a slot may have freed in the meantime), then everything left is dropped
    /// and counted.
    fn give_up(&mut self) {
        let mut dropped = 0u64;
        while let Some(frame) = self.system.pop_front() {
            if self.links.input.try_send(frame).is_err() {
                dropped += 1;
            }
        }
        dropped = dropped.saturating_add(u64::try_from(self.data.len()).unwrap_or(u64::MAX));
        dropped = dropped.saturating_add(u64::try_from(self.upstream.len()).unwrap_or(u64::MAX));
        self.data.clear();
        self.upstream.clear();
        self.counters
            .pending_dropped_at_hangup
            .fetch_add(dropped, Ordering::Relaxed);
        tracing::warn!(target: "brisvox::engine", dropped, "pump gave up delivering frames after the hangup");
        self.hangup_deadline = None;
    }

    fn pipeline_closed(&mut self) {
        if !self.pipeline_gone {
            tracing::debug!(target: "brisvox::engine", "pump: the pipeline's input is closed");
        }
        self.pipeline_gone = true;
        // Nothing queued for the pipeline's input can be delivered any more.
        let keys = u64::try_from(self.data.len()).unwrap_or(u64::MAX);
        let controls = u64::try_from(self.system.len()).unwrap_or(u64::MAX);
        self.data.clear();
        self.system.clear();
        self.counters
            .dtmf_dropped
            .fetch_add(keys, Ordering::Relaxed);
        self.counters
            .control_dropped
            .fetch_add(controls, Ordering::Relaxed);
    }

    /// Sends a lossless frame without waiting: straight into the target while its FIFO is empty
    /// and the target has room (keeping order), otherwise into the FIFO. `must_queue` frames
    /// (`Stop`, a fatal `Error`) are queued even when the FIFO is full; others are dropped and
    /// charged to `lane`.
    fn send_lossless(&mut self, frame: Frame, fifo: Fifo, must_queue: bool, lane: Lane) {
        let to_input = fifo != Fifo::Upstream;
        if to_input && self.pipeline_gone {
            self.charge(lane);
            return;
        }
        let Some(target) = Self::target(&self.links, fifo) else {
            bump(&self.counters.unrouted);
            return;
        };
        let queue_is_empty = match fifo {
            Fifo::Data => self.data.is_empty(),
            Fifo::System => self.system.is_empty(),
            Fifo::Upstream => self.upstream.is_empty(),
        };
        let frame = if queue_is_empty {
            match target.try_send(frame) {
                Ok(()) => return,
                Err(TrySendError::Closed(_)) => {
                    if to_input {
                        self.pipeline_closed();
                    }
                    self.charge(lane);
                    return;
                }
                Err(TrySendError::Full(frame)) => frame,
            }
        } else {
            frame
        };
        let capacity = self.config.pending_capacity;
        let queue = self.fifo_mut(fifo);
        if !must_queue && queue.len() >= capacity {
            self.charge(lane);
            return;
        }
        queue.push_back(frame);
    }

    fn handle(&mut self, event: TransportEvent) {
        match event {
            TransportEvent::Audio(frame) => self.audio(frame),
            TransportEvent::Started(info) => self.started(&info),
            TransportEvent::Dtmf { digit, at } => {
                if self.hung_up {
                    self.charge(Lane::Dtmf);
                    return;
                }
                bump(&self.counters.dtmf);
                let frame = Frame::Data(DataFrame::Dtmf(DtmfFrame {
                    epoch: TurnEpoch::ZERO,
                    digit,
                    at,
                }));
                self.send_lossless(frame, Fifo::Data, false, Lane::Dtmf);
            }
            TransportEvent::MarkPlayed { id } => match self.book.mark_played_resolved(&id) {
                Some(mark) => {
                    bump(&self.counters.end_marks);
                    if self.pipeline_gone || self.hung_up {
                        self.charge(Lane::TtsEnded);
                        return;
                    }
                    let frame = Frame::Control(ControlFrame {
                        epoch: mark.epoch,
                        kind: ControlKind::TtsEnded,
                    });
                    self.send_lossless(frame, Fifo::Upstream, false, Lane::TtsEnded);
                }
                None => bump(&self.counters.unknown_marks),
            },
            TransportEvent::MarkCleared { id } => match self.book.mark_cleared_resolved(&id) {
                Some(_) => bump(&self.counters.marks_cleared),
                None => bump(&self.counters.unknown_marks),
            },
            TransportEvent::PlaybackLead { lead } => {
                self.book.set_client_lead(lead);
                bump(&self.counters.leads);
            }
            TransportEvent::PlaybackPosition {
                epoch,
                utterance,
                played,
            } => {
                if self.book.position(epoch, utterance.get(), played) {
                    bump(&self.counters.positions);
                } else {
                    bump(&self.counters.positions_rejected);
                }
            }
            TransportEvent::Error { message } => {
                bump(&self.counters.transport_errors);
                tracing::warn!(target: "brisvox::engine", %message, "transport reported an error");
                if self.config.hangup_on_error {
                    self.hangup(PumpEnd::TransportError(message));
                }
            }
            TransportEvent::Stopped { reason } => self.hangup(PumpEnd::Stopped(reason)),
        }
    }

    fn audio(&mut self, frame: AudioFrame) {
        bump(&self.counters.audio);
        if self.hung_up || self.pipeline_gone {
            bump(&self.counters.audio_discarded);
            return;
        }
        if frame.sample_rate() != self.call_rates.input {
            bump(&self.counters.rate_mismatch);
        }
        // The user side stays in epoch zero whatever epoch the transport's ledger is in.
        let frame = if frame.epoch() == TurnEpoch::ZERO {
            frame
        } else {
            let (rate, timestamp, seq) = (frame.sample_rate(), frame.timestamp(), frame.seq());
            match AudioFrame::new(frame.into_data(), rate, timestamp, seq, TurnEpoch::ZERO) {
                Ok(frame) => frame,
                Err(_) => return,
            }
        };
        // Audio never waits: the first stage's inbox drops its oldest audio when full (counted
        // in the pipeline's stats). Only a closed inbox is news.
        match self
            .links
            .input
            .try_send(Frame::Data(DataFrame::Audio(frame)))
        {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => {
                self.pipeline_closed();
                bump(&self.counters.audio_discarded);
            }
            Err(TrySendError::Full(_)) => bump(&self.counters.audio_discarded),
        }
    }

    fn started(&mut self, info: &brisvox_transport::CallInfo) {
        let rates = match info.sample_rate() {
            Ok(Some(rates)) => rates,
            Ok(None) => self.config.rates.fallback_call,
            Err(error) => return self.bad_rates(error.to_string()),
        };
        if let Err(error) = self.config.rates.check_call_rates(rates) {
            return self.bad_rates(error.to_string());
        }
        self.call_rates = rates;
        self.rates_tx.publish(rates);
    }

    fn bad_rates(&mut self, message: String) {
        tracing::error!(target: "brisvox::engine", %message, "unusable call rates");
        let error = Frame::System(SystemFrame::Error(Box::new(ErrorFrame {
            stage: "transport.in",
            message: message.clone(),
            fatal: true,
        })));
        self.send_lossless(error, Fifo::System, true, Lane::Control);
        self.hangup(PumpEnd::BadRates(message));
    }

    /// Ends the call's input: `Stop` into the first stage, and the reason for the owner. Only
    /// the first hangup counts.
    fn hangup(&mut self, reason: PumpEnd) {
        if self.hung_up {
            return;
        }
        self.hung_up = true;
        bump(&self.counters.hangups);
        self.hangup_deadline = Some(brisvox_core::clock::deadline_after(
            self.clock.now(),
            self.config.flush_timeout,
        ));
        self.end_tx.send_replace(Some(reason));
        self.send_lossless(
            Frame::System(SystemFrame::Stop),
            Fifo::System,
            true,
            Lane::Control,
        );
    }
}

/// How a pending delivery ended.
enum Delivered {
    Sent,
    Unrouted,
    Closed,
}

/// Completes at `deadline` (never when there is none).
async fn wait_deadline(clock: &dyn Clock, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => brisvox_core::clock::wait_until(clock, deadline).await,
        None => std::future::pending().await,
    }
}
