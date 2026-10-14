//! The task that owns one Deepgram session: audio out, results in, keepalive, finalize,
//! reconnect with replay, hang-up.
//!
//! One task, one `select!` loop, no locks. Order of events matters, so everything that touches
//! the socket happens in this task:
//!
//! - **Audio in.** Every chunk is appended to the [`ReplayRing`] and then sent from the ring
//!   (`pump`). In steady state the ring holds only audio the server has not yet finalized.
//! - **Results in.** `Results` are mapped by the [`Assembler`]; every final result also trims the
//!   ring up to the end of the finalized audio (timestamps are relative to the connection and are
//!   offset by the connection's base position).
//! - **Watchdogs.** KeepAlive after `keepalive_interval` without audio; 20 ms of silence if a
//!   connection has seen no audio at all; the server must say something within `io_timeout`
//!   after audio was sent, else the connection counts as lost (a half-open socket or a server
//!   that stopped answering is the commonest real drop) and is replaced like any other, with
//!   `Core(Timeout)` as the final error when no reconnect is left; a `Finalize` is answered by
//!   Deepgram or ended synthetically after `finalize_timeout`, counted from the request (so the
//!   bound also holds while a reconnect and the replay are in the way).
//! - **Drops.** An unexpected close or I/O error reconnects (bounded attempts, backoff on the
//!   injected clock), replays the ring paced at `replay_speed`, and re-sends a pending
//!   `Finalize` at its original position. Audio and `Finalize` requests keep being accepted while
//!   reconnecting.
//! - **Hang-up.** Dropping the input (or the events receiver) ends the session: the events close
//!   at once, then `CloseStream` is sent and the socket drained for at most `close_timeout`.
//!   Nothing produced after the hang-up reaches the caller.

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
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::PROVIDER;
use super::assembler::Assembler;
use super::config::DeepgramConfig;
use super::connection::{Target, Ws, open, timed};
use super::protocol::{self, CLOSE_STREAM, FINALIZE, Incoming, KEEP_ALIVE, Results};
use super::replay::ReplayRing;
use crate::error::Error;
use crate::stt::{SttEvent, SttInput};

/// Length of the silence sent to a connection that has seen no audio.
const SILENCE: Duration = Duration::from_millis(20);

/// Largest reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How far short of a still-queued `Finalize` position a final may end and still answer it
/// (server timestamps are floats; this absorbs rounding, not unflushed speech).
const COVERS_QUEUED_TOLERANCE: Duration = Duration::from_millis(20);

/// Everything a session needs that does not change.
#[derive(Clone)]
pub(crate) struct Params {
    /// Adapter settings.
    pub(crate) cfg: Arc<DeepgramConfig>,
    /// Injected clock.
    pub(crate) clock: SharedClock,
    /// Where to connect.
    pub(crate) target: Target,
    /// Audio bytes per second of this session.
    pub(crate) bytes_per_second: u64,
}

/// Why `serve` or `absorb` stopped.
#[derive(Debug)]
enum Stop {
    /// The caller left (input or events dropped) or stopped consuming.
    HangUp,
    /// The connection was lost; reconnecting may help.
    Drop(&'static str),
    /// The server said nothing for `io_timeout` after audio was sent: lost like `Drop`, but if
    /// no reconnect is left the error is `Core(Timeout)`.
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
    /// Bytes of generated silence at the start of this connection (shift timestamps).
    synthetic: u64,
    /// Audio (real or generated) was sent on this connection.
    audio_started: bool,
    /// Last time anything was sent (audio or KeepAlive).
    last_tx: Instant,
    /// A KeepAlive was sent since the last audio: the server owes nothing until audio resumes.
    paused: bool,
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
            synthetic: 0,
            audio_started: false,
            last_tx: now,
            paused: false,
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
    assembler: Assembler,
    conn: ConnState,
    /// Absolute position at which a requested `Finalize` is to be sent.
    finalize_at: Option<u64>,
    /// Position at which the last `Finalize` was sent, until it is answered.
    finalize_sent_abs: Option<u64>,
    /// When the unanswered `Finalize` is given up on.
    finalize_deadline: Option<Instant>,
    /// Reconnect attempts used since the last `Results` message. Only a transcript message
    /// proves a connection works: a server that accepts, sends a `Metadata` and closes, over and
    /// over, must not be reconnected forever.
    attempts: u32,
    /// The connection that was just lost was lost to silence (see [`Stop::Silent`]).
    silent: bool,
    /// Absolute stream position up to which the server has reported processing audio on the
    /// current connection (end of the latest `Results`); `None` before the first one.
    processed_abs: Option<u64>,
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
    let window = params.cfg.replay_window.as_secs_f64() * params.bytes_per_second as f64;
    let capacity = if window.is_finite() && window > 0.0 {
        window as u64
    } else {
        0
    };
    let now = params.clock.now();
    let session = Session {
        ring: ReplayRing::new(capacity, params.cfg.encoding.bytes_per_sample()),
        p: params,
        input,
        events: Some(events),
        assembler: Assembler::default(),
        conn: ConnState::new(0, now),
        finalize_at: None,
        finalize_sent_abs: None,
        finalize_deadline: None,
        attempts: 0,
        silent: false,
        processed_abs: None,
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
                    self.fail(error).await;
                    return;
                }
                stop @ (Stop::Drop(_) | Stop::Silent) => {
                    let reason = match stop {
                        Stop::Drop(reason) => reason,
                        _ => "no message from the server",
                    };
                    self.silent = matches!(stop, Stop::Silent);
                    tracing::debug!(reason, "deepgram connection lost");
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

    fn cfg(&self) -> &DeepgramConfig {
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
            let keepalive_due = self.conn.last_tx + self.cfg().keepalive_interval;
            let idle_due = if self.conn.paused {
                None
            } else {
                self.conn
                    .anchor
                    .map(|a| deadline_after(a, self.cfg().io_timeout))
            };
            let pace_due = self.conn.next_send_at;
            let finalize_due = self.finalize_deadline;
            let step: Result<(), Stop> = tokio::select! {
                biased;
                () = events_closed(&self.events) => Err(Stop::HangUp),
                frame = ws.next() => self.on_frame(frame).await,
                () = sleep_until_opt(&*self.p.clock, finalize_due) => self.on_finalize_timeout().await,
                () = sleep_until_opt(&*self.p.clock, idle_due) => Err(Stop::Silent),
                () = wait_until(&*self.p.clock, keepalive_due) => self.on_keepalive(ws).await,
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

    /// Takes one caller message: audio goes into the ring, a finalize request is queued (or, if
    /// the server has nothing left to flush, answered at once).
    async fn on_input(&mut self, message: SttInput) -> Result<(), Stop> {
        match message {
            SttInput::Audio(bytes) => {
                self.ring.push(bytes);
                Ok(())
            }
            SttInput::Finalize => {
                // A second request while one is open adds nothing.
                if self.assembler.finalize_pending() {
                    return Ok(());
                }
                if self.settled() {
                    let mut out = std::mem::take(&mut self.scratch);
                    out.clear();
                    self.assembler.finalize_settled(&mut out);
                    return self.deliver(out).await;
                }
                self.assembler.finalize_requested();
                self.finalize_at = Some(self.ring.total());
                // The bound runs from the request, not from the moment the message goes out: a
                // reconnect or a replay in between must not stretch the caller's wait.
                self.finalize_deadline = Some(deadline_after(
                    self.clock().now(),
                    self.cfg().finalize_timeout,
                ));
                Ok(())
            }
        }
    }

    /// Whether a `Finalize` requested now has nothing to flush: no utterance in progress and the
    /// latest `Results` of this connection reaches to within `finalize_settle_gap` of the end of
    /// the audio (everything sent, including a replay backlog that is still to go out).
    fn settled(&self) -> bool {
        let gap = self.cfg().finalize_settle_gap;
        if gap.is_zero() || !self.assembler.is_idle() {
            return false;
        }
        let Some(done) = self.processed_abs else {
            return false;
        };
        self.ring.total().saturating_sub(done) <= self.bytes_in(gap)
    }

    /// Audio bytes in `span`.
    fn bytes_in(&self, span: Duration) -> u64 {
        (span.as_secs_f64() * self.p.bytes_per_second as f64) as u64
    }

    /// Sends what is due from the ring (and a queued `Finalize` at its position).
    async fn pump(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        loop {
            self.conn.sent_abs = self.conn.sent_abs.max(self.ring.front());
            if let Some(at) = self.finalize_at
                && self.conn.sent_abs >= at
            {
                self.write(ws, Message::text(FINALIZE)).await?;
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
            let Some(chunk) = self.ring.chunk_from(self.conn.sent_abs) else {
                self.conn.sent_abs = self.ring.total();
                continue;
            };
            let len = chunk.len() as u64;
            self.write(ws, Message::Binary(chunk)).await?;
            let sent_at = self.clock().now();
            self.conn.sent_abs = self.conn.sent_abs.saturating_add(len);
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
            "deepgram write",
            ws.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::Drop("write failed")),
            Err(_) => Err(Stop::Drop("write timed out")),
        }
    }

    /// No audio for `keepalive_interval`: silence if the connection never heard audio (KeepAlive
    /// alone does not prevent the `NET-0001` close), else KeepAlive.
    async fn on_keepalive(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        if self.conn.audio_started {
            self.write(ws, Message::text(KEEP_ALIVE)).await?;
            self.conn.paused = true;
            self.conn.anchor = None;
        } else {
            let samples = self.p.bytes_per_second as f64 * SILENCE.as_secs_f64();
            let silence = vec![self.cfg().encoding.silence_byte(); samples as usize];
            let len = silence.len() as u64;
            self.write(ws, Message::Binary(Bytes::from(silence)))
                .await?;
            self.conn.synthetic = self.conn.synthetic.saturating_add(len);
            self.conn.audio_started = true;
            let now = self.clock().now();
            self.conn.anchor.get_or_insert(now);
        }
        self.conn.last_tx = self.clock().now();
        Ok(())
    }

    // -- server messages ---------------------------------------------------------------------

    async fn on_frame(
        &mut self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<(), Stop> {
        use tokio_tungstenite::tungstenite::Error as WsError;
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
                // 1008 + DATA-0000: the audio could not be decoded (wrong encoding / rate).
                // A retry would fail the same way. Everything else (1011 NET-000x, 1000, ...)
                // is a drop worth a reconnect.
                if frame.is_some_and(|f| f.code == CloseCode::Policy) {
                    Err(Stop::Fatal(Error::Rejected {
                        provider: PROVIDER,
                        status: u16::from(CloseCode::Policy),
                    }))
                } else {
                    Err(Stop::Drop("closed by the server"))
                }
            }
            Some(Ok(_)) => Ok(()),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), Stop> {
        let now = self.clock().now();
        self.conn.anchor = if self.conn.paused || !self.conn.audio_started {
            None
        } else {
            Some(now)
        };
        let incoming = protocol::parse(text).map_err(|e| {
            Stop::Fatal(Error::Protocol {
                provider: PROVIDER,
                detail: e.0.to_owned(),
            })
        })?;
        let was_pending = self.assembler.finalize_pending();
        let mut flushed = false;
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        match incoming {
            Incoming::Results(results) => {
                self.attempts = 0;
                flushed = results.from_finalize;
                let end = self.results_end_abs(&results);
                self.processed_abs = Some(self.processed_abs.map_or(end, |p| p.max(end)));
                let covers = results.is_final
                    && final_covers_finalize(
                        end,
                        self.finalize_at,
                        self.finalize_sent_abs,
                        self.bytes_in(COVERS_QUEUED_TOLERANCE),
                        self.bytes_in(self.cfg().finalize_settle_gap),
                    );
                if results.is_final {
                    self.ring.trim_to(end);
                }
                self.assembler.on_results(&results, covers, &mut out);
            }
            Incoming::UtteranceEnd { last_word_end } => {
                self.assembler.on_utterance_end(last_word_end, &mut out);
            }
            Incoming::Error {
                variant,
                description,
            } => {
                // Deepgram calls these non-fatal (a fatal failure closes the socket). Only the two
                // variants that mean the adapter itself sent something wrong end the session; an
                // unknown or new variant is logged and the call goes on.
                if matches!(variant.as_str(), "SchemaError" | "InvalidConfigureMessage") {
                    return Err(Stop::Fatal(Error::Protocol {
                        provider: PROVIDER,
                        detail: format!("server error {variant}: {description}"),
                    }));
                }
                tracing::warn!(variant = %variant, "deepgram reported a non-fatal error");
                tracing::debug!(description = %description, "deepgram error description");
            }
            Incoming::Ignored => {}
        }
        if was_pending && !self.assembler.finalize_pending() {
            self.finalize_answered(flushed);
        }
        self.deliver(out).await
    }

    /// End of the audio the server reports for `results`, as an absolute stream position:
    /// timestamps are seconds on this connection, which starts at `base_abs` and may have started
    /// with generated silence that is not part of the stream.
    fn results_end_abs(&self, results: &Results) -> u64 {
        let end_secs = results.start + results.duration;
        let bytes = (end_secs * self.p.bytes_per_second as f64) as u64;
        self.conn
            .base_abs
            .saturating_add(bytes.saturating_sub(self.conn.synthetic))
            .min(self.conn.sent_abs)
    }

    /// The pending `Finalize` was answered. `flushed`: by its own `from_finalize` result, so the
    /// server has seen all audio up to the Finalize position and it is never replayed. (When a
    /// final that merely covers the position answered it, that final already trimmed the ring.)
    fn finalize_answered(&mut self, flushed: bool) {
        self.finalize_at = None;
        self.finalize_deadline = None;
        if let Some(at) = self.finalize_sent_abs.take()
            && flushed
        {
            self.ring.trim_to(at);
        }
    }

    /// The `Finalize` got no answer in time: end the utterance synthetically. A `Finalize` that
    /// is still queued (a reconnect and a replay are in the way) stays queued: when it goes out,
    /// its answer is real text the synthetic final lacked and arrives as a late utterance end.
    async fn on_finalize_timeout(&mut self) -> Result<(), Stop> {
        self.finalize_deadline = None;
        if !self.assembler.finalize_pending() {
            return Ok(());
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        self.assembler.finalize_timed_out(&mut out);
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
                    tracing::debug!("deepgram interim dropped: the events channel is full");
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(Stop::HangUp),
            };
        }
        match timed(
            &*self.p.clock,
            self.p.cfg.io_timeout,
            "deepgram event delivery",
            tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::HangUp),
            Err(_) => {
                tracing::warn!("deepgram events not consumed within the deadline; ending session");
                Err(Stop::HangUp)
            }
        }
    }

    // -- ending ------------------------------------------------------------------------------

    /// Delivers `error` as the last event and ends the session.
    async fn fail(&mut self, error: Error) {
        tracing::debug!(%error, "deepgram session failed");
        let _ = self.send_event(SttEvent::Error(error)).await;
        self.events = None;
    }

    /// Caller hang-up: the events close first, then the server is told to stop.
    async fn hang_up(&mut self, mut ws: Ws) {
        self.events = None;
        let close = async {
            if ws.send(Message::text(CLOSE_STREAM)).await.is_err() {
                return;
            }
            while let Some(Ok(message)) = ws.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        };
        let _ = timed(
            &*self.p.clock,
            self.p.cfg.close_timeout,
            "deepgram close",
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
                        what: "deepgram response",
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
                        status: 401..=403, ..
                    },
                ) => return Err(Stop::Fatal(error)),
                Err(error) => {
                    // Whatever the old connection did, the trouble is connecting now.
                    self.silent = false;
                    tracing::debug!(%error, "deepgram reconnect attempt failed");
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

    /// Resets per-connection state for a fresh connection that starts replaying at the ring.
    fn on_reconnected(&mut self) {
        let now = self.clock().now();
        self.conn = ConnState::new(self.ring.front(), now);
        self.assembler.connection_replaced();
        self.processed_abs = None;
        if self.assembler.finalize_pending() {
            // The old connection may have taken the Finalize with it: ask again at its position.
            // The deadline is kept: the caller's wait is bounded from its request.
            if let Some(at) = self.finalize_sent_abs.take() {
                self.finalize_at = Some(at);
            }
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

/// Whether a final that ends at absolute position `end` answers the open `Finalize`.
///
/// A `Finalize` still queued (`queued_at`, not yet written) has flushed nothing: the final must
/// reach its position within `queued_tolerance` (timestamp rounding), otherwise audio after the
/// final's end would never be flushed and the turn would end without it. Once the `Finalize` is
/// out (`sent_at`) the server holds it anyway and flushes the rest, so the wider
/// `sent_tolerance` (the settle gap) only decides whether the final is a good enough answer.
fn final_covers_finalize(
    end: u64,
    queued_at: Option<u64>,
    sent_at: Option<u64>,
    queued_tolerance: u64,
    sent_tolerance: u64,
) -> bool {
    match (queued_at, sent_at) {
        (Some(at), _) => end.saturating_add(queued_tolerance) >= at,
        (None, Some(at)) => end.saturating_add(sent_tolerance) >= at,
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 16 kHz 16-bit mono: 32 bytes per ms.
    const MS: u64 = 32;
    const QUEUED_TOL: u64 = 20 * MS;
    const SENT_TOL: u64 = 500 * MS;

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

    #[test]
    fn a_queued_finalize_is_answered_only_by_a_final_that_reaches_it() {
        let at = 2_050 * MS;
        // "bir iki" ends at 1.6 s, 450 ms short: inside the settle gap, but it must NOT answer a
        // Finalize that has not been sent (the speech after it would never be flushed).
        let covers = |end| final_covers_finalize(end, Some(at), None, QUEUED_TOL, SENT_TOL);
        assert!(!covers(1_600 * MS));
        // Reaching it, or within timestamp rounding, answers it.
        assert!(covers(at));
        assert!(covers(at - 10 * MS));
        assert!(!covers(at - 30 * MS));
    }

    #[test]
    fn a_sent_finalize_keeps_the_wide_tolerance_and_none_means_no_cover() {
        let at = 2_050 * MS;
        let covers = |end| final_covers_finalize(end, None, Some(at), QUEUED_TOL, SENT_TOL);
        assert!(covers(1_600 * MS));
        assert!(!covers(1_500 * MS));
        assert!(!final_covers_finalize(
            u64::MAX,
            None,
            None,
            QUEUED_TOL,
            SENT_TOL
        ));
    }
}
