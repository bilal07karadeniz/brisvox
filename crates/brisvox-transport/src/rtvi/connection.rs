//! One RTVI connection: upgrade checks, the `client-ready` handshake, then a reader and a writer
//! side that run concurrently inside the connection's single task. The structure and the shutdown
//! protocol are those of the Twilio connection (`twilio/connection.rs`); the wire is protobuf
//! `pipecat.Frame` in binary frames instead of JSON text.
//!
//! ```text
//!   client --audio/message--> reader --WireEvent--> core (CallLink)
//!                             reader --RtviRequest--> app (RtviInbox)
//!                             reader --replies/ctrl--> writer
//!   client <--audio/message-- writer <--WireOut----- core
//!                             writer <--json-------- app (RtviSender)
//!                             writer --MarkAck/Lead--> core (playout estimate)
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{AudioFrame, SampleRate};
use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::error::ProtocolError as WsProtocolError;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use super::playout::Playout;
use super::proto::{AudioRawFrame, Frame, frame};
use super::protocol::{
    self, ClientMessage, ErrorData, Inbound, Parsed, Reply, RtviEvent, bot_ready, render_event,
    render_reply,
};
use super::{
    Counters, INBOX_CAPACITY, OUTBOUND_CAPACITY, RtviCall, RtviConfig, RtviInbox, RtviRequest,
    RtviSender,
};
use crate::auth::{Authenticator, Grant};
use crate::call::{CallEnds, WireEvent, WireOut, call_pair};
use crate::channel;
use crate::event::CallInfo;
use crate::websocket::connection::ws_config;
use crate::websocket::upgrade_auth::{GrantSlot, accept, authenticate, refusal};
use crate::websocket::{Admitted, MIN_PING_INTERVAL, Shared, timeout};

type Ws = WebSocketStream<TcpStream>;

/// Capacity of the reader-to-writer queue of close requests (the first one wins).
const CTRL_CAPACITY: usize = 2;

/// Capacity of the reader-to-writer queue of replies (`error`, `error-response`, `bot-ready`). A
/// full queue drops the new reply (the writer only falls behind when the client stopped reading,
/// which ends the call anyway).
const REPLY_CAPACITY: usize = 16;

/// Non-fatal faults tolerated on one call before it is closed.
const MAX_FAULTS: u32 = 16;

/// Messages other than `client-ready` tolerated before the handshake completes. Each one costs a
/// parse and (mostly) a socket write while the connection is still unauthenticated, so the budget
/// is smaller than [`MAX_FAULTS`]; past it the connection is closed with 1008.
const MAX_EARLY_MESSAGES: u32 = 8;

/// Allocation unit of the outgoing message buffer: one allocation serves many frames.
const MESSAGE_CHUNK: usize = 16 * 1024;

/// Room left for the protobuf wrapper in a frame of at most `max_audio_bytes`.
const FRAME_OVERHEAD: usize = 64;

/// Longest close reason a close frame may carry (125 bytes of payload minus the status code).
const MAX_CLOSE_REASON: usize = 120;

/// Most `about` strings of the client copied into the call's metadata, and their length.
const MAX_ABOUT_ENTRIES: usize = 8;
const MAX_ABOUT_LEN: usize = 128;

/// The text of Pipecat's `error` for a message that is not a valid RTVI message.
const INVALID_MESSAGE: &str = "Invalid RTVI transport message";

/// State the upgrade check and every connection share.
pub(crate) struct RtviShared {
    pub(crate) config: RtviConfig,
    /// The credential check of the upgrade (static token and/or session tokens).
    pub(crate) auth: Authenticator,
    pub(crate) counters: Arc<Counters>,
    pub(crate) calls: mpsc::Sender<RtviCall>,
}

impl RtviShared {
    /// The upgrade check: path, origin, then the credential (`Authorization: Bearer`, the
    /// `brisvox-token.<token>` subprotocol or `?token=`, see [`crate::auth`]). A missing
    /// credential is 401, an invalid one 403, an oversize one 413.
    fn check(&self, request: &Request) -> Result<Grant, Box<ErrorResponse>> {
        let path = request.uri().path();
        if path.trim_end_matches('/') != self.config.path.trim_end_matches('/') {
            tracing::debug!(path, "rtvi upgrade refused: unknown path");
            return Err(refusal(StatusCode::NOT_FOUND, "not found"));
        }
        if !self.config.allowed_origins.is_empty() {
            let origin = request
                .headers()
                .get("origin")
                .and_then(|value| value.to_str().ok());
            let allowed = origin.is_some_and(|origin| {
                self.config
                    .allowed_origins
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(origin))
            });
            if !allowed {
                tracing::debug!("rtvi upgrade refused: origin not allowed");
                return Err(refusal(StatusCode::FORBIDDEN, "origin not allowed"));
            }
        }
        authenticate(&self.auth, request)
    }
}

/// The upgrade callback: the answer is the response (with `Cache-Control: no-store` and the
/// selected subprotocol), or a refusal. The verified grant is left in the slot.
struct Upgrade(Arc<RtviShared>, GrantSlot);

impl Callback for Upgrade {
    #[allow(clippy::result_large_err)] // the error type is fixed by tungstenite's `Callback`
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        match self.0.check(request) {
            Ok(grant) => {
                *self.1.lock().unwrap_or_else(PoisonError::into_inner) = Some(grant);
                Ok(accept(request, response))
            }
            Err(refusal) => Err(*refusal),
        }
    }
}

/// The client's `client-ready`.
struct Ready {
    id: String,
    version: Option<String>,
    about: Option<Value>,
}

/// What the connection's first messages turned out to be.
enum Opening {
    Ready(Box<Ready>),
    /// The client went away before it was ready.
    Gone,
    Refuse {
        reason: String,
        close: CloseCode,
    },
}

fn refuse_with(reason: &str, close: CloseCode) -> Opening {
    Opening::Refuse {
        reason: reason.to_owned(),
        close,
    }
}

/// Maps a websocket read error to the close to answer with, if the peer is to blame (a socket that
/// vanishes is nobody's fault).
fn blame(error: &WsError) -> Option<(&'static str, CloseCode)> {
    match error {
        WsError::Protocol(WsProtocolError::ResetWithoutClosingHandshake) | WsError::Io(_) => None,
        WsError::Capacity(_) => Some(("message exceeds the frame limit", CloseCode::Size)),
        WsError::Protocol(_) | WsError::Utf8(_) | WsError::AttackAttempt => {
            Some(("websocket protocol violation", CloseCode::Protocol))
        }
        _ => None,
    }
}

/// Encodes one RTVI JSON message as the binary websocket message the client expects.
fn json_message(json: String, buffer: &mut BytesMut) -> Option<Message> {
    if buffer.capacity() < 2 * 1024 {
        buffer.reserve(MESSAGE_CHUNK);
    }
    if Frame::message(json).encode_into(buffer).is_err() {
        buffer.clear();
        return None;
    }
    Some(Message::Binary(buffer.split().freeze()))
}

/// Writes one RTVI message on a connection that is not split yet. `false`: it did not go out.
async fn write_json(ws: &mut Ws, shared: &Shared, rtvi: &RtviShared, json: String) -> bool {
    let mut buffer = BytesMut::new();
    let Some(message) = json_message(json, &mut buffer) else {
        return false;
    };
    let write = timeout(
        &*shared.clock,
        shared.config.write_timeout,
        ws.send(message),
    );
    let sent = matches!(write.await, Ok(Ok(())));
    if sent {
        Counters::add(&rtvi.counters.messages_out, 1);
    }
    sent
}

/// Waits for `client-ready`. Audio before it is dropped; RTVI messages before it are answered
/// with an `error-response`; a text frame is refused.
async fn read_ready(ws: &mut Ws, shared: &Shared, rtvi: &RtviShared) -> Opening {
    let config = &shared.config;
    let read = async {
        let mut early = 0_u32;
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    let frame = match Frame::parse(bytes) {
                        Ok(frame) => frame,
                        Err(error) => {
                            tracing::debug!(%error, "rtvi frame before client-ready is invalid");
                            return refuse_with("invalid protobuf frame", CloseCode::Protocol);
                        }
                    };
                    let Some(frame::Kind::Message(message)) = frame.kind else {
                        // Audio the client queued before it is ready, or frames this server
                        // does not use: dropped.
                        continue;
                    };
                    if message.data.len() > config.max_control_bytes {
                        return refuse_with("control message too large", CloseCode::Size);
                    }
                    let parsed = protocol::parse(&message.data);
                    if !matches!(
                        &parsed,
                        Parsed::Message(Inbound {
                            message: ClientMessage::ClientReady { .. }
                                | ClientMessage::DisconnectBot,
                            ..
                        })
                    ) {
                        early = early.saturating_add(1);
                        if early > MAX_EARLY_MESSAGES {
                            return refuse_with(
                                "too many messages before client-ready",
                                CloseCode::Policy,
                            );
                        }
                    }
                    let reply = match parsed {
                        Parsed::Message(Inbound {
                            id,
                            message: ClientMessage::ClientReady { version, about },
                        }) => return Opening::Ready(Box::new(Ready { id, version, about })),
                        Parsed::Message(Inbound {
                            message: ClientMessage::DisconnectBot,
                            ..
                        }) => return Opening::Gone,
                        Parsed::Message(Inbound { id, .. }) => {
                            Some(error_response(&id, "RTVI client-ready expected first"))
                        }
                        Parsed::BadData { id, error } => Some(error_response(&id, &error)),
                        Parsed::Invalid => Some(render_event(&RtviEvent::Error(ErrorData {
                            error: INVALID_MESSAGE.to_owned(),
                            fatal: false,
                        }))),
                        Parsed::Foreign => None,
                    };
                    if let Some(reply) = reply
                        && !write_json(ws, shared, rtvi, reply).await
                    {
                        return Opening::Gone;
                    }
                }
                Some(Ok(Message::Text(_))) => {
                    return refuse_with(
                        "RTVI over websocket uses binary protobuf frames, not text",
                        CloseCode::Unsupported,
                    );
                }
                Some(Ok(Message::Close(_))) | None => return Opening::Gone,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return match blame(&error) {
                        Some((reason, close)) => refuse_with(reason, close),
                        None => Opening::Gone,
                    };
                }
            }
        }
    };
    tokio::select! {
        biased;
        // A browser that is connected but not ready yet is told, like a ready one: 1001.
        () = shared.cancel.cancelled() => refuse_with("server shutdown", CloseCode::Away),
        outcome = timeout(&*shared.clock, config.start_timeout, read) => match outcome {
            Ok(opening) => opening,
            Err(_) => refuse_with("no client-ready in time", CloseCode::Policy),
        },
    }
}

fn error_response(id: &str, error: &str) -> String {
    render_reply(
        &Reply::ErrorResponse {
            error: error.to_owned(),
        },
        id,
    )
}

/// Closes a connection that is not split yet and waits briefly for the peer's close frame.
async fn refuse(ws: &mut Ws, shared: &Shared, reason: &str, close: CloseCode) {
    let write = ws.send(Message::Close(Some(CloseFrame {
        code: close,
        reason: close_reason(reason),
    })));
    let clock = &*shared.clock;
    if !matches!(
        timeout(clock, shared.config.write_timeout, write).await,
        Ok(Ok(()))
    ) {
        return;
    }
    let drain = async { while let Some(Ok(_)) = ws.next().await {} };
    tokio::select! {
        biased;
        () = shared.cancel.cancelled() => {}
        _ = timeout(clock, shared.config.close_grace, drain) => {}
    }
}

/// A close reason that fits a close frame.
fn close_reason(reason: &str) -> Utf8Bytes {
    let mut end = reason.len().min(MAX_CLOSE_REASON);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Utf8Bytes::from(reason.get(..end).unwrap_or_default())
}

/// Call metadata from the handshake.
fn call_info(
    call_id: &str,
    peer: SocketAddr,
    config: &RtviConfig,
    ready: &Ready,
    grant: Option<&Grant>,
) -> CallInfo {
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("transport".to_owned(), "rtvi".to_owned());
    metadata.insert(
        CallInfo::INPUT_SAMPLE_RATE_KEY.to_owned(),
        config.input_rate.hz().to_string(),
    );
    metadata.insert(
        CallInfo::OUTPUT_SAMPLE_RATE_KEY.to_owned(),
        config.output_rate.hz().to_string(),
    );
    metadata.insert(
        "rtvi_server_version".to_owned(),
        protocol::PROTOCOL_VERSION.to_owned(),
    );
    if let Some(version) = &ready.version {
        metadata.insert("rtvi_client_version".to_owned(), clip_value(version));
    }
    if let Some(Value::Object(about)) = &ready.about {
        for (key, value) in about.iter().take(MAX_ABOUT_ENTRIES) {
            if let Value::String(value) = value {
                metadata.insert(format!("client.{}", clip_value(key)), clip_value(value));
            }
        }
    }
    metadata.insert("peer_addr".to_owned(), peer.to_string());
    if let Some(grant) = grant {
        grant.add_metadata(&mut metadata);
    }
    CallInfo {
        call_id: call_id.to_owned(),
        metadata,
    }
}

/// A client-supplied string cut to a bounded length (on a char boundary).
fn clip_value(text: &str) -> String {
    let mut end = text.len().min(MAX_ABOUT_LEN);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// Serves one admitted connection to its end. Holds the call permit (and the gate's ticket, until
/// it goes into the call's link) for as long as it runs.
pub(crate) async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    mut admitted: Admitted,
    shared: Arc<Shared>,
    rtvi: Arc<RtviShared>,
) {
    let _ = stream.set_nodelay(true);
    let grant_slot = GrantSlot::default();
    let handshake = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        Upgrade(Arc::clone(&rtvi), Arc::clone(&grant_slot)),
        Some(ws_config(&shared.config)),
    );
    let mut ws = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return,
        handshake = timeout(&*shared.clock, shared.config.handshake_timeout, handshake) => match handshake {
            Ok(Ok(ws)) => ws,
            Ok(Err(error)) => {
                tracing::debug!(%peer, %error, "rtvi handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(%peer, "rtvi handshake timed out");
                return;
            }
        },
    };
    let ready = match read_ready(&mut ws, &shared, &rtvi).await {
        Opening::Ready(ready) => ready,
        Opening::Gone => return,
        Opening::Refuse { reason, close } => {
            tracing::debug!(%peer, reason, "rtvi client-ready refused");
            refuse(&mut ws, &shared, &reason, close).await;
            return;
        }
    };

    let call_id = format!("rtvi-{}", shared.next_call.fetch_add(1, Ordering::Relaxed));
    let grant = grant_slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let info = call_info(&call_id, peer, &rtvi.config, &ready, grant.as_ref());
    let (mut link, ends) = call_pair();
    admitted.attach(&mut link);
    let CallEnds {
        events,
        outgoing,
        ended,
    } = ends;
    // The queue is empty, so `Started` goes in at once; the core sees it first.
    if events.send(WireEvent::Started(info)).await.is_err() {
        return;
    }
    let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
    let auto_events = Arc::new(AtomicBool::new(rtvi.config.auto_events));
    let call = RtviCall {
        link,
        sender: RtviSender {
            tx: out_tx,
            clock: shared.clock.clone(),
            send_timeout: shared.config.send_timeout,
            max_bytes: shared.config.max_control_bytes,
            auto_events: Arc::clone(&auto_events),
        },
        inbox: RtviInbox { rx: inbox_rx },
    };
    let handed = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => None,
        sent = timeout(&*shared.clock, shared.config.send_timeout, rtvi.calls.send(call)) => {
            Some(matches!(sent, Ok(Ok(()))))
        }
    };
    let Some(handed) = handed else {
        refuse(&mut ws, &shared, "server shutdown", CloseCode::Away).await;
        return;
    };
    if !handed {
        let busy = RtviEvent::Error(ErrorData {
            error: "engine is not taking calls".to_owned(),
            fatal: true,
        });
        write_json(&mut ws, &shared, &rtvi, render_event(&busy)).await;
        refuse(
            &mut ws,
            &shared,
            "engine is not taking calls",
            CloseCode::Again,
        )
        .await;
        return;
    }
    Counters::add(&rtvi.counters.calls, 1);
    tracing::info!(%call_id, %peer, "rtvi call started");

    // The handshake answer, before the writer starts (so before any bot audio or event).
    let check = protocol::check_version(ready.version.as_deref());
    if let Some(warning) = &check.warning
        && !write_json(&mut ws, &shared, &rtvi, error_response(&ready.id, warning)).await
    {
        return;
    }
    let answer = render_reply(&bot_ready(&check.echo), &ready.id);
    if !write_json(&mut ws, &shared, &rtvi, answer).await {
        return;
    }

    let (sink, stream) = ws.split();
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_CAPACITY);
    let (reply_tx, reply_rx) = mpsc::channel(REPLY_CAPACITY);
    let conn = Conn {
        shared: Arc::clone(&shared),
        rtvi: Arc::clone(&rtvi),
        call_id: call_id.clone(),
        auto_events,
        ended,
        reader_done: CancellationToken::new(),
        writer_done: CancellationToken::new(),
    };
    let reader_events = events.clone();
    let reader = async {
        Reader {
            conn: &conn,
            events: Some(reader_events),
            replies: reply_tx,
            ctrl: ctrl_tx,
            inbox: inbox_tx,
            seq: 0,
            faults: 0,
            rate_mismatch_reported: false,
        }
        .run(stream)
        .await;
        conn.reader_done.cancel();
    };
    let writer = async {
        Writer {
            conn: &conn,
            sink,
            events: Some(events),
            min_epoch: TurnEpoch::ZERO,
            rate_mismatch_reported: false,
            playout: Playout::default(),
            last_lead: None,
            buffer: BytesMut::new(),
        }
        .run(outgoing, ctrl_rx, reply_rx, out_rx)
        .await;
    };
    tokio::join!(reader, writer);
    tracing::info!(%call_id, "rtvi call ended");
}

/// What both sides of a connection share.
struct Conn {
    shared: Arc<Shared>,
    rtvi: Arc<RtviShared>,
    call_id: String,
    /// Whether the writer derives the bot speaking events (shared with the call's sender).
    auto_events: Arc<AtomicBool>,
    /// Cancelled by the core once it ended the call.
    ended: CancellationToken,
    /// Cancelled when the reader returns.
    reader_done: CancellationToken,
    /// Cancelled when the writer has said its last word (the close frame, or a broken socket).
    /// Until then the reader of a closing connection does not read: reading a close frame makes
    /// the websocket layer refuse every later write, so the farewell would be lost.
    writer_done: CancellationToken,
}

impl Conn {
    fn counters(&self) -> &Counters {
        &self.rtvi.counters
    }

    /// Resolves once the connection should wind down: the server stops, the core ended the call,
    /// or the writer is done.
    async fn winding_down(&self) {
        tokio::select! {
            () = self.shared.cancel.cancelled() => {}
            () = self.ended.cancelled() => {}
            () = self.writer_done.cancelled() => {}
        }
    }

    /// Resolves once the server stops or the core ended the call: a blocked write must give up.
    async fn interrupted(&self) {
        tokio::select! {
            () = self.shared.cancel.cancelled() => {}
            () = self.ended.cancelled() => {}
        }
    }
}

/// What the reader asks the writer to do.
enum Ctrl {
    /// Close the socket with this code and reason.
    Close {
        close: CloseCode,
        reason: &'static str,
    },
}

/// Outcome of handing an event to the core.
enum Delivery {
    Delivered,
    /// The server is stopping, the call ended or the core dropped its end: stop delivering.
    Stopped,
    /// The core takes nothing for `send_timeout`.
    Stalled,
}

/// What the reader does after a message.
enum Flow {
    Continue,
    /// Stop reading for the engine; wait for the peer's close.
    Drain,
    /// The peer closed the connection: the reader is finished.
    Exit,
}

struct Reader<'a> {
    conn: &'a Conn,
    /// `None` once the reader stopped feeding the core (the core then sees the call end).
    events: Option<channel::Sender<WireEvent>>,
    /// Replies for the writer, already rendered.
    replies: mpsc::Sender<String>,
    ctrl: mpsc::Sender<Ctrl>,
    /// Client requests for the app. Full or closed: answered with an error, never waited for.
    inbox: mpsc::Sender<RtviRequest>,
    seq: u64,
    faults: u32,
    /// The first client frame at a rate other than `input_rate` was reported to the core.
    rate_mismatch_reported: bool,
}

impl Reader<'_> {
    async fn run(mut self, mut stream: SplitStream<Ws>) {
        let conn = self.conn;
        let shared = &conn.shared;
        let clock = &shared.clock;
        let config = &shared.config;
        let mut last_seen = clock.now();
        let idle = clock.sleep_until(deadline_after(last_seen, config.ping_timeout));
        tokio::pin!(idle);
        let drain = clock.sleep(config.close_grace);
        tokio::pin!(drain);
        let mut draining = false;
        // One future for the whole loop: the select below polls it once per frame.
        let winding_down = conn.winding_down();
        tokio::pin!(winding_down);
        loop {
            // A closing connection is not read before the writer has said goodbye.
            let may_read = !draining || conn.writer_done.is_cancelled();
            tokio::select! {
                biased;
                () = &mut winding_down, if !draining => {
                    draining = true;
                    drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                }
                () = &mut drain, if draining => return,
                () = &mut idle, if !draining => {
                    // Frames keep arriving without touching the timer; re-arm only when it fires.
                    let due = deadline_after(last_seen, config.ping_timeout);
                    if clock.now() >= due {
                        self.fatal("no frame from the client in time", CloseCode::Policy).await;
                        draining = true;
                        drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                        continue;
                    }
                    idle.as_mut().reset(due);
                }
                () = conn.writer_done.cancelled(), if !may_read => {}
                message = stream.next(), if may_read => match message {
                    None => return,
                    Some(Err(error)) => {
                        if !draining && let Some((reason, close)) = blame(&error) {
                            self.fatal(reason, close).await;
                        }
                        return;
                    }
                    Some(Ok(message)) => {
                        last_seen = clock.now();
                        if draining {
                            if matches!(message, Message::Close(_)) {
                                return;
                            }
                            continue;
                        }
                        match self.handle(message).await {
                            Flow::Continue => {}
                            Flow::Drain => {
                                draining = true;
                                drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                            }
                            Flow::Exit => return,
                        }
                    }
                },
            }
        }
    }

    async fn handle(&mut self, message: Message) -> Flow {
        match message {
            Message::Binary(bytes) => self.binary(bytes).await,
            // The default JS serializer cannot even produce a text frame, and the server must
            // never send one; a client that does is speaking another protocol.
            Message::Text(_) => {
                self.fault(
                    "text frames are not part of the RTVI websocket protocol (binary protobuf only)"
                        .to_owned(),
                )
                .await
            }
            Message::Close(_) => {
                // A close frame is a clean hang-up; a vanished socket is not (see `run`).
                self.deliver(WireEvent::Hangup).await;
                self.finish_input();
                Flow::Exit
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Flow::Continue,
        }
    }

    async fn binary(&mut self, bytes: Bytes) -> Flow {
        let frame = match Frame::parse(bytes) {
            Ok(frame) => frame,
            Err(error) => return self.fault(format!("invalid protobuf frame: {error}")).await,
        };
        match frame.kind {
            Some(frame::Kind::Audio(audio)) => self.audio(audio).await,
            Some(frame::Kind::Message(message)) => self.control(&message.data).await,
            // TextFrame, TranscriptionFrame, InterruptionFrame and empty frames have no meaning
            // here (Pipecat accepts them from clients; this server has nothing to do with them).
            Some(_) | None => Flow::Continue,
        }
    }

    /// Caller audio: PCM16 mono at the frame's own rate.
    async fn audio(&mut self, audio: AudioRawFrame) -> Flow {
        // The websocket layer only bounds a message by the larger of the two limits.
        let max_audio = self.conn.shared.config.max_audio_bytes;
        if audio.audio.len() > max_audio {
            self.fatal("audio frame too large", CloseCode::Size).await;
            return Flow::Drain;
        }
        if audio.num_channels > 1 {
            return self.fault("audio must be mono".to_owned()).await;
        }
        let rate = if audio.sample_rate == 0 {
            self.conn.rtvi.config.input_rate
        } else if let Ok(rate) = SampleRate::try_from(audio.sample_rate) {
            rate
        } else {
            return self
                .fault(format!(
                    "unsupported audio sample rate {}",
                    audio.sample_rate
                ))
                .await;
        };
        if audio.audio.is_empty() {
            return Flow::Continue;
        }
        let expected = self.conn.rtvi.config.input_rate;
        if rate != expected && !self.rate_mismatch_reported {
            // The frame is passed on at its own rate, but the stages behind the transport were
            // set up for `input_rate`: tell the engine once (the core only counts mismatches).
            self.rate_mismatch_reported = true;
            let message = format!(
                "client audio is {rate} but this rtvi call expects {expected} (the client's                  recorderSampleRate must match RtviConfig::input_rate); frames are passed on at                  their own rate"
            );
            match self.flow(WireEvent::Fault { message }).await {
                Flow::Continue => {}
                other => return other,
            }
        }
        let now = self.conn.shared.clock.now();
        let Ok(frame) = AudioFrame::new(audio.audio, rate, now, self.seq, TurnEpoch::ZERO) else {
            return self
                .fault("audio is not whole PCM16 samples".to_owned())
                .await;
        };
        self.seq = self.seq.saturating_add(1);
        Counters::add(&self.conn.counters().audio_in, 1);
        // Input audio is `DropOldest`: this never waits, so no timeout is needed.
        let Some(events) = &self.events else {
            return Flow::Drain;
        };
        match events.send(WireEvent::Audio(frame)).await {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Drain,
        }
    }

    /// An RTVI message from the client.
    async fn control(&mut self, json: &str) -> Flow {
        let max = self.conn.shared.config.max_control_bytes;
        if json.len() > max {
            self.fatal("control message too large", CloseCode::Size)
                .await;
            return Flow::Drain;
        }
        match protocol::parse(json) {
            Parsed::Foreign => Flow::Continue,
            Parsed::Invalid => {
                self.reply(render_event(&RtviEvent::Error(ErrorData {
                    error: INVALID_MESSAGE.to_owned(),
                    fatal: false,
                })));
                self.fault(INVALID_MESSAGE.to_owned()).await
            }
            Parsed::BadData { id, error } => {
                self.reply(error_response(&id, &error));
                Flow::Continue
            }
            Parsed::Message(Inbound { id, message }) => {
                Counters::add(&self.conn.counters().messages_in, 1);
                self.message(id, message).await
            }
        }
    }

    async fn message(&mut self, id: String, message: ClientMessage) -> Flow {
        match message {
            ClientMessage::ClientReady { version, .. } => {
                // A repeated handshake: answer it again, the call goes on.
                let check = protocol::check_version(version.as_deref());
                if let Some(warning) = &check.warning {
                    self.reply(error_response(&id, warning));
                }
                self.reply(render_reply(&bot_ready(&check.echo), &id));
                Flow::Continue
            }
            ClientMessage::DisconnectBot => {
                self.deliver(WireEvent::Hangup).await;
                self.finish_input();
                Flow::Drain
            }
            ClientMessage::ClientMessage { t, d } => {
                self.to_inbox(
                    Some(&id),
                    RtviRequest::ClientMessage {
                        id: id.clone(),
                        t,
                        d,
                    },
                );
                Flow::Continue
            }
            ClientMessage::SendText {
                content,
                run_immediately,
                audio_response,
            } => {
                // No reply is defined for `send-text` (rtvi.md): a refused one is only counted.
                self.to_inbox(
                    None,
                    RtviRequest::SendText {
                        content,
                        run_immediately,
                        audio_response,
                    },
                );
                Flow::Continue
            }
            ClientMessage::FunctionCallResult {
                function_name,
                tool_call_id,
                arguments,
                result,
            } => {
                // Likewise no reply for `llm-function-call-result`.
                self.to_inbox(
                    None,
                    RtviRequest::FunctionCallResult {
                        function_name,
                        tool_call_id,
                        arguments,
                        result,
                    },
                );
                Flow::Continue
            }
            ClientMessage::Dtmf { buttons } => {
                for digit in buttons {
                    let at = self.conn.shared.clock.now();
                    match self.flow(WireEvent::Dtmf { digit, at }).await {
                        Flow::Continue => {}
                        other => return other,
                    }
                }
                Flow::Continue
            }
            ClientMessage::Ignored(_) => Flow::Continue,
            ClientMessage::Unsupported(kind) => {
                // Pipecat's text for a type it has no case for.
                let kind = clip_value(&kind);
                self.reply(error_response(&id, &format!("Unsupported type {kind}")));
                Flow::Continue
            }
        }
    }

    /// Hands a request to the app. The inbox never makes the reader wait: when it is full or
    /// nobody reads it, the request is counted and, if the protocol defines a reply to it
    /// (`id` is `Some`: `client-message`), answered with an `error-response` right here.
    fn to_inbox(&self, id: Option<&str>, request: RtviRequest) {
        let refused = match self.inbox.try_send(request) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(_)) => "server busy",
            Err(mpsc::error::TrySendError::Closed(_)) => "no handler for this message",
        };
        Counters::add(&self.conn.counters().requests_unanswered, 1);
        if let Some(id) = id {
            self.reply(error_response(id, refused));
        }
    }

    /// Queues a reply for the writer; dropped if the queue is full (see [`REPLY_CAPACITY`]).
    fn reply(&self, json: String) {
        let _ = self.replies.try_send(json);
    }

    /// Delivers `event` and maps the outcome to what the reader does next.
    async fn flow(&mut self, event: WireEvent) -> Flow {
        match self.deliver(event).await {
            Delivery::Delivered => Flow::Continue,
            Delivery::Stopped => Flow::Drain,
            Delivery::Stalled => {
                self.fatal("the engine is not taking input", CloseCode::Again)
                    .await;
                Flow::Drain
            }
        }
    }

    async fn deliver(&self, event: WireEvent) -> Delivery {
        let shared = &self.conn.shared;
        let Some(events) = &self.events else {
            return Delivery::Stopped;
        };
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => Delivery::Stopped,
            () = self.conn.ended.cancelled() => Delivery::Stopped,
            sent = timeout(&*shared.clock, shared.config.send_timeout, events.send(event)) => {
                match sent {
                    Ok(Ok(())) => Delivery::Delivered,
                    Ok(Err(_)) => Delivery::Stopped,
                    Err(_) => Delivery::Stalled,
                }
            }
        }
    }

    /// A recoverable fault: the engine sees a fault event; too many end the call.
    async fn fault(&mut self, message: String) -> Flow {
        Counters::add(&self.conn.counters().faults, 1);
        self.faults = self.faults.saturating_add(1);
        if self.faults > MAX_FAULTS {
            self.fatal("too many protocol faults", CloseCode::Policy)
                .await;
            return Flow::Drain;
        }
        self.flow(WireEvent::Fault { message }).await
    }

    /// A fatal fault: tells the writer to close, and the engine to note it.
    async fn fatal(&mut self, reason: &'static str, close: CloseCode) {
        tracing::debug!(call_id = %self.conn.call_id, reason, "rtvi call closed by protocol fault");
        // Only the first request matters; a full queue means one is already pending.
        let _ = self.ctrl.try_send(Ctrl::Close { close, reason });
        Counters::add(&self.conn.counters().faults, 1);
        self.deliver(WireEvent::Fault {
            message: reason.to_owned(),
        })
        .await;
        self.finish_input();
    }

    /// Stops feeding the core: dropping the queue's sender is how the core learns the client is
    /// gone (a hang-up queued before stays readable). The writer drops its clone when the reader
    /// is done.
    fn finish_input(&mut self) {
        self.events = None;
    }
}

struct Writer<'a> {
    conn: &'a Conn,
    sink: SplitSink<Ws, Message>,
    /// For mark acknowledgements, lead reports and faults. Dropped when the reader is done so
    /// that the core sees the channel close.
    events: Option<channel::Sender<WireEvent>>,
    min_epoch: TurnEpoch,
    rate_mismatch_reported: bool,
    playout: Playout,
    last_lead: Option<Instant>,
    /// Buffer the protobuf frames are encoded in; one allocation serves many frames.
    buffer: BytesMut,
}

/// What the writer's loop does next.
enum Step {
    Shutdown,
    Ended,
    Ctrl(Option<Ctrl>),
    Due,
    Reply(Option<String>),
    App(Option<String>),
    Out(Option<WireOut>),
    Ping,
    ReaderDone,
    GraceOver,
}

/// Why a write did not complete.
enum WriteFailure {
    /// The socket is closed or broke.
    Closed,
    /// The client did not take the data within the write timeout.
    Slow,
    /// The server is shutting down or the core ended the call while the write was blocked.
    Interrupted,
    /// The engine takes no events (a mark acknowledgement could not be delivered).
    EngineStalled,
}

/// How the writer leaves its main loop.
enum Departure {
    /// Close the socket with `close`.
    Farewell {
        reason: &'static str,
        close: CloseCode,
    },
    /// A close frame was already sent (a fatal fault).
    Said,
    /// The socket is gone or stuck: nothing more can be written.
    Broken,
}

impl Writer<'_> {
    /// One write, giving up after `limit`. With `interruptible`, it also gives up the moment the
    /// server shuts down or the core ends the call, so a client that stopped reading cannot hold
    /// the call's permit for a whole write timeout.
    async fn write(
        &mut self,
        message: Message,
        limit: Duration,
        interruptible: bool,
    ) -> Result<(), WriteFailure> {
        let conn = self.conn;
        let clock = &*conn.shared.clock;
        let write = timeout(clock, limit, self.sink.send(message));
        let outcome = if interruptible {
            tokio::select! {
                biased;
                () = conn.interrupted() => return Err(WriteFailure::Interrupted),
                outcome = write => outcome,
            }
        } else {
            write.await
        };
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(WriteFailure::Closed),
            Err(_) => {
                tracing::warn!(call_id = %conn.call_id, "rtvi client does not take output, closing the call");
                Err(WriteFailure::Slow)
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), WriteFailure> {
        let limit = self.conn.shared.config.write_timeout;
        self.write(message, limit, true).await
    }

    /// Sends one protobuf frame as a binary message.
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), WriteFailure> {
        if self.buffer.capacity() < 2 * 1024 {
            self.buffer.reserve(MESSAGE_CHUNK);
        }
        if frame.encode_into(&mut self.buffer).is_err() {
            self.buffer.clear();
            return Ok(());
        }
        let bytes = self.buffer.split().freeze();
        self.send(Message::Binary(bytes)).await
    }

    /// Sends one RTVI JSON message.
    async fn send_json(&mut self, json: String) -> Result<(), WriteFailure> {
        self.send_frame(&Frame::message(json)).await?;
        Counters::add(&self.conn.counters().messages_out, 1);
        Ok(())
    }

    /// Sends an event the adapter derived itself.
    async fn emit(&mut self, event: RtviEvent) -> Result<(), WriteFailure> {
        if !self.conn.auto_events.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.send_json(render_event(&event)).await
    }

    /// How long the goodbye may take: it is the last thing the call does, so it gets a short
    /// bound.
    fn farewell_limit(&self) -> Duration {
        let config = &self.conn.shared.config;
        config.write_timeout.min(config.close_grace)
    }

    /// Returns whether the socket still answers.
    async fn farewell(&mut self, reason: &str, close: CloseCode) -> bool {
        let limit = self.farewell_limit();
        let frame = CloseFrame {
            code: close,
            reason: close_reason(reason),
        };
        !matches!(
            self.write(Message::Close(Some(frame)), limit, false).await,
            Err(WriteFailure::Slow)
        )
    }

    /// What a failed write leads to.
    fn depart(failure: &WriteFailure, conn: &Conn) -> Departure {
        match failure {
            WriteFailure::Closed | WriteFailure::Slow => Departure::Broken,
            WriteFailure::EngineStalled => Departure::Farewell {
                reason: "the engine is not taking input",
                close: CloseCode::Again,
            },
            WriteFailure::Interrupted if conn.shared.cancel.is_cancelled() => Departure::Farewell {
                reason: "server shutdown",
                close: CloseCode::Away,
            },
            WriteFailure::Interrupted => Departure::Farewell {
                reason: "call ended",
                close: CloseCode::Normal,
            },
        }
    }

    async fn run(
        mut self,
        outgoing: channel::Receiver<WireOut>,
        ctrl: mpsc::Receiver<Ctrl>,
        replies: mpsc::Receiver<String>,
        app: mpsc::Receiver<String>,
    ) {
        let conn = self.conn;
        let departure = self.serve_loop(outgoing, ctrl, replies, app).await;
        let socket_ok = match departure {
            Departure::Farewell { reason, close } => self.farewell(reason, close).await,
            Departure::Said => true,
            Departure::Broken => false,
        };
        // The core must not wait on a writer that is gone.
        self.events = None;
        // From here on the reader may read the peer's close frame.
        conn.writer_done.cancel();
        if socket_ok {
            self.finish().await;
        }
    }

    /// After the last word: waits (bounded) for the reader to see the peer's close frame, then
    /// flushes and closes the sink, which sends the close reply the websocket layer queued.
    async fn finish(&mut self) {
        let conn = self.conn;
        let clock = &*conn.shared.clock;
        let grace = conn.shared.config.close_grace;
        tokio::select! {
            biased;
            () = conn.reader_done.cancelled() => {}
            () = clock.sleep(grace) => {}
        }
        let _ = timeout(clock, grace, self.sink.close()).await;
    }

    async fn serve_loop(
        &mut self,
        mut outgoing: channel::Receiver<WireOut>,
        mut ctrl: mpsc::Receiver<Ctrl>,
        mut replies: mpsc::Receiver<String>,
        mut app: mpsc::Receiver<String>,
    ) -> Departure {
        let conn = self.conn;
        let shared = &conn.shared;
        let clock = &shared.clock;
        let config = &shared.config;
        let ping_interval = config.ping_interval.max(MIN_PING_INTERVAL);
        let ping = clock.sleep(ping_interval);
        tokio::pin!(ping);
        let grace = clock.sleep(config.close_grace);
        tokio::pin!(grace);
        let due_timer = clock.sleep(ping_interval);
        tokio::pin!(due_timer);
        let mut armed: Option<Instant> = None;
        let mut grace_started = false;
        let mut ctrl_open = true;
        let mut replies_open = true;
        let mut app_open = true;
        // The select below runs once per frame: its token futures are created once.
        let shutdown = shared.cancel.cancelled();
        let ended = conn.ended.cancelled();
        let reader_done = conn.reader_done.cancelled();
        tokio::pin!(shutdown, ended, reader_done);
        loop {
            let next = self.playout.next_due();
            if next != armed {
                if let Some(at) = next {
                    due_timer.as_mut().reset(at);
                }
                armed = next;
            }
            let step = tokio::select! {
                biased;
                () = &mut shutdown => Step::Shutdown,
                received = ctrl.recv(), if ctrl_open => Step::Ctrl(received),
                () = &mut ended => Step::Ended,
                () = &mut due_timer, if armed.is_some() => Step::Due,
                received = replies.recv(), if replies_open => Step::Reply(received),
                received = app.recv(), if app_open => Step::App(received),
                out = outgoing.recv() => Step::Out(out),
                () = &mut ping => Step::Ping,
                () = &mut reader_done, if !grace_started => Step::ReaderDone,
                () = &mut grace, if grace_started => Step::GraceOver,
            };
            let sent = match step {
                Step::Shutdown => {
                    return Departure::Farewell {
                        reason: "server shutdown",
                        close: CloseCode::Away,
                    };
                }
                Step::Ended | Step::GraceOver | Step::Out(None | Some(WireOut::Hangup)) => {
                    return Departure::Farewell {
                        reason: "call ended",
                        close: CloseCode::Normal,
                    };
                }
                Step::Ctrl(None) => {
                    ctrl_open = false;
                    Ok(())
                }
                Step::Ctrl(Some(Ctrl::Close { close, reason })) => {
                    let frame = CloseFrame {
                        code: close,
                        reason: close_reason(reason),
                    };
                    return match self.send(Message::Close(Some(frame))).await {
                        Ok(()) => Departure::Said,
                        Err(failure) => Self::depart(&failure, conn),
                    };
                }
                Step::Due => {
                    armed = None;
                    self.due().await
                }
                Step::Reply(Some(json)) | Step::App(Some(json)) => self.send_json(json).await,
                Step::Reply(None) => {
                    replies_open = false;
                    Ok(())
                }
                // The app dropped its sender (the bare `Transport::accept` path): the call
                // goes on without RTVI events.
                Step::App(None) => {
                    app_open = false;
                    Ok(())
                }
                Step::Out(Some(WireOut::Audio(frame))) => self.audio(&frame).await,
                Step::Out(Some(WireOut::Clear { epoch })) => self.clear(epoch).await,
                Step::Out(Some(WireOut::Mark { id })) => self.engine_mark(id).await,
                Step::Ping => {
                    let sent = self.send(Message::Ping(Bytes::new())).await;
                    ping.as_mut()
                        .reset(deadline_after(clock.now(), ping_interval));
                    sent
                }
                Step::ReaderDone => {
                    grace_started = true;
                    // The core must see the input end now, not after the grace period.
                    self.events = None;
                    grace
                        .as_mut()
                        .reset(deadline_after(clock.now(), config.close_grace));
                    Ok(())
                }
            };
            if let Err(failure) = sent {
                return Self::depart(&failure, conn);
            }
        }
    }

    /// Delivers an event to the core, waiting at most `send_timeout`. Nothing to do once the
    /// reader is done (`events` is `None`).
    async fn deliver(&self, event: WireEvent) -> Result<(), WriteFailure> {
        let shared = &self.conn.shared;
        let Some(events) = &self.events else {
            return Ok(());
        };
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => Err(WriteFailure::Interrupted),
            () = self.conn.ended.cancelled() => Err(WriteFailure::Interrupted),
            sent = timeout(&*shared.clock, shared.config.send_timeout, events.send(event)) => {
                match sent {
                    Ok(Ok(())) => Ok(()),
                    // The core dropped its end: the call is over.
                    Ok(Err(_)) => Err(WriteFailure::Interrupted),
                    Err(_) => Err(WriteFailure::EngineStalled),
                }
            }
        }
    }

    /// What the playout estimate says is due: mark acknowledgements, the end of speech.
    async fn due(&mut self) -> Result<(), WriteFailure> {
        let now = self.conn.shared.clock.now();
        let due = self.playout.take_due(now);
        for id in due.marks {
            self.deliver(WireEvent::MarkAck { id }).await?;
        }
        self.report_lead(now, true).await?;
        if due.stopped_speaking {
            self.emit(RtviEvent::BotStoppedSpeaking).await?;
        }
        Ok(())
    }

    /// Reports the estimated lead to the core, at most every `lead_interval` unless `force`.
    async fn report_lead(&mut self, now: Instant, force: bool) -> Result<(), WriteFailure> {
        let interval = self.conn.rtvi.config.lead_interval;
        if interval.is_zero() {
            return Ok(());
        }
        let fresh = self
            .last_lead
            .is_some_and(|last| now.saturating_duration_since(last) < interval);
        if fresh && !force {
            return Ok(());
        }
        self.last_lead = Some(now);
        // A level: the channel replaces a queued report and never waits.
        self.deliver(WireEvent::Lead {
            lead: self.playout.lead(now),
        })
        .await
    }

    /// Barge-in: forget the playout estimate and the marks waiting on it (the core reports them
    /// as cut and ignores any acknowledgement), and tell the client. Audio the client already
    /// holds keeps playing: RTVI has no clear.
    async fn clear(&mut self, epoch: TurnEpoch) -> Result<(), WriteFailure> {
        self.min_epoch = self.min_epoch.max(epoch);
        let was_speaking = self.playout.clear();
        self.last_lead = None;
        self.emit(RtviEvent::BotInterrupted).await?;
        if was_speaking {
            self.emit(RtviEvent::BotStoppedSpeaking).await?;
        }
        // The estimate is empty now: the core must not keep the pre-barge-in lead until the next
        // audio frame (`next_due` is `None`, so nothing else would report it).
        let now = self.conn.shared.clock.now();
        self.report_lead(now, true).await
    }

    /// An engine mark: acknowledged when the playout estimate reaches it.
    async fn engine_mark(&mut self, id: String) -> Result<(), WriteFailure> {
        let now = self.conn.shared.clock.now();
        if let Some(early) = self.playout.mark(now, id) {
            // The bound forced the oldest mark out: answer it now rather than never.
            self.deliver(WireEvent::MarkAck { id: early }).await?;
        }
        Ok(())
    }

    /// Writes one frame of bot audio as protobuf `AudioRawFrame`s.
    async fn audio(&mut self, frame: &AudioFrame) -> Result<(), WriteFailure> {
        // Audio of an older epoch than the last clear never goes out.
        if frame.epoch().get() < self.min_epoch.get() {
            return Ok(());
        }
        let rate = self.conn.rtvi.config.output_rate;
        if frame.sample_rate() != rate {
            // Resampling is DSP work for the audio layer, not for a transport task.
            Counters::add(&self.conn.counters().bot_audio_dropped, 1);
            if !self.rate_mismatch_reported {
                self.rate_mismatch_reported = true;
                self.deliver(WireEvent::Fault {
                    message: format!(
                        "bot audio is {} but this rtvi call plays at {rate}; mismatched audio is dropped",
                        frame.sample_rate()
                    ),
                })
                .await?;
            }
            return Ok(());
        }
        let now = self.conn.shared.clock.now();
        if self.playout.audio_sent(now, frame.duration()) {
            self.emit(RtviEvent::BotStartedSpeaking).await?;
        }
        // Even byte counts keep every piece whole PCM16 samples.
        let max = self
            .conn
            .shared
            .config
            .max_audio_bytes
            .saturating_sub(FRAME_OVERHEAD)
            .max(AudioFrame::BYTES_PER_SAMPLE)
            & !1;
        let mut data = frame.data().clone();
        while !data.is_empty() {
            let take = data.len().min(max);
            let part = data.split_to(take);
            self.send_frame(&Frame::audio(part, rate.hz())).await?;
            let counters = self.conn.counters();
            Counters::add(&counters.audio_out, 1);
            Counters::add(&counters.audio_out_bytes, take as u64);
        }
        self.report_lead(now, false).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use brisvox_core::SystemClock;
    use tokio::net::TcpListener;

    use super::*;
    use crate::call::CallLink;
    use crate::websocket::ServerConfig;

    /// The writer's own stale-epoch guard. The session layer drops old-epoch audio before it
    /// reaches the wire, but a frame that went out ahead of a clear can still land behind
    /// `WireOut::Clear` (session.rs, "this frame went out ahead of the clear"); only the
    /// writer's `min_epoch` keeps it from the client.
    #[tokio::test]
    async fn audio_of_an_epoch_older_than_the_last_clear_never_goes_out() {
        // A loopback websocket: the writer on the accepted end, the test reads the other.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let accept = async {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        };
        let (client, server) = tokio::join!(tokio_tungstenite::connect_async(url), accept);
        let (mut client, _) = client.unwrap();
        let (sink, _stream) = server.split();

        let clock = SystemClock::shared();
        let (link, ends): (CallLink, CallEnds) = call_pair();
        let (calls, _calls_rx) = mpsc::channel(1);
        let (rtvi_calls, _rtvi_calls_rx) = mpsc::channel(1);
        let conn = Conn {
            shared: Arc::new(Shared {
                clock: clock.clone(),
                config: ServerConfig::default(),
                calls,
                next_call: AtomicU64::new(0),
                cancel: CancellationToken::new(),
                gate: None,
            }),
            rtvi: Arc::new(RtviShared {
                config: RtviConfig::default(),
                auth: Authenticator::new(crate::auth::Audience::Rtvi, None, None),
                counters: Arc::new(Counters::default()),
                calls: rtvi_calls,
            }),
            call_id: "rtvi-test".to_owned(),
            auto_events: Arc::new(AtomicBool::new(true)),
            ended: ends.ended.clone(),
            reader_done: CancellationToken::new(),
            writer_done: CancellationToken::new(),
        };
        let mut writer = Writer {
            conn: &conn,
            sink,
            events: Some(ends.events),
            min_epoch: TurnEpoch::ZERO,
            rate_mismatch_reported: false,
            playout: Playout::default(),
            last_lead: None,
            buffer: BytesMut::new(),
        };
        let frame = |epoch: TurnEpoch, fill: u8, seq: u64| {
            AudioFrame::new(
                Bytes::from(vec![fill; 960]),
                SampleRate::Hz24000,
                clock.now(),
                seq,
                epoch,
            )
            .unwrap()
        };

        assert!(writer.clear(TurnEpoch::new(1)).await.is_ok());
        // Behind the clear: audio of epoch 0 (stale), then of epoch 1.
        assert!(writer.audio(&frame(TurnEpoch::ZERO, 7, 1)).await.is_ok());
        assert!(writer.audio(&frame(TurnEpoch::new(1), 9, 2)).await.is_ok());

        let mut audio = Vec::new();
        let mut messages = 0;
        while audio.is_empty() {
            let message = tokio::time::timeout(Duration::from_secs(5), client.next())
                .await
                .expect("nothing from the writer")
                .expect("socket closed")
                .expect("socket error");
            let Message::Binary(bytes) = message else {
                continue;
            };
            match Frame::parse(bytes).unwrap().kind {
                Some(frame::Kind::Audio(a)) => audio = a.audio.to_vec(),
                _ => messages += 1,
            }
        }
        assert_eq!(audio, vec![9u8; 960], "the stale frame reached the client");
        // bot-interrupted and bot-started-speaking came first.
        assert_eq!(messages, 2);
        assert_eq!(conn.counters().audio_out.load(Ordering::Relaxed), 1);
        drop((link, client));
    }
}
