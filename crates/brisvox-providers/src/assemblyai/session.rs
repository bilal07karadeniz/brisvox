//! The task that owns one AssemblyAI session: audio out, turns in, keepalive, finalize,
//! reconnect with replay, hang-up.
//!
//! One task, one `select!` loop, no locks. Order of events matters, so everything that touches
//! the socket happens in this task:
//!
//! - **Audio in.** Frames are batched into 50 ms..500 ms chunks (the server closes the session on
//!   anything outside 50..1000 ms), every chunk is appended to the [`ReplayRing`] and then sent
//!   from the ring (`pump`). In steady state the ring holds only audio of the turn in progress.
//! - **Turns in.** `Turn` messages are mapped by the [`Assembler`]; a final turn also trims the
//!   ring up to the end of its last word (word times are milliseconds on the connection and are
//!   offset by the connection's base position).
//! - **Watchdogs.** After audio is given to the session the server must say something within
//!   `io_timeout` (the `Heartbeat` messages exist for this; without heartbeats a WebSocket `Ping`
//!   probes the silent connection first, and a second `io_timeout` without any frame counts), else
//!   the connection counts as lost (a half-open socket is the commonest real drop) and is replaced
//!   like any other, with `Core(Timeout)` as the final error when no reconnect is left. A quiet
//!   line pauses the watch (after `keepalive_interval` without audio; with `inactivity_timeout`
//!   set a `KeepAlive` is sent too). A held audio remainder shorter than 50 ms waits for the next
//!   audio; it is padded and sent when a `Finalize` needs it out (and after `batch_flush`, when
//!   that is set). A `ForceEndpoint` is answered by AssemblyAI or ended synthetically
//!   after `finalize_timeout`, counted from the request (so the bound also holds while a
//!   reconnect and the replay are in the way).
//! - **Drops.** An unexpected close or I/O error reconnects (bounded attempts, backoff on the
//!   injected clock), replays the ring paced at `replay_speed`, and re-sends a pending
//!   `ForceEndpoint` at its original position. Audio and `Finalize` requests keep being accepted
//!   while reconnecting. Only audio that can still matter is replayed: the turn in progress (from
//!   [`TURN_PRE_ROLL`] before its first partial), a pending `Finalize`'s audio, everything the old
//!   connection never received, and a short pre-roll ([`REPLAY_PRE_ROLL`]); the silence before a
//!   caller's turn is skipped, because a replay paced at real time never catches up with live
//!   audio and its backlog would lag every later word. If the ring evicts audio a lagging replay
//!   has not sent yet, the connection's time base moves past the gap. A connection that is replaced because it went silent, and one that ends
//!   in a fatal error, is sent a best-effort `Terminate` first (billing is by connection time and
//!   a zombie session counts against the concurrent-session limit).
//! - **Hang-up.** Dropping the input (or the events receiver) ends the session: the events close
//!   at once, then `Terminate` is sent and the socket drained until `Termination` for at most
//!   `close_timeout`. Nothing produced after the hang-up reaches the caller.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Clock;
use brisvox_core::SharedClock;
use brisvox_core::clock::{deadline_after, wait_until};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::PROVIDER;
use super::assembler::Assembler;
use super::batch::Batcher;
use super::config::AssemblyAiConfig;
use super::connection::{Closed, Target, Ws, classify_close, open, timed};
use super::protocol::{self, FORCE_ENDPOINT, Incoming, KEEP_ALIVE, TERMINATE};
use super::replay::ReplayRing;
use crate::error::Error;
use crate::stt::{SttEvent, SttInput};

/// Longest wait for a best-effort `Terminate` before a connection is dropped.
const TERMINATE_BOUND: Duration = Duration::from_millis(250);

/// Largest reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How much already-sent audio a replay starts before the old connection's send position when no
/// turn is known to be open: speech whose first partial the server had not produced yet must not
/// be cut off. Everything older that does not belong to an open turn is silence or was
/// delivered, and replaying it would only add lag (a replay is paced at `replay_speed`, which is
/// 1.0 by default, so the backlog never drains).
const REPLAY_PRE_ROLL: Duration = Duration::from_secs(1);

/// How much audio a replay starts before the first partial of an open turn (or before the
/// `ForceEndpoint` of a pending `Finalize`): the model reports a partial some hundreds of
/// milliseconds after the speech began.
const TURN_PRE_ROLL: Duration = Duration::from_secs(2);

/// Everything a session needs that does not change.
#[derive(Clone)]
pub(crate) struct Params {
    /// Adapter settings.
    pub(crate) cfg: Arc<AssemblyAiConfig>,
    /// Injected clock.
    pub(crate) clock: SharedClock,
    /// Where to connect.
    pub(crate) target: Target,
    /// Audio bytes per second of this session.
    pub(crate) bytes_per_second: u64,
    /// Bytes in the smallest chunk the server accepts.
    pub(crate) min_chunk: usize,
    /// Bytes in the largest chunk the adapter sends.
    pub(crate) max_chunk: usize,
}

/// Why `serve` or `absorb` stopped.
#[derive(Debug)]
enum Stop {
    /// The caller left (input or events dropped) or stopped consuming.
    HangUp,
    /// The connection was lost; reconnecting may help.
    Drop(&'static str),
    /// The server said nothing for `io_timeout` after audio was given to the session: lost like
    /// `Drop`, but if no reconnect is left the error is `Core(Timeout)`.
    Silent,
    /// The session failed; the error is delivered and the session ends.
    Fatal(Error),
}

/// State of the current connection.
struct ConnState {
    /// Absolute stream position of the first byte sent on this connection.
    base_abs: u64,
    /// Absolute stream position up to which audio was sent on this connection.
    sent_abs: u64,
    /// Audio was given to the session since this connection opened.
    audio_started: bool,
    /// Last time anything was sent (audio, ForceEndpoint) or the line was declared quiet.
    last_tx: Instant,
    /// The line went quiet: the server owes nothing until audio resumes.
    paused: bool,
    /// A `Ping` probe is out (heartbeats off) and nothing was received since.
    pinged: bool,
    /// Start of the current wait for a server message; `None` when none is owed.
    anchor: Option<Instant>,
    /// Earliest time the next paced (replayed) chunk may go out.
    next_send_at: Option<Instant>,
}

impl ConnState {
    fn new(base_abs: u64, now: Instant) -> Self {
        Self {
            base_abs,
            sent_abs: base_abs,
            audio_started: false,
            last_tx: now,
            paused: false,
            pinged: false,
            anchor: None,
            next_send_at: None,
        }
    }
}

struct Session {
    p: Params,
    input: mpsc::Receiver<SttInput>,
    /// `None` once the session ended: the caller then sees the channel close.
    events: Option<mpsc::Sender<SttEvent>>,
    ring: ReplayRing,
    batcher: Batcher,
    /// When audio was last given to the batcher (for `batch_flush`).
    last_push: Option<Instant>,
    assembler: Assembler,
    conn: ConnState,
    /// Audio was given to the session since the last final turn or `Finalize` request: speech
    /// may be on its way even though the server has not produced a partial for it yet.
    audio_since_final: bool,
    /// A final turn was delivered and no partial has come since: the model has nothing open, so
    /// a `Finalize` that finds the assembler idle is not racing speech it has yet to report
    /// (cleared by a partial, a `Finalize` and a reconnect).
    final_delivered: bool,
    /// Absolute stream position at which the turn in progress was first seen (its first
    /// partial), `None` while no turn is open. A replay restarts a little before it.
    turn_anchor: Option<u64>,
    /// Absolute position at which a requested `ForceEndpoint` is to be sent.
    finalize_at: Option<u64>,
    /// Position at which the last `ForceEndpoint` was sent, until it is answered.
    finalize_sent_abs: Option<u64>,
    /// When the unanswered `Finalize` is given up on.
    finalize_deadline: Option<Instant>,
    /// Reconnect attempts used since the last `Turn` or `Heartbeat`. Only those prove a
    /// connection works: a server that accepts, sends `Begin` and closes, over and over, must
    /// not be reconnected forever.
    attempts: u32,
    /// The connection that was just lost was lost to silence (see [`Stop::Silent`]).
    silent: bool,
    /// Reused output buffer of the assembler.
    scratch: Vec<SttEvent>,
    /// Reused buffer for the chunks the batcher produces.
    chunks: Vec<Bytes>,
}

/// Runs a session on an open connection until it ends.
pub(crate) async fn run(
    params: Params,
    ws: Ws,
    input: mpsc::Receiver<SttInput>,
    events: mpsc::Sender<SttEvent>,
) {
    let window = params.cfg.replay_window.as_secs_f64() * params.bytes_per_second as f64;
    let capacity = if window.is_finite() && window > 0.0 {
        window as u64
    } else {
        0
    };
    let now = params.clock.now();
    let session = Session {
        ring: ReplayRing::new(capacity),
        batcher: Batcher::new(
            params.min_chunk,
            params.max_chunk,
            usize::try_from(params.cfg.encoding.bytes_per_sample()).unwrap_or(1),
            params.cfg.encoding.silence_byte(),
        ),
        last_push: None,
        assembler: Assembler::new(params.cfg.waits_for_formatted()),
        p: params,
        input,
        events: Some(events),
        conn: ConnState::new(0, now),
        audio_since_final: false,
        final_delivered: false,
        turn_anchor: None,
        finalize_at: None,
        finalize_sent_abs: None,
        finalize_deadline: None,
        attempts: 0,
        silent: false,
        scratch: Vec::new(),
        chunks: Vec::new(),
    };
    session.run(ws).await;
}

impl Session {
    async fn run(mut self, mut ws: Ws) {
        loop {
            match self.serve(&mut ws).await {
                Stop::HangUp => {
                    self.hang_up(ws).await;
                    return;
                }
                Stop::Fatal(error) => {
                    self.fail(error).await;
                    // The socket may still be open (a malformed message, say): end the
                    // session on the server too.
                    self.terminate_best_effort(&mut ws).await;
                    return;
                }
                stop @ (Stop::Drop(_) | Stop::Silent) => {
                    let reason = match stop {
                        Stop::Drop(reason) => reason,
                        _ => "no message from the server",
                    };
                    self.silent = matches!(stop, Stop::Silent);
                    tracing::debug!(reason, "assemblyai connection lost");
                    if self.silent {
                        // Half-open or stuck: the server may still be billing it.
                        self.terminate_best_effort(&mut ws).await;
                    }
                    drop(ws);
                    match self.reconnect().await {
                        Ok(new) => ws = new,
                        Err(Stop::Fatal(error)) => {
                            self.fail(error).await;
                            return;
                        }
                        Err(_) => {
                            self.events = None;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn cfg(&self) -> &AssemblyAiConfig {
        &self.p.cfg
    }

    fn clock(&self) -> &dyn Clock {
        &*self.p.clock
    }

    // -- main loop ---------------------------------------------------------------------------

    async fn serve(&mut self, ws: &mut Ws) -> Stop {
        if let Err(stop) = self.pump(ws).await {
            return stop;
        }
        loop {
            let keepalive_due = deadline_after(self.conn.last_tx, self.cfg().keepalive_interval);
            // The server only speaks up during silence through heartbeats; without them the
            // watch probes with a `Ping` (see `on_idle`).
            let idle_due = if self.conn.paused {
                None
            } else {
                self.conn
                    .anchor
                    .map(|a| deadline_after(a, self.cfg().io_timeout))
            };
            let batch_due = match self.cfg().batch_flush {
                Some(gap) if !self.batcher.is_empty() => {
                    self.last_push.map(|t| deadline_after(t, gap))
                }
                _ => None,
            };
            let pace_due = self.conn.next_send_at;
            let finalize_due = self.finalize_deadline;
            let step: Result<(), Stop> = tokio::select! {
                biased;
                () = events_closed(&self.events) => Err(Stop::HangUp),
                frame = ws.next() => self.on_frame(frame).await,
                () = sleep_until_opt(&*self.p.clock, finalize_due) => self.on_finalize_timeout().await,
                () = sleep_until_opt(&*self.p.clock, idle_due) => self.on_idle(ws).await,
                () = sleep_until_opt(&*self.p.clock, batch_due) => {
                    self.flush_batch();
                    self.pump(ws).await
                },
                () = wait_until(&*self.p.clock, keepalive_due) => self.on_quiet(ws).await,
                () = sleep_until_opt(&*self.p.clock, pace_due) => self.pump(ws).await,
                message = self.input.recv() => match message {
                    None => Err(Stop::HangUp),
                    Some(message) => match self.on_input(message).await {
                        Ok(()) => self.pump(ws).await,
                        Err(stop) => Err(stop),
                    },
                },
            };
            if let Err(stop) = step {
                return stop;
            }
        }
    }

    /// Takes one caller message: audio is batched into the ring, a finalize request is queued.
    async fn on_input(&mut self, message: SttInput) -> Result<(), Stop> {
        match message {
            SttInput::Audio(bytes) => {
                let now = self.clock().now();
                self.last_push = Some(now);
                // From here on the server owes a message (a heartbeat at least) until the line
                // goes quiet.
                self.conn.audio_started = true;
                self.conn.paused = false;
                self.conn.anchor.get_or_insert(now);
                self.audio_since_final = true;
                let mut chunks = std::mem::take(&mut self.chunks);
                self.batcher.push(bytes, &mut chunks);
                for chunk in chunks.drain(..) {
                    self.ring.push(chunk);
                }
                self.chunks = chunks;
                Ok(())
            }
            SttInput::Finalize => {
                // A second request while one is open adds nothing.
                if self.assembler.finalize_pending() {
                    return Ok(());
                }
                // Audio shorter than 50 ms cannot be sent as it is: pad what is held, so that
                // the ForceEndpoint comes after all the audio the caller gave.
                self.flush_batch();
                // A turn that is not there yet is not the same as nothing to flush: a short
                // utterance can end (by the engine's VAD) before the model has produced its
                // first partial, and its final then takes about as long as any turn
                // completion. Only a line with nothing open gets the short wait: one that heard
                // no audio since the last final or `Finalize`, or whose last word was a final
                // turn with no partial after it (the engine forwards silence too, so "audio
                // since the final" is true again a frame later; the model ended that turn by
                // itself and the VAD stop is merely late).
                let nothing_open = !self.audio_since_final || self.final_delivered;
                let wait = if self.assembler.is_idle() && nothing_open {
                    self.cfg().finalize_idle_timeout
                } else {
                    self.cfg().finalize_timeout
                };
                self.audio_since_final = false;
                self.final_delivered = false;
                self.assembler.finalize_requested();
                self.finalize_at = Some(self.ring.total());
                // The bound runs from the request, not from the moment the message goes out: a
                // reconnect or a replay in between must not stretch the caller's wait.
                self.finalize_deadline = Some(deadline_after(self.clock().now(), wait));
                Ok(())
            }
        }
    }

    /// Pads the held audio remainder with silence and moves it into the ring. Only used where
    /// the audio has to go out now (a `Finalize`, the optional `batch_flush`): padding adds up to
    /// 49 ms of audio the caller never produced.
    fn flush_batch(&mut self) {
        let mut chunks = std::mem::take(&mut self.chunks);
        self.batcher.flush_padded(&mut chunks);
        for chunk in chunks.drain(..) {
            self.ring.push(chunk);
        }
        self.chunks = chunks;
    }

    /// Sends what is due from the ring (and a queued `ForceEndpoint` at its position).
    async fn pump(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        loop {
            let front = self.ring.front();
            if front > self.conn.sent_abs {
                // The ring evicted audio this connection was never given (a replay that lags
                // behind the live audio): the server's clock does not run over that gap, so the
                // word times it reports must be mapped past it.
                self.conn.base_abs = self
                    .conn
                    .base_abs
                    .saturating_add(front - self.conn.sent_abs);
                self.conn.sent_abs = front;
            }
            if let Some(at) = self.finalize_at
                && self.conn.sent_abs >= at
            {
                self.write(ws, Message::text(FORCE_ENDPOINT)).await?;
                let now = self.clock().now();
                self.finalize_at = None;
                // A Finalize that was already given up on (synthetic answer) goes out all the
                // same, to flush the audio; its late answer is handled as such.
                if self.assembler.finalize_pending() {
                    self.finalize_sent_abs = Some(at);
                }
                self.conn.last_tx = now;
                self.conn.anchor.get_or_insert(now);
                continue;
            }
            if self.conn.sent_abs >= self.ring.total() {
                self.conn.next_send_at = None;
                return Ok(());
            }
            let now = self.clock().now();
            if self.conn.next_send_at.is_some_and(|t| t > now) {
                return Ok(());
            }
            let Some((start, chunk)) = self.ring.chunk_from(self.conn.sent_abs) else {
                self.conn.sent_abs = self.ring.total();
                continue;
            };
            let len = chunk.len() as u64;
            self.write(ws, Message::Binary(chunk)).await?;
            let sent_at = self.clock().now();
            self.conn.sent_abs = start.saturating_add(len);
            self.conn.audio_started = true;
            self.conn.paused = false;
            self.conn.last_tx = sent_at;
            self.conn.anchor.get_or_insert(sent_at);
            self.conn.next_send_at = if self.conn.sent_abs < self.ring.total() {
                // Catching up: pace the replay.
                let secs =
                    len as f64 / (self.p.bytes_per_second.max(1) as f64 * self.cfg().replay_speed);
                let base = self.conn.next_send_at.map_or(now, |t| t.max(now));
                Some(deadline_after(
                    base,
                    Duration::try_from_secs_f64(secs).unwrap_or(Duration::ZERO),
                ))
            } else {
                None
            };
        }
    }

    /// Writes one message within the write deadline.
    async fn write(&mut self, ws: &mut Ws, message: Message) -> Result<(), Stop> {
        match timed(
            self.clock(),
            self.cfg().io_timeout,
            "assemblyai write",
            ws.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::Drop("write failed")),
            Err(_) => Err(Stop::Drop("write timed out")),
        }
    }

    /// No audio for `keepalive_interval`: the line is quiet, so the server owes nothing until
    /// audio resumes, and with `inactivity_timeout` set a `KeepAlive` keeps the session open.
    async fn on_quiet(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        if self.cfg().inactivity_timeout.is_some() {
            self.write(ws, Message::text(KEEP_ALIVE)).await?;
        }
        self.conn.paused = true;
        self.conn.anchor = None;
        self.conn.last_tx = self.clock().now();
        Ok(())
    }

    /// The server said nothing for `io_timeout` although audio was given to the session. With
    /// heartbeats that is a lost connection. Without them silence is normal (turns only come
    /// with speech), so a WebSocket `Ping` asks for any sign of life first.
    async fn on_idle(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        if self.cfg().heartbeat || self.conn.pinged {
            return Err(Stop::Silent);
        }
        self.write(ws, Message::Ping(Bytes::new())).await?;
        self.conn.pinged = true;
        self.conn.anchor = Some(self.clock().now());
        Ok(())
    }

    // -- server messages ---------------------------------------------------------------------

    async fn on_frame(
        &mut self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<(), Stop> {
        use tokio_tungstenite::tungstenite::Error as WsError;
        if matches!(frame, Some(Ok(_))) {
            // Any frame is a sign of life.
            self.conn.pinged = false;
            self.conn.anchor = if self.conn.paused || !self.conn.audio_started {
                None
            } else {
                Some(self.clock().now())
            };
        }
        match frame {
            None => Err(Stop::Drop("socket closed")),
            Some(Err(WsError::Capacity(_) | WsError::Utf8(_) | WsError::AttackAttempt)) => {
                Err(Stop::Fatal(Error::Protocol {
                    provider: PROVIDER,
                    detail: "invalid frame from the server".to_owned(),
                }))
            }
            Some(Err(_)) => Err(Stop::Drop("socket error")),
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()).await,
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = frame.map_or((1005_u16, String::new()), |f| {
                    (u16::from(f.code), f.reason.to_string())
                });
                tracing::debug!(code, "assemblyai closed the session");
                match classify_close(code, &reason) {
                    Closed::Rejected(status) => Err(Stop::Fatal(Error::Rejected {
                        provider: PROVIDER,
                        status,
                    })),
                    Closed::Protocol(code) => Err(Stop::Fatal(Error::Protocol {
                        provider: PROVIDER,
                        detail: format!("the server closed the session with code {code}"),
                    })),
                    Closed::Retry => Err(Stop::Drop("closed by the server")),
                }
            }
            Some(Ok(_)) => Ok(()),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), Stop> {
        let now = self.clock().now();
        let incoming = protocol::parse(text).map_err(|e| {
            Stop::Fatal(Error::Protocol {
                provider: PROVIDER,
                detail: e.0.to_owned(),
            })
        })?;
        let was_pending = self.assembler.finalize_pending();
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        match incoming {
            Incoming::Turn(turn) => {
                self.attempts = 0;
                let ended = self.assembler.on_turn(&turn, now, &mut out);
                self.track_turn(ended);
                if ended {
                    self.audio_since_final = false;
                    // The turn is over: the audio up to its last word is not to be replayed. A
                    // final without usable word times (the docs show turns without `words`)
                    // ends at the latest where the audio sent so far ends, or at the
                    // `ForceEndpoint` that asked for it: replaying a delivered turn would
                    // deliver it twice.
                    let pos = match turn.last_word_end_ms {
                        Some(ms) => self.abs_of_ms(ms),
                        None => self
                            .finalize_sent_abs
                            .filter(|_| was_pending)
                            .unwrap_or(self.conn.sent_abs),
                    };
                    self.ring.trim_to(pos);
                }
            }
            Incoming::Heartbeat => self.attempts = 0,
            Incoming::Termination => {
                self.scratch = out;
                return Err(Stop::Drop("terminated by the server"));
            }
            Incoming::Error { description } => {
                // Always followed by a close whose code decides what it means.
                tracing::warn!("assemblyai reported an error");
                tracing::debug!(description = %description, "assemblyai error description");
            }
            Incoming::Begin { .. } | Incoming::Ignored => {}
        }
        if was_pending && !self.assembler.finalize_pending() {
            self.finalize_answered();
        }
        self.deliver(out).await
    }

    /// Keeps `turn_anchor` and `final_delivered` in step with the assembler after a `Turn`.
    fn track_turn(&mut self, ended: bool) {
        if ended {
            self.final_delivered = true;
        }
        if self.assembler.has_interim() {
            self.final_delivered = false;
            self.turn_anchor.get_or_insert(self.conn.sent_abs);
        } else {
            self.turn_anchor = None;
        }
    }

    /// Absolute stream position of `ms` milliseconds on the current connection.
    fn abs_of_ms(&self, ms: f64) -> u64 {
        let bytes = (ms / 1000.0 * self.p.bytes_per_second as f64) as u64;
        self.conn
            .base_abs
            .saturating_add(bytes)
            .min(self.conn.sent_abs)
    }

    /// The pending `Finalize` was answered. The server has ended the turn at the position the
    /// `ForceEndpoint` was sent, so audio before it is never replayed. (A queued `ForceEndpoint`
    /// that has not gone out yet still goes out: it flushes whatever the answer did not cover.)
    fn finalize_answered(&mut self) {
        self.finalize_deadline = None;
        if let Some(at) = self.finalize_sent_abs.take() {
            self.ring.trim_to(at);
        }
    }

    /// The `Finalize` got no answer in time: end the utterance synthetically. A `ForceEndpoint`
    /// that is still queued (a reconnect and a replay are in the way) stays queued: when it goes
    /// out, its answer is real text the synthetic final lacked and arrives as a late utterance.
    async fn on_finalize_timeout(&mut self) -> Result<(), Stop> {
        self.finalize_deadline = None;
        if !self.assembler.finalize_pending() {
            return Ok(());
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let now = self.clock().now();
        self.assembler.finalize_timed_out(now, &mut out);
        self.turn_anchor = None;
        if let Some(at) = self.finalize_sent_abs.take() {
            // The turn is over: that audio is not to be replayed into a new connection.
            self.ring.trim_to(at);
        }
        self.deliver(out).await
    }

    // -- delivery ----------------------------------------------------------------------------

    /// Hands events to the caller. Overflow policy: an interim that finds the channel full is
    /// dropped (the next one supersedes it); finals and errors wait, bounded by `io_timeout`, and
    /// a caller that stalls longer is treated as gone.
    async fn deliver(&mut self, mut out: Vec<SttEvent>) -> Result<(), Stop> {
        let mut result = Ok(());
        for event in out.drain(..) {
            if result.is_ok() {
                result = self.send_event(event).await;
            }
        }
        self.scratch = out;
        result
    }

    async fn send_event(&mut self, event: SttEvent) -> Result<(), Stop> {
        let Some(tx) = &self.events else {
            return Err(Stop::HangUp);
        };
        if matches!(
            event,
            SttEvent::Transcript {
                is_final: false,
                ..
            }
        ) {
            return match tx.try_send(event) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("assemblyai interim dropped: the events channel is full");
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(Stop::HangUp),
            };
        }
        match timed(
            &*self.p.clock,
            self.p.cfg.io_timeout,
            "assemblyai event delivery",
            tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::HangUp),
            Err(_) => {
                tracing::warn!(
                    "assemblyai events not consumed within the deadline; ending session"
                );
                Err(Stop::HangUp)
            }
        }
    }

    // -- ending ------------------------------------------------------------------------------

    /// Delivers `error` as the last event and ends the session.
    async fn fail(&mut self, error: Error) {
        tracing::debug!(%error, "assemblyai session failed");
        let _ = self.send_event(SttEvent::Error(error)).await;
        self.events = None;
    }

    /// Tells the server to end the session, if the socket still takes it within a short bound,
    /// and does not wait for the answer. Used where the connection is dropped while it may still
    /// be open on the server's side (billing is by connection time).
    async fn terminate_best_effort(&self, ws: &mut Ws) {
        let bound = self.cfg().close_timeout.min(TERMINATE_BOUND);
        let _ = timed(
            self.clock(),
            bound,
            "assemblyai terminate",
            ws.send(Message::text(TERMINATE)),
        )
        .await;
    }

    /// Caller hang-up: the events close first, then the server is told to stop and drained until
    /// its `Termination` (closing earlier would lose the last transcript, which nobody wants
    /// any more, but it would also leave the session billed until the server notices).
    async fn hang_up(&mut self, mut ws: Ws) {
        self.events = None;
        let close = async {
            if ws.send(Message::text(TERMINATE)).await.is_err() {
                return;
            }
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Close(_) => break,
                    Message::Text(text)
                        if matches!(protocol::parse(text.as_str()), Ok(Incoming::Termination)) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        };
        let _ = timed(
            &*self.p.clock,
            self.p.cfg.close_timeout,
            "assemblyai close",
            close,
        )
        .await;
    }

    // -- reconnect ---------------------------------------------------------------------------

    /// Opens a new connection (bounded attempts) while still taking audio and finalize requests
    /// from the caller.
    async fn reconnect(&mut self) -> Result<Ws, Stop> {
        loop {
            if self.attempts >= self.cfg().max_reconnects {
                let error = if self.silent {
                    Error::Core(brisvox_core::Error::Timeout {
                        what: "assemblyai response",
                        after: self.cfg().io_timeout,
                    })
                } else {
                    Error::Disconnected { provider: PROVIDER }
                };
                return Err(Stop::Fatal(error));
            }
            let backoff = backoff_for(self.cfg().reconnect_backoff, self.attempts);
            self.attempts += 1;
            let p = self.p.clone();
            self.absorb(async { p.clock.sleep(backoff).await }).await?;
            let p = self.p.clone();
            let opened = self
                .absorb(async { open(&p.cfg, &*p.clock, &p.target).await })
                .await?;
            match opened {
                Ok(ws) => {
                    self.on_reconnected();
                    return Ok(ws);
                }
                // Credentials, billing and permission problems do not heal by retrying.
                Err(
                    error @ Error::Rejected {
                        status: 401..=403 | 1008,
                        ..
                    },
                ) => return Err(Stop::Fatal(error)),
                Err(error) => {
                    // Whatever the old connection did, the trouble is connecting now.
                    self.silent = false;
                    tracing::debug!(%error, "assemblyai reconnect attempt failed");
                }
            }
        }
    }

    /// Runs `future` while taking caller input into the ring and keeping the `Finalize` bound.
    async fn absorb<T>(&mut self, future: impl std::future::Future<Output = T>) -> Result<T, Stop> {
        tokio::pin!(future);
        loop {
            let finalize_due = self.finalize_deadline;
            tokio::select! {
                biased;
                () = events_closed(&self.events) => return Err(Stop::HangUp),
                out = &mut future => return Ok(out),
                () = sleep_until_opt(&*self.p.clock, finalize_due) => {
                    self.on_finalize_timeout().await?;
                }
                message = self.input.recv() => match message {
                    None => return Err(Stop::HangUp),
                    Some(message) => self.on_input(message).await?,
                },
            }
        }
    }

    /// Resets per-connection state for a fresh connection and picks where its replay starts.
    ///
    /// Only audio that can still matter is replayed. Audio the old connection never received
    /// (queued after its last write, or left behind by a lagging replay) always is. Of what it
    /// did receive, only the turn in progress is: from [`TURN_PRE_ROLL`] before its first
    /// partial, or before the `ForceEndpoint` position of a pending `Finalize`. With neither, the
    /// ring holds silence (a caller who listens while the agent talks) and the replay starts
    /// [`REPLAY_PRE_ROLL`] before the old send position: replaying seconds of silence at
    /// `replay_speed` would leave every later word, and every later `ForceEndpoint`, that much
    /// late for the rest of the call.
    fn on_reconnected(&mut self) {
        let now = self.clock().now();
        let old_sent = self.conn.sent_abs;
        let mut start = old_sent.saturating_sub(self.audio_bytes(REPLAY_PRE_ROLL));
        let turn_pre_roll = self.audio_bytes(TURN_PRE_ROLL);
        if let Some(anchor) = self.turn_anchor {
            start = start.min(anchor.saturating_sub(turn_pre_roll));
        }
        if self.assembler.finalize_pending()
            && let Some(at) = self.finalize_sent_abs.or(self.finalize_at)
        {
            start = start.min(at.saturating_sub(turn_pre_roll));
        }
        // The replay is made of whole chunks: begin at the boundary at or before `start`.
        let base = self
            .ring
            .chunk_from(start)
            .map_or_else(|| self.ring.total(), |(chunk_start, _)| chunk_start);
        self.ring.trim_to(base);
        self.conn = ConnState::new(self.ring.front(), now);
        self.assembler.connection_replaced();
        self.turn_anchor = None;
        self.final_delivered = false;
        if self.assembler.finalize_pending() {
            // The old connection may have taken the ForceEndpoint with it: ask again at its
            // position. The deadline is kept: the caller's wait is bounded from its request.
            if let Some(at) = self.finalize_sent_abs.take() {
                self.finalize_at = Some(at);
            }
        }
    }

    /// Bytes of audio in `span`.
    fn audio_bytes(&self, span: Duration) -> u64 {
        let bytes = span.as_secs_f64() * self.p.bytes_per_second as f64;
        if bytes.is_finite() && bytes > 0.0 {
            bytes as u64
        } else {
            0
        }
    }
}

/// Completes when the caller dropped the events receiver (or the session has none).
async fn events_closed(events: &Option<mpsc::Sender<SttEvent>>) {
    match events {
        Some(tx) => tx.closed().await,
        None => std::future::pending().await,
    }
}

/// Sleeps until `deadline`; never completes for `None`.
async fn sleep_until_opt(clock: &dyn Clock, deadline: Option<Instant>) {
    match deadline {
        Some(at) => wait_until(clock, at).await,
        None => std::future::pending().await,
    }
}

/// The wait before reconnect attempt number `attempts` (0 for the first): `base * 2^attempts`,
/// never more than [`MAX_BACKOFF`].
fn backoff_for(base: Duration, attempts: u32) -> Duration {
    let factor = 1_u32.checked_shl(attempts).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reconnect_backoff_doubles_and_is_capped() {
        let base = Duration::from_millis(250);
        let waits: Vec<_> = (0..6).map(|n| backoff_for(base, n)).collect();
        assert_eq!(
            waits,
            [
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                MAX_BACKOFF,
            ]
        );
        // Absurd attempt counts and bases saturate instead of overflowing.
        assert_eq!(backoff_for(base, 64), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::MAX, 3), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::ZERO, 3), Duration::ZERO);
    }
}
