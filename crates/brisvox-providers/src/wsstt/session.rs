//! The task that owns one session: audio out, messages in, finalize queue, liveness, reconnect
//! with replay, rotation, hang-up. See the [module docs](super) for the behaviour; this file is
//! the mechanism.
//!
//! One task, one `select!` loop, no locks. Everything that touches the socket happens here, in
//! order. The protocol ([`Wire`]) only decides what to send and what to tell the caller.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::SampleRate;
use brisvox_core::{Clock, SharedClock};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::ring::Ring;
use super::{Actions, Closed, Settings, Target, Trim, Wire, Ws, finalize_answer, open, timed};
use crate::error::{Error, Result};
use crate::stt::{SttEvent, SttInput};
use crate::timer::Timer;

/// Largest reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How long the close frame to a connection that is being replaced may take to be written.
const POLITE_CLOSE: Duration = Duration::from_millis(50);

/// Most `Finalize` requests kept waiting. The engine bounds its own ledger far below this; a
/// request beyond it is ignored (and logged) instead of growing the queue.
const MAX_PENDING_FINALIZES: usize = 8;

/// Everything a session needs that does not change.
pub(crate) struct Params<W> {
    pub(crate) settings: Arc<Settings>,
    pub(crate) clock: SharedClock,
    pub(crate) target: Target,
    pub(crate) wire: W,
    pub(crate) resampler: Option<Resample>,
}

/// Rate conversion of the engine's audio to the wire rate, with its scratch buffers (no
/// allocation per frame once they have grown to the largest frame).
pub(crate) struct Resample {
    inner: StreamResampler,
    samples_in: Vec<i16>,
    samples_out: Vec<i16>,
    bytes_out: BytesMut,
}

impl Resample {
    /// A converter from `from` to `to`; `None` when the rates are equal (the audio passes
    /// through untouched). The filter tables of a downsampler to 8 kHz take about a millisecond
    /// to build, so that case is built off the async workers.
    pub(crate) async fn new(
        from: SampleRate,
        to: SampleRate,
        provider: &'static str,
    ) -> Result<Option<Self>> {
        if from == to {
            return Ok(None);
        }
        let protocol = |detail: String| Error::Protocol { provider, detail };
        let inner = if to == SampleRate::Hz8000 {
            tokio::task::spawn_blocking(move || StreamResampler::new(from, to))
                .await
                .map_err(|e| protocol(format!("resampler setup failed: {e}")))?
        } else {
            StreamResampler::new(from, to)
        }
        .map_err(|e| protocol(format!("cannot resample {from} to {to}: {e}")))?;
        Ok(Some(Self {
            inner,
            samples_in: Vec::new(),
            samples_out: Vec::new(),
            bytes_out: BytesMut::new(),
        }))
    }

    /// Converts one frame; `None` while the filter is still buffering.
    fn convert(&mut self, pcm: &[u8]) -> Result<Option<Bytes>, brisvox_audio::Error> {
        self.samples_in.clear();
        for pair in pcm.as_chunks::<2>().0 {
            self.samples_in.push(i16::from_le_bytes(*pair));
        }
        let needed = self.inner.max_output_len(self.samples_in.len());
        if self.samples_out.len() < needed {
            self.samples_out.resize(needed, 0);
        }
        let written = self
            .inner
            .process(&self.samples_in, &mut self.samples_out)?;
        let Some(out) = self.samples_out.get(..written).filter(|o| !o.is_empty()) else {
            return Ok(None);
        };
        self.bytes_out.reserve(out.len().saturating_mul(2));
        for sample in out {
            self.bytes_out.put_i16_le(*sample);
        }
        Ok(Some(self.bytes_out.split().freeze()))
    }
}

/// Why `serve` or `absorb` stopped.
#[derive(Debug)]
enum Stop {
    /// The caller left (input or events dropped) or stopped consuming.
    HangUp,
    /// The connection was lost; reconnecting may help.
    Drop(&'static str),
    /// The server showed no sign of life for `io_timeout` after audio was written: lost like
    /// `Drop`, but if no reconnect is left the error is `Core(Timeout)`.
    Silent,
    /// The protocol declared the connection spent: replace it now.
    Rotate,
    /// The session failed; the error is delivered and the session ends.
    Fatal(Error),
}

/// A `Finalize` waiting for its answer.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Ring position of the request.
    mark: u64,
    /// When it is given up on.
    deadline: Instant,
    /// Handed to the protocol (on the current connection).
    sent: bool,
}

/// State of the current connection.
struct ConnState {
    /// Last time anything was written.
    last_tx: Instant,
    /// Start of the current wait for a sign of life; `None` when none is owed.
    anchor: Option<Instant>,
    /// The half-way ping of the current wait went out.
    probed: bool,
    /// A keepalive ping went out since the last audio: nothing is owed until audio resumes.
    paused: bool,
    /// Audio was written on this connection.
    audio_started: bool,
}

impl ConnState {
    fn new(now: Instant) -> Self {
        Self {
            last_tx: now,
            anchor: None,
            probed: false,
            paused: false,
            audio_started: false,
        }
    }
}

struct Session<W: Wire> {
    p: Params<W>,
    input: mpsc::Receiver<SttInput>,
    /// `None` once the session ended: the caller then sees the channel close.
    events: Option<mpsc::Sender<SttEvent>>,
    ring: Ring,
    conn: ConnState,
    /// While shut, audio is held in the ring.
    gate_open: bool,
    finalizes: VecDeque<Pending>,
    /// Reused action buffers handed to the protocol.
    act: Actions,
    /// Reconnect attempts used since the last proof of health.
    attempts: u32,
    /// The connection that was just lost was lost to silence.
    silent: bool,
    /// Replay-ring overflow episodes, so the warning is logged once per episode, not once per
    /// frame (up to 50 a second while the gate is shut or the connection is down).
    overflow: OverflowEpisode,
}

/// What [`OverflowEpisode::record`] says should be logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowLog {
    /// Nothing new.
    Quiet,
    /// Unsent audio started to be dropped.
    Started,
    /// The ring has room again; this many chunks were dropped over the episode.
    Ended(usize),
}

/// Tracks whether the replay ring is dropping audio that was never sent.
#[derive(Debug, Default)]
struct OverflowEpisode {
    /// Chunks dropped so far in the current episode; `None` outside one.
    dropped: Option<usize>,
}

impl OverflowEpisode {
    /// Takes the number of unsent chunks one push dropped.
    fn record(&mut self, dropped_unsent: usize) -> OverflowLog {
        match (self.dropped, dropped_unsent) {
            (None, 0) => OverflowLog::Quiet,
            (None, n) => {
                self.dropped = Some(n);
                OverflowLog::Started
            }
            (Some(total), 0) => {
                self.dropped = None;
                OverflowLog::Ended(total)
            }
            (Some(total), n) => {
                self.dropped = Some(total.saturating_add(n));
                OverflowLog::Quiet
            }
        }
    }
}

/// Runs a session on an open connection until it ends.
pub(crate) async fn run<W: Wire>(
    params: Params<W>,
    ws: Ws,
    input: mpsc::Receiver<SttInput>,
    events: mpsc::Sender<SttEvent>,
) {
    let now = params.clock.now();
    let cap = params.settings.wire_bytes_in(params.settings.replay_window);
    let session = Session {
        ring: Ring::new(cap),
        p: params,
        input,
        events: Some(events),
        conn: ConnState::new(now),
        gate_open: true,
        finalizes: VecDeque::new(),
        act: Actions::default(),
        attempts: 0,
        silent: false,
        overflow: OverflowEpisode::default(),
    };
    session.run(ws).await;
}

impl<W: Wire> Session<W> {
    async fn run(mut self, mut ws: Ws) {
        loop {
            let stop = self.serve(&mut ws).await;
            let planned = matches!(stop, Stop::Rotate);
            match stop {
                Stop::HangUp => {
                    self.hang_up(ws).await;
                    return;
                }
                Stop::Fatal(error) => {
                    self.fail(error).await;
                    return;
                }
                Stop::Drop(_) | Stop::Silent | Stop::Rotate => {
                    if let Stop::Drop(reason) = &stop {
                        tracing::debug!(
                            provider = self.p.settings.provider,
                            reason,
                            "connection lost"
                        );
                    }
                    self.silent = matches!(stop, Stop::Silent);
                    if planned {
                        // The server finished with this connection (or is about to): say
                        // goodbye properly, without waiting for its answer.
                        let _ = timed(
                            &*self.p.clock,
                            POLITE_CLOSE,
                            "close",
                            ws.send(Message::Close(None)),
                        )
                        .await;
                    }
                    drop(ws);
                    match self.reconnect(planned).await {
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

    fn settings(&self) -> &Settings {
        &self.p.settings
    }

    fn clock(&self) -> &dyn Clock {
        &*self.p.clock
    }

    // -- main loop ---------------------------------------------------------------------------

    async fn serve(&mut self, ws: &mut Ws) -> Stop {
        let now = self.clock().now();
        self.conn = ConnState::new(now);
        self.gate_open = true;
        self.ring.rewind();
        for pending in &mut self.finalizes {
            pending.sent = false;
        }
        self.act.reset();
        self.p.wire.opened(&mut self.act);
        if let Err(stop) = self.apply(Some(ws)).await {
            return stop;
        }
        if let Err(stop) = self.pump(ws).await {
            return stop;
        }
        // One pinned timer per deadline, reset only when its deadline moves (a pass per audio
        // frame used to register and drop four timer entries).
        let (mut finalize_timer, mut idle_timer, mut probe_timer, mut keepalive_timer) = (
            Timer::new(self.clock()),
            Timer::new(self.clock()),
            Timer::new(self.clock()),
            Timer::new(self.clock()),
        );
        loop {
            let io = self.settings().io_timeout;
            let keepalive_due = self.conn.last_tx + self.settings().keepalive_interval;
            let idle_due = if self.conn.paused {
                None
            } else {
                self.conn.anchor.map(|a| deadline_after(a, io))
            };
            let probe_due = match (self.conn.anchor, self.conn.probed, self.conn.paused) {
                (Some(a), false, false) => Some(deadline_after(a, io / 2)),
                _ => None,
            };
            finalize_timer.set(self.finalizes.front().map(|p| p.deadline));
            idle_timer.set(idle_due);
            probe_timer.set(probe_due);
            keepalive_timer.set(Some(keepalive_due));
            let step: Result<(), Stop> = tokio::select! {
                biased;
                () = events_closed(&self.events) => Err(Stop::HangUp),
                frame = ws.next() => match self.on_frame(frame, ws).await {
                    // A server message may have opened the gate or answered a finalize.
                    Ok(()) => self.pump(ws).await,
                    Err(stop) => Err(stop),
                },
                () = finalize_timer.wait(&*self.p.clock) => {
                    match self.on_finalize_timeout(Some(ws)).await {
                        Ok(()) => self.pump(ws).await,
                        Err(stop) => Err(stop),
                    }
                }
                () = idle_timer.wait(&*self.p.clock) => Err(Stop::Silent),
                () = probe_timer.wait(&*self.p.clock) => self.on_probe(ws).await,
                () = keepalive_timer.wait(&*self.p.clock) => self.on_keepalive(ws).await,
                message = self.input.recv() => match message {
                    None => Err(Stop::HangUp),
                    Some(message) => match self.on_input(message) {
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

    // -- input -------------------------------------------------------------------------------

    /// Takes one caller message: audio goes into the ring, a finalize request is queued at its
    /// position in the audio.
    fn on_input(&mut self, message: SttInput) -> Result<(), Stop> {
        match message {
            SttInput::Audio(bytes) => {
                let chunk = match self.p.resampler.as_mut() {
                    None => Some(bytes),
                    Some(resample) => resample.convert(&bytes).map_err(|e| {
                        Stop::Fatal(Error::Protocol {
                            provider: self.p.settings.provider,
                            detail: format!("resampling failed: {e}"),
                        })
                    })?,
                };
                if let Some(chunk) = chunk.filter(|c| !c.is_empty()) {
                    let pushed = self.ring.push(chunk);
                    self.note_overflow(pushed.dropped_unsent);
                }
            }
            SttInput::Finalize => {
                if self.finalizes.len() >= MAX_PENDING_FINALIZES {
                    tracing::warn!(
                        provider = self.p.settings.provider,
                        "finalize request ignored: too many are waiting"
                    );
                    return Ok(());
                }
                // The bound runs from the request, not from the moment the protocol gets it: a
                // reconnect or a replay in between must not stretch the caller's wait.
                let deadline = deadline_after(self.clock().now(), self.settings().finalize_timeout);
                self.finalizes.push_back(Pending {
                    mark: self.ring.end(),
                    deadline,
                    sent: false,
                });
            }
        }
        Ok(())
    }

    /// Logs a replay-ring overflow once per episode: a warning when unsent audio starts to be
    /// dropped, a debug line with the total when the ring has room again.
    fn note_overflow(&mut self, dropped_unsent: usize) {
        match self.overflow.record(dropped_unsent) {
            OverflowLog::Quiet => {}
            OverflowLog::Started => tracing::warn!(
                provider = self.p.settings.provider,
                "held audio is being dropped: the replay window is full"
            ),
            OverflowLog::Ended(chunks) => tracing::debug!(
                provider = self.p.settings.provider,
                chunks,
                "held audio no longer dropped"
            ),
        }
    }

    /// Writes what is due: queued finalizes at their positions and the audio between them, as
    /// far as the gate lets it.
    async fn pump(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        loop {
            let front_due = self
                .finalizes
                .front()
                .is_some_and(|p| !p.sent && self.ring.next_index() >= p.mark);
            if front_due {
                if let Some(front) = self.finalizes.front_mut() {
                    front.sent = true;
                }
                self.act.reset();
                self.p.wire.finalize(&mut self.act);
                self.apply(Some(ws)).await?;
                continue;
            }
            if !self.gate_open {
                return Ok(());
            }
            let limit = self
                .finalizes
                .iter()
                .find(|p| !p.sent)
                .map_or(u64::MAX, |p| p.mark);
            let Some(chunk) = self.ring.peek_next(limit).cloned() else {
                return Ok(());
            };
            let message = self.p.wire.audio(&chunk).map_err(Stop::Fatal)?;
            self.write(ws, message).await?;
            self.ring.advance();
            let now = self.clock().now();
            self.conn.audio_started = true;
            self.conn.paused = false;
            self.conn.last_tx = now;
            self.conn.anchor.get_or_insert(now);
        }
    }

    /// Writes one message within the write deadline.
    async fn write(&mut self, ws: &mut Ws, message: Message) -> Result<(), Stop> {
        match timed(
            self.clock(),
            self.settings().io_timeout,
            "write",
            ws.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::Drop("write failed")),
            Err(_) => Err(Stop::Drop("write timed out")),
        }
    }

    // -- timers ------------------------------------------------------------------------------

    /// Half of `io_timeout` without a sign of life: ask for one.
    async fn on_probe(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        self.conn.probed = true;
        self.write(ws, Message::Ping(Bytes::new())).await?;
        self.conn.last_tx = self.clock().now();
        Ok(())
    }

    /// No audio for `keepalive_interval`: a ping keeps the path open and the wait is suspended
    /// until audio resumes.
    async fn on_keepalive(&mut self, ws: &mut Ws) -> Result<(), Stop> {
        self.write(ws, Message::Ping(Bytes::new())).await?;
        self.conn.paused = true;
        self.conn.anchor = None;
        self.conn.last_tx = self.clock().now();
        Ok(())
    }

    /// The oldest `Finalize` is out of time. One the protocol never saw (a reconnect is in the
    /// way) is answered empty here; one it has is answered by the protocol with what it knows.
    async fn on_finalize_timeout(&mut self, ws: Option<&mut Ws>) -> Result<(), Stop> {
        let Some(front) = self.finalizes.front().copied() else {
            return Ok(());
        };
        self.act.reset();
        if front.sent {
            self.p.wire.finalize_timed_out(&mut self.act);
        }
        if !self.act.answered {
            self.act.events.push(finalize_answer(""));
            self.act.answered = true;
        }
        // The turn is over: that audio is not to be replayed into a new connection.
        self.act.trim = Trim::ToMark;
        self.apply(ws).await
    }

    // -- server messages ---------------------------------------------------------------------

    fn refresh_liveness(&mut self) {
        let now = self.clock().now();
        self.conn.anchor = if self.conn.paused || !self.conn.audio_started {
            None
        } else {
            Some(now)
        };
        self.conn.probed = false;
    }

    async fn on_frame(
        &mut self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
        ws: &mut Ws,
    ) -> Result<(), Stop> {
        use tokio_tungstenite::tungstenite::Error as WsError;
        match frame {
            None => self.on_close(None, ws).await,
            Some(Err(WsError::Capacity(_) | WsError::Utf8(_) | WsError::AttackAttempt)) => {
                Err(Stop::Fatal(Error::Protocol {
                    provider: self.p.settings.provider,
                    detail: "invalid frame from the server".to_owned(),
                }))
            }
            Some(Err(_)) => Err(Stop::Drop("socket error")),
            Some(Ok(Message::Text(text))) => {
                self.refresh_liveness();
                self.act.reset();
                self.p
                    .wire
                    .text(text.as_str(), &mut self.act)
                    .map_err(Stop::Fatal)?;
                self.apply(Some(ws)).await
            }
            Some(Ok(Message::Close(frame))) => self.on_close(frame.map(|f| f.code), ws).await,
            Some(Ok(_)) => {
                // Binary, ping (answered by the library) and pong: signs of life.
                self.refresh_liveness();
                Ok(())
            }
        }
    }

    /// The server closed the connection, or the stream ended.
    async fn on_close(&mut self, code: Option<CloseCode>, ws: &mut Ws) -> Result<(), Stop> {
        self.act.reset();
        let verdict = self.p.wire.closed(code, &mut self.act);
        // A protocol whose server closes after its answer produces the answer here (and may ask
        // for a replacement connection, which `apply` reports as `Rotate`).
        self.apply(Some(ws)).await?;
        match verdict {
            Closed::Lost => Err(Stop::Drop("closed by the server")),
            Closed::Fatal(error) => Err(Stop::Fatal(error)),
        }
    }

    // -- applying a protocol step --------------------------------------------------------------

    /// Carries out what the protocol asked for in `self.act`: events to the caller, the answered
    /// `Finalize`, trimming, the gate, messages to write (only with a socket) and rotation.
    async fn apply(&mut self, ws: Option<&mut Ws>) -> Result<(), Stop> {
        let mut events = std::mem::take(&mut self.act.events);
        let mut delivered = Ok(());
        for event in events.drain(..) {
            if delivered.is_ok() {
                delivered = self.send_event(event).await;
            }
        }
        self.act.events = events;
        delivered?;
        if self.act.healthy {
            self.attempts = 0;
        }
        match self.act.trim {
            Trim::None => {}
            Trim::ToMark => {
                if let Some(front) = self.finalizes.front() {
                    self.ring.trim_to(front.mark);
                }
            }
            Trim::KeepLast(span) => {
                let keep = self.settings().wire_bytes_in(span);
                self.ring.keep_last_sent(keep);
            }
        }
        if self.act.answered {
            self.finalizes.pop_front();
        }
        if let Some(open) = self.act.gate {
            self.gate_open = open;
        }
        let rotate = self.act.rotate;
        let mut sends = std::mem::take(&mut self.act.send);
        let mut written = Ok(());
        if let Some(ws) = ws {
            for message in sends.drain(..) {
                if written.is_ok() {
                    written = self.write(ws, message).await;
                    self.conn.last_tx = self.clock().now();
                }
            }
        }
        sends.clear();
        self.act.send = sends;
        self.act.rotate = false;
        written?;
        if rotate { Err(Stop::Rotate) } else { Ok(()) }
    }

    // -- delivery ----------------------------------------------------------------------------

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
                    tracing::debug!(
                        provider = self.p.settings.provider,
                        "interim dropped: the events channel is full"
                    );
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(Stop::HangUp),
            };
        }
        match timed(
            &*self.p.clock,
            self.p.settings.io_timeout,
            "event delivery",
            tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Stop::HangUp),
            Err(_) => {
                tracing::warn!(
                    provider = self.p.settings.provider,
                    "events not consumed within the deadline; ending session"
                );
                Err(Stop::HangUp)
            }
        }
    }

    // -- ending ------------------------------------------------------------------------------

    /// Delivers `error` as the last event and ends the session.
    async fn fail(&mut self, error: Error) {
        tracing::debug!(provider = self.p.settings.provider, %error, "session failed");
        let _ = self.send_event(SttEvent::Error(error)).await;
        self.events = None;
    }

    /// Caller hang-up: the events close first, then the server is told and the socket closed.
    async fn hang_up(&mut self, mut ws: Ws) {
        self.events = None;
        let goodbye = self.p.wire.goodbye();
        let close = async {
            for message in goodbye {
                if ws.send(message).await.is_err() {
                    return;
                }
            }
            if ws.send(Message::Close(None)).await.is_err() {
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
            self.p.settings.close_timeout,
            "close",
            close,
        )
        .await;
    }

    // -- reconnect ---------------------------------------------------------------------------

    /// Opens a new connection (bounded attempts) while still taking audio and finalize requests
    /// from the caller. A `planned` replacement (the protocol's own rotation) does not use the
    /// reconnect budget of drops: it has its own, of the same size, and starts without a wait.
    async fn reconnect(&mut self, planned: bool) -> Result<Ws, Stop> {
        if planned {
            // The replacement is not needed before the caller sends again (audio follows within
            // a frame in normal operation): a call that ends right after a turn leaves no
            // stray connection behind.
            self.wait_for_input().await?;
        }
        let mut tries = 0_u32;
        loop {
            let backoff = if planned {
                if tries > self.settings().max_reconnects {
                    return Err(Stop::Fatal(Error::Disconnected {
                        provider: self.settings().provider,
                    }));
                }
                let wait = if tries == 0 {
                    Duration::ZERO
                } else {
                    backoff_for(self.settings().reconnect_backoff, tries - 1)
                };
                tries += 1;
                wait
            } else {
                if self.attempts >= self.settings().max_reconnects {
                    let error = if self.silent {
                        Error::Core(brisvox_core::Error::Timeout {
                            what: "server response",
                            after: self.settings().io_timeout,
                        })
                    } else {
                        Error::Disconnected {
                            provider: self.settings().provider,
                        }
                    };
                    return Err(Stop::Fatal(error));
                }
                let wait = backoff_for(self.settings().reconnect_backoff, self.attempts);
                self.attempts += 1;
                wait
            };
            if !backoff.is_zero() {
                let clock = Arc::clone(&self.p.clock);
                self.absorb(async move { clock.sleep(backoff).await })
                    .await?;
            }
            let settings = Arc::clone(&self.p.settings);
            let clock = Arc::clone(&self.p.clock);
            let target = self.p.target.clone();
            let opened = self
                .absorb(async move { open(&settings, &*clock, &target).await })
                .await?;
            match opened {
                Ok(ws) => return Ok(ws),
                // Credentials, billing and permission problems do not heal by retrying.
                Err(
                    error @ Error::Rejected {
                        status: 401..=403, ..
                    },
                ) => return Err(Stop::Fatal(error)),
                Err(error) => {
                    // Whatever the old connection did, the trouble is connecting now.
                    self.silent = false;
                    tracing::debug!(
                        provider = self.p.settings.provider,
                        %error,
                        "reconnect attempt failed"
                    );
                }
            }
        }
    }

    /// Returns once there is audio to send or a `Finalize` to handle, taking caller input
    /// meanwhile.
    async fn wait_for_input(&mut self) -> Result<(), Stop> {
        while !self.ring.has_unsent() && self.finalizes.is_empty() {
            tokio::select! {
                biased;
                () = events_closed(&self.events) => return Err(Stop::HangUp),
                message = self.input.recv() => match message {
                    None => return Err(Stop::HangUp),
                    Some(message) => self.on_input(message)?,
                },
            }
        }
        Ok(())
    }

    /// Runs `future` while taking caller input into the ring and keeping the `Finalize` bounds.
    async fn absorb<T>(&mut self, future: impl std::future::Future<Output = T>) -> Result<T, Stop> {
        tokio::pin!(future);
        let mut finalize_timer = Timer::new(self.clock());
        loop {
            finalize_timer.set(self.finalizes.front().map(|p| p.deadline));
            tokio::select! {
                biased;
                () = events_closed(&self.events) => return Err(Stop::HangUp),
                out = &mut future => return Ok(out),
                () = finalize_timer.wait(&*self.p.clock) => {
                    // No socket: the protocol's answer is delivered, its messages are moot.
                    match self.on_finalize_timeout(None).await {
                        Ok(()) | Err(Stop::Rotate) => {}
                        Err(stop) => return Err(stop),
                    }
                }
                message = self.input.recv() => match message {
                    None => return Err(Stop::HangUp),
                    Some(message) => self.on_input(message)?,
                },
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
        assert_eq!(backoff_for(base, 64), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::MAX, 3), MAX_BACKOFF);
        assert_eq!(backoff_for(Duration::ZERO, 3), Duration::ZERO);
    }

    #[test]
    fn a_ring_overflow_is_logged_once_per_episode() {
        let mut episode = OverflowEpisode::default();
        assert_eq!(episode.record(0), OverflowLog::Quiet);
        assert_eq!(episode.record(1), OverflowLog::Started);
        // 200 more frames while the gate stays shut: nothing more to log.
        for _ in 0..200 {
            assert_eq!(episode.record(1), OverflowLog::Quiet);
        }
        assert_eq!(episode.record(2), OverflowLog::Quiet);
        assert_eq!(episode.record(0), OverflowLog::Ended(203));
        assert_eq!(episode.record(0), OverflowLog::Quiet);
        // A new episode warns again.
        assert_eq!(episode.record(1), OverflowLog::Started);
    }

    #[tokio::test]
    async fn equal_rates_pass_the_audio_through_and_different_rates_convert() {
        let same = Resample::new(SampleRate::Hz16000, SampleRate::Hz16000, "t").await;
        assert!(matches!(same, Ok(None)));
        let up = Resample::new(SampleRate::Hz16000, SampleRate::Hz24000, "t").await;
        let Ok(Some(mut up)) = up else {
            panic!("a 16 -> 24 kHz converter");
        };
        // 100 ms of a ramp at 16 kHz: about 150 ms of samples' worth at 24 kHz come out over a
        // few frames (the filter buffers whole blocks).
        let frame: Vec<u8> = (0..320_i16).flat_map(|s| (s * 50).to_le_bytes()).collect();
        let mut out_bytes = 0;
        for _ in 0..5 {
            if let Ok(Some(chunk)) = up.convert(&frame) {
                assert_eq!(chunk.len() % 2, 0);
                out_bytes += chunk.len();
            }
        }
        // 5 frames x 320 samples x 1.5 = 2400 samples = 4800 bytes, minus what the filter holds.
        assert!(out_bytes > 3000 && out_bytes <= 4800, "{out_bytes}");
    }

    #[tokio::test]
    async fn a_downsampler_to_8_khz_is_built_off_the_async_worker() {
        let down = Resample::new(SampleRate::Hz16000, SampleRate::Hz8000, "t").await;
        assert!(matches!(down, Ok(Some(_))));
    }
}
