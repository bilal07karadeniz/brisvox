//! The task that owns one ElevenLabs WebSocket and turns engine input into client frames and
//! server messages into [`TtsEvent`]s (module docs: `super`).

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

use super::context::{Live, MAX_LIVE, count_spoken};
use super::wire::{
    AudioDecoder, Envelope, Incoming, close_frame, flush_frame, keepalive_frame, map_server_error,
    text_frame,
};
use super::{
    Inner, PROVIDER, Socket, Template, map_close_code, map_ws_error, server_context_id,
    with_timeout,
};
use crate::error::Error;
use crate::tts::{
    ContextId, TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY, TtsEvent, TtsInput, TtsSession,
};

/// The socket is not read while the outgoing queue holds this many events.
const OUT_HIGH_WATER: usize = 64;

/// Cancelled contexts remembered, so input still queued for them is ignored.
const CANCELLED_MEMORY: usize = 64;

/// Flushed contexts remembered (their `Flushed` was produced), so text and a second flush that
/// arrive for them later are ignored whether or not the server's `isFinal` had come already.
const ENDED_MEMORY: usize = 64;

/// Most entries one alignment array of a server message may have. A real message covers one
/// generated chunk (a few hundred characters at most); more is a misbehaving endpoint, and the
/// words of one message are queued in memory before the socket is paused (a protocol error).
const MAX_ALIGNMENT_CHARS: usize = 16 * 1024;

/// Longest wait for the polite close of the socket at the end of a session.
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

type Frame = Option<Result<Message, WsError>>;

/// Spawns the session task around `socket`.
pub(super) fn spawn(
    inner: Arc<Inner>,
    socket: Socket,
    template: Template,
    session: u64,
) -> TtsSession {
    let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
    let clock = Arc::clone(&inner.config.clock);
    let io_timeout = inner.config.io_timeout;
    let task = Task {
        shutdown: inner.config.shutdown.clone(),
        keepalive: inner.config.keepalive_interval,
        grace: inner.config.final_grace,
        by_alignment: inner.config.completion_by_alignment,
        normalized_words: inner.config.use_normalized_alignment,
        sync_alignment: inner.config.sync_alignment,
        idle: Box::pin(clock.sleep(io_timeout)),
        wake: Box::pin(clock.sleep(io_timeout)),
        clock,
        io_timeout,
        inner,
        template,
        session,
        next_context: 0,
        ws: Some(socket),
        live: Vec::new(),
        cancelled: VecDeque::with_capacity(CANCELLED_MEMORY),
        ended: VecDeque::with_capacity(ENDED_MEMORY),
        out: VecDeque::with_capacity(OUT_HIGH_WATER),
        wire_queue: VecDeque::new(),
        decoder: AudioDecoder::default(),
        events: events_tx,
        input_rx,
        cancel_rx,
        input_open: true,
        cancel_open: true,
        armed: false,
        wake_armed: false,
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
    keepalive: Duration,
    grace: Duration,
    by_alignment: bool,
    normalized_words: bool,
    sync_alignment: bool,
    template: Template,
    session: u64,
    next_context: u64,
    /// `None` after the server closed an idle socket (reconnected lazily), after a failure, or
    /// while a write has the socket in hand.
    ws: Option<Socket>,
    /// Engine contexts with text sent that the engine has not ended yet, at most `MAX_LIVE`
    /// (oldest first). A linear scan is fine.
    live: Vec<Live>,
    cancelled: VecDeque<ContextId>,
    /// Engine contexts whose `Flushed` was produced (at most `ENDED_MEMORY`, oldest forgotten
    /// first). Later text and flushes for them are ignored, so the same input sequence behaves
    /// the same whether the server's `isFinal` arrived before it or after.
    ended: VecDeque<ContextId>,
    /// Events not yet handed to the consumer. Bounded by `OUT_HIGH_WATER` plus one message
    /// (plus the cancel acknowledgements, which are tiny and replace purged events).
    out: VecDeque<TtsEvent>,
    /// Frames that must go out next but were produced while a write was in flight (cancels
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
    /// Fires when a keepalive is due or a closing context's grace has run out.
    wake: Pin<Box<Sleep>>,
    wake_armed: bool,
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

/// Adds `context` to a bounded memory of ids, forgetting the oldest at the cap.
fn remember(list: &mut VecDeque<ContextId>, cap: usize, context: ContextId) {
    if list.contains(&context) {
        return;
    }
    if list.len() >= cap {
        list.pop_front();
    }
    list.push_back(context);
}

/// Ends the unfinished word of `live` and queues it.
fn flush_words(live: &mut Live, out: &mut VecDeque<TtsEvent>) {
    let context = live.id;
    live.words.finish(|word, start| {
        out.push_back(TtsEvent::Word {
            context,
            word,
            start,
        });
    });
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
                    let _ = with_timeout(&*clock, timeout, "elevenlabs close", socket.close(None))
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
        !self.input_open && self.out.is_empty() && !self.live.iter().any(|l| l.server_open)
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
                () = &mut self.idle, if self.armed && room => self.on_idle(),
                () = &mut self.wake, if self.wake_armed => self.on_wake().await,
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
                if self.shutdown.is_cancelled() {
                    // A write or connect that was cut short by the shutdown.
                    return End::Closed;
                }
                return End::Fatal(error);
            }
            self.rearm_wake();
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

    // ----- timers -----------------------------------------------------------------------

    /// Re-evaluates the stall deadline: armed while the server owes output, restarted by
    /// `progress` (a message for a live context) or when it was not armed yet.
    fn touch(&mut self, progress: bool) {
        let by = self.by_alignment;
        if !self.live.iter().any(|l| l.awaiting_server(by)) {
            self.armed = false;
            return;
        }
        if progress || !self.armed {
            let deadline = deadline_after(self.clock.now(), self.io_timeout);
            self.idle.as_mut().reset(deadline);
            self.armed = true;
        }
    }

    /// The stall timer fired: the server owes output and has been quiet for `io_timeout`.
    ///
    /// A context that did respond to its last text but whose alignment covers fewer characters
    /// than were sent is a healthy server with an alignment that omits characters (the docs
    /// leave the 1:1 mapping open): coverage is no longer measured for it. Anything else is a
    /// stall and ends the session.
    fn on_idle(&mut self) -> Result<(), Error> {
        let by = self.by_alignment;
        let mut stalled = false;
        for live in self.live.iter_mut().filter(|l| l.awaiting_server(by)) {
            if !live.degrade(by) {
                stalled = true;
            }
        }
        if stalled {
            return Err(Error::Core(CoreError::Timeout {
                what: "elevenlabs response",
                after: self.io_timeout,
            }));
        }
        self.touch(false);
        Ok(())
    }

    /// Arms the wake timer for the earliest keepalive or end-of-grace.
    fn rearm_wake(&mut self) {
        let by = self.by_alignment;
        let next = self
            .live
            .iter()
            .filter(|l| l.server_open)
            .filter_map(|l| {
                if l.finish_sent {
                    (!l.owed(by)).then(|| deadline_after(l.last_activity, l.grace(self.grace)))
                } else {
                    Some(deadline_after(l.last_send, self.keepalive))
                }
            })
            .min();
        match next {
            Some(at) => {
                self.wake.as_mut().reset(at);
                self.wake_armed = true;
            }
            None => self.wake_armed = false,
        }
    }

    /// The wake timer fired: finish the closing contexts whose grace has run out (no `isFinal`
    /// came) and send keepalives to the quiet open ones.
    async fn on_wake(&mut self) -> Result<(), Error> {
        let now = self.clock.now();
        let (by, grace, keepalive) = (self.by_alignment, self.grace, self.keepalive);
        let finished: Vec<ContextId> = self
            .live
            .iter()
            .filter(|l| {
                l.server_open
                    && l.finish_sent
                    && !l.owed(by)
                    && now >= deadline_after(l.last_activity, l.grace(grace))
            })
            .map(|l| l.id)
            .collect();
        for id in finished {
            self.complete(id);
        }
        let quiet: Vec<ContextId> = self
            .live
            .iter()
            .filter(|l| {
                l.server_open && !l.finish_sent && now >= deadline_after(l.last_send, keepalive)
            })
            .map(|l| l.id)
            .collect();
        for id in quiet {
            let Some(live) = self.live.iter_mut().find(|l| l.id == id) else {
                continue;
            };
            live.last_send = now;
            let json = keepalive_frame(&live.server_id)?;
            self.write(Message::text(json)).await?;
        }
        Ok(())
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
                    () = self.shutdown.cancelled() => {
                        break Some(Err(WsError::ConnectionClosed));
                    }
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
                what: "elevenlabs send",
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
        let url = self.template.url.clone();
        let socket = {
            let connect = inner.open(&url);
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => {
                        return Err(Error::Disconnected { provider: PROVIDER });
                    }
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
    /// sockets; the next text reconnects and opens new server contexts), a failure otherwise.
    /// Contexts that were closing and owed nothing are complete.
    fn connection_lost(&mut self, error: Error) -> Result<(), Error> {
        self.ws = None;
        self.wire_queue.clear();
        let by = self.by_alignment;
        if self.live.iter().any(|l| l.awaiting_server(by)) {
            return Err(error);
        }
        let closing: Vec<ContextId> = self
            .live
            .iter()
            .filter(|l| l.server_open && l.finish_sent)
            .map(|l| l.id)
            .collect();
        for id in closing {
            self.complete(id);
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

    /// The engine context ended: its unfinished word is delivered, `Flushed` follows if the
    /// engine asked for the end.
    fn complete(&mut self, context: ContextId) {
        let Some(idx) = self.live.iter().position(|l| l.id == context) else {
            return;
        };
        let mut live = self.live.remove(idx);
        flush_words(&mut live, &mut self.out);
        if live.engine_flush {
            self.out.push_back(TtsEvent::Flushed { context });
            remember(&mut self.ended, ENDED_MEMORY, context);
        }
    }

    async fn on_input(&mut self, input: TtsInput) -> Result<(), Error> {
        match input {
            TtsInput::Text { context, text } => {
                // Text with nothing to speak (blank, or only punctuation / symbols / emoji) is
                // dropped: the server may answer it with no audio and no alignment, and the
                // adapter would then wait for output that never comes.
                if !text.chars().any(char::is_alphanumeric)
                    || self.is_cancelled(context)
                    || self.has_ended(context)
                {
                    return Ok(());
                }
                self.send_text(context, &text).await?;
            }
            TtsInput::Flush { context } => {
                if self.is_cancelled(context) || self.has_ended(context) {
                    return Ok(());
                }
                self.on_flush(context).await?;
            }
        }
        self.touch(false);
        Ok(())
    }

    /// Sends text of `context`, opening (or re-opening) its server context first if needed.
    async fn send_text(&mut self, context: ContextId, text: &str) -> Result<(), Error> {
        let reopen = match self.find(context) {
            // Text after the flush of the same context: the server context is closing.
            Some(live) if live.finish_sent => return Ok(()),
            Some(live) => !live.server_open,
            None => true,
        };
        if reopen && !self.open_server_context(context).await? {
            return Ok(());
        }
        let Some(live) = self.find(context) else {
            return Ok(()); // cancelled meanwhile
        };
        let json = text_frame(&live.server_id, text)?;
        self.write(Message::text(json)).await?;
        let now = self.clock.now();
        let spoken = text.chars().filter(|c| !c.is_whitespace()).count() as u64;
        if let Some(live) = self.find_mut(context) {
            live.chars_sent = live.chars_sent.saturating_add(spoken);
            live.responded = false;
            live.last_send = now;
        }
        Ok(())
    }

    /// Gives `context` a new server context on a connected socket and sends its first message.
    /// `false`: it was cancelled while connecting. The counters, audio count and word state of
    /// an existing engine context carry over, so the offsets continue. A socket that turns out
    /// to be dead (the server closed an idle one before we noticed) is replaced once.
    async fn open_server_context(&mut self, context: ContextId) -> Result<bool, Error> {
        let mut retried = false;
        loop {
            self.ensure_connected().await?;
            if self.is_cancelled(context) {
                return Ok(false);
            }
            let now = self.clock.now();
            let server_id = server_context_id(self.session, self.next_context);
            self.next_context = self.next_context.saturating_add(1);
            if let Some(live) = self.find_mut(context) {
                live.reopen(server_id.clone(), now);
            } else {
                self.make_room();
                self.live.push(Live::new(context, server_id.clone(), now));
            }
            let json = self.template.settings.init(&server_id)?;
            match self.write(Message::text(json)).await {
                Ok(()) => return Ok(true),
                Err(error @ Error::Disconnected { .. }) if !retried => {
                    retried = true;
                    self.connection_lost(error)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Evicts one context when the table is full: first one without an open server context,
    /// then the oldest one the engine has not flushed (its server context is closed on the
    /// wire; if the engine uses its id again it starts a new context and the offsets restart).
    /// Only when every context awaits its `isFinal` is the oldest of those ended early, with
    /// the `Flushed` the engine waits for (its remaining audio is lost).
    fn make_room(&mut self) {
        if self.live.len() < MAX_LIVE {
            return;
        }
        let victim = self
            .live
            .iter()
            .position(|l| !l.server_open)
            .or_else(|| self.live.iter().position(|l| !l.finish_sent))
            .unwrap_or(0);
        let Some(gone) = self.live.get(victim) else {
            return;
        };
        if gone.finish_sent {
            let id = gone.id;
            self.complete(id);
            return;
        }
        let gone = self.live.remove(victim);
        if gone.server_open
            && let Ok(json) = close_frame(&gone.server_id)
        {
            self.wire_queue.push_back(json);
        }
    }

    async fn on_flush(&mut self, context: ContextId) -> Result<(), Error> {
        let now = self.clock.now();
        let Some(live) = self.find_mut(context) else {
            // Nothing was sent for it (or it was evicted): nothing is buffered.
            self.out.push_back(TtsEvent::Flushed { context });
            remember(&mut self.ended, ENDED_MEMORY, context);
            return Ok(());
        };
        if live.finish_sent {
            return Ok(());
        }
        live.engine_flush = true;
        if !live.server_open {
            // The server context is gone (closed by the server, or lost with the socket while
            // nothing was owed): everything it owed has been delivered.
            self.complete(context);
            return Ok(());
        }
        live.finish_sent = true;
        live.last_activity = now;
        let server_id = live.server_id.clone();
        self.write(Message::text(flush_frame(&server_id)?)).await?;
        self.write(Message::text(close_frame(&server_id)?)).await
    }

    /// The input was dropped: close every context that is still open (and silently: the engine
    /// did not ask for `Flushed`), then wait for the servers' `isFinal`s and end.
    async fn on_input_closed(&mut self) -> Result<(), Error> {
        self.input_open = false;
        let now = self.clock.now();
        self.live.retain(|l| l.server_open);
        let ids: Vec<ContextId> = self
            .live
            .iter()
            .filter(|l| !l.finish_sent)
            .map(|l| l.id)
            .collect();
        for id in ids {
            let Some(live) = self.find_mut(id) else {
                continue;
            };
            live.finish_sent = true;
            live.last_activity = now;
            let json = close_frame(&live.server_id)?;
            self.write(Message::text(json)).await?;
        }
        // The closing requests owe an `isFinal`: that wait carries the grace and the stall
        // timers.
        self.touch(false);
        Ok(())
    }

    // ----- cancel -----------------------------------------------------------------------

    fn is_cancelled(&self, context: ContextId) -> bool {
        self.cancelled.contains(&context)
    }

    /// `Flushed` was already produced for `context`.
    fn has_ended(&self, context: ContextId) -> bool {
        self.ended.contains(&context)
    }

    /// The local half of a cancel, which never waits for I/O: remember the id, purge what the
    /// consumer has not received, hand out `Cancelled`, forget the context and queue the wire
    /// `close_context`.
    fn local_cancel(&mut self, context: ContextId) {
        if !self.is_cancelled(context) {
            remember(&mut self.cancelled, CANCELLED_MEMORY, context);
        }
        // Whatever the consumer has not received yet is stale now.
        self.out.retain(|e| event_context(e) != Some(context));
        self.out.push_front(TtsEvent::Cancelled { context });
        if let Some(idx) = self.live.iter().position(|l| l.id == context) {
            let live = self.live.remove(idx);
            if live.server_open
                && !live.finish_sent
                && let Ok(json) = close_frame(&live.server_id)
            {
                self.wire_queue.push_back(json);
            }
        }
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
            None => self.connection_lost(Error::Disconnected { provider: PROVIDER }),
            Some(Ok(Message::Close(close))) => {
                let error = close.map_or(Error::Disconnected { provider: PROVIDER }, |frame| {
                    map_close_code(u16::from(frame.code))
                });
                self.connection_lost(error)
            }
            Some(Err(error)) => self.connection_lost(map_ws_error(error)),
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()),
            Some(Ok(Message::Ping(_))) => {
                // The pong is queued by the protocol layer; push it out.
                if let Some(socket) = self.ws.as_mut() {
                    let _ = with_timeout(
                        &*self.clock,
                        self.io_timeout,
                        "elevenlabs pong",
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

    fn on_text(&mut self, text: &str) -> Result<(), Error> {
        // One parse in the normal case. A message that does not parse is routed leniently
        // (`Envelope`): for a context that is not live (cancelled, finished, replaced by a
        // re-open, unknown) it is dropped even if malformed, for a live one or none it fails.
        let incoming = match Incoming::parse(text) {
            Ok(incoming) => incoming,
            Err(error) => {
                let elsewhere = Envelope::parse(text)?
                    .context_id()
                    .is_some_and(|id| !self.is_live_server_id(id));
                return if elsewhere { Ok(()) } else { Err(error) };
            }
        };
        let Some(server_id) = incoming.context_id() else {
            // Not about a context: an error here is about the connection.
            return if incoming.is_error() {
                Err(map_server_error(text))
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
        if incoming.is_error() {
            return Err(map_server_error(text));
        }
        self.on_message(idx, &incoming)?;
        self.touch(true);
        Ok(())
    }

    /// Whether `server_id` is an open server context of a live engine context.
    fn is_live_server_id(&self, server_id: &str) -> bool {
        self.live
            .iter()
            .any(|l| l.server_open && l.server_id == server_id)
    }

    /// One audio / alignment / final message of the live context at `idx`.
    fn on_message(&mut self, idx: usize, incoming: &Incoming<'_>) -> Result<(), Error> {
        let now = self.clock.now();
        let (rate, bytes_per_sample) = (self.template.rate, self.template.bytes_per_sample);
        let (by, use_normalized) = (self.by_alignment, self.normalized_words);
        let sync_alignment = self.sync_alignment;
        let Some(live) = self.live.get_mut(idx) else {
            return Ok(());
        };
        let id = live.id;
        live.last_activity = now;
        // Where the alignment times start: this message's audio (`sync_alignment` off) or the
        // server context (on). See `Live::alignment_origin`.
        let origin = live.alignment_origin(sync_alignment, rate, bytes_per_sample);
        let mut responded = false;

        for alignment in [&incoming.alignment, &incoming.normalized]
            .into_iter()
            .flatten()
        {
            if alignment
                .chars
                .as_ref()
                .is_some_and(|c| c.len() > MAX_ALIGNMENT_CHARS)
            {
                return Err(protocol("alignment with too many characters"));
            }
        }

        if let Some(data) = incoming.audio.as_deref().filter(|d| !d.is_empty()) {
            responded = true;
            let mut audio = self.decoder.decode(data)?;
            if bytes_per_sample == 2 && (live.carry.is_some() || audio.len() % 2 == 1) {
                audio = align_pcm16(&mut live.carry, &audio);
            }
            if !audio.is_empty() {
                live.audio_bytes = live.audio_bytes.saturating_add(audio.len() as u64);
                self.out.push_back(TtsEvent::Audio { context: id, audio });
            }
        }

        let plain = incoming.alignment.as_ref().filter(|a| !a.is_empty());
        let normalized = incoming.normalized.as_ref().filter(|a| !a.is_empty());
        if let Some(chars) = plain.and_then(|a| a.chars.as_deref()) {
            live.chars_aligned = live.chars_aligned.saturating_add(count_spoken(chars));
            live.plain_seen = true;
        }
        if let Some(chars) = normalized.and_then(|a| a.chars.as_deref()) {
            live.chars_normalized = live.chars_normalized.saturating_add(count_spoken(chars));
        }
        if plain.is_some() || normalized.is_some() {
            live.aligned_seen = true;
            responded = true;
        }
        let words_from = if use_normalized {
            normalized.or(plain)
        } else {
            plain.or(normalized)
        };
        if let Some(alignment) = words_from
            && let Some(chars) = alignment.chars.as_deref()
        {
            let starts = alignment.starts.as_deref().unwrap_or(&[]);
            live.words.feed(origin, chars, starts, |word, start| {
                self.out.push_back(TtsEvent::Word {
                    context: id,
                    word,
                    start,
                });
            });
        }
        if responded {
            live.responded = true;
        }
        if live.covered(by) || (!by && !live.owed(false)) {
            // The alignment covers everything sent: a word left open (no trailing whitespace in
            // the alignment) is complete. Never earlier: a message may end inside a word, and
            // the rest of it follows in the next one. (With coverage switched off the first
            // response is all there is to go by.)
            flush_words(live, &mut self.out);
        }
        if incoming.is_final == Some(true) {
            return self.on_final(idx);
        }
        Ok(())
    }

    /// `isFinal` for the live context at `idx`. Solicited (after the closing request): the
    /// engine context ends and `Flushed` follows for an engine `Flush`.
    ///
    /// Unsolicited, the docs do not say whether it is the server's inactivity timeout closing
    /// the context or only the end of one flushed piece (every text is sent with `flush`).
    /// With nothing owed both readings agree: the server context is treated as closed, a
    /// `close_context` is sent so that it cannot linger (the server allows few contexts per
    /// connection) and the next text opens a new one. With text still owed it is taken as the
    /// per-flush marker and ignored: the output of the text that follows is still delivered, and
    /// if the server really closed the context the stall timer ends the wait.
    fn on_final(&mut self, idx: usize) -> Result<(), Error> {
        let by = self.by_alignment;
        let Some(live) = self.live.get_mut(idx) else {
            return Ok(());
        };
        if live.finish_sent {
            let id = live.id;
            self.complete(id);
            return Ok(());
        }
        if live.owed(by) {
            return Ok(());
        }
        live.server_open = false;
        flush_words(live, &mut self.out);
        if let Ok(json) = close_frame(&live.server_id) {
            self.wire_queue.push_back(json);
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
