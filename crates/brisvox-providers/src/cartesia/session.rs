//! The task that owns one Cartesia WebSocket and turns engine input into requests and server
//! messages into [`TtsEvent`]s (module docs: `super`).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_for};
use brisvox_core::{Error as CoreError, SharedClock};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Sleep;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::context::Live;
use super::wire::{
    AudioDecoder, Envelope, Incoming, RequestKind, RequestTemplate, encode_cancel,
    is_concurrency_limited, map_server_error,
};
use super::{Inner, PROVIDER, Socket, map_ws_error, server_context_id, with_timeout};
use crate::error::Error;
use crate::tts::{
    ContextId, TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY, TtsConfig, TtsEvent, TtsInput, TtsSession,
};

/// The socket is not read while the outgoing queue holds this many events.
const OUT_HIGH_WATER: usize = 64;

/// Cancelled contexts remembered, so input still queued for them is ignored.
const CANCELLED_MEMORY: usize = 64;

/// Engine contexts kept at once. A context ends with the engine's flush or cancel; one that
/// never does is evicted (oldest first) when a new one would exceed this.
const MAX_LIVE: usize = 64;

/// Longest wait for the polite close of the socket at the end of a session.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Re-opens of a context refused for concurrency before the refusal ends the session.
const MAX_CONCURRENCY_RETRIES: u8 = 4;

/// Re-opens of an expired context (an error on a context that had produced output) before the
/// error ends the session.
const MAX_EXPIRY_REOPENS: u8 = 2;

/// Why the task stopped.
enum End {
    /// The input is closed and nothing is owed: close the socket politely.
    Graceful,
    /// The consumer is gone or the provider shut down: just drop everything.
    Closed,
    /// The session failed: deliver what is queued, then the error.
    Fatal(Error),
}

type Frame = Option<Result<Message, WsError>>;

/// Spawns the session task around `socket`.
pub(super) fn spawn(
    inner: Arc<Inner>,
    socket: Socket,
    config: &TtsConfig,
    template: RequestTemplate,
    session: u64,
) -> TtsSession {
    let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let clock = Arc::clone(&inner.config.clock);
    let io_timeout = inner.config.io_timeout;
    let task = Task {
        shutdown: inner.config.shutdown.clone(),
        flush_after_text: inner.config.flush_after_text,
        expiry: inner.config.context_expiry,
        retry_delay: inner.config.concurrency_retry_delay,
        bytes_per_sample: inner.config.encoding.bytes_per_sample(),
        rate: config.sample_rate,
        idle: Box::pin(clock.sleep(io_timeout)),
        retry: Box::pin(clock.sleep(io_timeout)),
        clock,
        io_timeout,
        inner,
        template,
        session,
        next_context: 0,
        ws: Some(socket),
        live: Vec::new(),
        cancelled: VecDeque::with_capacity(CANCELLED_MEMORY),
        out: VecDeque::with_capacity(OUT_HIGH_WATER),
        wire_queue: VecDeque::new(),
        decoder: AudioDecoder::default(),
        events: events_tx,
        input_rx,
        cancel_rx,
        input_open: true,
        cancel_open: true,
        armed: false,
        retry_armed: false,
        paused: false,
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
    flush_after_text: bool,
    expiry: Duration,
    retry_delay: Duration,
    bytes_per_sample: u64,
    rate: brisvox_core::frame::SampleRate,
    template: RequestTemplate,
    session: u64,
    next_context: u64,
    /// `None` after the server closed an idle socket (reconnected lazily), after a failure, or
    /// while a write has the socket in hand.
    ws: Option<Socket>,
    /// Engine contexts with text sent that the engine has not ended yet, at most `MAX_LIVE`
    /// (oldest first). A linear scan is fine.
    live: Vec<Live>,
    cancelled: VecDeque<ContextId>,
    /// Events not yet handed to the consumer. Bounded by `OUT_HIGH_WATER` plus one message
    /// (plus the cancel acknowledgements, which are tiny and replace purged events).
    out: VecDeque<TtsEvent>,
    /// Requests that must go out next but were produced while a write was in flight (cancels
    /// of barge-ins that arrived during it, the close of an evicted context). One entry per
    /// cancel or eviction, so bounded by the cancel channel's rate; drained after every step.
    wire_queue: VecDeque<String>,
    decoder: AudioDecoder,
    events: mpsc::Sender<TtsEvent>,
    input_rx: mpsc::Receiver<TtsInput>,
    cancel_rx: mpsc::Receiver<ContextId>,
    input_open: bool,
    cancel_open: bool,
    /// Fires `io_timeout` after the last progress while the server owes output.
    idle: Pin<Box<Sleep>>,
    armed: bool,
    /// Fires when a refused context is due to be opened again.
    retry: Pin<Box<Sleep>>,
    retry_armed: bool,
    /// The socket is not being read because the consumer is behind (backpressure).
    paused: bool,
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

fn protocol(detail: &str) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.to_owned(),
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
                    let _ =
                        with_timeout(&*clock, timeout, "cartesia close", socket.close(None)).await;
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
            && self.out.is_empty()
            && !self
                .live
                .iter()
                .any(|l| l.server_open || l.retry_at.is_some())
    }

    async fn drive(&mut self) -> End {
        loop {
            if self.finished() {
                return End::Graceful;
            }
            let room = self.out.len() < OUT_HIGH_WATER;
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
                    Some(context) => self.on_cancel(context).await,
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
                () = &mut self.idle, if self.armed && room => Err(Error::Core(CoreError::Timeout {
                    what: "cartesia response",
                    after: self.io_timeout,
                })),
                () = &mut self.retry, if self.retry_armed => self.on_retry().await,
                frame = next_frame(&mut self.ws), if room && self.ws.is_some() => {
                    self.on_frame(frame).await
                }
                input = self.input_rx.recv(), if room && self.input_open => match input {
                    Some(input) => self.on_input(input).await,
                    None => self.on_input_closed().await,
                },
            };
            let step = match step {
                Ok(()) => self.flush_wire().await,
                error => error,
            };
            if let Err(error) = step {
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
    fn pump(&mut self) {
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

    /// Re-evaluates the stall deadline: armed while the server owes output, restarted by
    /// `progress` (a message for a live context) or when it was not armed yet.
    fn touch(&mut self, progress: bool) {
        if !self.live.iter().any(Live::awaiting_server) {
            self.armed = false;
            return;
        }
        if progress || !self.armed {
            let deadline = deadline_after(self.clock.now(), self.io_timeout);
            self.idle.as_mut().reset(deadline);
            self.armed = true;
        }
    }

    /// Arms the retry timer for the earliest scheduled re-open.
    fn arm_retry(&mut self) {
        match self.live.iter().filter_map(|l| l.retry_at).min() {
            Some(at) => {
                self.retry.as_mut().reset(at);
                self.retry_armed = true;
            }
            None => self.retry_armed = false,
        }
    }

    // ----- socket -----------------------------------------------------------------------

    /// Sends one frame within the deadline. While the send is pending, cancels are still taken:
    /// the local acknowledgement goes out at once and the wire cancel is queued behind the send
    /// (`wire_queue`).
    async fn write(&mut self, message: Message) -> Result<(), Error> {
        let Some(mut socket) = self.ws.take() else {
            return Err(Error::Disconnected { provider: PROVIDER });
        };
        let clock = Arc::clone(&self.clock);
        let outcome = {
            let send = socket.send(message);
            let timeout = wait_for(&*clock, self.io_timeout);
            tokio::pin!(send, timeout);
            loop {
                tokio::select! {
                    biased;
                    sent = &mut send => break Some(sent),
                    () = &mut timeout => break None,
                    cancel = self.cancel_rx.recv(), if self.cancel_open => match cancel {
                        Some(context) => self.local_cancel(context),
                        None => self.cancel_open = false,
                    },
                }
            }
        };
        match outcome {
            Some(Ok(())) => {
                self.ws = Some(socket);
                Ok(())
            }
            // The socket is dropped: after a failure or a half-written frame it is unusable.
            Some(Err(error)) => Err(map_ws_error(error)),
            None => Err(Error::Core(CoreError::Timeout {
                what: "cartesia send",
                after: self.io_timeout,
            })),
        }
    }

    /// Opens a socket if there is none (not the one parked for the next `connect`). Cancels
    /// are still taken while connecting.
    async fn ensure_connected(&mut self) -> Result<(), Error> {
        if self.ws.is_some() {
            return Ok(());
        }
        let inner = Arc::clone(&self.inner);
        let socket = {
            let connect = inner.open();
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    biased;
                    opened = &mut connect => break opened?,
                    cancel = self.cancel_rx.recv(), if self.cancel_open => match cancel {
                        Some(context) => self.local_cancel(context),
                        None => self.cancel_open = false,
                    },
                }
            }
        };
        self.ws = Some(socket);
        Ok(())
    }

    /// Sends what `wire_queue` holds (barge-in cancels that arrived during a write).
    async fn flush_wire(&mut self) -> Result<(), Error> {
        while let Some(json) = self.wire_queue.pop_front() {
            if self.ws.is_none() {
                self.wire_queue.clear();
                break;
            }
            if let Err(error) = self.write(Message::text(json)).await {
                self.connection_lost(error)?;
            }
        }
        Ok(())
    }

    /// The socket is gone. Benign while the server owes nothing (the server closes idle
    /// sockets after 5 minutes; the next text reconnects and opens new server contexts), a
    /// failure otherwise.
    fn connection_lost(&mut self, error: Error) -> Result<(), Error> {
        self.ws = None;
        self.wire_queue.clear();
        if self.live.iter().any(Live::awaiting_server) {
            return Err(error);
        }
        for live in &mut self.live {
            live.server_open = false;
        }
        Ok(())
    }

    // ----- input ------------------------------------------------------------------------

    fn find(&self, context: ContextId) -> Option<&Live> {
        self.live.iter().find(|l| l.id == context)
    }

    fn find_mut(&mut self, context: ContextId) -> Option<&mut Live> {
        self.live.iter_mut().find(|l| l.id == context)
    }

    fn remove(&mut self, context: ContextId) -> Option<Live> {
        let idx = self.live.iter().position(|l| l.id == context)?;
        Some(self.live.remove(idx))
    }

    async fn on_input(&mut self, input: TtsInput) -> Result<(), Error> {
        match input {
            TtsInput::Text { context, text } => {
                if text.is_empty() || self.is_cancelled(context) {
                    return Ok(());
                }
                self.send_text(context, &text).await?;
            }
            TtsInput::Flush { context } => {
                if self.is_cancelled(context) {
                    return Ok(());
                }
                self.on_flush(context).await?;
            }
        }
        self.touch(false);
        Ok(())
    }

    async fn on_flush(&mut self, context: ContextId) -> Result<(), Error> {
        let now = self.clock.now();
        let (expiry, markers) = (self.expiry, self.flush_after_text);
        let Some(live) = self.find_mut(context) else {
            // Nothing was sent for it (or it already ended): nothing is buffered.
            self.out.push_back(TtsEvent::Flushed { context });
            return Ok(());
        };
        if live.finish_sent || live.finish_wanted {
            return Ok(());
        }
        live.engine_flush = true;
        if live.retry_at.is_some() {
            // A re-open is pending: the closing request follows it.
            live.finish_wanted = true;
            return Ok(());
        }
        if !live.server_open || live.expired(now, expiry, markers) {
            // The server context is gone (closed, lost with the socket, expired): everything
            // it owed has been delivered, so the end of the context is reached.
            self.remove(context);
            self.out.push_back(TtsEvent::Flushed { context });
            return Ok(());
        }
        self.finish(context).await
    }

    /// The input was dropped: close every context that is still open (and silently: the engine
    /// did not ask for `Flushed`), then wait for the servers' `done`s and end.
    async fn on_input_closed(&mut self) -> Result<(), Error> {
        self.input_open = false;
        let now = self.clock.now();
        let (expiry, markers) = (self.expiry, self.flush_after_text);
        let ids: Vec<ContextId> = self.live.iter().map(|l| l.id).collect();
        for id in ids {
            let Some(live) = self.find_mut(id) else {
                continue;
            };
            if live.finish_sent
                || live.finish_wanted
                || (!live.server_open && live.retry_at.is_none())
            {
                continue;
            }
            if live.retry_at.is_some() {
                live.finish_wanted = true;
            } else if live.expired(now, expiry, markers) {
                live.server_open = false;
            } else {
                self.finish(id).await?;
            }
        }
        // The closing requests owe a `done`: that wait carries the stall timer too.
        self.touch(false);
        Ok(())
    }

    /// Sends the closing request (`continue: false`, empty transcript) of `context`.
    async fn finish(&mut self, context: ContextId) -> Result<(), Error> {
        let Some(live) = self.live.iter_mut().find(|l| l.id == context) else {
            return Ok(());
        };
        let json = self
            .template
            .encode(&live.server_id, "", RequestKind::Finish)?;
        live.finish_sent = true;
        self.write(Message::text(json)).await
    }

    /// Sends text of `context`, opening (or re-opening) its server context first if needed.
    async fn send_text(&mut self, context: ContextId, text: &str) -> Result<(), Error> {
        let now = self.clock.now();
        let (expiry, markers) = (self.expiry, self.flush_after_text);
        match self.find_mut(context) {
            Some(live) if live.finish_sent || live.finish_wanted => {
                // Text after the flush of the same context: the server context is closed.
                return Ok(());
            }
            Some(live) if live.retry_at.is_some() => {
                // A re-open is pending: the text joins what it re-sends.
                live.remember_text(text);
                return Ok(());
            }
            Some(live) if live.server_open && !live.expired(now, expiry, markers) => {}
            _ => {
                if !self.open_server_context(context, false).await? {
                    return Ok(());
                }
            }
        }
        self.send_chunk(context, text, true).await
    }

    /// Gives `context` a new server context on a connected socket. `false`: it was cancelled
    /// while connecting. The audio count and word base of an existing engine context carry
    /// over, so the offsets continue; `keep_pending` keeps the text to re-send.
    async fn open_server_context(
        &mut self,
        context: ContextId,
        keep_pending: bool,
    ) -> Result<bool, Error> {
        self.ensure_connected().await?;
        if self.is_cancelled(context) {
            return Ok(false);
        }
        let now = self.clock.now();
        let server_id = server_context_id(self.session, self.next_context);
        self.next_context = self.next_context.saturating_add(1);
        let (rate, bytes_per_sample) = (self.rate, self.bytes_per_sample);
        if let Some(live) = self.find_mut(context) {
            let audio = live.audio_duration(rate, bytes_per_sample);
            live.reopen(server_id, audio, now);
            if !keep_pending {
                live.clear_pending();
                live.attempts = 0;
                live.lost_dones = 0;
            }
        } else {
            self.make_room();
            self.live.push(Live::new(context, server_id, now));
        }
        Ok(true)
    }

    /// Evicts the oldest context (one that has no open server context first) when the table is
    /// full. An evicted open server context is closed on the wire; if the engine uses its id
    /// again it starts a new context (offsets restart).
    fn make_room(&mut self) {
        if self.live.len() < MAX_LIVE {
            return;
        }
        let victim = self
            .live
            .iter()
            .position(|l| !l.server_open && l.retry_at.is_none())
            .unwrap_or(0);
        if victim >= self.live.len() {
            return;
        }
        let gone = self.live.remove(victim);
        if gone.server_open
            && let Ok(json) = self
                .template
                .encode(&gone.server_id, "", RequestKind::Finish)
        {
            self.wire_queue.push_back(json);
        }
        self.arm_retry();
    }

    /// Writes one text request (and its flush marker) to the current server context of
    /// `context`. A write that finds the socket dead is retried once on a fresh socket if
    /// nothing of the server context had been sent yet.
    async fn send_chunk(
        &mut self,
        context: ContextId,
        text: &str,
        record: bool,
    ) -> Result<(), Error> {
        let mut retried = false;
        loop {
            let Some(live) = self.find(context) else {
                return Ok(()); // cancelled meanwhile
            };
            let fresh = !live.established && live.pending_text.is_empty() && live.markers_out == 0;
            let json = self
                .template
                .encode(&live.server_id, text, RequestKind::Text)?;
            match self.write(Message::text(json)).await {
                Ok(()) => break,
                // The server may have closed an idle socket before we noticed: one fresh
                // connection is safe when nothing of this server context was sent.
                Err(error @ Error::Disconnected { .. }) if fresh && !retried => {
                    retried = true;
                    self.connection_lost(error)?;
                    if !self.open_server_context(context, true).await? {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let now = self.clock.now();
        let markers = self.flush_after_text;
        let Some(live) = self.live.iter_mut().find(|l| l.id == context) else {
            return Ok(());
        };
        live.last_activity = now;
        if record && (markers || !live.established) {
            live.remember_text(text);
        }
        if markers {
            let json = self
                .template
                .encode(&live.server_id, "", RequestKind::Marker)?;
            live.markers_out = live.markers_out.saturating_add(1);
            self.write(Message::text(json)).await?;
        } else {
            live.first_pending = true;
        }
        Ok(())
    }

    // ----- cancel -----------------------------------------------------------------------

    fn is_cancelled(&self, context: ContextId) -> bool {
        self.cancelled.contains(&context)
    }

    /// The local half of a cancel, which never waits for I/O: remember the id, purge what the
    /// consumer has not received, hand out `Cancelled`, forget the context and queue the wire
    /// cancel.
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
        if let Some(live) = self.remove(context)
            && live.server_open
            && let Ok(json) = encode_cancel(&live.server_id)
        {
            self.wire_queue.push_back(json);
        }
        self.arm_retry();
        self.touch(false);
        self.pump();
    }

    async fn on_cancel(&mut self, context: ContextId) -> Result<(), Error> {
        self.local_cancel(context);
        self.flush_wire().await
    }

    // ----- server messages --------------------------------------------------------------

    async fn on_frame(&mut self, frame: Frame) -> Result<(), Error> {
        match frame {
            None | Some(Ok(Message::Close(_))) => {
                self.connection_lost(Error::Disconnected { provider: PROVIDER })
            }
            Some(Err(error)) => self.connection_lost(map_ws_error(error)),
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()).await,
            Some(Ok(Message::Ping(_))) => {
                // The pong is queued by the protocol layer; push it out.
                if let Some(socket) = self.ws.as_mut() {
                    let _ = with_timeout(
                        &*self.clock,
                        self.io_timeout,
                        "cartesia pong",
                        socket.flush(),
                    )
                    .await;
                }
                Ok(())
            }
            Some(Ok(Message::Pong(_))) => Ok(()),
            Some(Ok(Message::Binary(_) | Message::Frame(_))) => {
                Err(protocol("unexpected binary frame"))
            }
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), Error> {
        // Route first, leniently: a message for a context that is not live (cancelled,
        // finished, replaced by a re-open, unknown) is dropped even if it is malformed.
        let envelope = Envelope::parse(text)?;
        let kind = envelope.kind();
        let Some(server_id) = envelope.context_id.as_deref() else {
            // Not about a context: an error here is about the connection.
            return if kind == "error" {
                Err(map_server_error(&Incoming::parse(text)?))
            } else {
                Ok(())
            };
        };
        let Some(idx) = self
            .live
            .iter()
            .position(|l| l.server_open && l.server_id == server_id)
        else {
            return Ok(());
        };
        let incoming = Incoming::parse(text)?;
        let Some(live) = self.live.get_mut(idx) else {
            return Ok(());
        };
        let id = live.id;
        if kind == "error" {
            return self.on_error(id, &incoming).await;
        }
        live.first_pending = false;
        live.last_activity = self.clock.now();
        let progress = matches!(kind, "chunk" | "timestamps" | "flush_done" | "done");
        if progress {
            live.established = true;
            live.attempts = 0;
            if !self.flush_after_text {
                live.clear_pending();
            }
        }
        match kind {
            "chunk" => self.on_chunk(idx, &incoming)?,
            "timestamps" => self.on_timestamps(idx, &incoming),
            "flush_done" => self.on_flush_done(idx),
            // `done`, phoneme timestamps and message types this version does not know.
            _ => {}
        }
        if kind == "done" || incoming.done == Some(true) {
            self.on_done(idx).await?;
        }
        self.touch(true);
        Ok(())
    }

    /// A server `error` for a live context. Mostly the end of the session; two cases are
    /// recoverable by opening a new server context for that engine context only:
    /// - `concurrency_limited` before the context produced anything: a context cancelled by a
    ///   barge-in may still count against the plan while the server finishes it, so the open
    ///   is retried after a growing delay (`MAX_CONCURRENCY_RETRIES` times);
    /// - any other non-refusal error after the context had produced output: the server
    ///   expired or lost it (the docs say input on an expired context is "not supported"), so
    ///   the text that was not answered yet is sent again to a new one.
    async fn on_error(&mut self, id: ContextId, incoming: &Incoming<'_>) -> Result<(), Error> {
        let error = map_server_error(incoming);
        let now = self.clock.now();
        let (delay, markers) = (self.retry_delay, self.flush_after_text);
        let concurrency = is_concurrency_limited(incoming);
        let Some(live) = self.find_mut(id) else {
            return Err(error);
        };
        if live.resend_lost {
            return Err(error);
        }
        // A closing request that went to the failed server context is void; it is sent again
        // to the new one (or the context is complete if there is nothing left to say).
        let finishing = live.finish_sent || live.finish_wanted;
        if concurrency && !live.established && live.attempts < MAX_CONCURRENCY_RETRIES {
            live.attempts = live.attempts.saturating_add(1);
            live.server_open = false;
            live.markers_out = 0;
            live.first_pending = false;
            live.finish_sent = false;
            live.finish_wanted = finishing;
            let wait = delay.saturating_mul(u32::from(live.attempts));
            live.retry_at = Some(deadline_after(now, wait));
            self.arm_retry();
            self.touch(true);
            return Ok(());
        }
        if live.established
            && markers
            && !matches!(error, Error::Rejected { .. })
            && live.attempts < MAX_EXPIRY_REOPENS
        {
            return self.reopen_unanswered(id).await;
        }
        Err(error)
    }

    /// The server context of `id` is gone (expired or lost) and may have left text
    /// unanswered: forgets it and sends the unanswered text again to a new one. A closing
    /// request that went to the lost context is void; it follows the re-send (or, with nothing
    /// left to say, the context is complete).
    async fn reopen_unanswered(&mut self, id: ContextId) -> Result<(), Error> {
        let Some(live) = self.find_mut(id) else {
            return Ok(());
        };
        let finishing = live.finish_sent || live.finish_wanted;
        live.attempts = live.attempts.saturating_add(1);
        live.server_open = false;
        live.markers_out = 0;
        live.first_pending = false;
        live.finish_sent = false;
        live.finish_wanted = finishing;
        if live.pending_text.is_empty() {
            // Nothing unanswered. If the engine was closing the context it is complete
            // (the expired server context had delivered everything); otherwise the next
            // text opens a new server context.
            if finishing {
                let engine_flush = live.engine_flush;
                self.remove(id);
                if engine_flush {
                    self.out.push_back(TtsEvent::Flushed { context: id });
                }
            }
            self.touch(true);
            return Ok(());
        }
        self.resend(id).await
    }

    /// Opens a new server context for `id` and sends the text the old one did not answer.
    async fn resend(&mut self, id: ContextId) -> Result<(), Error> {
        if !self.open_server_context(id, true).await? {
            return Ok(());
        }
        let Some(text) = self.find(id).map(|l| l.pending_text.clone()) else {
            return Ok(());
        };
        if !text.is_empty() {
            self.send_chunk(id, &text, false).await?;
        }
        let input_open = self.input_open;
        let finish = self
            .find(id)
            .is_some_and(|l| (l.finish_wanted || !input_open) && !l.finish_sent);
        if finish {
            self.finish(id).await?;
        }
        Ok(())
    }

    /// The retry timer fired: open the contexts that are due.
    async fn on_retry(&mut self) -> Result<(), Error> {
        self.retry_armed = false;
        let now = self.clock.now();
        let due: Vec<ContextId> = self
            .live
            .iter()
            .filter(|l| l.retry_at.is_some_and(|at| at <= now))
            .map(|l| l.id)
            .collect();
        for id in due {
            if let Some(live) = self.find_mut(id) {
                live.retry_at = None;
            }
            self.resend(id).await?;
        }
        self.arm_retry();
        self.touch(false);
        Ok(())
    }

    fn on_chunk(&mut self, idx: usize, incoming: &Incoming<'_>) -> Result<(), Error> {
        let Some(data) = incoming.data.as_deref().filter(|d| !d.is_empty()) else {
            return Ok(());
        };
        let Some(live) = self.live.get_mut(idx) else {
            return Ok(());
        };
        let mut audio = self.decoder.decode(data)?;
        if self.bytes_per_sample == 2 && (live.carry.is_some() || audio.len() % 2 == 1) {
            audio = align_pcm16(&mut live.carry, &audio);
        }
        if audio.is_empty() {
            return Ok(());
        }
        live.audio_bytes = live.audio_bytes.saturating_add(audio.len() as u64);
        self.out.push_back(TtsEvent::Audio {
            context: live.id,
            audio,
        });
        Ok(())
    }

    /// Word times only drive truncation, so a bad time never fails the session: missing or
    /// invalid ones are clamped by the clock (`WordClock::offset`).
    fn on_timestamps(&mut self, idx: usize, incoming: &Incoming<'_>) {
        let Some(timestamps) = incoming.word_timestamps.as_ref() else {
            return;
        };
        let Some(live) = self.live.get_mut(idx) else {
            return;
        };
        if let Some(first) = timestamps.start.first() {
            live.words.begin_message(*first);
        }
        for (position, word) in timestamps.words.iter().enumerate() {
            let offset = live.words.offset(timestamps.start.get(position).copied());
            self.out.push_back(TtsEvent::Word {
                context: live.id,
                word: Arc::from(word.as_str()),
                start: offset,
            });
        }
    }

    fn on_flush_done(&mut self, idx: usize) {
        let (rate, bytes_per_sample) = (self.rate, self.bytes_per_sample);
        if let Some(live) = self.live.get_mut(idx) {
            live.markers_out = live.markers_out.saturating_sub(1);
            let audio = live.audio_duration(rate, bytes_per_sample);
            live.words.boundary(audio);
            live.lost_dones = 0;
            if live.markers_out == 0 {
                live.clear_pending();
            }
        }
    }

    /// The server finished the context. If the engine (or the closing of the input) asked for
    /// the end, the context ends: `Flushed` follows for an engine `Flush`. A `done` nobody asked
    /// for (the server expired the context) only closes the server context; the engine
    /// context lives on and its next text opens a new one.
    ///
    /// With flush markers a solicited `done` always follows the `flush_done` of every marker,
    /// so a `done` that finds a marker unanswered (or text not yet confirmed) means the server
    /// expired the context just as that text arrived: the text was never spoken. It is treated
    /// like the expiry `error` (the unanswered text goes to a new server context, together
    /// with the closing request if one was sent or wanted); it is never a completion, and
    /// never a `Flushed`. A server that keeps doing this fails the session after
    /// `MAX_EXPIRY_REOPENS` re-sends without a confirmed answer.
    async fn on_done(&mut self, idx: usize) -> Result<(), Error> {
        let markers = self.flush_after_text;
        let Some(live) = self.live.get_mut(idx) else {
            return Ok(());
        };
        let unanswered = markers && (live.markers_out > 0 || !live.pending_text.is_empty());
        if unanswered {
            let id = live.id;
            if live.resend_lost || live.lost_dones >= MAX_EXPIRY_REOPENS {
                return Err(protocol(
                    "context closed by the server with unanswered text",
                ));
            }
            live.lost_dones = live.lost_dones.saturating_add(1);
            return self.reopen_unanswered(id).await;
        }
        live.server_open = false;
        live.markers_out = 0;
        live.first_pending = false;
        live.clear_pending();
        if live.finish_sent {
            let live = self.live.remove(idx);
            if live.engine_flush {
                self.out.push_back(TtsEvent::Flushed { context: live.id });
            }
        }
        Ok(())
    }
}

/// Completes PCM16 samples across chunk boundaries: prepends the carried byte, keeps a new odd
/// trailing byte for the next chunk. Rare path (chunks are normally whole samples).
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
    fn odd_chunks_are_realigned_to_whole_samples() {
        let mut carry = None;
        let first = align_pcm16(&mut carry, &Bytes::from_static(&[1, 2, 3]));
        assert_eq!(&first[..], &[1, 2]);
        assert_eq!(carry, Some(3));
        let second = align_pcm16(&mut carry, &Bytes::from_static(&[4, 5, 6]));
        assert_eq!(&second[..], &[3, 4, 5, 6]);
        assert_eq!(carry, None);
    }
}
