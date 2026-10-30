//! The RTVI bridge: a call's engine events to a Pipecat RTVI client, and the client's
//! app messages into the call. Protocol facts: `docs/providers/rtvi.md`.
//!
//! [`RtviBridge::attach`] takes the two side channels of an
//! [`RtviCall`](brisvox_transport::rtvi::RtviCall) (its [`RtviSender`] and [`RtviInbox`]),
//! installs an [`EngineEvents`] ring and a typed-message channel on the [`SessionBuilder`], and
//! starts two tasks:
//!
//! - **events**: reads the engine events ([`crate::events`]) and sends the RTVI messages below
//!   through the sender, in order, one at a time (each send bounded by the server's
//!   `send_timeout`). A client that reads slowly makes this task wait, never the pipeline: the
//!   event ring drops its oldest events instead ([`RtviBridgeStats::events_lost`]).
//! - **inbox**: `send-text` becomes a typed user message ([`crate::text_input`]: the bot is
//!   interrupted and answers the text, which the runner appends to the context as a user
//!   message); `client-message` goes to the application's [`ClientMessageHandler`], whose answer
//!   becomes the `server-response` (or an `error-response`, also after
//!   [`RtviBridgeConfig::reply_timeout`] or without a handler: the client's
//!   `sendClientRequest()` never resolves otherwise). Up to [`MAX_PENDING_REPLIES`] replies run
//!   at once beside the inbox, so a slow handler never holds up a `send-text` barge-in behind
//!   it; a `client-message` beyond that is answered with an `error-response` at once.
//!   `llm-function-call-result` is not supported (the engine runs its tools itself) and is
//!   counted.
//!
//! # Deviations from Pipecat's `send-text`
//! - A typed turn raises no `user-started-speaking` / `user-stopped-speaking` (Pipecat's
//!   neither): the client gets `bot-interrupted` (if the bot was busy) and the `user-llm-text`.
//! - `run_immediately: false`: Pipecat appends the text to the context now, without running the
//!   LLM. The engine holds it and joins it to the next user turn (spoken or typed), so the LLM
//!   sees it in that turn's user message, not as a message of its own before it; nothing is
//!   sent to the client until then.
//! - `audio_response: false` is answered with audio anyway (counted).
//!
//! # Event mapping
//! | engine event | RTVI message |
//! |---|---|
//! | `UserStartedSpeaking` / `UserStoppedSpeaking` | `user-started-speaking` / `user-stopped-speaking` |
//! | `UserTranscript` | `user-transcription` (`final` as the provider said; timestamp from the injected clock's wall time) |
//! | `UserText` | `user-llm-text` |
//! | `BotLlmStarted` / `BotLlmText` / `BotLlmStopped` | `bot-llm-started` / `bot-llm-text` / `bot-llm-stopped` |
//! | `BotTtsText` | `bot-tts-started` (before the first chunk of a response), `bot-tts-text`, and `bot-output` (`aggregated_by: "sentence"`, `will_be_spoken: true`) |
//! | `BotTtsStopped` | `bot-tts-stopped` |
//! | `BotStartedSpeaking` | `bot-started-speaking`, then the `bot-output`s held until the bot spoke (Pipecat's order) |
//! | `TurnMetrics` | `metrics`: `ttfb` of `llm` and `tts`, `processing` of `stt` (final delay), `turn` (end-of-turn delay) and `response` (server voice-to-voice), in seconds |
//! | `BotStoppedSpeaking` | `bot-stopped-speaking` |
//! | `BotInterrupted` | `bot-interrupted`, then `bot-stopped-speaking`, `bot-tts-stopped` and `bot-llm-stopped` for what was running; held `bot-output`s are dropped (never spoken) |
//! | `Error` | `error` (`"<stage>: <message>"`, `fatal` as reported) |
//!
//! Bot events of an epoch older than the newest seen (a frame a barge-in overtook) are dropped,
//! and the started/stopped pairs are kept balanced: a duplicate start or a stop without a start
//! sends nothing. The user's pair is balanced by the turn driver (every spoken turn it starts is
//! stopped once, also when it ends empty, is superseded or the call hangs up) and again here: a
//! start while the user is speaking first sends the missing stop.
//!
//! The bot speaking events come from the engine (the transport's playout). With
//! [`RtviBridgeConfig::bot_speaking_events`] on (the default) [`RtviBridge::attach`] switches
//! the adapter's own derivation off for the call
//! ([`RtviSender::set_auto_events`](brisvox_transport::rtvi::RtviSender::set_auto_events)), so
//! the client never gets them twice whatever
//! [`RtviConfig::auto_events`](brisvox_transport::rtvi::RtviConfig) says, and every RTVI message
//! of the call travels on one queue and keeps the order above. With it off the adapter's
//! derivation is left as configured.
//!
//! Not supported (counted, documented in `docs/providers/rtvi.md`): `llm-function-call-*`
//! events are not sent and client function-call results are ignored.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use brisvox_core::{SharedClock, TurnEpoch, TurnMetrics};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use brisvox_transport::rtvi::{
    BotOutput, ErrorData, MetricEntry, Metrics, RtviEvent, RtviInbox, RtviRequest, RtviSender,
    TextData, UserTranscription, format_timestamp,
};
use serde_json::Value;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::events::{EngineEvent, EngineEvents, EventStream};
use crate::guard::TaskGuard;
use crate::session::SessionBuilder;
use crate::text_input::{TextTurn, TextTurnSender, text_turns};
use crate::truncation::BoxFuture;

/// Most `bot-output` messages held until the bot starts speaking; beyond it the oldest is
/// dropped (counted in [`RtviBridgeStats::outputs_dropped`]).
const MAX_HELD_OUTPUTS: usize = 32;

/// Most `client-message` replies in flight at once; a request beyond it is answered with an
/// `error-response` at once (counted in [`RtviBridgeStats::client_errors`]).
pub const MAX_PENDING_REPLIES: usize = 8;

/// The application's answer to a `client-message`: the `d` of the `server-response`, or the text
/// of an `error-response`.
pub type ClientReply = std::result::Result<Option<Value>, String>;

/// Answers a client's `client-message` `{ t, d }` (called with `t` and `d`). It runs on the
/// bridge's reply tasks, beside the inbox (at most [`MAX_PENDING_REPLIES`] at once), bounded by
/// [`RtviBridgeConfig::reply_timeout`]. It must not block the thread: await, or offload heavy
/// work.
pub type ClientMessageHandler =
    Arc<dyn Fn(String, Option<Value>) -> BoxFuture<'static, ClientReply> + Send + Sync>;

/// Settings of an [`RtviBridge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtviBridgeConfig {
    /// Send `bot-started-speaking`, `bot-stopped-speaking` and `bot-interrupted` from the engine
    /// (default); [`RtviBridge::attach`] then switches the adapter's own derivation off for the
    /// call (see the module docs).
    pub bot_speaking_events: bool,
    /// Send `metrics` once per turn (default).
    pub metrics: bool,
    /// How long the [`ClientMessageHandler`] may take before the client gets an
    /// `error-response` (default 5 s).
    pub reply_timeout: Duration,
    /// Capacity of the engine event ring (default [`crate::DEFAULT_EVENT_CAPACITY`], clamped to
    /// [`crate::MAX_EVENT_CAPACITY`]).
    pub event_capacity: usize,
    /// Typed messages waiting for the turn driver (default 8, clamped to
    /// [`crate::MAX_TEXT_CAPACITY`]); a `send-text` beyond it is dropped and counted.
    pub text_capacity: usize,
}

impl Default for RtviBridgeConfig {
    fn default() -> Self {
        Self {
            bot_speaking_events: true,
            metrics: true,
            reply_timeout: Duration::from_secs(5),
            event_capacity: crate::events::DEFAULT_EVENT_CAPACITY,
            text_capacity: 8,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    events_sent: AtomicU64,
    events_failed: AtomicU64,
    events_lost: AtomicU64,
    stale_dropped: AtomicU64,
    outputs_dropped: AtomicU64,
    texts: AtomicU64,
    texts_dropped: AtomicU64,
    text_only_requests: AtomicU64,
    client_messages: AtomicU64,
    client_errors: AtomicU64,
    unsupported: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A snapshot of an [`RtviBridge`]'s counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RtviBridgeStats {
    /// RTVI messages queued to the client.
    pub events_sent: u64,
    /// RTVI messages the sender refused (queue full past the send timeout, or too large).
    pub events_failed: u64,
    /// Engine events overwritten in the ring before the bridge read them (a slow client).
    pub events_lost: u64,
    /// Bot events of an epoch a barge-in had already ended.
    pub stale_dropped: u64,
    /// Held `bot-output` messages dropped by their bound.
    pub outputs_dropped: u64,
    /// `send-text` messages handed to the turn driver.
    pub texts: u64,
    /// `send-text` messages dropped (empty, or the typed-message channel was full or closed).
    pub texts_dropped: u64,
    /// `send-text` messages that asked for a text-only answer (`audio_response: false`), which
    /// is not supported: they were answered with audio.
    pub text_only_requests: u64,
    /// `client-message` requests received.
    pub client_messages: u64,
    /// `client-message` requests answered with `error-response` (no handler, the handler's
    /// error, its timeout, or too many replies in flight).
    pub client_errors: u64,
    /// Requests the bridge does not support (`llm-function-call-result`).
    pub unsupported: u64,
}

/// The running bridge of one call; dropping it stops its tasks.
#[derive(Debug)]
pub struct RtviBridge {
    counters: Arc<Counters>,
    text: TextTurnSender,
    _events: TaskGuard,
    _inbox: TaskGuard,
}

impl RtviBridge {
    /// Installs the bridge's event ring and typed-message channel on `builder` and starts its
    /// two tasks (see the module docs) under `cancel`; they also end when the call's events
    /// end, when the client's inbox closes, and when the bridge is dropped. Start the call
    /// with the returned builder ([`run_call`](crate::run_call)). `clock` bounds the handler's
    /// replies and stamps the transcriptions.
    #[allow(clippy::too_many_arguments)]
    pub fn attach<S: SttProvider, L: LlmProvider, T: TtsProvider>(
        builder: SessionBuilder<S, L, T>,
        sender: RtviSender,
        inbox: RtviInbox,
        config: RtviBridgeConfig,
        handler: Option<ClientMessageHandler>,
        clock: SharedClock,
        cancel: &CancellationToken,
    ) -> (SessionBuilder<S, L, T>, Self) {
        if config.bot_speaking_events {
            // The engine sends them: the adapter's derivation would send them a second time.
            sender.set_auto_events(false);
        }
        let events = EngineEvents::new(config.event_capacity);
        let stream = events.subscribe();
        let (text, text_input) = text_turns(config.text_capacity);
        let builder = builder.with_events(events).with_text_input(text_input);
        let counters = Arc::new(Counters::default());
        let mapper = Mapper::new(config, clock.clone());
        let events_task = TaskGuard::spawn(
            "rtvi_events",
            forward_events(
                stream,
                mapper,
                sender.clone(),
                Arc::clone(&counters),
                cancel.clone(),
            ),
        );
        let inbox_task = TaskGuard::spawn(
            "rtvi_inbox",
            serve_inbox(
                inbox,
                Inbox {
                    sender,
                    text: text.clone(),
                    handler,
                    clock,
                    reply_timeout: config.reply_timeout,
                    counters: Arc::clone(&counters),
                },
                cancel.clone(),
            ),
        );
        (
            builder,
            Self {
                counters,
                text,
                _events: events_task,
                _inbox: inbox_task,
            },
        )
    }

    /// The counters now.
    pub fn stats(&self) -> RtviBridgeStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        RtviBridgeStats {
            events_sent: get(&c.events_sent),
            events_failed: get(&c.events_failed),
            events_lost: get(&c.events_lost),
            stale_dropped: get(&c.stale_dropped),
            outputs_dropped: get(&c.outputs_dropped),
            texts: get(&c.texts),
            texts_dropped: get(&c.texts_dropped),
            text_only_requests: get(&c.text_only_requests),
            client_messages: get(&c.client_messages),
            client_errors: get(&c.client_errors),
            unsupported: get(&c.unsupported),
        }
    }

    /// The call's typed-message channel (the one `send-text` uses), for the application's own
    /// text input.
    pub fn text_sender(&self) -> TextTurnSender {
        self.text.clone()
    }
}

/// Whether the sender's error means the call is over.
fn closed(error: &brisvox_transport::Error) -> bool {
    matches!(
        error,
        brisvox_transport::Error::Core(brisvox_core::Error::ChannelClosed(_))
    )
}

async fn forward_events(
    mut stream: EventStream,
    mut mapper: Mapper,
    sender: RtviSender,
    counters: Arc<Counters>,
    cancel: CancellationToken,
) {
    let mut out = Vec::new();
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            event = stream.recv() => event,
        };
        counters.events_lost.store(stream.lost(), Ordering::Relaxed);
        let Some(event) = event else {
            return;
        };
        mapper.map(event, &mut out);
        counters
            .stale_dropped
            .store(mapper.stale_dropped, Ordering::Relaxed);
        counters
            .outputs_dropped
            .store(mapper.outputs_dropped, Ordering::Relaxed);
        for message in out.drain(..) {
            let sent = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                sent = sender.send(message) => sent,
            };
            match sent {
                Ok(()) => bump(&counters.events_sent),
                Err(error) if closed(&error) => return,
                Err(error) => {
                    bump(&counters.events_failed);
                    tracing::debug!(target: "brisvox::engine", %error, "rtvi event not sent");
                }
            }
        }
    }
}

/// What the inbox task needs besides the inbox.
struct Inbox {
    sender: RtviSender,
    text: TextTurnSender,
    handler: Option<ClientMessageHandler>,
    clock: SharedClock,
    reply_timeout: Duration,
    counters: Arc<Counters>,
}

impl Inbox {
    fn send_text(&self, content: &str, run_immediately: bool, audio_response: bool) {
        if !audio_response {
            bump(&self.counters.text_only_requests);
        }
        if content.trim().is_empty() {
            bump(&self.counters.texts_dropped);
            return;
        }
        let turn = TextTurn {
            text: Arc::from(content),
            run_immediately,
        };
        match self.text.try_send(turn) {
            Ok(()) => bump(&self.counters.texts),
            Err(_) => bump(&self.counters.texts_dropped),
        }
    }

    async fn client_message(&self, id: String, t: String, d: Option<Value>) {
        let reply = match &self.handler {
            None => Err("no handler for client-message".to_owned()),
            Some(handler) => {
                let answer = handler(t.clone(), d);
                tokio::select! {
                    reply = answer => reply,
                    () = brisvox_core::clock::wait_for(&*self.clock, self.reply_timeout) => {
                        Err("client-message handler timed out".to_owned())
                    }
                }
            }
        };
        self.reply(&id, &t, reply).await;
    }

    /// Sends the answer of the request `id` (each send bounded by the server's `send_timeout`).
    async fn reply(&self, id: &str, t: &str, reply: ClientReply) {
        let sent = match reply {
            Ok(d) => self.sender.respond(id, t, d).await,
            Err(error) => {
                bump(&self.counters.client_errors);
                self.sender.respond_error(id, &error).await
            }
        };
        if let Err(error) = sent {
            tracing::debug!(target: "brisvox::engine", %error, "rtvi reply not sent");
        }
    }
}

/// Serves the client's requests. Replies to `client-message` run in their own tasks (at most
/// [`MAX_PENDING_REPLIES`], aborted when this task ends), so a `send-text` is handed to the turn
/// driver as soon as it arrives, never behind a slow handler.
async fn serve_inbox(mut inbox: RtviInbox, state: Inbox, cancel: CancellationToken) {
    let state = Arc::new(state);
    // Dropped with this task: every reply still running is aborted.
    let mut replies = JoinSet::new();
    loop {
        let request = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            // A reply finished (a panicking handler only ends its own task).
            Some(_) = replies.join_next(), if !replies.is_empty() => continue,
            request = inbox.recv() => request,
        };
        let Some(request) = request else {
            return;
        };
        match request {
            RtviRequest::SendText {
                content,
                run_immediately,
                audio_response,
            } => state.send_text(&content, run_immediately, audio_response),
            RtviRequest::ClientMessage { id, t, d } => {
                bump(&state.counters.client_messages);
                if replies.len() < MAX_PENDING_REPLIES {
                    let state = Arc::clone(&state);
                    replies.spawn(async move { state.client_message(id, t, d).await });
                } else {
                    // Refused at once; only the queue send is awaited (bounded).
                    let busy = Err("too many client-message requests in flight".to_owned());
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        () = state.reply(&id, &t, busy) => {}
                    }
                }
            }
            _ => bump(&state.counters.unsupported),
        }
    }
}

/// Engine events to RTVI messages (the table of the module docs). Pure apart from the clock's
/// wall time for transcription stamps.
struct Mapper {
    config: RtviBridgeConfig,
    clock: SharedClock,
    /// The newest epoch seen: bot events of older ones are stale.
    epoch: TurnEpoch,
    /// The user's `user-started-speaking` is not yet matched by a stop.
    user_speaking: bool,
    bot_speaking: bool,
    tts_open: bool,
    llm_open: bool,
    /// `bot-output`s of the current response held until the bot starts speaking.
    held: VecDeque<BotOutput>,
    stale_dropped: u64,
    outputs_dropped: u64,
}

impl Mapper {
    fn new(config: RtviBridgeConfig, clock: SharedClock) -> Self {
        Self {
            config,
            clock,
            epoch: TurnEpoch::ZERO,
            user_speaking: false,
            bot_speaking: false,
            tts_open: false,
            llm_open: false,
            held: VecDeque::with_capacity(MAX_HELD_OUTPUTS),
            stale_dropped: 0,
            outputs_dropped: 0,
        }
    }

    /// Whether an event of `epoch` is current; a newer epoch becomes the current one.
    fn current(&mut self, epoch: TurnEpoch) -> bool {
        if epoch < self.epoch {
            self.stale_dropped = self.stale_dropped.saturating_add(1);
            return false;
        }
        self.epoch = epoch;
        true
    }

    fn stop_bot(&mut self, out: &mut Vec<RtviEvent>) {
        if self.bot_speaking {
            self.bot_speaking = false;
            if self.config.bot_speaking_events {
                out.push(RtviEvent::BotStoppedSpeaking);
            }
        }
    }

    fn release_held(&mut self, out: &mut Vec<RtviEvent>) {
        out.extend(self.held.drain(..).map(RtviEvent::BotOutput));
    }

    fn map(&mut self, event: EngineEvent, out: &mut Vec<RtviEvent>) {
        match event {
            // The user's events come from the turn driver in its order, never stale: they are
            // kept balanced, not filtered by epoch (a held end that a typed barge-in supersedes
            // stops with its own, older epoch).
            EngineEvent::UserStartedSpeaking { epoch } => {
                self.epoch = self.epoch.max(epoch);
                if std::mem::replace(&mut self.user_speaking, true) {
                    // The stop was lost (overwritten in the ring): close the old turn first.
                    out.push(RtviEvent::UserStoppedSpeaking);
                }
                out.push(RtviEvent::UserStartedSpeaking);
            }
            EngineEvent::UserStoppedSpeaking { .. } => {
                if std::mem::take(&mut self.user_speaking) {
                    out.push(RtviEvent::UserStoppedSpeaking);
                }
            }
            EngineEvent::UserTranscript { text, is_final } => {
                out.push(RtviEvent::UserTranscription(UserTranscription {
                    text: text.to_string(),
                    user_id: String::new(),
                    timestamp: format_timestamp(self.clock.wall()),
                    is_final,
                }));
            }
            EngineEvent::UserText { epoch, text } => {
                if self.current(epoch) {
                    out.push(RtviEvent::UserLlmText(TextData::new(&*text)));
                }
            }
            EngineEvent::BotLlmStarted { epoch } => {
                if self.current(epoch) {
                    self.llm_open = true;
                    out.push(RtviEvent::BotLlmStarted);
                }
            }
            EngineEvent::BotLlmText { epoch, text } => {
                if self.current(epoch) {
                    out.push(RtviEvent::BotLlmText(TextData::new(&*text)));
                }
            }
            EngineEvent::BotLlmStopped { epoch, .. } => {
                if self.current(epoch) && std::mem::take(&mut self.llm_open) {
                    out.push(RtviEvent::BotLlmStopped);
                }
            }
            EngineEvent::BotTtsText { epoch, text } => {
                if !self.current(epoch) {
                    return;
                }
                if !std::mem::replace(&mut self.tts_open, true) {
                    out.push(RtviEvent::BotTtsStarted);
                }
                out.push(RtviEvent::BotTtsText(TextData::new(&*text)));
                let output = BotOutput {
                    text: text.to_string(),
                    aggregated_by: "sentence".to_owned(),
                    will_be_spoken: Some(true),
                    spoken_status: None,
                };
                if self.bot_speaking {
                    out.push(RtviEvent::BotOutput(output));
                } else {
                    if self.held.len() >= MAX_HELD_OUTPUTS {
                        self.held.pop_front();
                        self.outputs_dropped = self.outputs_dropped.saturating_add(1);
                    }
                    self.held.push_back(output);
                }
            }
            EngineEvent::BotTtsStopped { epoch } => {
                if self.current(epoch) && std::mem::take(&mut self.tts_open) {
                    out.push(RtviEvent::BotTtsStopped);
                }
            }
            EngineEvent::BotStartedSpeaking { epoch } => {
                if !self.current(epoch) {
                    return;
                }
                if !std::mem::replace(&mut self.bot_speaking, true)
                    && self.config.bot_speaking_events
                {
                    out.push(RtviEvent::BotStartedSpeaking);
                }
                self.release_held(out);
            }
            EngineEvent::BotStoppedSpeaking { epoch } => {
                if self.current(epoch) {
                    self.stop_bot(out);
                    // Text that never got audio (nothing played) is still the bot's output.
                    self.release_held(out);
                }
            }
            EngineEvent::BotInterrupted { epoch } => {
                if !self.current(epoch) {
                    return;
                }
                if self.config.bot_speaking_events {
                    out.push(RtviEvent::BotInterrupted);
                }
                self.stop_bot(out);
                if std::mem::take(&mut self.tts_open) {
                    out.push(RtviEvent::BotTtsStopped);
                }
                if std::mem::take(&mut self.llm_open) {
                    out.push(RtviEvent::BotLlmStopped);
                }
                // Never spoken: not the bot's output.
                self.held.clear();
            }
            EngineEvent::TurnMetrics { epoch, metrics } => {
                if self.config.metrics && self.current(epoch) {
                    let metrics = rtvi_metrics(&metrics);
                    if !metrics.ttfb.is_empty() || !metrics.processing.is_empty() {
                        out.push(RtviEvent::Metrics(metrics));
                    }
                }
            }
            // RTVI has no message for it.
            EngineEvent::LanguageSwitched { .. } => {}
            EngineEvent::Error {
                stage,
                message,
                fatal,
            } => out.push(RtviEvent::Error(ErrorData {
                error: format!("{stage}: {message}"),
                fatal,
            })),
        }
    }
}

/// A turn's latencies as RTVI `metrics` (seconds).
fn rtvi_metrics(metrics: &TurnMetrics) -> Metrics {
    let entry = |processor: &str, value: Option<Duration>| {
        value.map(|value| MetricEntry {
            processor: processor.to_owned(),
            value: value.as_secs_f64(),
            model: None,
        })
    };
    Metrics {
        ttfb: [
            entry("llm", metrics.llm_time_to_first_token),
            entry("tts", metrics.tts_time_to_first_byte),
        ]
        .into_iter()
        .flatten()
        .collect(),
        processing: [
            entry("stt", metrics.stt_final_delay),
            entry("turn", metrics.end_of_turn_delay),
            entry("response", metrics.response_latency),
        ]
        .into_iter()
        .flatten()
        .collect(),
        characters: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::SimClock;

    use super::*;

    fn ep(n: u32) -> TurnEpoch {
        let mut epoch = TurnEpoch::ZERO;
        for _ in 0..n {
            epoch = epoch.next();
        }
        epoch
    }

    fn mapper() -> Mapper {
        Mapper::new(RtviBridgeConfig::default(), Arc::new(SimClock::new()))
    }

    fn names(events: &[RtviEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                serde_json::to_value(event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    fn run(mapper: &mut Mapper, events: Vec<EngineEvent>) -> Vec<String> {
        let mut out = Vec::new();
        for event in events {
            mapper.map(event, &mut out);
        }
        names(&out)
    }

    #[test]
    fn a_response_is_announced_in_pipecat_order_and_its_text_waits_for_the_audio() {
        let mut m = mapper();
        let e = ep(0);
        let text = |s: &str| Arc::<str>::from(s);
        let got = run(
            &mut m,
            vec![
                EngineEvent::UserStartedSpeaking { epoch: e },
                EngineEvent::UserTranscript {
                    text: text("hi"),
                    is_final: false,
                },
                EngineEvent::UserText {
                    epoch: e,
                    text: text("hi there"),
                },
                EngineEvent::UserStoppedSpeaking { epoch: e },
                EngineEvent::BotLlmStarted { epoch: e },
                EngineEvent::BotLlmText {
                    epoch: e,
                    text: text("Hello."),
                },
                EngineEvent::BotTtsText {
                    epoch: e,
                    text: text("Hello."),
                },
                EngineEvent::BotLlmStopped {
                    epoch: e,
                    usage: None,
                },
                EngineEvent::BotStartedSpeaking { epoch: e },
                EngineEvent::TurnMetrics {
                    epoch: e,
                    metrics: TurnMetrics {
                        llm_time_to_first_token: Some(Duration::from_millis(200)),
                        ..TurnMetrics::default()
                    },
                },
                EngineEvent::BotTtsStopped { epoch: e },
                // Duplicates are not repeated.
                EngineEvent::BotStartedSpeaking { epoch: e },
                EngineEvent::BotStoppedSpeaking { epoch: e },
                EngineEvent::BotStoppedSpeaking { epoch: e },
            ],
        );
        assert_eq!(
            got,
            [
                "user-started-speaking",
                "user-transcription",
                "user-llm-text",
                "user-stopped-speaking",
                "bot-llm-started",
                "bot-llm-text",
                "bot-tts-started",
                "bot-tts-text",
                "bot-llm-stopped",
                "bot-started-speaking",
                "bot-output",
                "metrics",
                "bot-tts-stopped",
                "bot-stopped-speaking",
            ]
        );
    }

    #[test]
    fn a_barge_in_closes_what_runs_and_drops_the_old_epoch() {
        let mut m = mapper();
        let (old, new) = (ep(0), ep(1));
        let text = Arc::<str>::from("Long answer.");
        let got = run(
            &mut m,
            vec![
                EngineEvent::BotLlmStarted { epoch: old },
                EngineEvent::BotTtsText {
                    epoch: old,
                    text: Arc::clone(&text),
                },
                EngineEvent::BotStartedSpeaking { epoch: old },
                EngineEvent::BotTtsText {
                    epoch: old,
                    text: Arc::clone(&text),
                },
                EngineEvent::BotInterrupted { epoch: new },
                // Late frames of the old epoch.
                EngineEvent::BotStoppedSpeaking { epoch: old },
                EngineEvent::BotTtsStopped { epoch: old },
                EngineEvent::BotLlmText {
                    epoch: old,
                    text: Arc::clone(&text),
                },
                EngineEvent::UserStartedSpeaking { epoch: new },
            ],
        );
        assert_eq!(
            got,
            [
                "bot-llm-started",
                "bot-tts-started",
                "bot-tts-text",
                "bot-started-speaking",
                "bot-output",
                "bot-tts-text",
                "bot-output",
                "bot-interrupted",
                "bot-stopped-speaking",
                "bot-tts-stopped",
                "bot-llm-stopped",
                "user-started-speaking",
            ]
        );
        assert_eq!(m.stale_dropped, 3);
    }

    #[test]
    fn without_bot_speaking_events_the_adapter_owns_them() {
        let mut m = Mapper::new(
            RtviBridgeConfig {
                bot_speaking_events: false,
                ..RtviBridgeConfig::default()
            },
            Arc::new(SimClock::new()),
        );
        let e = ep(0);
        let got = run(
            &mut m,
            vec![
                EngineEvent::BotTtsText {
                    epoch: e,
                    text: Arc::from("Hi."),
                },
                EngineEvent::BotStartedSpeaking { epoch: e },
                EngineEvent::BotInterrupted { epoch: ep(1) },
            ],
        );
        assert_eq!(
            got,
            [
                "bot-tts-started",
                "bot-tts-text",
                "bot-output",
                "bot-tts-stopped"
            ]
        );
    }

    #[test]
    fn the_user_pair_stays_balanced_whatever_the_epochs() {
        let mut m = mapper();
        let got = run(
            &mut m,
            vec![
                EngineEvent::UserStartedSpeaking { epoch: ep(0) },
                // The stop was lost in the ring: the next start closes the old turn first.
                EngineEvent::UserStartedSpeaking { epoch: ep(0) },
                // A held end superseded by a typed barge-in stops with its older epoch.
                EngineEvent::BotInterrupted { epoch: ep(1) },
                EngineEvent::UserStoppedSpeaking { epoch: ep(0) },
                // A stop without a start sends nothing.
                EngineEvent::UserStoppedSpeaking { epoch: ep(1) },
                EngineEvent::UserStartedSpeaking { epoch: ep(1) },
                EngineEvent::UserStoppedSpeaking { epoch: ep(1) },
            ],
        );
        assert_eq!(
            got,
            [
                "user-started-speaking",
                "user-stopped-speaking",
                "user-started-speaking",
                "bot-interrupted",
                "user-stopped-speaking",
                "user-started-speaking",
                "user-stopped-speaking",
            ]
        );
    }

    #[test]
    fn held_outputs_are_bounded() {
        let mut m = mapper();
        let mut out = Vec::new();
        for _ in 0..MAX_HELD_OUTPUTS + 5 {
            m.map(
                EngineEvent::BotTtsText {
                    epoch: ep(0),
                    text: Arc::from("x"),
                },
                &mut out,
            );
        }
        assert_eq!(m.held.len(), MAX_HELD_OUTPUTS);
        assert_eq!(m.outputs_dropped, 5);
    }

    #[test]
    fn metrics_are_seconds_per_processor_and_empty_ones_are_not_sent() {
        let metrics = rtvi_metrics(&TurnMetrics {
            llm_time_to_first_token: Some(Duration::from_millis(250)),
            tts_time_to_first_byte: Some(Duration::from_millis(80)),
            response_latency: Some(Duration::from_millis(900)),
            ..TurnMetrics::default()
        });
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ttfb": [{"processor": "llm", "value": 0.25}, {"processor": "tts", "value": 0.08}],
                "processing": [{"processor": "response", "value": 0.9}],
            })
        );
        let mut m = mapper();
        let got = run(
            &mut m,
            vec![EngineEvent::TurnMetrics {
                epoch: ep(0),
                metrics: TurnMetrics::default(),
            }],
        );
        assert!(got.is_empty());
    }
}
