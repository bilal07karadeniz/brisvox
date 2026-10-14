//! The task that owns one Aura WebSocket and turns engine input into `Speak`/`Flush`/`Clear`
//! messages and server frames into [`TtsEvent`]s (module docs: `super`).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_for};
use brisvox_core::{Error as CoreError, SharedClock};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{Instant, Sleep};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::limits::{CharWindow, FlushWindow};
use super::wire::{
    CLEAR, CLOSE, FLUSH, Incoming, Segment, SessionParams, close_error, encode_speak, split_text,
};
use super::{FlushPolicy, Inner, PROVIDER, Socket, map_ws_error, with_timeout};
use crate::error::Error;
use crate::tts::{
    ContextId, TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY, TtsEvent, TtsInput, TtsSession,
};

/// The socket is not read while the outgoing queue holds this many events.
const OUT_HIGH_WATER: usize = 64;

/// Cancelled contexts remembered, so input still queued for them is ignored.
const CANCELLED_MEMORY: usize = 64;

/// Contexts kept in the FIFO at once; input is not read beyond this.
const MAX_ENTRIES: usize = 64;

/// Text pieces kept waiting in the FIFO; input is not read beyond this (each piece is at most
/// `max_speak_chars` long, so this bounds the memory too).
const MAX_PENDING_SEGMENTS: usize = 128;

/// Flushes under `FlushPolicy::PerText` stop this far short of the limit, so the flush that
/// ends a turn always finds room.
const FLUSH_RESERVE: usize = 4;

/// Characters of one context sent since its last `Flush`, above which the next `Speak` is
/// preceded by a flush. The docs limit a request to 2000 characters and do not say whether
/// that means one `Speak`, one flush or the connection (UNVERIFIED), so no flush interval is
/// allowed to carry more than this: the cost is one flush of the budget, not a dead call
/// (close 1009).
const MID_TURN_FLUSH_CHARS: usize = 1800;

/// Longest wait for the polite close of the socket.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Why the task stopped.
enum End {
    /// The input is closed and nothing is owed: close the socket politely.
    Graceful,
    /// The consumer is gone or the provider shut down: just drop everything.
    Closed,
    /// The session failed: deliver what is queued, then the error.
    Fatal(Error),
}

/// How [`Task::serve`] ended.
enum Served<T> {
    /// The future finished.
    Done(T),
    /// The deadline passed first.
    TimedOut,
    /// The shutdown token was cancelled.
    Shutdown,
}

type Frame = Option<Result<Message, WsError>>;

/// One engine context in the FIFO. Only the head is on the wire.
#[derive(Debug)]
struct Entry {
    id: ContextId,
    /// Text not yet sent (only the head sends).
    pending: VecDeque<Segment>,
    /// The engine flushed the context: `Flushed` is reported when it completes. A context
    /// that is flushed only because the input closed completes silently.
    engine_flush: bool,
    /// A `Flush` must follow the pending text.
    wants_flush: bool,
    /// The closing flush has been sent, or there was nothing to flush.
    flush_sent: bool,
    /// Some text was sent (or is being written) on the CURRENT connection: the server may hold
    /// state for this context. Set only once a connection is open and the write begins, and
    /// cleared again when the connection is replaced.
    started: bool,
    /// Text was sent after the last `Flush`.
    unflushed: bool,
    /// Characters sent after the last `Flush`.
    unflushed_chars: usize,
    /// `Flush`es sent whose `Flushed` has not arrived.
    owed: u32,
}

impl Entry {
    fn new(id: ContextId) -> Self {
        Self {
            id,
            pending: VecDeque::new(),
            engine_flush: false,
            wants_flush: false,
            flush_sent: false,
            started: false,
            unflushed: false,
            unflushed_chars: 0,
            owed: 0,
        }
    }

    /// Whether the server may hold text of this context that a `Clear` would discard or a
    /// `Flushed` is still owed for. A context that was fully answered (`owed == 0`, nothing
    /// sent since the last flush) holds nothing, however long it stays in the FIFO.
    fn holds_text(&self) -> bool {
        self.started && (self.unflushed || self.owed > 0)
    }

    /// Forgets what the old connection knew (it was replaced): nothing of this context is on
    /// the new one.
    fn wire_reset(&mut self) {
        self.started = false;
        self.unflushed = false;
        self.unflushed_chars = 0;
    }

    fn is_complete(&self) -> bool {
        self.pending.is_empty() && self.wants_flush && self.flush_sent && self.owed == 0
    }
}

/// Spawns the session task around `socket`.
pub(super) fn spawn(inner: Arc<Inner>, socket: Socket, params: SessionParams) -> TtsSession {
    let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let clock = Arc::clone(&inner.config.clock);
    let io_timeout = inner.config.io_timeout;
    let window = inner.config.rate_window;
    let task = Task {
        shutdown: inner.config.shutdown.clone(),
        policy: inner.config.flush_policy,
        max_speak: inner.config.max_speak_chars,
        max_age: inner.config.max_connection_age,
        flushes: FlushWindow::new(inner.config.max_flushes_per_window, window),
        chars: CharWindow::new(inner.config.chars_per_window, window),
        bytes_per_sample: params.encoding.bytes_per_sample(),
        connected_at: clock.now(),
        idle: Box::pin(clock.sleep(io_timeout)),
        probe: Box::pin(clock.sleep(io_timeout)),
        gate: Box::pin(clock.sleep(io_timeout)),
        clock,
        io_timeout,
        inner,
        params,
        ws: Some(socket),
        entries: VecDeque::new(),
        cancelled: VecDeque::with_capacity(CANCELLED_MEMORY),
        out: VecDeque::with_capacity(OUT_HIGH_WATER),
        events: events_tx,
        input_rx,
        cancel_rx,
        input_open: true,
        cancel_open: true,
        armed: false,
        probe_armed: false,
        gate_armed: false,
        paused: false,
        clearing: 0,
        clear_wanted: false,
        carry: None,
    };
    tokio::spawn(task.run());
    TtsSession {
        input: input_tx,
        cancel: cancel_tx,
        events: events_rx,
    }
}

struct Task {
    inner: Arc<Inner>,
    shutdown: CancellationToken,
    clock: SharedClock,
    io_timeout: Duration,
    policy: FlushPolicy,
    max_speak: usize,
    max_age: Duration,
    flushes: FlushWindow,
    chars: CharWindow,
    bytes_per_sample: u64,
    params: SessionParams,
    /// When the current socket was opened (or taken from the parked one).
    connected_at: Instant,
    /// `None` after the server closed an idle socket (reconnected lazily), after a failure, or
    /// while a write has the socket in hand.
    ws: Option<Socket>,
    /// Engine contexts in arrival order; the head is the one on the wire. At most
    /// `MAX_ENTRIES` (input is not read beyond that), a linear scan is fine.
    entries: VecDeque<Entry>,
    cancelled: VecDeque<ContextId>,
    /// Events not yet handed to the consumer. Bounded by `OUT_HIGH_WATER` plus one frame (plus
    /// the cancel acknowledgements, which are tiny and replace purged events).
    out: VecDeque<TtsEvent>,
    events: mpsc::Sender<TtsEvent>,
    input_rx: mpsc::Receiver<TtsInput>,
    cancel_rx: mpsc::Receiver<ContextId>,
    input_open: bool,
    cancel_open: bool,
    /// Fires `io_timeout` after the last progress while the server owes output.
    idle: Pin<Box<Sleep>>,
    armed: bool,
    /// Fires half an `io_timeout` after the last sign of life while text is on the server and
    /// nothing is owed: a WebSocket `Ping` goes out, its `Pong` is the sign of life.
    probe: Pin<Box<Sleep>>,
    probe_armed: bool,
    /// Fires when a send held back by a rate limit may go out.
    gate: Pin<Box<Sleep>>,
    gate_armed: bool,
    /// The socket is not being read because the consumer is behind (backpressure).
    paused: bool,
    /// `Clear`s sent whose `Cleared` has not arrived: frames are dropped and nothing new is sent.
    clearing: u32,
    /// A `Clear` must be sent as soon as the socket is free (a started context was cancelled).
    clear_wanted: bool,
    /// An odd trailing byte of the previous PCM16 frame, completed by the next one.
    carry: Option<u8>,
}

/// The next frame of the socket; never ready while there is no socket.
async fn next_frame(ws: &mut Option<Socket>) -> Frame {
    match ws {
        Some(socket) => socket.next().await,
        None => std::future::pending().await,
    }
}

fn event_context(event: &TtsEvent) -> Option<ContextId> {
    match event {
        TtsEvent::Audio { context, .. }
        | TtsEvent::Word { context, .. }
        | TtsEvent::Flushed { context }
        | TtsEvent::Cancelled { context } => Some(*context),
        TtsEvent::Error(_) => None,
    }
}

impl Task {
    async fn run(mut self) {
        match self.drive().await {
            End::Graceful => {
                let socket = self.ws.take();
                let clock = Arc::clone(&self.clock);
                let timeout = self.io_timeout.min(CLOSE_TIMEOUT);
                // The consumer sees the end of the stream before the polite close.
                drop(self);
                if let Some(mut socket) = socket {
                    let _ = with_timeout(
                        &*clock,
                        timeout,
                        "deepgram-aura close",
                        socket.send(Message::text(CLOSE)),
                    )
                    .await;
                    let _ =
                        with_timeout(&*clock, timeout, "deepgram-aura close", socket.close(None))
                            .await;
                }
            }
            End::Closed => {}
            End::Fatal(error) => {
                self.out.push_back(TtsEvent::Error(error));
                self.drain().await;
            }
        }
    }

    /// Nothing is owed to or by the server any more (the engine's input is closed).
    fn finished(&self) -> bool {
        !self.input_open
            && self.entries.is_empty()
            && self.out.is_empty()
            && self.clearing == 0
            && !self.clear_wanted
    }

    /// Whether the FIFO can take more input.
    fn has_input_room(&self) -> bool {
        self.entries.len() < MAX_ENTRIES
            && self
                .entries
                .iter()
                .map(|e| e.pending.len())
                .fold(0, usize::saturating_add)
                < MAX_PENDING_SEGMENTS
    }

    async fn drive(&mut self) -> End {
        loop {
            if self.finished() {
                return End::Graceful;
            }
            let room = self.out.len() < OUT_HIGH_WATER;
            let input_room = room && self.has_input_room();
            if room == self.paused {
                // Reading stopped or resumed. A server we are not listening to cannot be
                // stalled: the stall timer restarts when we listen again.
                self.paused = !room;
                if room {
                    self.touch(true);
                }
            }
            let step: Result<(), Error> = tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return End::Closed,
                cancel = self.cancel_rx.recv(), if self.cancel_open => match cancel {
                    Some(context) => {
                        self.local_cancel(context);
                        Ok(())
                    }
                    None => {
                        self.cancel_open = false;
                        Ok(())
                    }
                },
                permit = self.events.clone().reserve_owned(), if !self.out.is_empty() => match permit {
                    Ok(permit) => {
                        if let Some(event) = self.out.pop_front() {
                            drop(permit.send(event));
                        }
                        Ok(())
                    }
                    Err(_) => return End::Closed,
                },
                () = self.events.closed() => return End::Closed,
                () = &mut self.idle, if self.armed && room => self.on_stall(),
                () = &mut self.probe, if self.probe_armed && room => self.on_probe().await,
                () = &mut self.gate, if self.gate_armed => {
                    self.gate_armed = false;
                    Ok(())
                },
                frame = next_frame(&mut self.ws), if room && self.ws.is_some() => {
                    self.on_frame(frame).await
                }
                input = self.input_rx.recv(), if input_room && self.input_open => match input {
                    Some(input) => {
                        self.on_input(input);
                        Ok(())
                    }
                    None => {
                        self.on_input_closed();
                        Ok(())
                    }
                },
            };
            let step = match step {
                Ok(()) => self.settle().await,
                error => error,
            };
            if let Err(error) = step {
                // A shutdown that interrupted a connect, write or close is no failure.
                if self.shutdown.is_cancelled() {
                    return End::Closed;
                }
                return End::Fatal(error);
            }
        }
    }

    /// Hands the queued events (the failure last) to the consumer, each within the deadline.
    async fn drain(&mut self) {
        while let Some(event) = self.out.pop_front() {
            let delivered = tokio::select! {
                sent = self.events.send(event) => sent.is_ok(),
                () = wait_for(&*self.clock, self.io_timeout) => false,
                () = self.shutdown.cancelled() => false,
            };
            if !delivered {
                break;
            }
        }
    }

    /// Hands queued events to the consumer without waiting (as far as the channel has room).
    fn deliver(&mut self) {
        while let Some(event) = self.out.pop_front() {
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.out.push_front(event);
                    break;
                }
                // The consumer is gone: `drive` notices through `events.closed()`.
                Err(TrySendError::Closed(_)) => break,
            }
        }
    }

    // ----- timeouts ---------------------------------------------------------------------

    /// Whether the server owes something: a `Flushed` for the head or a `Cleared`.
    fn awaiting_server(&self) -> bool {
        self.clearing > 0 || self.entries.front().is_some_and(|e| e.owed > 0)
    }

    /// Text of the head is on the server, nothing is owed and nothing is being cleared: the
    /// server may legitimately say nothing (`FlushPolicy::PerTurn` between the first `Speak`
    /// and the turn's `Flush`, or a send held back by a rate limit). Silence is then tested
    /// with a WebSocket `Ping` instead of waiting for output.
    fn quiet_wait(&self) -> bool {
        self.clearing == 0
            && !self.clear_wanted
            && self
                .entries
                .front()
                .is_some_and(|e| e.started && e.unflushed && e.owed == 0)
    }

    /// Re-evaluates the stall deadline: armed while the server owes output or the connection
    /// is being probed ([`Self::quiet_wait`]), restarted by `progress` (a frame, a `Pong`) or
    /// when it was not armed yet. While probing, a `Ping` is due half a deadline after the last
    /// sign of life.
    fn touch(&mut self, progress: bool) {
        let probing = self.quiet_wait();
        if !self.awaiting_server() && !probing {
            self.armed = false;
            self.probe_armed = false;
            return;
        }
        let now = self.clock.now();
        let restart = progress || !self.armed;
        if restart {
            self.idle
                .as_mut()
                .reset(deadline_after(now, self.io_timeout));
            self.armed = true;
        }
        if !probing {
            self.probe_armed = false;
        } else if restart || !self.probe_armed {
            self.probe
                .as_mut()
                .reset(deadline_after(now, self.io_timeout / 2));
            self.probe_armed = true;
        }
    }

    /// The probe is due: ask the server to prove it is alive. The `Pong` (or any frame)
    /// restarts the deadline; without one the stall deadline ends the session.
    async fn on_probe(&mut self) -> Result<(), Error> {
        self.probe_armed = false;
        if !self.quiet_wait() {
            return Ok(());
        }
        match self.write(Message::Ping(Bytes::new())).await {
            Ok(()) => Ok(()),
            Err(error) => self.connection_lost(error),
        }
    }

    /// The stall deadline passed. A `Cleared` that never comes is not fatal (the server may not
    /// answer a `Clear` on an empty buffer, UNVERIFIED): anything the cancelled utterance still
    /// had to send would have arrived within the deadline. A `Flushed` that never comes is.
    fn on_stall(&mut self) -> Result<(), Error> {
        self.armed = false;
        self.probe_armed = false;
        let head_owes = self.entries.front().is_some_and(|e| e.owed > 0);
        if self.clearing > 0 && !head_owes {
            tracing::warn!("deepgram-aura: no Cleared within the deadline, going on");
            self.clearing = 0;
            return Ok(());
        }
        Err(Error::Core(CoreError::Timeout {
            what: "deepgram-aura response",
            after: self.io_timeout,
        }))
    }

    /// Holds sends back until `wait` has passed.
    fn arm_gate(&mut self, wait: Duration) {
        let at = deadline_after(self.clock.now(), wait);
        self.gate.as_mut().reset(at);
        self.gate_armed = true;
    }

    // ----- socket -----------------------------------------------------------------------

    /// Awaits `future` for at most `timeout` (`None`: no deadline of its own) while taking
    /// cancels (their local half runs at once; the `Clear` is sent after the caller is done)
    /// and watching the shutdown token. `future` must not borrow the task: a socket it works on
    /// is taken out of `self.ws` first.
    async fn serve<F: Future>(
        &mut self,
        future: F,
        timeout: Option<Duration>,
    ) -> Served<F::Output> {
        let clock = Arc::clone(&self.clock);
        let deadline = async {
            match timeout {
                Some(after) => wait_for(&*clock, after).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(future, deadline);
        loop {
            tokio::select! {
                biased;
                output = &mut future => return Served::Done(output),
                () = &mut deadline => return Served::TimedOut,
                () = self.shutdown.cancelled() => return Served::Shutdown,
                cancel = self.cancel_rx.recv(), if self.cancel_open => match cancel {
                    Some(context) => self.local_cancel(context),
                    None => self.cancel_open = false,
                },
            }
        }
    }

    /// Sends one frame within the deadline. While the send is pending, cancels are taken and
    /// the shutdown token is watched (see [`Self::serve`]).
    async fn write(&mut self, message: Message) -> Result<(), Error> {
        let Some(mut socket) = self.ws.take() else {
            return Err(Error::Disconnected { provider: PROVIDER });
        };
        let served = {
            let send = socket.send(message);
            self.serve(send, Some(self.io_timeout)).await
        };
        match served {
            Served::Done(Ok(())) => {
                self.ws = Some(socket);
                Ok(())
            }
            // The socket is dropped: after a failure or a half-written frame it is unusable.
            Served::Done(Err(error)) => Err(map_ws_error(error)),
            Served::TimedOut => Err(Error::Core(CoreError::Timeout {
                what: "deepgram-aura send",
                after: self.io_timeout,
            })),
            Served::Shutdown => Err(Error::Disconnected { provider: PROVIDER }),
        }
    }

    /// Opens a socket if there is none (not the one parked for the next `connect`). Cancels
    /// are taken and the shutdown token is watched while connecting; the connect carries its
    /// own deadline.
    async fn ensure_connected(&mut self) -> Result<(), Error> {
        if self.ws.is_some() {
            return Ok(());
        }
        let inner = Arc::clone(&self.inner);
        let params = self.params.clone();
        let served = {
            let connect = inner.open(&params);
            self.serve(connect, None).await
        };
        let socket = match served {
            Served::Done(opened) => opened?,
            Served::TimedOut => {
                return Err(Error::Core(CoreError::Timeout {
                    what: "deepgram-aura connect",
                    after: self.io_timeout,
                }));
            }
            Served::Shutdown => return Err(Error::Disconnected { provider: PROVIDER }),
        };
        self.ws = Some(socket);
        self.connected_at = self.clock.now();
        self.carry = None;
        Ok(())
    }

    /// Sends one message of the head context `id`: connects first if needed. `speak_chars`:
    /// the message is a `Speak` of that many characters, so the context counts as started (text
    /// possibly on the server) from the moment the write begins on an open connection, not
    /// before: a cancel that lands while connecting has nothing to clear. `fresh`: nothing of
    /// the context had been sent before, so one dead-socket failure is retried on a new socket
    /// (the failed write leaves no state behind). Does nothing if the context was cancelled
    /// meanwhile.
    async fn send(
        &mut self,
        id: ContextId,
        json: &str,
        fresh: bool,
        speak_chars: Option<usize>,
    ) -> Result<(), Error> {
        let message = Message::text(json);
        let mut retried = false;
        loop {
            self.ensure_connected().await?;
            let Some(entry) = self.entries.front_mut().filter(|e| e.id == id) else {
                return Ok(());
            };
            if let Some(chars) = speak_chars {
                entry.started = true;
                entry.unflushed = true;
                entry.unflushed_chars = entry.unflushed_chars.saturating_add(chars);
            }
            match self.write(message.clone()).await {
                Ok(()) => return Ok(()),
                // The server may have closed an idle socket before we noticed: one fresh
                // connection is safe when nothing of this context was sent.
                Err(Error::Disconnected { .. }) if fresh && !retried => {
                    retried = true;
                    if let Some(entry) = self.entry_mut(id) {
                        entry.wire_reset();
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The socket is gone. Benign while no unfinished utterance has text on the server (the
    /// server closes idle sockets; the next send reconnects), a failure otherwise or when the
    /// failure is not a plain disconnect.
    fn connection_lost(&mut self, error: Error) -> Result<(), Error> {
        self.ws = None;
        // A new connection starts with an empty buffer: nothing left to clear.
        self.clearing = 0;
        self.clear_wanted = false;
        self.carry = None;
        // Only text the server still owes audio for is lost with the connection; a head that
        // was fully answered (a flushed `PerText` context waiting for more text or for the
        // engine flush) loses nothing.
        let at_risk = self.entries.front().is_some_and(Entry::holds_text);
        if at_risk || !matches!(error, Error::Disconnected { .. }) {
            return Err(error);
        }
        if let Some(head) = self.entries.front_mut() {
            head.wire_reset();
        }
        Ok(())
    }

    /// Whether the connection should be replaced before the next utterance (the server limit
    /// is 60 minutes per connection).
    fn connection_is_old(&self) -> bool {
        self.ws.is_some()
            && self
                .clock
                .now()
                .saturating_duration_since(self.connected_at)
                >= self.max_age
    }

    // ----- input ------------------------------------------------------------------------

    fn is_cancelled(&self, context: ContextId) -> bool {
        self.cancelled.contains(&context)
    }

    fn entry_mut(&mut self, context: ContextId) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == context)
    }

    fn on_input(&mut self, input: TtsInput) {
        match input {
            TtsInput::Text { context, text } => {
                if text.is_empty() || self.is_cancelled(context) {
                    return;
                }
                self.enqueue_text(context, &text);
            }
            TtsInput::Flush { context } => {
                if self.is_cancelled(context) {
                    return;
                }
                match self.entry_mut(context) {
                    Some(entry) => {
                        entry.engine_flush = true;
                        entry.wants_flush = true;
                    }
                    None => {
                        let mut entry = Entry::new(context);
                        entry.engine_flush = true;
                        entry.wants_flush = true;
                        self.entries.push_back(entry);
                    }
                }
            }
        }
    }

    fn enqueue_text(&mut self, context: ContextId, text: &str) {
        let per_text = self.policy == FlushPolicy::PerText;
        let pieces = split_text(text, self.max_speak);
        let count = pieces.len();
        if self.entry_mut(context).is_none() {
            self.entries.push_back(Entry::new(context));
        }
        let Some(entry) = self.entry_mut(context) else {
            return;
        };
        if entry.wants_flush {
            // Text after the flush of the same context: it is closed.
            return;
        }
        for (index, text) in pieces.into_iter().enumerate() {
            entry.pending.push_back(Segment {
                text,
                flush_after: per_text && index.saturating_add(1) == count,
            });
        }
    }

    /// The input was dropped: every context is flushed (silently unless the engine flushed it),
    /// then the session ends once the server has answered.
    fn on_input_closed(&mut self) {
        self.input_open = false;
        for entry in &mut self.entries {
            entry.wants_flush = true;
        }
    }

    // ----- sending ----------------------------------------------------------------------

    /// Settles everything that can go out: the pending `Clear`, then the head's text and
    /// flush, completing the head when the server has answered. Cancels taken during a write
    /// can request a `Clear`; it goes out before anything else.
    async fn settle(&mut self) -> Result<(), Error> {
        loop {
            self.flush_clear().await?;
            self.pump().await?;
            if !self.clear_wanted {
                break;
            }
        }
        self.touch(false);
        Ok(())
    }

    /// Sends the `Clear` of a cancelled, started context.
    async fn flush_clear(&mut self) -> Result<(), Error> {
        while self.clear_wanted {
            self.clear_wanted = false;
            if self.ws.is_none() {
                // Nothing to clear on a connection that is gone.
                continue;
            }
            self.clearing = self.clearing.saturating_add(1);
            self.carry = None;
            if let Err(error) = self.write(Message::text(CLEAR)).await {
                self.connection_lost(error)?;
            }
        }
        Ok(())
    }

    /// Whether a `PerText` flush may be sent now (the budget keeps a reserve for turn flushes).
    fn per_text_flush_allowed(&mut self, now: Instant) -> bool {
        self.flushes.count(now).saturating_add(FLUSH_RESERVE) < self.flushes.limit()
    }

    /// Sends a `Flush` for the head `id` and books it.
    async fn send_flush(&mut self, id: ContextId) -> Result<(), Error> {
        let now = self.clock.now();
        self.flushes.record(now);
        let Some(entry) = self.entry_mut(id) else {
            return Ok(());
        };
        entry.owed = entry.owed.saturating_add(1);
        entry.unflushed = false;
        entry.unflushed_chars = 0;
        // Text of this context is already on the wire, so there is nothing to retry blindly.
        self.send(id, FLUSH, false, None).await
    }

    /// Moves the head forward as far as the server and the limits allow.
    async fn pump(&mut self) -> Result<(), Error> {
        loop {
            if self.clear_wanted || self.clearing > 0 {
                return Ok(());
            }
            let now = self.clock.now();
            let Some(head) = self.entries.front() else {
                return Ok(());
            };
            let id = head.id;
            if head.is_complete() {
                self.complete_head();
                continue;
            }
            if !head.started && self.connection_is_old() {
                self.rotate_connection().await;
                // Cancels taken meanwhile may have changed the head or asked for a `Clear`.
                continue;
            }
            if let Some(chars) = head.pending.front().map(|s| s.text.chars().count()) {
                if head.unflushed_chars > 0
                    && head.unflushed_chars.saturating_add(chars) > MID_TURN_FLUSH_CHARS
                {
                    // A very long turn: flush what is on the server before more text, so that
                    // no flush interval can pass the (UNVERIFIED) 2000 character limit.
                    if let Some(wait) = self.flushes.wait(now) {
                        self.arm_gate(wait);
                        return Ok(());
                    }
                    self.send_flush(id).await?;
                    continue;
                }
                if let Some(wait) = self.chars.wait(now, chars) {
                    self.arm_gate(wait);
                    return Ok(());
                }
                self.send_segment(id, chars, now).await?;
                continue;
            }
            if head.wants_flush && !head.flush_sent {
                if head.unflushed {
                    if let Some(wait) = self.flushes.wait(now) {
                        self.arm_gate(wait);
                        return Ok(());
                    }
                    self.send_flush(id).await?;
                }
                if let Some(entry) = self.entry_mut(id) {
                    entry.flush_sent = true;
                }
                continue;
            }
            return Ok(());
        }
    }

    /// Sends the next pending segment of the head `id` (and the `PerText` flush after it).
    async fn send_segment(
        &mut self,
        id: ContextId,
        chars: usize,
        now: Instant,
    ) -> Result<(), Error> {
        let Some(entry) = self.entry_mut(id) else {
            return Ok(());
        };
        let Some(segment) = entry.pending.pop_front() else {
            return Ok(());
        };
        let fresh = !entry.started;
        self.chars.record(now, chars);
        let json = encode_speak(&segment.text)?;
        self.send(id, &json, fresh, Some(chars)).await?;
        if segment.flush_after
            && self.entries.front().map(|e| e.id) == Some(id)
            && self.per_text_flush_allowed(now)
        {
            self.send_flush(id).await?;
        }
        Ok(())
    }

    /// Replaces a connection that is about to hit the server's time limit.
    async fn rotate_connection(&mut self) {
        let Some(mut socket) = self.ws.take() else {
            return;
        };
        let timeout = self.io_timeout.min(CLOSE_TIMEOUT);
        // Cancels and the shutdown token are served while the close is pending.
        let _ = {
            let close = socket.close(None);
            self.serve(close, Some(timeout)).await
        };
        self.carry = None;
    }

    /// The head's last `Flushed` arrived: report it (if the engine asked) and move on.
    fn complete_head(&mut self) {
        if let Some(entry) = self.entries.pop_front() {
            self.carry = None;
            if entry.engine_flush {
                self.out.push_back(TtsEvent::Flushed { context: entry.id });
            }
        }
    }

    // ----- cancel -----------------------------------------------------------------------

    /// The local half of a cancel, which never waits for I/O: remember the id, purge what the
    /// consumer has not received, hand out `Cancelled`, forget the context and, if the server
    /// holds text of it (sent on the current connection, unflushed or with a `Flushed` still
    /// owed), request a `Clear` (sent as soon as the socket is free).
    fn local_cancel(&mut self, context: ContextId) {
        if !self.is_cancelled(context) {
            if self.cancelled.len() >= CANCELLED_MEMORY {
                self.cancelled.pop_front();
            }
            self.cancelled.push_back(context);
        }
        // Whatever the consumer has not received yet is stale now.
        self.out.retain(|e| event_context(e) != Some(context));
        self.out.push_front(TtsEvent::Cancelled { context });
        if let Some(index) = self.entries.iter().position(|e| e.id == context)
            && let Some(entry) = self.entries.remove(index)
            && entry.holds_text()
        {
            self.clear_wanted = true;
            self.carry = None;
        }
        self.touch(false);
        self.deliver();
    }

    // ----- server messages --------------------------------------------------------------

    async fn on_frame(&mut self, frame: Frame) -> Result<(), Error> {
        match frame {
            None => self.connection_lost(Error::Disconnected { provider: PROVIDER }),
            Some(Ok(Message::Close(close))) => {
                let error = close.map_or(Error::Disconnected { provider: PROVIDER }, |frame| {
                    close_error(u16::from(frame.code), frame.reason.as_str())
                });
                self.connection_lost(error)
            }
            Some(Err(error)) => self.connection_lost(map_ws_error(error)),
            Some(Ok(Message::Binary(data))) => {
                self.on_audio(data);
                Ok(())
            }
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()),
            Some(Ok(Message::Ping(_))) => {
                // The pong is queued by the protocol layer; push it out (cancels and the
                // shutdown token are served meanwhile). The server is alive.
                if let Some(mut socket) = self.ws.take() {
                    let served = {
                        let flush = socket.flush();
                        self.serve(flush, Some(self.io_timeout)).await
                    };
                    if !matches!(served, Served::Shutdown) {
                        self.ws = Some(socket);
                    }
                }
                self.touch(true);
                Ok(())
            }
            Some(Ok(Message::Pong(_))) => {
                // The answer to a probe: the server is alive.
                self.touch(true);
                Ok(())
            }
            Some(Ok(Message::Frame(_))) => Ok(()),
        }
    }

    /// A binary frame: audio of the head, unless a `Clear` is on its way or in flight (then it
    /// is the tail of the cancelled utterance) or the head has sent nothing.
    fn on_audio(&mut self, data: Bytes) {
        if self.clear_wanted || self.clearing > 0 {
            // The server is still delivering the cancelled utterance's tail: it is alive.
            self.touch(true);
            return;
        }
        let Some(head) = self.entries.front() else {
            tracing::debug!("deepgram-aura: audio frame with no utterance dropped");
            return;
        };
        if !head.holds_text() || data.is_empty() {
            // Nothing is owed for the head (it was answered, or nothing was sent on this
            // connection): a frame now cannot belong to it.
            tracing::debug!("deepgram-aura: audio frame nothing is owed for dropped");
            return;
        }
        let context = head.id;
        let audio = if self.bytes_per_sample == 2 && (self.carry.is_some() || data.len() % 2 == 1) {
            align_pcm16(&mut self.carry, &data)
        } else {
            data
        };
        if audio.is_empty() {
            return;
        }
        self.out.push_back(TtsEvent::Audio { context, audio });
        self.touch(true);
    }

    fn on_text(&mut self, text: &str) -> Result<(), Error> {
        match Incoming::parse(text)? {
            Incoming::Metadata { request_id } => {
                tracing::debug!(%request_id, "deepgram-aura metadata");
            }
            Incoming::Flushed => self.on_flushed(),
            Incoming::Cleared => self.on_cleared(),
            Incoming::Warning { code, description } => {
                tracing::warn!(%code, %description, "deepgram-aura warning");
            }
            Incoming::Error { code, description } => {
                return Err(Error::Protocol {
                    provider: PROVIDER,
                    detail: format!("server error {code}: {description}"),
                });
            }
            Incoming::Other(kind) => {
                tracing::debug!(%kind, "deepgram-aura: ignoring unknown message type");
            }
        }
        Ok(())
    }

    /// `Flushed`: the oldest flush of the head is answered. While a `Clear` is pending it
    /// belongs to the cancelled utterance and is dropped.
    fn on_flushed(&mut self) {
        if self.clear_wanted || self.clearing > 0 {
            self.touch(true);
            return;
        }
        match self.entries.front_mut() {
            Some(head) if head.owed > 0 => {
                head.owed = head.owed.saturating_sub(1);
                self.touch(true);
            }
            _ => tracing::debug!("deepgram-aura: unexpected Flushed ignored"),
        }
    }

    /// `Cleared`: the cancelled utterance's tail is over; the next context may go out.
    fn on_cleared(&mut self) {
        if self.clearing == 0 {
            tracing::debug!("deepgram-aura: unexpected Cleared ignored");
            return;
        }
        self.clearing = self.clearing.saturating_sub(1);
        self.touch(true);
    }
}

/// Completes PCM16 samples across frame boundaries: prepends the carried byte, keeps a new odd
/// trailing byte for the next frame. Rare path (frames are normally whole samples; alignment
/// is undocumented, UNVERIFIED).
fn align_pcm16(carry: &mut Option<u8>, audio: &Bytes) -> Bytes {
    let mut joined = BytesMut::with_capacity(audio.len().saturating_add(1));
    if let Some(byte) = carry.take() {
        joined.put_u8(byte);
    }
    joined.extend_from_slice(audio);
    if joined.len() % 2 == 1 {
        let last = joined.split_off(joined.len().saturating_sub(1));
        *carry = last.first().copied();
    }
    joined.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_holds_server_text_only_while_something_is_unflushed_or_owed() {
        let mut entry = Entry::new(ContextId(1));
        assert!(!entry.holds_text(), "nothing sent");
        entry.started = true;
        entry.unflushed = true;
        entry.unflushed_chars = 10;
        assert!(entry.holds_text(), "text waits for a flush");
        entry.unflushed = false;
        entry.unflushed_chars = 0;
        entry.owed = 1;
        assert!(entry.holds_text(), "a Flushed is owed");
        entry.owed = 0;
        assert!(
            !entry.holds_text(),
            "fully answered: nothing to clear or lose"
        );
    }

    #[test]
    fn a_replaced_connection_forgets_what_the_old_one_knew() {
        let mut entry = Entry::new(ContextId(1));
        entry.started = true;
        entry.unflushed = true;
        entry.unflushed_chars = 42;
        entry.wire_reset();
        assert!(!entry.started && !entry.unflushed && entry.unflushed_chars == 0);
        assert!(!entry.holds_text());
    }

    /// The two limits that keep a turn clear of the server's flush and request limits.
    #[test]
    fn the_flush_reserve_and_the_mid_turn_threshold_are_pinned() {
        // Docs: at most 20 flushes per 60 s; `PerText` flushes stop 4 short of the limit.
        assert_eq!(FLUSH_RESERVE, 4);
        // Docs: 2000 characters per request (scope UNVERIFIED): 200 characters of margin.
        assert_eq!(MID_TURN_FLUSH_CHARS, 1800);
        // Bounded queues.
        assert_eq!(OUT_HIGH_WATER, 64);
        assert_eq!(MAX_ENTRIES, 64);
        assert_eq!(MAX_PENDING_SEGMENTS, 128);
    }

    #[test]
    fn odd_frames_are_realigned_to_whole_samples() {
        let mut carry = None;
        let first = align_pcm16(&mut carry, &Bytes::from_static(&[1, 2, 3]));
        assert_eq!(&first[..], &[1, 2]);
        assert_eq!(carry, Some(3));
        let second = align_pcm16(&mut carry, &Bytes::from_static(&[4, 5, 6]));
        assert_eq!(&second[..], &[3, 4, 5, 6]);
        assert_eq!(carry, None);
    }
}
