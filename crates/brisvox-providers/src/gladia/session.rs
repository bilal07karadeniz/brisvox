//! The task that owns one Gladia session: audio out, transcripts in, silence keepalive, finalize,
//! reconnect with replay, hang-up.
//!
//! One task, one `select!` loop, no locks. Order of events matters, so everything that touches
//! the socket happens in this task:
//!
//! - **Audio in.** Every chunk is appended to the [`ReplayRing`] and then sent from the ring
//!   (`pump`) as one binary frame. In steady state the ring holds only audio that has not become
//!   a final transcript yet.
//! - **Transcripts in.** Messages are mapped by the [`Assembler`]; every final also trims the
//!   ring up to the end of the utterance. `utterance.end` is seconds on the session's own
//!   timeline, which holds generated silence and does not hold audio that was skipped before it
//!   was sent; a [`Timeline`] records both as they happen and maps the position back to the
//!   caller's stream.
//! - **Finalize.** Gladia has no flush: a request is answered by the assembler, from Gladia's own
//!   final or, after `finalize_timeout` counted from the request (also across a reconnect), from
//!   the last partial. Nothing is sent to the server for it.
//! - **Keepalive.** Gladia has no keepalive message and closes a socket without audio after about
//!   30 s: after `keepalive_interval` without audio the task sends 20 ms of digital silence.
//! - **Liveness** (`liveness_ping`, on by default): after `keepalive_interval` without any frame
//!   from the server a WebSocket Ping goes out, and no frame at all within `io_timeout` counts
//!   the session as lost. This detects a half-open link (the kernel buffers writes for a long
//!   time) whatever the application protocol does, with acknowledgments on or off.
//! - **Watchdog** (only with acknowledgments on): once audio was sent the server must say
//!   something within `io_timeout`, else the session counts as lost. A lost session is replaced
//!   like any other drop and ends with `Core(Timeout)` when no reconnect is left.
//! - **Drops.** An unexpected close or I/O error opens a new session (a new init call; bounded
//!   attempts, backoff on the injected clock), replays the ring paced at `replay_speed`, and
//!   keeps a pending `Finalize` and its deadline. Audio and `Finalize` requests keep being
//!   accepted while reconnecting. A session that is replaced or failed while its socket may
//!   still be alive is told `stop_recording` and closed first (bounded by `close_timeout`):
//!   Gladia counts live sessions against the plan's concurrency limit (1 on the free plan) and
//!   would answer the new init call with 429 until the old one timed out.
//! - **Hang-up.** Dropping the input (or the events receiver) ends the session: the events close
//!   at once, then `stop_recording` is sent and the socket is drained for at most
//!   `close_timeout`. Nothing produced after the hang-up reaches the caller.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Clock;
use brisvox_core::clock::{deadline_after, wait_until};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::PROVIDER;
use super::assembler::{Assembler, FinalizeStart, InFlight};
use super::config::GladiaConfig;
use super::connection::{Shared, Ws, timed};
use super::protocol::{self, AckKind, Incoming, STOP_RECORDING};
use super::replay::ReplayRing;
use crate::error::Error;
use crate::stt::{SttConfig, SttEvent, SttInput};

/// Length of the silence sent to a session that gets no audio.
const SILENCE: Duration = Duration::from_millis(20);

/// Largest reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// A session that lived this long counts as healthy: its drop does not use up the attempts.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Everything a session needs that does not change.
pub(crate) struct Params {
    /// Settings, clock and HTTP client.
    pub(crate) shared: Arc<Shared>,
    /// What the caller asked for (sample rate, language); every reconnect repeats it.
    pub(crate) stt: SttConfig,
    /// Audio bytes per second of this session.
    pub(crate) bytes_per_second: u64,
}

/// Why `serve` or `absorb` stopped.
#[derive(Debug)]
enum Stop {
    /// The caller left (input or events dropped) or stopped consuming.
    HangUp,
    /// The connection was lost; reconnecting may help. `usable` tells whether the socket may
    /// still work (a server-side error message, for one), so that the old session is stopped
    /// before the new one is opened; a failed write or a close from the peer leaves nothing to
    /// stop.
    Drop {
        /// Why, for the log.
        reason: &'static str,
        /// The old socket may still carry a `stop_recording`.
        usable: bool,
    },
    /// The server gave no sign of life for `io_timeout` (no answer to the ping, or no message
    /// after audio was sent with acknowledgments on): lost like `Drop`, but if no reconnect is
    /// left the error is `Core(Timeout)`.
    Silent,
    /// The session failed; the error is delivered and the session ends.
    Fatal(Error),
}

/// Most runs a [`Timeline`] keeps (older ones are forgotten: they are far behind the ring).
const MAX_RUNS: usize = 4096;

/// A stretch of the caller's audio that was sent on a session without a break.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// Byte position on the session's timeline of the first byte.
    session_start: u64,
    /// Byte position in the caller's stream of the first byte.
    stream_start: u64,
    /// Length in bytes.
    len: u64,
}

/// Maps positions on a session's timeline (what Gladia's timestamps count) to positions in the
/// caller's stream (what the replay ring is addressed by).
///
/// The two differ by every stretch the session timeline has and the stream does not (generated
/// keepalive silence), and by every stretch the stream has and was never sent on the session
/// (audio evicted from the ring before it went out). Each is recorded at the moment it happens,
/// so a mapping is exact for the audio sent before it and does not move when later silence or
/// gaps are added.
#[derive(Debug, Default)]
struct Timeline {
    /// Runs of real audio in session order. A hole between `session_start` and the end of the
    /// previous run is generated silence; a jump in `stream_start` is skipped audio.
    runs: VecDeque<Run>,
    /// Bytes sent on the session so far, real and generated.
    sent: u64,
}

impl Timeline {
    /// `len` bytes of the stream starting at `stream_start` were sent.
    fn real(&mut self, stream_start: u64, len: u64) {
        match self.runs.back_mut() {
            Some(run)
                if run.session_start.saturating_add(run.len) == self.sent
                    && run.stream_start.saturating_add(run.len) == stream_start =>
            {
                run.len = run.len.saturating_add(len);
            }
            _ => {
                if self.runs.len() >= MAX_RUNS {
                    self.runs.pop_front();
                }
                self.runs.push_back(Run {
                    session_start: self.sent,
                    stream_start,
                    len,
                });
            }
        }
        self.sent = self.sent.saturating_add(len);
    }

    /// `len` bytes of generated silence were sent.
    fn silence(&mut self, len: u64) {
        self.sent = self.sent.saturating_add(len);
    }

    /// The stream position that session position `session_pos` stands for: inside generated
    /// silence it is the end of the audio before it. `None` if it lies before every run kept.
    fn to_stream(&self, session_pos: u64) -> Option<u64> {
        let after = self
            .runs
            .partition_point(|r| r.session_start <= session_pos);
        let run = self.runs.get(after.checked_sub(1)?)?;
        Some(
            run.stream_start
                .saturating_add(session_pos.saturating_sub(run.session_start).min(run.len)),
        )
    }

    /// Forgets runs that end before stream position `pos` (the ring no longer holds them).
    fn forget_before(&mut self, pos: u64) {
        while self.runs.len() > 1
            && self
                .runs
                .front()
                .is_some_and(|r| r.stream_start.saturating_add(r.len) <= pos)
        {
            self.runs.pop_front();
        }
    }
}

/// State of the current session.
struct ConnState {
    /// Absolute stream position up to which audio was sent on this session.
    sent_abs: u64,
    /// Session timeline against the stream (see [`Timeline`]).
    timeline: Timeline,
    /// Last time any frame (including a Pong) came from the server.
    last_rx: Instant,
    /// When the unanswered liveness Ping was sent.
    ping_out: Option<Instant>,
    /// Audio (real or generated) was sent on this session.
    audio_started: bool,
    /// Last time audio was sent.
    last_tx: Instant,
    /// Start of the current wait for a server message; `None` when none is owed.
    anchor: Option<Instant>,
    /// Earliest time the next paced (replayed) chunk may go out.
    next_send_at: Option<Instant>,
    /// When the session was opened.
    opened_at: Instant,
}

impl ConnState {
    fn new(base_abs: u64, now: Instant) -> Self {
        Self {
            sent_abs: base_abs,
            timeline: Timeline::default(),
            last_rx: now,
            ping_out: None,
            audio_started: false,
            last_tx: now,
            anchor: None,
            next_send_at: None,
            opened_at: now,
        }
    }
}

struct Session {
    p: Params,
    input: mpsc::Receiver<SttInput>,
    /// `None` once the session ended: the caller then sees the channel close.
    events: Option<mpsc::Sender<SttEvent>>,
    ring: ReplayRing,
    assembler: Assembler,
    conn: ConnState,
    /// Stream position at which the open `Finalize` was requested.
    finalize_total: Option<u64>,
    /// When the unanswered `Finalize` is given up on.
    finalize_deadline: Option<Instant>,
    /// Reconnect attempts used since a session proved itself (a transcript or an
    /// acknowledgment arrived, or it lived [`STABLE_AFTER`]). A server that accepts, says nothing
    /// useful and closes, over and over, must not be reconnected forever: every attempt costs
    /// an init call and a concurrency slot.
    attempts: u32,
    /// The session that was just lost was lost to silence (see [`Stop::Silent`]).
    silent: bool,
    /// A replacement session is being opened: audio and `Finalize` requests are taken, but no
    /// server has heard the unfinalized audio yet.
    reconnecting: bool,
    /// The error of the last failed reconnect attempt, reported when the attempts run out.
    last_connect_error: Option<Error>,
    /// Reused output buffer of the assembler.
    scratch: Vec<SttEvent>,
}

/// Runs a session on an open connection until it ends.
pub(crate) async fn run(
    params: Params,
    ws: Ws,
    input: mpsc::Receiver<SttInput>,
    events: mpsc::Sender<SttEvent>,
) {
    let cfg = &params.shared.cfg;
    let window = cfg.replay_window.as_secs_f64() * params.bytes_per_second as f64;
    let capacity = if window.is_finite() && window > 0.0 {
        window as u64
    } else {
        0
    };
    let now = params.shared.clock().now();
    let session = Session {
        ring: ReplayRing::new(capacity, cfg.encoding.bytes_per_sample()),
        p: params,
        input,
        events: Some(events),
        assembler: Assembler::default(),
        conn: ConnState::new(0, now),
        finalize_total: None,
        finalize_deadline: None,
        attempts: 0,
        silent: false,
        reconnecting: false,
        last_connect_error: None,
        scratch: Vec::new(),
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
                    // The caller hears the error first; the server is told afterwards.
                    self.fail(error).await;
                    self.retire(ws).await;
                    return;
                }
                stop @ (Stop::Drop { .. } | Stop::Silent) => {
                    let (reason, usable) = match stop {
                        Stop::Drop { reason, usable } => (reason, usable),
                        _ => ("no sign of life from the server", true),
                    };
                    self.silent = matches!(stop, Stop::Silent);
                    tracing::debug!(reason, "gladia session lost");
                    if usable {
                        self.retire(ws).await;
                    } else {
                        drop(ws);
                    }
                    let lived = self
                        .clock()
                        .now()
                        .saturating_duration_since(self.conn.opened_at);
                    if lived >= STABLE_AFTER {
                        self.attempts = 0;
                    }
                    self.reconnecting = true;
                    let reconnected = self.reconnect().await;
                    self.reconnecting = false;
                    match reconnected {
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

    fn cfg(&self) -> &GladiaConfig {
        &self.p.shared.cfg
    }

    fn clock(&self) -> &dyn Clock {
        self.p.shared.clock()
    }

    // -- main loop ---------------------------------------------------------------------------

    async fn serve(&mut self, ws: &mut Ws) -> Stop {
        if let Err(stop) = self.pump(ws).await {
            return stop;
        }
        loop {
            let keepalive_due = self.conn.last_tx + self.cfg().effective_keepalive();
            let idle_due = if self.cfg().acknowledgments {
                self.conn
                    .anchor
                    .map(|a| deadline_after(a, self.cfg().io_timeout))
            } else {
                None
            };
            let ping_due = (self.cfg().liveness_ping && self.conn.ping_out.is_none())
                .then(|| self.conn.last_rx + self.cfg().effective_keepalive());
            let pong_due = self
                .conn
                .ping_out
                .map(|at| deadline_after(at, self.cfg().io_timeout));
            let pace_due = self.conn.next_send_at;
            let finalize_due = self.finalize_deadline;
            let step: Result<(), Stop> = tokio::select! {
                biased;
                () = events_closed(&self.events) => Err(Stop::HangUp),
                frame = ws.next() => self.on_frame(frame).await,
                () = sleep_until_opt(self.p.shared.clock(), finalize_due) => self.on_finalize_timeout().await,
                () = sleep_until_opt(self.p.shared.clock(), idle_due) => Err(Stop::Silent),
                () = sleep_until_opt(self.p.shared.clock(), pong_due) => Err(Stop::Silent),
                () = sleep_until_opt(self.p.shared.clock(), ping_due) => self.on_ping(ws).await,
                () = wait_until(self.p.shared.clock(), keepalive_due) => self.on_keepalive(ws).await,
                () = sleep_until_opt(self.p.shared.clock(), pace_due) => self.pump(ws).await,
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

    /// Takes one caller message: audio goes into the ring, a finalize request is registered (or,
    /// if nothing is open or in flight, answered at once).
    async fn on_input(&mut self, message: SttInput) -> Result<(), Stop> {
        match message {
            SttInput::Audio(bytes) => {
                self.ring.push(bytes);
                Ok(())
            }
            SttInput::Finalize => {
                let mut out = std::mem::take(&mut self.scratch);
                out.clear();
                let in_flight = InFlight {
                    audio_unfinalized: self.ring.len() > 0,
                    reconnecting: self.reconnecting,
                };
                match self.assembler.finalize_requested(in_flight, &mut out) {
                    FinalizeStart::Waiting => {
                        self.scratch = out;
                        self.finalize_total = Some(self.ring.total());
                        // The bound runs from the request: a reconnect or a replay in between
                        // must not stretch the caller's wait.
                        self.finalize_deadline = Some(deadline_after(
                            self.clock().now(),
                            self.cfg().finalize_timeout,
                        ));
                        Ok(())
                    }
                    FinalizeStart::Answered => self.deliver(out).await,
                    FinalizeStart::AlreadyPending => {
                        self.scratch = out;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Sends what is due from the ring.
    async fn pump(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        loop {
            // Audio the ring evicted before it went out is skipped: the session never hears it.
            self.conn.sent_abs = self.conn.sent_abs.max(self.ring.front());
            self.conn.timeline.forget_before(self.ring.front());
            if self.conn.sent_abs >= self.ring.total() {
                self.conn.next_send_at = None;
                return Ok(());
            }
            let now = self.clock().now();
            if self.conn.next_send_at.is_some_and(|t| t > now) {
                return Ok(());
            }
            let Some(chunk) = self.ring.chunk_from(self.conn.sent_abs) else {
                self.conn.sent_abs = self.ring.total();
                continue;
            };
            let len = chunk.len() as u64;
            self.write(ws, Message::Binary(chunk)).await?;
            let sent_at = self.clock().now();
            self.conn.timeline.real(self.conn.sent_abs, len);
            self.conn.sent_abs = self.conn.sent_abs.saturating_add(len);
            self.conn.audio_started = true;
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
            "gladia write",
            ws.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::Drop {
                reason: "write failed",
                usable: false,
            }),
            Err(_) => Err(Stop::Drop {
                reason: "write timed out",
                usable: false,
            }),
        }
    }

    /// No audio for the keepalive interval: 20 ms of digital silence (Gladia wants audio, not a
    /// control message).
    async fn on_keepalive(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        let samples = self.p.bytes_per_second as f64 * SILENCE.as_secs_f64();
        let silence = vec![self.cfg().encoding.silence_byte(); samples as usize];
        let len = silence.len() as u64;
        self.write(ws, Message::Binary(Bytes::from(silence)))
            .await?;
        self.conn.timeline.silence(len);
        self.conn.audio_started = true;
        let now = self.clock().now();
        self.conn.last_tx = now;
        self.conn.anchor.get_or_insert(now);
        Ok(())
    }

    /// Nothing came from the server for the keepalive interval: a WebSocket Ping. Any frame in
    /// answer (the Pong, or a message) within `io_timeout` proves the link; none ends the
    /// session as [`Stop::Silent`].
    async fn on_ping(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        self.write(ws, Message::Ping(Bytes::new())).await?;
        self.conn.ping_out = Some(self.clock().now());
        Ok(())
    }

    // -- server messages ---------------------------------------------------------------------

    async fn on_frame(
        &mut self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<(), Stop> {
        use tokio_tungstenite::tungstenite::Error as WsError;
        if matches!(frame, Some(Ok(_))) {
            // Any frame proves the link, a Pong included.
            self.conn.last_rx = self.clock().now();
            self.conn.ping_out = None;
        }
        match frame {
            None => Err(Stop::Drop {
                reason: "socket closed",
                usable: false,
            }),
            Some(Err(WsError::Capacity(_) | WsError::Utf8(_) | WsError::AttackAttempt)) => {
                Err(Stop::Fatal(Error::Protocol {
                    provider: PROVIDER,
                    detail: "invalid frame from the server".to_owned(),
                }))
            }
            Some(Err(_)) => Err(Stop::Drop {
                reason: "socket error",
                usable: false,
            }),
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()).await,
            Some(Ok(Message::Close(frame))) => {
                // The reference says: 1000 is intentional, any other code means reconnect. A 1000
                // in the middle of a call is not intentional on our side either.
                tracing::debug!(
                    code = frame.as_ref().map(|f| u16::from(f.code)),
                    "gladia closed the session"
                );
                Err(Stop::Drop {
                    reason: "closed by the server",
                    usable: false,
                })
            }
            Some(Ok(_)) => Ok(()),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), Stop> {
        let now = self.clock().now();
        self.conn.anchor = if self.conn.audio_started {
            Some(now)
        } else {
            None
        };
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
            Incoming::Transcript(t) => {
                self.attempts = 0;
                self.last_connect_error = None;
                if t.is_final {
                    let end = self.end_abs(t.end);
                    self.ring.trim_to(end);
                }
                self.assembler.on_transcript(&t, &mut out);
            }
            Incoming::SpeechStart => self.assembler.speech_start(),
            Incoming::SpeechEnd => self.assembler.speech_end(),
            Incoming::Ack {
                kind: AckKind::Audio,
                acknowledged,
                error,
            } => {
                if acknowledged {
                    self.attempts = 0;
                    self.last_connect_error = None;
                } else {
                    // The server dropped audio it was sent: the declared format probably does not
                    // match the bytes, and a retry would fail the same way.
                    return Err(Stop::Fatal(Error::Protocol {
                        provider: PROVIDER,
                        detail: format!(
                            "the server did not accept the audio ({})",
                            error.as_deref().unwrap_or("no detail")
                        ),
                    }));
                }
            }
            Incoming::Ack {
                kind: AckKind::Stop,
                acknowledged,
                ..
            } => {
                tracing::debug!(acknowledged, "gladia answered stop_recording");
            }
            Incoming::SessionEnded => {
                return Err(Stop::Drop {
                    reason: "the server ended the session",
                    usable: false,
                });
            }
            Incoming::Error { status, detail } => {
                // A server-side failure may heal on a new session; a complaint about what the
                // adapter sent (4xx) will not.
                return Err(if status.is_some_and(|s| s >= 500) {
                    tracing::debug!(detail = %detail, "gladia reported a server error");
                    Stop::Drop {
                        reason: "server error",
                        usable: true,
                    }
                } else {
                    Stop::Fatal(Error::Protocol {
                        provider: PROVIDER,
                        detail: format!("server error {detail}"),
                    })
                });
            }
            Incoming::Ignored => {}
        }
        if was_pending && !self.assembler.finalize_pending() {
            self.finalize_deadline = None;
            self.finalize_total = None;
        }
        self.deliver(out).await
    }

    /// End of the audio an utterance that ends `end_secs` into the session covers, as an absolute
    /// stream position: timestamps count bytes of the session's timeline, which holds generated
    /// silence and lacks skipped audio, so the position goes through the [`Timeline`]. A position
    /// that cannot be mapped trims nothing (the audio is replayed after a drop, never lost).
    fn end_abs(&self, end_secs: f64) -> u64 {
        let session_pos = (end_secs * self.p.bytes_per_second as f64) as u64;
        self.conn
            .timeline
            .to_stream(session_pos)
            .unwrap_or(0)
            .min(self.conn.sent_abs)
    }

    /// The `Finalize` got no answer in time: end the utterance with the last partial. The turn is
    /// over, so its audio is not to be replayed into a new session.
    async fn on_finalize_timeout(&mut self) -> Result<(), Stop> {
        self.finalize_deadline = None;
        if !self.assembler.finalize_pending() {
            return Ok(());
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        self.assembler.finalize_timed_out(&mut out);
        if let Some(total) = self.finalize_total.take() {
            self.ring.trim_to(total);
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
                    tracing::debug!("gladia partial dropped: the events channel is full");
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(Stop::HangUp),
            };
        }
        match timed(
            &*self.p.shared.clock,
            self.p.shared.cfg.io_timeout,
            "gladia event delivery",
            tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::HangUp),
            Err(_) => {
                tracing::warn!("gladia events not consumed within the deadline; ending session");
                Err(Stop::HangUp)
            }
        }
    }

    // -- ending ------------------------------------------------------------------------------

    /// Delivers `error` as the last event and ends the session.
    async fn fail(&mut self, error: Error) {
        tracing::debug!(%error, "gladia session failed");
        let _ = self.send_event(SttEvent::Error(error)).await;
        self.events = None;
    }

    /// Ends a session the adapter is done with but the server may not know it: `stop_recording`
    /// and a close frame, bounded by `close_timeout`, without waiting for the server's answer.
    /// Best effort: a socket that does not take the writes is given up on at the deadline.
    async fn retire(&self, mut ws: Ws) {
        let stop = async {
            if ws.send(Message::text(STOP_RECORDING)).await.is_ok() {
                let _ = ws.close(None).await;
            }
        };
        let _ = timed(
            self.clock(),
            self.p.shared.cfg.close_timeout,
            "gladia retire",
            stop,
        )
        .await;
    }

    /// Caller hang-up: the events close first, then the server is told to stop.
    async fn hang_up(&mut self, mut ws: Ws) {
        self.events = None;
        let close = async {
            if ws.send(Message::text(STOP_RECORDING)).await.is_err() {
                return;
            }
            while let Some(Ok(message)) = ws.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        };
        let _ = timed(
            self.clock(),
            self.p.shared.cfg.close_timeout,
            "gladia close",
            close,
        )
        .await;
    }

    // -- reconnect ---------------------------------------------------------------------------

    /// Opens a new session (bounded attempts; each is an init call) while still taking audio and
    /// finalize requests from the caller.
    async fn reconnect(&mut self) -> Result<Ws, Stop> {
        loop {
            if self.attempts >= self.cfg().max_reconnects {
                let error = match self.last_connect_error.take() {
                    Some(error) => error,
                    None if self.silent => Error::Core(brisvox_core::Error::Timeout {
                        what: "gladia response",
                        after: self.cfg().io_timeout,
                    }),
                    None => Error::Disconnected { provider: PROVIDER },
                };
                return Err(Stop::Fatal(error));
            }
            let backoff = backoff_for(self.cfg().reconnect_backoff, self.attempts);
            self.attempts += 1;
            let shared = Arc::clone(&self.p.shared);
            self.absorb(async { shared.clock().sleep(backoff).await })
                .await?;
            let shared = Arc::clone(&self.p.shared);
            let stt = self.p.stt.clone();
            let opened = self.absorb(async { shared.open(&stt).await }).await?;
            match opened {
                Ok(ws) => {
                    self.on_reconnected();
                    return Ok(ws);
                }
                // Credentials, billing, permission and request problems do not heal by
                // retrying; 408 and 429 (concurrency limit) may.
                Err(
                    error @ Error::Rejected {
                        status: 400..=499, ..
                    },
                ) if !matches!(
                    error,
                    Error::Rejected {
                        status: 408 | 429,
                        ..
                    }
                ) =>
                {
                    return Err(Stop::Fatal(error));
                }
                Err(error) => {
                    tracing::debug!(%error, "gladia reconnect attempt failed");
                    // Whatever the old session did, the trouble is connecting now.
                    self.silent = false;
                    self.last_connect_error = Some(error);
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
                () = sleep_until_opt(self.p.shared.clock(), finalize_due) => {
                    self.on_finalize_timeout().await?;
                }
                message = self.input.recv() => match message {
                    None => return Err(Stop::HangUp),
                    Some(message) => self.on_input(message).await?,
                },
            }
        }
    }

    /// Resets per-session state for a fresh session that starts replaying at the ring.
    fn on_reconnected(&mut self) {
        let now = self.clock().now();
        self.conn = ConnState::new(self.ring.front(), now);
        self.assembler.connection_replaced();
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
    fn the_timeline_maps_session_positions_back_through_silence_and_gaps() {
        let mut t = Timeline::default();
        t.real(0, 3200); // session 0..3200 = stream 0..3200
        t.silence(640); // session 3200..3840 is generated
        t.real(3200, 3200); // session 3840..7040 = stream 3200..6400
        assert_eq!(t.to_stream(0), Some(0));
        assert_eq!(t.to_stream(1600), Some(1600));
        assert_eq!(t.to_stream(3200), Some(3200));
        assert_eq!(
            t.to_stream(3500),
            Some(3200),
            "inside the silence: its start"
        );
        assert_eq!(t.to_stream(3840), Some(3200));
        assert_eq!(t.to_stream(5440), Some(4800));
        assert_eq!(t.to_stream(99_999), Some(6400), "past the end: the end");
        // Silence sent later does not move what was sent before.
        t.silence(640);
        t.real(6400, 3200);
        assert_eq!(t.to_stream(5440), Some(4800));
        assert_eq!(t.to_stream(7040 + 640 + 1600), Some(6400 + 1600));
        // Audio skipped before it was sent: 9600 stream bytes were never on the session.
        t.real(16_000, 3200);
        let session_after = t.sent;
        assert_eq!(t.to_stream(session_after), Some(19_200));
        assert_eq!(t.to_stream(session_after - 1600), Some(17_600));
        assert_eq!(
            t.to_stream(session_after - 3200 - 1),
            Some(6400 + 3199),
            "before the gap the mapping is unchanged"
        );
    }

    #[test]
    fn the_timeline_forgets_runs_behind_the_ring_and_has_no_answer_before_its_runs() {
        let mut t = Timeline::default();
        t.real(0, 100);
        t.silence(10);
        t.real(100, 100);
        t.forget_before(100);
        assert_eq!(t.runs.len(), 1);
        assert_eq!(t.to_stream(50), None, "before the first run kept");
        assert_eq!(t.to_stream(150), Some(140));
        // The last run is always kept.
        t.forget_before(10_000);
        assert_eq!(t.runs.len(), 1);
        let mut empty = Timeline::default();
        assert_eq!(empty.to_stream(5), None);
        empty.forget_before(5);
    }

    #[test]
    fn the_timeline_is_bounded() {
        let mut t = Timeline::default();
        for i in 0..(MAX_RUNS as u64 + 10) {
            t.real(i * 100, 10);
        }
        assert_eq!(t.runs.len(), MAX_RUNS);
    }

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
        assert_eq!(backoff_for(base, 64), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::MAX, 3), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::ZERO, 3), Duration::ZERO);
    }
}
