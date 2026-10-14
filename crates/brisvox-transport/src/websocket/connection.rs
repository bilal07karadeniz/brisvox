//! One websocket connection: handshake, `start`, then a reader and a writer side that run
//! concurrently inside the connection's single task.
//!
//! ```text
//!   client --binary/text--> reader --WireEvent--> core (CallLink)
//!   client <--binary/text-- writer <--WireOut---- core
//!                  reader --Ctrl (errors)--> writer
//! ```
//!
//! Shutdown is symmetric. Whichever side finishes first cancels its `*_done` token; the other side
//! then has `close_grace` to finish (the writer to say goodbye, the reader to see the peer's
//! close frame), so neither waits forever for the other.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use brisvox_audio::Codec;
use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{AudioFrame, SampleRate};
use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::RETRY_AFTER;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_util::sync::CancellationToken;

use super::protocol::{ClientMessage, Encoding, MAX_MARK_ID, ServerMessage, Start, code, stop};
use super::upgrade_auth::{AuthCallback, GrantSlot};
use super::{Admitted, MIN_PING_INTERVAL, ServerConfig, Shared, timeout};
use crate::admission::Refusal;
use crate::auth::{Authenticator, Grant};
use crate::call::{CallEnds, WireEvent, WireOut, call_pair};
use crate::channel;
use crate::event::CallInfo;

type Ws = WebSocketStream<TcpStream>;

/// Capacity of the reader-to-writer queue of error reports.
const CTRL_CAPACITY: usize = 8;

/// Size of the chunks the mu-law conversion buffers are allocated in: one allocation serves many
/// frames (a 20 ms mu-law frame is 160 bytes).
const SCRATCH_CHUNK: usize = 16 * 1024;

/// Non-fatal faults tolerated on one call before it is closed.
const MAX_FAULTS: u32 = 16;

/// The websocket settings for a connection: small buffers (hundreds of connections share a
/// process), eager writes, and the frame limits of the configuration.
pub(crate) fn ws_config(config: &ServerConfig) -> WebSocketConfig {
    let limit = config.max_audio_bytes.max(config.max_control_bytes);
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        // Output is written in chunks of at most `limit`; a chunk must fit the write buffer.
        .max_write_buffer_size(limit.saturating_mul(2).saturating_add(64 * 1024))
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit))
}

/// Answers a refused connection (beyond the call limit, or refused by the host's gate) with
/// HTTP 503 with `Retry-After` ([`Refusal::retry_after_secs`]), none when the host is shutting
/// down.
#[allow(clippy::result_large_err)] // the error type is fixed by tungstenite's `Callback`
pub(crate) async fn reject_busy(
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
    refusal: Refusal,
    shared: Arc<Shared>,
) {
    let busy = move |_: &Request, _: Response| -> Result<Response, ErrorResponse> {
        let mut response = ErrorResponse::new(Some(refusal.as_str().to_owned()));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        if let Some(secs) = refusal.retry_after_secs() {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        Err(response)
    };
    let handshake = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        busy,
        Some(ws_config(&shared.config)),
    );
    tokio::select! {
        biased;
        () = shared.cancel.cancelled() => {}
        _ = timeout(&*shared.clock, shared.config.handshake_timeout, handshake) => {}
    }
}

/// Fills `len` bytes of `scratch` with `fill` and hands them out as `Bytes`, or `None` if `fill`
/// reports failure. One allocation of [`SCRATCH_CHUNK`] bytes serves many calls (the returned
/// `Bytes` share it), so the mu-law path does not allocate per frame.
pub(crate) fn scratch_bytes(
    scratch: &mut BytesMut,
    len: usize,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Option<Bytes> {
    if scratch.capacity() < len {
        scratch.reserve(len.max(SCRATCH_CHUNK));
    }
    scratch.resize(len, 0);
    if fill(scratch.as_mut()) {
        Some(scratch.split().freeze())
    } else {
        scratch.clear();
        None
    }
}

/// What the client's first message turned out to be.
enum Opening {
    Start(Start, SampleRate),
    /// The client went away before starting.
    Gone,
    Refuse {
        code: &'static str,
        message: String,
        close: CloseCode,
    },
}

/// Maps a websocket read error to the protocol error to report, if the peer is to blame.
///
/// A socket that vanishes (EOF or reset without a close handshake, any I/O error) is not a
/// protocol violation: there is nobody left to report to, and the engine must see a plain
/// `PeerDisconnected`, not an `invalid_message` fault. tungstenite reports an EOF in the middle of
/// a call as `Protocol(ResetWithoutClosingHandshake)`, so it is excluded explicitly.
fn blame(error: &WsError) -> Option<(&'static str, String, CloseCode)> {
    match error {
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) | WsError::Io(_) => None,
        WsError::Capacity(_) => Some((
            code::FRAME_TOO_LARGE,
            "message exceeds the frame limit".to_owned(),
            CloseCode::Size,
        )),
        WsError::Protocol(_) | WsError::Utf8(_) | WsError::AttackAttempt => Some((
            code::INVALID_MESSAGE,
            "websocket protocol violation".to_owned(),
            CloseCode::Protocol,
        )),
        _ => None,
    }
}

/// Waits for the first text frame and validates it as `start`.
async fn read_start(ws: &mut Ws, shared: &Shared) -> Opening {
    let config = &shared.config;
    let read = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => return Ok(text),
                Some(Ok(Message::Binary(_))) => {
                    return Err(Opening::Refuse {
                        code: code::AUDIO_BEFORE_START,
                        message: "audio before start".to_owned(),
                        close: CloseCode::Policy,
                    });
                }
                Some(Ok(Message::Close(_))) | None => return Err(Opening::Gone),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(match blame(&error) {
                        Some((code, message, close)) => Opening::Refuse {
                            code,
                            message,
                            close,
                        },
                        None => Opening::Gone,
                    });
                }
            }
        }
    };
    let outcome = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return Opening::Gone,
        outcome = timeout(&*shared.clock, config.start_timeout, read) => outcome,
    };
    let text = match outcome {
        Ok(Ok(text)) => text,
        Ok(Err(opening)) => return opening,
        Err(_) => {
            return Opening::Refuse {
                code: code::START_TIMEOUT,
                message: format!("no start message within {:?}", config.start_timeout),
                close: CloseCode::Policy,
            };
        }
    };
    if text.len() > config.max_control_bytes {
        return Opening::Refuse {
            code: code::FRAME_TOO_LARGE,
            message: format!("control message exceeds {} bytes", config.max_control_bytes),
            close: CloseCode::Size,
        };
    }
    match ClientMessage::parse(text.as_str()) {
        Ok(ClientMessage::Start(start)) => match start.validate() {
            Ok(rate) => Opening::Start(start, rate),
            Err(error) => Opening::Refuse {
                code: error.code(),
                message: error.to_string(),
                close: CloseCode::Policy,
            },
        },
        Ok(_) => Opening::Refuse {
            code: code::INVALID_MESSAGE,
            message: "the first message must be start".to_owned(),
            close: CloseCode::Policy,
        },
        Err(error) => Opening::Refuse {
            code: error.code(),
            message: error.to_string(),
            close: CloseCode::Protocol,
        },
    }
}

/// Reports a fatal error on a connection that is not split yet, closes it and waits briefly for
/// the peer's close frame (so the peer sees a clean close, not a reset).
async fn refuse(ws: &mut Ws, shared: &Shared, code: &str, message: &str, close: CloseCode) {
    let error = ServerMessage::error(code, message, true).to_json();
    let write = async {
        ws.send(Message::text(error)).await?;
        ws.send(Message::Close(Some(CloseFrame {
            code: close,
            reason: code.into(),
        })))
        .await
    };
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

/// Serves one admitted connection to its end. Holds the call permit for as long as it runs.
pub(crate) async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    admitted: Admitted,
    shared: Arc<Shared>,
) {
    serve_with_auth(stream, peer, admitted, shared, None).await;
}

/// [`serve`] where the upgrade request must carry a credential `auth` accepts.
pub(crate) async fn serve_with_auth(
    stream: TcpStream,
    peer: SocketAddr,
    admitted: Admitted,
    shared: Arc<Shared>,
    auth: Option<Arc<Authenticator>>,
) {
    let _ = stream.set_nodelay(true);
    let grant_slot = GrantSlot::default();
    let handshake = async {
        let config = Some(ws_config(&shared.config));
        match &auth {
            Some(auth) => {
                let callback = AuthCallback {
                    auth: Arc::clone(auth),
                    slot: Arc::clone(&grant_slot),
                };
                tokio_tungstenite::accept_hdr_async_with_config(stream, callback, config).await
            }
            None => tokio_tungstenite::accept_async_with_config(stream, config).await,
        }
    };
    let ws = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return,
        handshake = timeout(&*shared.clock, shared.config.handshake_timeout, handshake) => match handshake {
            Ok(Ok(ws)) => ws,
            Ok(Err(error)) => {
                tracing::debug!(%peer, %error, "websocket handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(%peer, "websocket handshake timed out");
                return;
            }
        },
    };
    let grant = grant_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    run_upgraded(ws, peer, admitted, shared, grant, None, "websocket").await;
}

/// Serves a connection whose websocket upgrade is done (by [`serve_with_auth`], or by a host
/// listener that answered the upgrade itself: the WebRTC signaling port): the `start`
/// message, the call's hand-over to the engine, and the audio both ways to the end.
///
/// `grant` is what the upgrade's credential check granted (its session claims reach the call's
/// metadata); `max_duration` ends the call (a goodbye, `stop` with reason `ended`) that long
/// after its `started`, `None` for no cap.
pub(crate) async fn run_upgraded(
    mut ws: Ws,
    peer: SocketAddr,
    mut admitted: Admitted,
    shared: Arc<Shared>,
    grant: Option<Grant>,
    max_duration: Option<Duration>,
    transport: &'static str,
) {
    let (start, rate) = match read_start(&mut ws, &shared).await {
        Opening::Start(start, rate) => (start, rate),
        Opening::Gone => return,
        Opening::Refuse {
            code,
            message,
            close,
        } => {
            tracing::debug!(%peer, code, "websocket start refused");
            refuse(&mut ws, &shared, code, &message, close).await;
            return;
        }
    };
    // The handshake is over: the call no longer counts against the host's handshake pool.
    admitted.finish_handshake();

    let call_id = format!("ws-{}", shared.next_call.fetch_add(1, Ordering::Relaxed));
    let mut metadata = start.metadata.clone();
    if let Some(client_id) = &start.call_id {
        metadata.insert("client_call_id".to_owned(), client_id.clone());
    }
    // `input_sample_rate` and `output_sample_rate` are keys this adapter never sets (it speaks
    // one symmetric rate): a client-supplied value would make the engine expect caller audio or
    // produce bot audio at a rate the client did not declare, or fail the call on a value that
    // does not parse.
    metadata.remove(CallInfo::INPUT_SAMPLE_RATE_KEY);
    metadata.remove(CallInfo::OUTPUT_SAMPLE_RATE_KEY);
    // The `session` keys describe what the application's backend signed. The client's own
    // metadata must not be able to claim them.
    metadata.retain(|key, _| key != "session_id" && !key.starts_with("session."));
    if let Some(grant) = grant {
        grant.add_metadata(&mut metadata);
    }
    metadata.insert("transport".to_owned(), transport.to_owned());
    metadata.insert(CallInfo::SAMPLE_RATE_KEY.to_owned(), rate.hz().to_string());
    metadata.insert("encoding".to_owned(), start.encoding.as_str().to_owned());
    metadata.insert("peer_addr".to_owned(), peer.to_string());
    let info = CallInfo {
        call_id: call_id.clone(),
        metadata,
    };

    let (mut link, ends) = call_pair();
    admitted.attach(&mut link);
    link.set_call_id(&call_id);
    if transport != "websocket" {
        link.set_transport(transport);
    }
    let CallEnds {
        events,
        outgoing,
        ended,
    } = ends;
    // The queue is empty, so `Started` goes in at once; the core sees it first.
    if events.send(WireEvent::Started(info)).await.is_err() {
        return;
    }
    let handed = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return,
        sent = timeout(&*shared.clock, shared.config.send_timeout, shared.calls.send(link)) => {
            matches!(sent, Ok(Ok(())))
        }
    };
    if !handed {
        refuse(
            &mut ws,
            &shared,
            code::SERVER_BUSY,
            "the engine is not taking new calls",
            CloseCode::Again,
        )
        .await;
        return;
    }
    tracing::info!(%call_id, %peer, sample_rate = rate.hz(), encoding = start.encoding.as_str(), "websocket call started");

    let (sink, stream) = ws.split();
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_CAPACITY);
    let conn = Conn {
        shared: Arc::clone(&shared),
        call_id: call_id.clone(),
        rate,
        encoding: start.encoding,
        max_duration,
        ended,
        reader_done: CancellationToken::new(),
        writer_done: CancellationToken::new(),
    };
    let reader = async {
        Reader {
            conn: &conn,
            events: Some(events),
            ctrl: ctrl_tx,
            seq: 0,
            faults: 0,
            scratch: BytesMut::new(),
        }
        .run(stream)
        .await;
        conn.reader_done.cancel();
    };
    let writer = async {
        Writer {
            conn: &conn,
            sink,
            min_epoch: TurnEpoch::ZERO,
            rate_mismatch_reported: false,
            scratch: BytesMut::new(),
        }
        .run(outgoing, ctrl_rx)
        .await;
    };
    tokio::join!(reader, writer);
    tracing::info!(%call_id, "websocket call ended");
}

/// What both sides of a connection share.
struct Conn {
    shared: Arc<Shared>,
    call_id: String,
    rate: SampleRate,
    encoding: Encoding,
    /// The longest the call may last, from its `started` (a session token's `max_s` on the
    /// WebRTC listener); `None`: no cap.
    max_duration: Option<Duration>,
    /// Cancelled by the core once it ended the call.
    ended: CancellationToken,
    /// Cancelled when the reader returns.
    reader_done: CancellationToken,
    /// Cancelled when the writer has said its last word (farewell or fatal error with its close
    /// frame, or a broken socket). Until then the reader of a closing connection does not read:
    /// reading a close frame makes the websocket layer refuse every later write, so the farewell
    /// would be lost.
    writer_done: CancellationToken,
}

impl Conn {
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

/// What the reader asks the writer to put on the wire.
enum Ctrl {
    /// A non-fatal `error` message.
    Notice { code: &'static str, message: String },
    /// A fatal `error` message, then a close with `close`.
    Fatal {
        code: &'static str,
        message: String,
        close: CloseCode,
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
    ctrl: mpsc::Sender<Ctrl>,
    seq: u64,
    faults: u32,
    /// Buffer the mu-law decoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
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
            // A closing connection is not read before the writer has said goodbye: reading the
            // peer's close frame makes the websocket layer refuse every later write.
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
                        self.fatal(
                            code::PING_TIMEOUT,
                            format!("no frame from the client for {:?}", config.ping_timeout),
                            CloseCode::Policy,
                        )
                        .await;
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
                        if !draining && let Some((code, message, close)) = blame(&error) {
                            self.fatal(code, message, close).await;
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
            Message::Binary(data) => self.audio(data).await,
            Message::Text(text) => self.control(text.as_str()).await,
            Message::Close(_) => {
                // A close frame is a clean hang-up; a vanished socket is not (see `run`).
                self.deliver(WireEvent::Hangup).await;
                self.finish_input();
                Flow::Exit
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Flow::Continue,
        }
    }

    async fn audio(&mut self, data: Bytes) -> Flow {
        let config = &self.conn.shared.config;
        if data.is_empty() {
            return Flow::Continue;
        }
        if data.len() > config.max_audio_bytes {
            self.fatal(
                code::FRAME_TOO_LARGE,
                format!("audio frame exceeds {} bytes", config.max_audio_bytes),
                CloseCode::Size,
            )
            .await;
            return Flow::Drain;
        }
        let pcm = match self.conn.encoding {
            Encoding::Pcm16 => {
                if !data.len().is_multiple_of(AudioFrame::BYTES_PER_SAMPLE) {
                    self.fatal(
                        code::BAD_AUDIO,
                        "pcm16 audio must be whole 16-bit samples".to_owned(),
                        CloseCode::Policy,
                    )
                    .await;
                    return Flow::Drain;
                }
                data
            }
            Encoding::Mulaw => {
                let len = Codec::Ulaw.decoded_len(data.len());
                let decoded = scratch_bytes(&mut self.scratch, len, |out| {
                    Codec::Ulaw.decode(&data, out).is_ok()
                });
                match decoded {
                    Some(pcm) => pcm,
                    None => return Flow::Continue,
                }
            }
        };
        let now = self.conn.shared.clock.now();
        let Ok(frame) = AudioFrame::new(pcm, self.conn.rate, now, self.seq, TurnEpoch::ZERO) else {
            return Flow::Continue;
        };
        self.seq = self.seq.saturating_add(1);
        // Input audio is `DropOldest`: this never waits, so no timeout is needed.
        let Some(events) = &self.events else {
            return Flow::Drain;
        };
        match events.send(WireEvent::Audio(frame)).await {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Drain,
        }
    }

    async fn control(&mut self, text: &str) -> Flow {
        let config = &self.conn.shared.config;
        if text.len() > config.max_control_bytes {
            self.fatal(
                code::FRAME_TOO_LARGE,
                format!("control message exceeds {} bytes", config.max_control_bytes),
                CloseCode::Size,
            )
            .await;
            return Flow::Drain;
        }
        match ClientMessage::parse(text) {
            Ok(ClientMessage::Start(_)) => {
                self.fatal(
                    code::UNEXPECTED_START,
                    "start was already received".to_owned(),
                    CloseCode::Policy,
                )
                .await;
                Flow::Drain
            }
            Ok(ClientMessage::Dtmf { digit }) => {
                let at = self.conn.shared.clock.now();
                self.flow(WireEvent::Dtmf { digit, at }).await
            }
            Ok(ClientMessage::Mark { id }) => self.flow(WireEvent::MarkAck { id }).await,
            Ok(ClientMessage::Stop) => {
                self.deliver(WireEvent::Hangup).await;
                self.finish_input();
                Flow::Drain
            }
            Err(error) if !error.is_fatal() => self.notice(error.code(), error.to_string()).await,
            Err(error) => {
                self.fatal(error.code(), error.to_string(), CloseCode::Protocol)
                    .await;
                Flow::Drain
            }
        }
    }

    /// Delivers `event` and maps the outcome to what the reader does next.
    async fn flow(&mut self, event: WireEvent) -> Flow {
        match self.deliver(event).await {
            Delivery::Delivered => Flow::Continue,
            Delivery::Stopped => Flow::Drain,
            Delivery::Stalled => {
                self.fatal(
                    code::OVERLOADED,
                    "the engine is not taking input".to_owned(),
                    CloseCode::Again,
                )
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

    /// A recoverable protocol fault: the client hears about it, the engine sees a fault event.
    async fn notice(&mut self, code: &'static str, message: String) -> Flow {
        self.faults = self.faults.saturating_add(1);
        if self.faults > MAX_FAULTS {
            self.fatal(
                code::TOO_MANY_ERRORS,
                format!("more than {MAX_FAULTS} protocol faults on one call"),
                CloseCode::Policy,
            )
            .await;
            return Flow::Drain;
        }
        let fault = WireEvent::Fault {
            message: format!("{code}: {message}"),
        };
        // Bounded queue: a full one drops the notice, the fault count still grows.
        let _ = self.ctrl.try_send(Ctrl::Notice { code, message });
        self.flow(fault).await
    }

    /// A fatal fault: tells the writer to report it and close, and the engine to note it.
    async fn fatal(&mut self, code: &'static str, message: String, close: CloseCode) {
        tracing::debug!(call_id = %self.conn.call_id, code, "websocket call closed by protocol fault");
        let fault = WireEvent::Fault {
            message: format!("{code}: {message}"),
        };
        // A fatal report must not be lost to queued notices, so it waits for the writer (which
        // may be stuck on the socket). The wait is bounded by the write timeout, and ends at
        // once when the server stops or the core ended the call.
        let shared = &self.conn.shared;
        let report = Ctrl::Fatal {
            code,
            message,
            close,
        };
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => {}
            () = self.conn.ended.cancelled() => {}
            _ = timeout(&*shared.clock, shared.config.write_timeout, self.ctrl.send(report)) => {}
        }
        self.deliver(fault).await;
        self.finish_input();
    }

    /// Stops feeding the core: dropping the queue's sender is how the core learns the client is
    /// gone (a hang-up queued before stays readable).
    fn finish_input(&mut self) {
        self.events = None;
    }
}

struct Writer<'a> {
    conn: &'a Conn,
    sink: SplitSink<Ws, Message>,
    min_epoch: TurnEpoch,
    rate_mismatch_reported: bool,
    /// Buffer the mu-law encoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
}

/// What the writer's loop does next.
enum Step {
    Shutdown,
    Ended,
    Ctrl(Option<Ctrl>),
    Out(Option<WireOut>),
    Ping,
    ReaderDone,
    GraceOver,
}

/// Why a write did not complete.
enum WriteFailure {
    /// The socket is closed or broke (or the websocket layer already answered a close frame).
    Closed,
    /// The client did not take the data within the write timeout.
    Slow,
    /// The server is shutting down or the core ended the call while the write was blocked.
    Interrupted,
}

/// How the writer leaves its main loop.
enum Departure {
    /// Say goodbye: `stop{reason}`, then a close frame with `close`.
    Farewell {
        reason: &'static str,
        close: CloseCode,
    },
    /// Everything that had to be said was said (a fatal error and its close frame).
    Said,
    /// The socket is gone or stuck: nothing more can be written.
    Broken,
}

impl Writer<'_> {
    /// One write, giving up after `limit`. With `interruptible`, it also gives up the moment the
    /// server shuts down or the core ends the call, so a client that stopped reading cannot hold
    /// the call's permit for a whole write timeout (`CallEnds::ended`).
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
                tracing::warn!(call_id = %conn.call_id, code = code::SLOW_CLIENT, "client does not take output, closing the call");
                Err(WriteFailure::Slow)
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), WriteFailure> {
        let limit = self.conn.shared.config.write_timeout;
        self.write(message, limit, true).await
    }

    async fn send_json(&mut self, message: ServerMessage) -> Result<(), WriteFailure> {
        self.send(Message::text(message.to_json())).await
    }

    /// How long the goodbye may take: it is the last thing the call does, so it gets a short
    /// bound.
    fn farewell_limit(&self) -> Duration {
        let config = &self.conn.shared.config;
        config.write_timeout.min(config.close_grace)
    }

    /// Says goodbye: a `stop` message, then a close frame. Returns whether the socket still
    /// answers (a stuck client does not). If the peer's close frame was read before, the websocket
    /// layer refuses both writes; the pending close reply is flushed by [`Self::finish`].
    async fn farewell(&mut self, reason: &str, close: CloseCode) -> bool {
        let limit = self.farewell_limit();
        let stop = ServerMessage::Stop {
            reason: reason.to_owned(),
        };
        match self
            .write(Message::text(stop.to_json()), limit, false)
            .await
        {
            Ok(()) => {}
            Err(WriteFailure::Closed) => return true,
            Err(_) => return false,
        }
        let frame = CloseFrame {
            code: close,
            reason: reason.into(),
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
            WriteFailure::Interrupted if conn.shared.cancel.is_cancelled() => Departure::Farewell {
                reason: stop::SHUTDOWN,
                close: CloseCode::Away,
            },
            WriteFailure::Interrupted => Departure::Farewell {
                reason: stop::ENDED,
                close: CloseCode::Normal,
            },
        }
    }

    async fn run(mut self, outgoing: channel::Receiver<WireOut>, ctrl: mpsc::Receiver<Ctrl>) {
        let conn = self.conn;
        let departure = self.serve_loop(outgoing, ctrl).await;
        let socket_ok = match departure {
            Departure::Farewell { reason, close } => self.farewell(reason, close).await,
            Departure::Said => true,
            Departure::Broken => false,
        };
        // From here on the reader may read the peer's close frame.
        conn.writer_done.cancel();
        if socket_ok {
            self.finish().await;
        }
    }

    /// After the last word: waits (bounded) for the reader to see the peer's close frame, then
    /// flushes and closes the sink. That sends the close reply the websocket layer queued when it
    /// read the peer's close frame, so the peer gets a close handshake, not a reset.
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
    ) -> Departure {
        let conn = self.conn;
        let shared = &conn.shared;
        let clock = &shared.clock;
        let config = &shared.config;
        let started = ServerMessage::Started {
            version: super::protocol::PROTOCOL_VERSION,
            call_id: conn.call_id.clone(),
            sample_rate: conn.rate.hz(),
            encoding: conn.encoding,
        };
        if let Err(failure) = self.send_json(started).await {
            return Self::depart(&failure, conn);
        }
        let ping_interval = config.ping_interval.max(MIN_PING_INTERVAL);
        let ping = clock.sleep(ping_interval);
        tokio::pin!(ping);
        let grace = clock.sleep(config.close_grace);
        tokio::pin!(grace);
        let capped = conn.max_duration.is_some();
        let cap = clock.sleep(conn.max_duration.unwrap_or(Duration::from_secs(1)));
        tokio::pin!(cap);
        let mut grace_started = false;
        let mut ctrl_open = true;
        // The select below runs once per frame: its token futures are created once.
        let shutdown = shared.cancel.cancelled();
        let ended = conn.ended.cancelled();
        let reader_done = conn.reader_done.cancelled();
        tokio::pin!(shutdown, ended, reader_done);
        loop {
            let step = tokio::select! {
                biased;
                () = &mut shutdown => Step::Shutdown,
                received = ctrl.recv(), if ctrl_open => Step::Ctrl(received),
                () = &mut ended => Step::Ended,
                () = &mut cap, if capped => Step::Ended,
                out = outgoing.recv() => Step::Out(out),
                () = &mut ping => Step::Ping,
                () = &mut reader_done, if !grace_started => Step::ReaderDone,
                () = &mut grace, if grace_started => Step::GraceOver,
            };
            let sent = match step {
                Step::Shutdown => {
                    return Departure::Farewell {
                        reason: stop::SHUTDOWN,
                        close: CloseCode::Away,
                    };
                }
                Step::Ended | Step::GraceOver | Step::Out(None | Some(WireOut::Hangup)) => {
                    return Departure::Farewell {
                        reason: stop::ENDED,
                        close: CloseCode::Normal,
                    };
                }
                Step::Ctrl(None) => {
                    ctrl_open = false;
                    Ok(())
                }
                Step::Ctrl(Some(Ctrl::Notice { code, message })) => {
                    self.send_json(ServerMessage::error(code, message, false))
                        .await
                }
                Step::Ctrl(Some(Ctrl::Fatal {
                    code,
                    message,
                    close,
                })) => {
                    let reported = self
                        .send_json(ServerMessage::error(code, message, true))
                        .await;
                    if let Err(failure) = reported {
                        return Self::depart(&failure, conn);
                    }
                    let frame = CloseFrame {
                        code: close,
                        reason: code.into(),
                    };
                    return match self.send(Message::Close(Some(frame))).await {
                        Ok(()) => Departure::Said,
                        Err(failure) => Self::depart(&failure, conn),
                    };
                }
                Step::Out(Some(WireOut::Audio(frame))) => self.audio(&frame).await,
                Step::Out(Some(WireOut::Clear { epoch })) => {
                    self.min_epoch = self.min_epoch.max(epoch);
                    self.send_json(ServerMessage::Clear { epoch: epoch.get() })
                        .await
                }
                Step::Out(Some(WireOut::Mark { id })) => self.mark(id).await,
                Step::Ping => {
                    let sent = self.send(Message::Ping(Bytes::new())).await;
                    ping.as_mut()
                        .reset(deadline_after(clock.now(), ping_interval));
                    sent
                }
                Step::ReaderDone => {
                    grace_started = true;
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

    /// Sends a mark to the client. An id longer than [`MAX_MARK_ID`] would make a compliant client's
    /// echo a protocol violation, so it is not sent: the client gets a non-fatal
    /// `mark_id_too_long` error instead, and the engine sees no `MarkPlayed` for it.
    async fn mark(&mut self, id: String) -> Result<(), WriteFailure> {
        if id.len() > MAX_MARK_ID {
            let message = format!("a mark id is longer than {MAX_MARK_ID} bytes; mark not sent");
            return self
                .send_json(ServerMessage::error(code::MARK_ID_TOO_LONG, message, false))
                .await;
        }
        self.send_json(ServerMessage::Mark { id }).await
    }

    /// Writes one frame of bot audio in the call's encoding.
    async fn audio(&mut self, frame: &AudioFrame) -> Result<(), WriteFailure> {
        // Audio of an older epoch than the last clear never goes out.
        if frame.epoch().get() < self.min_epoch.get() {
            return Ok(());
        }
        if frame.sample_rate() != self.conn.rate {
            // Resampling is DSP work for the audio layer, not for a transport task.
            if !self.rate_mismatch_reported {
                self.rate_mismatch_reported = true;
                let message = format!(
                    "bot audio is {} but the call negotiated {}; mismatched audio is dropped",
                    frame.sample_rate(),
                    self.conn.rate
                );
                self.send_json(ServerMessage::error(
                    code::OUTPUT_RATE_MISMATCH,
                    message,
                    false,
                ))
                .await?;
            }
            return Ok(());
        }
        let mut payload = match self.conn.encoding {
            Encoding::Pcm16 => frame.data().clone(),
            Encoding::Mulaw => {
                let len = Codec::Ulaw.encoded_len(frame.data().len());
                let source = frame.data();
                let encoded = scratch_bytes(&mut self.scratch, len, |out| {
                    Codec::Ulaw.encode(source, out).is_ok()
                });
                match encoded {
                    Some(payload) => payload,
                    None => return Ok(()),
                }
            }
        };
        // Chunks of at most `max_audio_bytes` (whole samples), so a client with the same frame
        // limit accepts everything the engine sends, however long its frames are.
        let chunk = (self.conn.shared.config.max_audio_bytes & !1).max(2);
        while !payload.is_empty() {
            let take = payload.len().min(chunk);
            self.send(Message::Binary(payload.split_to(take))).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Number of distinct allocations behind `frames`: a frame that does not start where the
    /// previous one ended opens a new chunk.
    fn allocations(frames: &[Bytes]) -> usize {
        let mut count = 0;
        let mut expected_next: Option<*const u8> = None;
        for frame in frames {
            if expected_next != Some(frame.as_ptr()) {
                count += 1;
            }
            expected_next = Some(frame.as_ptr().wrapping_add(frame.len()));
        }
        count
    }

    #[test]
    fn scratch_frames_keep_their_content_across_chunk_refills() {
        // 320 bytes is a decoded 20 ms mu-law frame at 8 kHz: a 16 KiB chunk holds 51 of them, so
        // 300 frames refill the chunk several times.
        const FRAME: usize = 320;
        const FRAMES: usize = 300;
        let mut scratch = BytesMut::new();
        let mut kept = Vec::new();
        for i in 0..FRAMES {
            let fill = u8::try_from(i % 251).unwrap();
            let frame = scratch_bytes(&mut scratch, FRAME, |out| {
                out.fill(fill);
                true
            })
            .expect("fill succeeded");
            assert_eq!(frame.len(), FRAME);
            kept.push(frame);
        }
        // Every frame still holds its own bytes: no refill overwrote or aliased an earlier one.
        for (i, frame) in kept.iter().enumerate() {
            let fill = u8::try_from(i % 251).unwrap();
            assert!(
                frame.iter().all(|&b| b == fill),
                "frame {i} was overwritten"
            );
        }
        // About one allocation per chunk: 300 * 320 B = 94 KiB is 6 chunks of 16 KiB, nowhere
        // near one allocation per frame.
        let allocations = allocations(&kept);
        assert!(
            (2..=8).contains(&allocations),
            "{allocations} allocations for {FRAMES} frames"
        );
    }

    #[test]
    fn a_failed_fill_hands_out_nothing_and_does_not_poison_the_buffer() {
        let mut scratch = BytesMut::new();
        let first = scratch_bytes(&mut scratch, 160, |out| {
            out.fill(1);
            true
        })
        .expect("fill succeeded");
        assert!(scratch_bytes(&mut scratch, 160, |_| false).is_none());
        let second = scratch_bytes(&mut scratch, 160, |out| {
            out.fill(2);
            true
        })
        .expect("fill succeeded");
        assert!(first.iter().all(|&b| b == 1));
        assert!(second.iter().all(|&b| b == 2));
        assert_eq!(second.len(), 160);
    }

    #[test]
    fn a_frame_larger_than_a_chunk_still_works() {
        let mut scratch = BytesMut::new();
        let big = scratch_bytes(&mut scratch, SCRATCH_CHUNK * 2 + 10, |out| {
            out.fill(7);
            true
        })
        .expect("fill succeeded");
        assert_eq!(big.len(), SCRATCH_CHUNK * 2 + 10);
        let small = scratch_bytes(&mut scratch, 4, |out| {
            out.fill(9);
            true
        })
        .expect("fill succeeded");
        assert_eq!(small.as_ref(), &[9, 9, 9, 9]);
        assert!(big.iter().all(|&b| b == 7));
    }

    #[test]
    fn a_vanished_socket_is_nobodys_fault() {
        // EOF or a reset without a close handshake: the peer is gone, there is nobody to tell.
        assert!(
            blame(&WsError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake
            ))
            .is_none()
        );
        assert!(blame(&WsError::Io(std::io::ErrorKind::ConnectionReset.into())).is_none());
        assert!(blame(&WsError::ConnectionClosed).is_none());
        // Real violations are still reported.
        let (error_code, _, close) =
            blame(&WsError::Protocol(ProtocolError::UnmaskedFrameFromClient))
                .expect("a violation is blamed on the peer");
        assert_eq!(error_code, code::INVALID_MESSAGE);
        assert_eq!(u16::from(close), 1002);
        assert!(blame(&WsError::Utf8(String::new())).is_some());
    }
}
