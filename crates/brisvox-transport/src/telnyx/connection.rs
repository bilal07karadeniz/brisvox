//! One Telnyx media streaming connection: upgrade (path and stream auth token), `connected` /
//! `start`, then a reader and a writer side that run concurrently inside the connection's single
//! task. The structure and the shutdown protocol are those of the Twilio connection
//! (`twilio/connection.rs`) and of the generic websocket connection (`websocket/connection.rs`);
//! what differs is the wire format, the reorder buffer of the reader and the hang-up hook of the
//! writer.
//!
//! ```text
//!   Telnyx --media/dtmf/mark/stop/error--> reader --WireEvent--> core (CallLink)
//!   Telnyx <--media/mark/clear------------ writer <--WireOut---- core
//!                  reader --Ctrl::Close--> writer
//!                  reader <--fault text--- writer
//!                  reader, writer share the MarkBook
//! ```
//!
//! Telnyx accepts only `media`, `mark` and `clear` from the server, so there is no error or stop
//! message to send: a problem on the connection is reported to the engine as a fault event, and
//! a fatal one closes the socket with a close code and reason. Closing the socket does not hang
//! the call up (not documented); the host's hang-up hook does, see [`super::HangupHook`]. Only the
//! `stop` frame, which Telnyx sends when the call ends, makes the hook unnecessary: a bare close
//! frame is not documented as ending the call, so it is hung up like a vanished socket. The
//! exception is a stream the host stopped on purpose ([`super::StreamStopper`]): its end is
//! neither a caller hang-up nor a reason to call the hook, and the engine's session is kept (see
//! [`Reader::run`] and `linger`).

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use brisvox_audio::Codec;
use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::DtmfDigit;
use brisvox_core::frame::{AudioFrame, SampleRate};
use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use super::protocol::{
    self, Inbound, SAMPLE_RATE_HZ, StartInfo, TelnyxCodec, TokenText, parse, write_media,
};
use super::registry::{Claim, Registry, StopMark};
use super::reorder::{Offer, Released, Reorder};
use super::stats::Counters;
use super::{HangupRequest, MIN_CHUNK_BYTES, STOPPED_LINGER, TelnyxConfig};
use crate::call::{CallEnds, WireEvent, WireOut, call_pair};
use crate::channel;
use crate::event::CallInfo;
use crate::telephony::{
    BASE64, Echo, MarkBook, MarkName, Ws, blame, close_reason, refusal, refuse,
};
use crate::websocket::connection::{scratch_bytes, ws_config};
use crate::websocket::{Admitted, MIN_PING_INTERVAL, Shared, timeout};

/// The upgrade header Telnyx puts the stream auth token in (HTTP header names are
/// case-insensitive, so the casing Telnyx uses does not matter).
const TOKEN_HEADER: &str = "x-telnyx-streaming-auth-token";

/// Capacity of the reader-to-writer queue of close requests (the first one wins).
const CTRL_CAPACITY: usize = 2;

/// Capacity of the writer-to-reader queue of fault texts. A full queue drops the new text (the
/// fault is still counted).
const NOTICE_CAPACITY: usize = 8;

/// Non-fatal faults tolerated on one call before it is closed.
const MAX_FAULTS: u32 = 16;

/// Messages other than `connected` tolerated before `start` (Telnyx does not order its frames, so
/// a `media` may overtake `start`; they are dropped). More than this refuses the connection.
const MAX_BEFORE_START: u32 = 64;

/// Allocation unit of the outgoing message buffer: one allocation serves many messages (a 20 ms
/// `media` message is about 250 bytes).
const MESSAGE_CHUNK: usize = 16 * 1024;

/// G.711 bytes per millisecond at 8 kHz.
const BYTES_PER_MS: u64 = 8;

/// Microseconds of audio in one G.711 byte at 8 kHz.
const MICROS_PER_BYTE: u64 = 1_000_000 / 8_000;

/// `dtmf` events held at once for ordering; one more flushes them (the buffer is bounded, and
/// nobody keys 32 digits within one hold timeout).
const MAX_HELD_DTMF: usize = 32;

/// Longest Telnyx `error` detail repeated to the engine.
const MAX_ERROR_TEXT: usize = 160;

/// State the upgrade check and every connection share.
pub(crate) struct TelnyxShared {
    pub(crate) config: TelnyxConfig,
    pub(crate) counters: Arc<Counters>,
    pub(crate) registry: Arc<Registry>,
}

/// Constant-time comparison of the configured token with what the peer sent. Both are run through
/// HMAC-SHA256 under a fixed key first (the key is not a secret, it only makes the compared values
/// equally long), and the tags are compared by `ring::hmac::verify`, so neither the content nor
/// the length of the token leaks through timing.
fn token_matches(expected: &str, given: &[u8]) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"brisvox telnyx stream token");
    let wanted = ring::hmac::sign(&key, expected.as_bytes());
    ring::hmac::verify(&key, given, wanted.as_ref()).is_ok()
}

impl TelnyxShared {
    /// The upgrade check: path, then the token header when an auth token is configured. A header
    /// that is present and wrong is refused (401); one that is absent is left to the `connected`
    /// frame. Sets `header_ok` when the header matched.
    fn check(&self, request: &Request, header_ok: &AtomicBool) -> Result<(), Box<ErrorResponse>> {
        let path = request.uri().path();
        let wanted = self.config.path.trim_end_matches('/');
        // The path may be the only secret there is (TeXML has no `stream_auth_token`), so it is
        // compared like one.
        if !token_matches(wanted, path.trim_end_matches('/').as_bytes()) {
            tracing::debug!(path, "telnyx upgrade refused: unknown path");
            return Err(refusal(StatusCode::NOT_FOUND, "not found"));
        }
        let Some(token) = &self.config.auth_token else {
            return Ok(());
        };
        match request.headers().get(TOKEN_HEADER) {
            None => Ok(()),
            Some(value) if token_matches(token.expose(), value.as_bytes()) => {
                header_ok.store(true, Ordering::Release);
                Ok(())
            }
            Some(_) => {
                tracing::debug!("telnyx upgrade refused: stream auth token does not match");
                Err(refusal(
                    StatusCode::UNAUTHORIZED,
                    "invalid stream auth token",
                ))
            }
        }
    }
}

/// The upgrade callback: the answer is the unchanged response, or a refusal.
struct Upgrade {
    shared: Arc<TelnyxShared>,
    header_ok: Arc<AtomicBool>,
}

impl Callback for Upgrade {
    #[allow(clippy::result_large_err)] // the error type is fixed by tungstenite's `Callback`
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        match self.shared.check(request, &self.header_ok) {
            Ok(()) => Ok(response),
            Err(refusal) => Err(*refusal),
        }
    }
}

/// What the stream's first messages turned out to be.
enum Opening {
    /// `start`, with the version `connected` announced.
    Start(Box<StartInfo>, Option<String>),
    /// Telnyx went away before starting.
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

/// Whether the connection proved itself: with an auth token configured, the header matched or the
/// `connected` frame carried the right token, and no token that was sent was wrong.
fn authorized(config: &TelnyxConfig, header_ok: bool, frame_token: Option<&TokenText>) -> bool {
    let Some(expected) = &config.auth_token else {
        return true;
    };
    let frame_ok = frame_token.map(|t| token_matches(expected.expose(), t.expose().as_bytes()));
    frame_ok != Some(false) && (header_ok || frame_ok == Some(true))
}

/// Whether a text frame of `len` bytes is within the size limit for its kind. The control limit
/// (`max_control_bytes`) bounds control messages; a `media` frame is bounded by the audio limit
/// instead (the websocket layer allows the larger of the two, and `Reader::media` checks the
/// payload against `max_audio_bytes`), so a big frame of audio is not a fatal "control message
/// too large". A frame that does not parse is judged by the control limit.
fn within_limit(
    len: usize,
    max_control: usize,
    parsed: &Result<protocol::Parsed<'_>, protocol::ProtocolError>,
) -> bool {
    len <= max_control
        || matches!(
            parsed,
            Ok(protocol::Parsed {
                message: Inbound::Media { .. },
                ..
            })
        )
}

/// Waits for `start` (ignoring a leading `connected`, and anything unknown).
async fn read_start(
    ws: &mut Ws,
    shared: &Shared,
    telnyx: &TelnyxShared,
    header_ok: bool,
) -> Opening {
    let config = &shared.config;
    let read = async {
        let mut version = None;
        let mut token = None;
        let mut early = 0u32;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let parsed = parse(text.as_str());
                    if !within_limit(text.len(), config.max_control_bytes, &parsed) {
                        return refuse_with("control message too large", CloseCode::Size);
                    }
                    match parsed {
                        Ok(parsed) => match parsed.message {
                            Inbound::Connected {
                                version: sent,
                                token: sent_token,
                            } => {
                                version = sent;
                                token = sent_token;
                            }
                            Inbound::Start(info) => {
                                if !authorized(&telnyx.config, header_ok, token.as_ref()) {
                                    return refuse_with("unauthorized", CloseCode::Policy);
                                }
                                return Opening::Start(info, version);
                            }
                            Inbound::Stop => return Opening::Gone,
                            // Frames are not ordered: media, DTMF and the like may overtake
                            // `start`. Drop them (a few 20 ms frames), up to a limit.
                            Inbound::Media { .. } | Inbound::Dtmf(_) | Inbound::Mark { .. } => {
                                Counters::add(&telnyx.counters.media_before_start, 1);
                                early += 1;
                                if early > MAX_BEFORE_START {
                                    return refuse_with("start expected first", CloseCode::Policy);
                                }
                            }
                            Inbound::Error { .. } | Inbound::Unknown(_) => {}
                        },
                        Err(error) => {
                            tracing::debug!(%error, "telnyx message before start is invalid");
                            return refuse_with("invalid message", CloseCode::Protocol);
                        }
                    }
                }
                // Binary frames are not mentioned in the docs: ignore them (UNVERIFIED).
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
        () = shared.cancel.cancelled() => Opening::Gone,
        outcome = timeout(&*shared.clock, config.start_timeout, read) => match outcome {
            Ok(opening) => opening,
            Err(_) => refuse_with("no start message in time", CloseCode::Policy),
        },
    }
}

/// Checks the media format of a `start`: PCMU or PCMA, 8 kHz, mono. A stream that says otherwise
/// is refused (the adapter converts only G.711). Absent fields are taken as the configured codec,
/// 8 kHz and mono (the spec marks `media_format` required, so this is only defensive).
fn check_format(start: &StartInfo, default: TelnyxCodec) -> Result<TelnyxCodec, &'static str> {
    let codec = match &start.encoding {
        Some(encoding) => TelnyxCodec::parse(encoding).ok_or("unsupported media encoding")?,
        None => default,
    };
    if let Some(rate) = start.sample_rate
        && rate != u64::from(SAMPLE_RATE_HZ)
    {
        return Err("unsupported sample rate");
    }
    if let Some(channels) = start.channels
        && channels != 1
    {
        return Err("unsupported channel count");
    }
    Ok(codec)
}

/// Serves one admitted connection to its end. Holds the call permit (and the gate's ticket, until
/// it goes into the call's link) for as long as it runs.
pub(crate) async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    mut admitted: Admitted,
    shared: Arc<Shared>,
    telnyx: Arc<TelnyxShared>,
) {
    let _ = stream.set_nodelay(true);
    let header_ok = Arc::new(AtomicBool::new(false));
    let handshake = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        Upgrade {
            shared: Arc::clone(&telnyx),
            header_ok: Arc::clone(&header_ok),
        },
        Some(ws_config(&shared.config)),
    );
    let mut ws = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return,
        handshake = timeout(&*shared.clock, shared.config.handshake_timeout, handshake) => match handshake {
            Ok(Ok(ws)) => ws,
            Ok(Err(error)) => {
                tracing::debug!(%peer, %error, "telnyx handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(%peer, "telnyx handshake timed out");
                return;
            }
        },
    };
    let (start, version) =
        match read_start(&mut ws, &shared, &telnyx, header_ok.load(Ordering::Acquire)).await {
            Opening::Start(start, version) => (start, version),
            Opening::Gone => return,
            Opening::Refuse { reason, close } => {
                tracing::debug!(%peer, reason, "telnyx start refused");
                refuse(&mut ws, &shared, &reason, close).await;
                return;
            }
        };
    let call_id = if start.call_control_id.is_empty() {
        start.stream_id.clone()
    } else {
        start.call_control_id.clone()
    };
    let Some(mut claim): Option<Claim> = telnyx.registry.claim(&call_id) else {
        Counters::add(&telnyx.counters.duplicate_calls_refused, 1);
        tracing::debug!(%peer, %call_id, "telnyx start refused: call already streamed");
        // No hang-up: the call is being served on another connection (or was).
        refuse(
            &mut ws,
            &shared,
            "call already streaming",
            CloseCode::Policy,
        )
        .await;
        return;
    };
    // What the hook is called with if this connection has to give the call up before the engine
    // has it, and later, when the call ends.
    let hangup = HangupRequest {
        call_control_id: start.call_control_id.clone(),
        call_session_id: start.call_session_id.clone(),
        stream_id: start.stream_id.clone(),
        client_state: start.client_state.clone(),
    };
    let in_codec = match check_format(&start, telnyx.config.codec) {
        Ok(codec) => codec,
        Err(reason) => {
            tracing::debug!(%peer, reason, "telnyx start refused");
            // The call is answered and streaming has started: closing the socket alone would
            // leave the caller on a silent open call.
            request_hangup(&shared, &telnyx, &hangup, &call_id).await;
            // Released before the peer is told, so a prompt retry finds the key free.
            drop(claim);
            refuse(&mut ws, &shared, reason, CloseCode::Policy).await;
            return;
        }
    };
    let mut metadata = std::collections::BTreeMap::new();
    for (name, value) in &start.custom_parameters {
        metadata.insert(format!("custom.{name}"), value.clone());
    }
    metadata.insert("transport".to_owned(), "telnyx".to_owned());
    metadata.insert(
        CallInfo::SAMPLE_RATE_KEY.to_owned(),
        SAMPLE_RATE_HZ.to_string(),
    );
    metadata.insert("encoding".to_owned(), in_codec.metadata_name().to_owned());
    metadata.insert(
        "outbound_encoding".to_owned(),
        telnyx.config.codec.metadata_name().to_owned(),
    );
    metadata.insert("stream_id".to_owned(), start.stream_id.clone());
    metadata.insert("call_control_id".to_owned(), start.call_control_id.clone());
    for (key, value) in [
        ("call_session_id", &start.call_session_id),
        ("user_id", &start.user_id),
        ("from", &start.from),
        ("to", &start.to),
        ("client_state", &start.client_state),
        ("telnyx_version", &version),
    ] {
        if let Some(value) = value {
            metadata.insert(key.to_owned(), value.clone());
        }
    }
    if !start.tags.is_empty() {
        metadata.insert("tags".to_owned(), start.tags.join(","));
    }
    metadata.insert("peer_addr".to_owned(), peer.to_string());
    let info = CallInfo {
        call_id: call_id.clone(),
        metadata,
    };

    let (mut link, ends) = call_pair();
    admitted.attach(&mut link);
    let CallEnds {
        events,
        mut outgoing,
        ended,
    } = ends;
    // The queue is empty, so `Started` goes in at once; the core sees it first.
    if events.send(WireEvent::Started(info)).await.is_err() {
        request_hangup(&shared, &telnyx, &hangup, &call_id).await;
        return;
    }
    let handed = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => None,
        sent = timeout(&*shared.clock, shared.config.send_timeout, shared.calls.send(link)) => {
            Some(matches!(sent, Ok(Ok(()))))
        }
    };
    if handed != Some(true) {
        // The engine never had this call: hang it up, and let the claim go so that a retry
        // (Telnyx may reconnect after the 1013) is not refused as a duplicate.
        request_hangup(&shared, &telnyx, &hangup, &call_id).await;
        // Released before the peer is told to try again, so the retry finds the key free.
        drop(claim);
        if handed == Some(false) {
            refuse(
                &mut ws,
                &shared,
                "engine is not taking calls",
                CloseCode::Again,
            )
            .await;
        }
        return;
    }
    claim.served();
    Counters::add(&telnyx.counters.calls, 1);
    tracing::info!(%call_id, %peer, "telnyx call started");

    let (sink, stream) = ws.split();
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_CAPACITY);
    let (notice_tx, notice_rx) = mpsc::channel(NOTICE_CAPACITY);
    let conn = Conn {
        shared: Arc::clone(&shared),
        telnyx: Arc::clone(&telnyx),
        call_id: call_id.clone(),
        stream_id: start.stream_id.clone(),
        in_codec,
        hangup,
        book: Mutex::new(MarkBook::default()),
        ended,
        peer_over: AtomicBool::new(false),
        stop: claim.stop_mark(),
        reader_done: CancellationToken::new(),
        writer_done: CancellationToken::new(),
    };
    let reader = async {
        let kept = Reader {
            conn: &conn,
            events: Some(events),
            ctrl: ctrl_tx,
            seq: 0,
            faults: 0,
            last_chunk: None,
            reorder: Reorder::new(telnyx.config.reorder_window),
            armed: None,
            dtmf: Vec::new(),
            dtmf_seq: 0,
            raw: Vec::new(),
            scratch: BytesMut::new(),
        }
        .run(stream, notice_rx)
        .await;
        conn.reader_done.cancel();
        kept
    };
    let writer = async {
        Writer {
            conn: &conn,
            sink,
            min_epoch: TurnEpoch::ZERO,
            rate_mismatch_reported: false,
            notices: notice_tx,
            scratch: BytesMut::new(),
            messages: BytesMut::new(),
            carry: Vec::new(),
            silence: silence_byte(telnyx.config.codec.g711()),
        }
        .run(&mut outgoing, ctrl_rx)
        .await;
    };
    let (kept, ()) = tokio::join!(reader, writer);
    if let Some(kept) = kept {
        linger(&conn, &mut outgoing).await;
        drop(kept);
    }
    drop(claim);
    tracing::info!(%call_id, "telnyx call ended");
}

/// Keeps the engine's session of a call whose stream the host stopped on purpose alive without a
/// socket: what the engine sends is discarded, until it ends the call, the server stops or
/// [`STOPPED_LINGER`] has passed. Bounded, and cancellable by the server's token.
async fn linger(conn: &Conn, outgoing: &mut channel::Receiver<WireOut>) {
    tracing::info!(call_id = %conn.call_id, "telnyx stream stopped by the host, session kept");
    let over = conn.shared.clock.sleep(STOPPED_LINGER);
    tokio::pin!(over);
    loop {
        tokio::select! {
            biased;
            () = conn.interrupted() => return,
            () = &mut over => return,
            out = outgoing.recv() => {
                if out.is_none() {
                    return;
                }
            }
        }
    }
}

/// Asks the host to hang the call up (REST). Telnyx does not document that closing the socket
/// ends a Call Control call, so this is what ends it. Bounded by `hangup_timeout`; a failure is
/// counted and logged, never fatal. Without a hook, or without a `call_control_id` to name the
/// call, there is nothing to do (the latter is logged and counted).
async fn request_hangup(
    shared: &Shared,
    telnyx: &TelnyxShared,
    request: &HangupRequest,
    call_id: &str,
) {
    let Some(hook) = &telnyx.config.hangup else {
        return;
    };
    let counters = &telnyx.counters;
    if request.call_control_id.is_empty() {
        Counters::add(&counters.hangups_skipped, 1);
        tracing::warn!(%call_id, "telnyx start carried no call_control_id: cannot hang the call up");
        return;
    }
    Counters::add(&counters.hangups_requested, 1);
    let limit = telnyx.config.hangup_timeout;
    match timeout(&*shared.clock, limit, hook.call(request.clone())).await {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            Counters::add(&counters.hangups_failed, 1);
            tracing::warn!(%call_id, reason, "telnyx hang-up failed");
        }
        Err(_) => {
            Counters::add(&counters.hangups_failed, 1);
            tracing::warn!(%call_id, "telnyx hang-up timed out");
        }
    }
}

/// The G.711 byte of a zero sample.
fn silence_byte(codec: Codec) -> u8 {
    let mut out = [0u8; 1];
    let fallback = match codec {
        Codec::Alaw => 0xD5,
        _ => 0xFF,
    };
    match codec.encode(&[0, 0], &mut out) {
        Ok(_) => out.first().copied().unwrap_or(fallback),
        Err(_) => fallback,
    }
}

/// What both sides of a connection share.
struct Conn {
    shared: Arc<Shared>,
    telnyx: Arc<TelnyxShared>,
    call_id: String,
    stream_id: String,
    /// The codec of caller audio (`start.media_format.encoding`).
    in_codec: TelnyxCodec,
    /// What the hang-up hook is called with.
    hangup: HangupRequest,
    book: Mutex<MarkBook>,
    /// Cancelled by the core once it ended the call.
    ended: CancellationToken,
    /// Set when Telnyx sent `stop` ("sent when the call ends"): the call needs no hang-up from us.
    /// A bare close frame does not set it: it is not documented as ending the call.
    peer_over: AtomicBool,
    /// The host's announcement that it stops this call's stream on purpose
    /// ([`super::StreamStopper`]).
    stop: Arc<StopMark>,
    /// Cancelled when the reader returns.
    reader_done: CancellationToken,
    /// Cancelled when the writer has said its last word (the close frame, or a broken socket).
    /// Until then the reader of a closing connection does not read: reading a close frame makes
    /// the websocket layer refuse every later write, so the farewell would be lost.
    writer_done: CancellationToken,
}

impl Conn {
    fn book(&self) -> MutexGuard<'_, MarkBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn counters(&self) -> &Counters {
        &self.telnyx.counters
    }

    /// Whether the host announced a stop of this stream that is still expected: the end of the
    /// stream is then the host's doing, not the caller's, and needs no hang-up.
    fn host_stopped(&self) -> bool {
        self.stop.expected(self.shared.clock.now())
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

/// A `dtmf` event held for ordering.
struct HeldDtmf {
    seq: u64,
    digit: DtmfDigit,
    /// When it arrived (the engine is told this, not the flush time).
    at: Instant,
}

struct Reader<'a> {
    conn: &'a Conn,
    /// `None` once the reader stopped feeding the core (the core then sees the call end).
    events: Option<channel::Sender<WireEvent>>,
    ctrl: mpsc::Sender<Ctrl>,
    seq: u64,
    faults: u32,
    /// `media.chunk` of the last inbound media message, to count lost audio when the reorder
    /// buffer is off.
    last_chunk: Option<u64>,
    /// Holds inbound audio that arrived ahead of its turn.
    reorder: Reorder,
    /// The deadline the hold timer is set for (the oldest wait of the media and DTMF held), or
    /// `None` when nothing is held.
    armed: Option<Instant>,
    /// `dtmf` events waiting to be put in `sequence_number` order, in arrival order. At most
    /// [`MAX_HELD_DTMF`]: one more flushes them all.
    dtmf: Vec<HeldDtmf>,
    /// The highest `sequence_number` seen on a `dtmf` event (the stand-in for one without).
    dtmf_seq: u64,
    /// Buffer the base64 payload is decoded into (grown once, reused).
    raw: Vec<u8>,
    /// Buffer the G.711 decoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
}

impl Reader<'_> {
    /// Reads until the connection ends. Returns the engine's event sender when the stream was
    /// stopped by the host on purpose and the engine has not been told the call is over: the
    /// caller keeps it (and so the session) alive for a while, see `linger`.
    async fn run(
        mut self,
        stream: SplitStream<Ws>,
        notices: mpsc::Receiver<String>,
    ) -> Option<channel::Sender<WireEvent>> {
        self.read_loop(stream, notices).await;
        if self.conn.host_stopped() {
            self.conn.peer_over.store(true, Ordering::Release);
            self.events.take()
        } else {
            None
        }
    }

    async fn read_loop(
        &mut self,
        mut stream: SplitStream<Ws>,
        mut notices: mpsc::Receiver<String>,
    ) {
        let conn = self.conn;
        let shared = &conn.shared;
        let clock = &shared.clock;
        let config = &shared.config;
        let hold_for = conn.telnyx.config.reorder_timeout;
        let mut last_seen = clock.now();
        let idle = clock.sleep_until(deadline_after(last_seen, config.ping_timeout));
        tokio::pin!(idle);
        let drain = clock.sleep(config.close_grace);
        tokio::pin!(drain);
        let hold = clock.sleep(hold_for);
        tokio::pin!(hold);
        let mut draining = false;
        let mut notices_open = true;
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
                        self.fatal("no frame from Telnyx in time", CloseCode::Policy).await;
                        draining = true;
                        drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                        continue;
                    }
                    idle.as_mut().reset(due);
                }
                () = conn.writer_done.cancelled(), if !may_read => {}
                () = &mut hold, if self.armed.is_some() && !draining => {
                    // A chunk did not arrive in time: release what has waited long enough.
                    self.armed = None;
                    match self.flush_due(hold_for).await {
                        Flow::Continue => {}
                        Flow::Drain => {
                            draining = true;
                            drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                        }
                        Flow::Exit => return,
                    }
                    self.sync_hold(hold.as_mut(), hold_for);
                }
                notice = notices.recv(), if notices_open && !draining => match notice {
                    Some(message) => {
                        if matches!(self.fault(message).await, Flow::Drain) {
                            draining = true;
                            drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                        }
                    }
                    None => notices_open = false,
                },
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
                        self.sync_hold(hold.as_mut(), hold_for);
                    }
                },
            }
        }
    }

    /// Sets the hold timer for the oldest wait there is: the gap in front of the held media (it
    /// ends `reorder_timeout` after the first frame behind it arrived) or the oldest held `dtmf`.
    /// Called after every message and every flush, so a frame that is still held after a partial
    /// release keeps the wait of its own arrival, neither the stale deadline of a frame that has
    /// left nor a fresh one that would delay it for ever.
    fn sync_hold(&mut self, hold: Pin<&mut Sleep>, hold_for: Duration) {
        let due = [
            self.reorder.due_at(hold_for),
            self.dtmf.first().map(|d| deadline_after(d.at, hold_for)),
        ]
        .into_iter()
        .flatten()
        .min();
        match due {
            None => self.armed = None,
            Some(due) => {
                if self.armed != Some(due) {
                    hold.reset(due);
                    self.armed = Some(due);
                }
            }
        }
    }

    async fn handle(&mut self, message: Message) -> Flow {
        match message {
            Message::Text(text) => self.control(text.as_str()).await,
            Message::Close(_) => {
                // Telnyx closed the stream. That is not documented as ending the call (it may
                // recycle the socket), so `peer_over` stays unset: the writer asks the host to
                // hang up, which is harmless if the call is over already. Only `stop` skips it
                // (and a stream the host stopped on purpose: the call goes on, so the engine
                // hears nothing and `run` keeps its session).
                self.release_all().await;
                if !self.conn.host_stopped() {
                    self.deliver(WireEvent::Hangup).await;
                    self.finish_input();
                }
                Flow::Exit
            }
            // The docs mention no binary frames (UNVERIFIED): ignore them.
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                Flow::Continue
            }
        }
    }

    async fn control(&mut self, text: &str) -> Flow {
        let max = self.conn.shared.config.max_control_bytes;
        let parsed = parse(text);
        if !within_limit(text.len(), max, &parsed) {
            self.fatal("control message too large", CloseCode::Size)
                .await;
            return Flow::Drain;
        }
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => return self.fault(error.to_string()).await,
        };
        if let Some(id) = &parsed.stream_id
            && id.as_ref() != self.conn.stream_id
        {
            // Never act on another stream's messages.
            Counters::add(&self.conn.counters().foreign_stream_messages, 1);
            return Flow::Continue;
        }
        let sequence = parsed.sequence;
        match parsed.message {
            Inbound::Connected { .. } | Inbound::Unknown(_) => Flow::Continue,
            Inbound::Start(_) => self.fault("start was already received".to_owned()).await,
            Inbound::Media {
                track,
                chunk,
                payload,
                ..
            } => {
                if track.as_deref().is_some_and(|t| t != "inbound") {
                    return Flow::Continue;
                }
                self.media(chunk, &payload).await
            }
            Inbound::Dtmf(digit) => self.dtmf(digit, sequence).await,
            Inbound::Mark { name } => self.mark(&name).await,
            Inbound::Error {
                code,
                title,
                detail,
            } => {
                Counters::add(&self.conn.counters().telnyx_errors, 1);
                let code = code.map_or_else(|| "?".to_owned(), |code| code.to_string());
                let mut text = format!(
                    "telnyx error {code} {}: {}",
                    title.unwrap_or_default(),
                    detail.unwrap_or_default()
                );
                if text.len() > MAX_ERROR_TEXT {
                    let mut end = MAX_ERROR_TEXT;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                // Telnyx's own error frames (rate limits among them) are reported, but they do not
                // count toward the limit that ends the call: they say nothing about this
                // adapter's input, and a long call may see a few.
                self.report(text).await
            }
            Inbound::Stop => {
                self.conn.peer_over.store(true, Ordering::Release);
                self.release_all().await;
                // `stop` after the host's own `streaming_stop` is not the caller hanging up.
                if !self.conn.host_stopped() {
                    self.deliver(WireEvent::Hangup).await;
                    self.finish_input();
                }
                Flow::Drain
            }
        }
    }

    /// Caller audio: base64 G.711 to PCM16 at 8 kHz, put in `media.chunk` order. A frame whose
    /// payload is unusable still occupies its chunk number ([`consume`](Self::consume)), so the
    /// frames behind it neither wait for it nor count as late.
    async fn media(&mut self, chunk: Option<u64>, payload: &str) -> Flow {
        let max_g711 = self.conn.shared.config.max_audio_bytes / AudioFrame::BYTES_PER_SAMPLE;
        let mut problem = None;
        self.raw.clear();
        // Decoded size of the base64 text, an upper bound.
        if payload.len() / 4 * 3 > max_g711.saturating_add(3) {
            problem = Some(format!("media payload exceeds {max_g711} bytes"));
        } else if !payload.is_empty() {
            let needed = base64::decoded_len_estimate(payload.len());
            self.raw.resize(needed, 0);
            if let Ok(count) = BASE64.decode_slice(payload.as_bytes(), &mut self.raw) {
                self.raw.truncate(count);
            } else {
                self.raw.clear();
                problem = Some("media payload is not valid base64".to_owned());
            }
        }
        if problem.is_some() || self.raw.is_empty() {
            let flow = self.consume(chunk).await;
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
            return match problem {
                Some(message) => self.fault(message).await,
                None => Flow::Continue,
            };
        }
        let ordering = self.conn.telnyx.config.reorder_window > 0;
        let Some(chunk) = chunk else {
            // Without a chunk number there is nothing to order by.
            return self.deliver_raw().await;
        };
        if !ordering {
            self.count_gap(chunk);
            return self.deliver_raw().await;
        }
        if self.reorder.in_order(chunk) {
            self.reorder.advance(chunk);
            return self.deliver_raw().await;
        }
        // Ahead of its turn (or late): the only frames that are copied.
        let now = self.conn.shared.clock.now();
        match self.reorder.offer(chunk, self.raw.clone(), now) {
            Offer::Held => Counters::add(&self.conn.counters().media_reordered, 1),
            Offer::Dropped => {
                Counters::add(&self.conn.counters().media_late, 1);
                return Flow::Continue;
            }
            Offer::Resynced { discarded } => {
                // The numbering changed under us: follow it, and tell the engine, because the
                // alternative is a caller who is silently not heard.
                self.resynced(chunk, discarded);
                self.reorder.advance(chunk);
                let flow = self.deliver_raw().await;
                if !matches!(flow, Flow::Continue) {
                    return flow;
                }
                return self
                    .fault(format!(
                        "telnyx media chunk numbering changed; following chunk {chunk}"
                    ))
                    .await;
            }
        }
        self.release_ready().await
    }

    fn resynced(&self, chunk: u64, discarded: usize) {
        let counters = self.conn.counters();
        Counters::add(&counters.chunk_resyncs, 1);
        Counters::add(
            &counters.inbound_chunks_lost,
            u64::try_from(discarded).unwrap_or(u64::MAX),
        );
        tracing::warn!(call_id = %self.conn.call_id, chunk, discarded, "telnyx media chunk numbering changed");
    }

    /// A media frame arrived whose payload is not usable: its chunk number is consumed without
    /// audio, so the reorder buffer moves past it.
    async fn consume(&mut self, chunk: Option<u64>) -> Flow {
        let Some(chunk) = chunk else {
            return Flow::Continue;
        };
        if self.conn.telnyx.config.reorder_window == 0 {
            self.count_gap(chunk);
            return Flow::Continue;
        }
        if self.reorder.in_order(chunk) {
            self.reorder.advance(chunk);
            return Flow::Continue;
        }
        let now = self.conn.shared.clock.now();
        match self.reorder.offer(chunk, Vec::new(), now) {
            Offer::Held => self.release_ready().await,
            Offer::Dropped => Flow::Continue,
            Offer::Resynced { discarded } => {
                self.resynced(chunk, discarded);
                self.reorder.advance(chunk);
                Flow::Continue
            }
        }
    }

    /// A `dtmf` event. Telnyx does not order its frames, so digits wait up to `reorder_timeout`
    /// and leave in `sequence_number` order (`occurred_at` is RFC 3339 with a variable number of
    /// fraction digits and so is no better a key); with the reorder buffer off they are
    /// delivered at once.
    async fn dtmf(&mut self, digit: DtmfDigit, sequence: Option<u64>) -> Flow {
        let at = self.conn.shared.clock.now();
        if self.conn.telnyx.config.reorder_window == 0 {
            return self.flow(WireEvent::Dtmf { digit, at }).await;
        }
        let seq = match sequence {
            Some(seq) => {
                self.dtmf_seq = self.dtmf_seq.max(seq);
                seq
            }
            // Without a number a digit keeps its place in arrival order.
            None => self.dtmf_seq,
        };
        self.dtmf.push(HeldDtmf { seq, digit, at });
        if self.dtmf.len() >= MAX_HELD_DTMF {
            return self.flush_dtmf().await;
        }
        Flow::Continue
    }

    /// Delivers every held `dtmf`, in `sequence_number` order (arrival order among equals).
    async fn flush_dtmf(&mut self) -> Flow {
        let mut held = std::mem::take(&mut self.dtmf);
        let arrival: Vec<u64> = held.iter().map(|d| d.seq).collect();
        held.sort_by_key(|d| d.seq);
        let moved = held
            .iter()
            .zip(&arrival)
            .filter(|(d, seq)| d.seq != **seq)
            .count();
        Counters::add(
            &self.conn.counters().dtmf_reordered,
            u64::try_from(moved).unwrap_or(u64::MAX),
        );
        let mut flow = Flow::Continue;
        for d in held.drain(..) {
            flow = self
                .flow(WireEvent::Dtmf {
                    digit: d.digit,
                    at: d.at,
                })
                .await;
            if !matches!(flow, Flow::Continue) {
                break;
            }
        }
        // Keeps the allocation (empty unless the loop stopped early).
        held.clear();
        self.dtmf = held;
        flow
    }

    /// Without the reorder buffer: counts the chunk numbers that were skipped.
    fn count_gap(&mut self, chunk: u64) {
        if let Some(last) = self.last_chunk
            && chunk > last.saturating_add(1)
        {
            let lost = chunk - last - 1;
            Counters::add(&self.conn.counters().inbound_chunks_lost, lost);
            tracing::debug!(call_id = %self.conn.call_id, lost, "telnyx dropped inbound audio");
        }
        self.last_chunk = Some(chunk);
    }

    /// Delivers the payload in `self.raw`.
    async fn deliver_raw(&mut self) -> Flow {
        let raw = std::mem::take(&mut self.raw);
        let flow = self.audio(&raw).await;
        self.raw = raw;
        flow
    }

    /// Delivers a frame the reorder buffer released (nothing for a chunk that had no audio).
    async fn released(&mut self, released: Released) -> Flow {
        Counters::add(&self.conn.counters().inbound_chunks_lost, released.lost);
        if released.data.is_empty() {
            return Flow::Continue;
        }
        self.audio(&released.data).await
    }

    /// Delivers the frames the reorder buffer can release now.
    async fn release_ready(&mut self) -> Flow {
        while let Some(released) = self.reorder.pop_ready() {
            let flow = self.released(released).await;
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
        }
        Flow::Continue
    }

    /// The hold timer fired: delivers the media whose gap has been waited on for `hold_for`, in
    /// order, giving up on those gaps, and the held `dtmf` if the oldest has waited that long.
    async fn flush_due(&mut self, hold_for: Duration) -> Flow {
        let now = self.conn.shared.clock.now();
        while let Some(released) = self.reorder.pop_due(now, hold_for) {
            let flow = self.released(released).await;
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
        }
        if self
            .dtmf
            .first()
            .is_some_and(|d| deadline_after(d.at, hold_for) <= now)
        {
            return self.flush_dtmf().await;
        }
        Flow::Continue
    }

    /// Delivers every frame the reorder buffer holds, in order, giving up on the gaps.
    async fn flush_held(&mut self) -> Flow {
        while let Some(released) = self.reorder.pop_oldest() {
            let flow = self.released(released).await;
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
        }
        Flow::Continue
    }

    /// The stream is over: what is held still belongs to the call.
    async fn release_all(&mut self) {
        let _ = self.flush_held().await;
        if !self.dtmf.is_empty() {
            let _ = self.flush_dtmf().await;
        }
        self.armed = None;
    }

    /// Hands decoded caller audio to the engine.
    async fn audio(&mut self, g711: &[u8]) -> Flow {
        let codec = self.conn.in_codec.g711();
        let len = codec.decoded_len(g711.len());
        let decoded = scratch_bytes(&mut self.scratch, len, |out| {
            codec.decode(g711, out).is_ok()
        });
        let Some(pcm) = decoded else {
            return Flow::Continue;
        };
        let now = self.conn.shared.clock.now();
        let Ok(frame) = AudioFrame::new(pcm, SampleRate::Hz8000, now, self.seq, TurnEpoch::ZERO)
        else {
            return Flow::Continue;
        };
        self.seq = self.seq.saturating_add(1);
        Counters::add(&self.conn.counters().media_in, 1);
        // Input audio is `DropOldest`: this never waits, so no timeout is needed.
        let Some(events) = &self.events else {
            return Flow::Drain;
        };
        match events.send(WireEvent::Audio(frame)).await {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Drain,
        }
    }

    /// Telnyx echoes a mark: played, or flushed by our `clear`.
    async fn mark(&mut self, name: &str) -> Flow {
        let counters = self.conn.counters();
        let now = self.conn.shared.clock.now();
        let (echo, unplayed) = match MarkName::parse(name) {
            Some(name) => {
                let mut book = self.conn.book();
                (book.echo(name, now), book.unplayed_bytes())
            }
            None => (Echo::Unknown, 0),
        };
        // The audio sent after the echoed mark has not played: that is how far the caller is
        // behind the core's playback position.
        let lead = WireEvent::Lead {
            lead: Duration::from_micros(unplayed.saturating_mul(MICROS_PER_BYTE)),
        };
        match echo {
            Echo::Engine { id, latency } => {
                Counters::add(&counters.marks_echoed, 1);
                note_latency(counters, latency);
                // The lead follows the ack, so the engine sees "played" before the level.
                match self.flow(WireEvent::MarkAck { id }).await {
                    Flow::Continue => self.flow(lead).await,
                    other => other,
                }
            }
            Echo::Progress { latency } => {
                Counters::add(&counters.marks_echoed, 1);
                note_latency(counters, latency);
                self.flow(lead).await
            }
            Echo::Unknown => {
                Counters::add(&counters.marks_unmatched, 1);
                Flow::Continue
            }
        }
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

    /// A recoverable fault in the input: the engine sees a fault event; too many end the call.
    async fn fault(&mut self, message: String) -> Flow {
        self.faults = self.faults.saturating_add(1);
        if self.faults > MAX_FAULTS {
            Counters::add(&self.conn.counters().faults, 1);
            self.fatal("too many protocol faults", CloseCode::Policy)
                .await;
            return Flow::Drain;
        }
        self.report(message).await
    }

    /// Tells the engine about something that went wrong without counting it toward the limit.
    async fn report(&mut self, message: String) -> Flow {
        Counters::add(&self.conn.counters().faults, 1);
        self.flow(WireEvent::Fault { message }).await
    }

    /// A fatal fault: tells the writer to close, and the engine to note it.
    async fn fatal(&mut self, reason: &'static str, close: CloseCode) {
        tracing::debug!(call_id = %self.conn.call_id, reason, "telnyx call closed by protocol fault");
        // Only the first request matters; a full queue means one is already pending.
        let _ = self.ctrl.try_send(Ctrl::Close { close, reason });
        Counters::add(&self.conn.counters().faults, 1);
        self.deliver(WireEvent::Fault {
            message: reason.to_owned(),
        })
        .await;
        self.finish_input();
    }

    /// Stops feeding the core: dropping the queue's sender is how the core learns Telnyx is
    /// gone (a hang-up queued before stays readable).
    fn finish_input(&mut self) {
        self.events = None;
    }
}

fn note_latency(counters: &Counters, latency: Duration) {
    let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
    Counters::max(&counters.max_echo_latency_us, micros);
}

struct Writer<'a> {
    conn: &'a Conn,
    sink: SplitSink<Ws, Message>,
    min_epoch: TurnEpoch,
    rate_mismatch_reported: bool,
    /// Fault texts for the reader, which owns the engine's event queue.
    notices: mpsc::Sender<String>,
    /// Buffer the G.711 encoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
    /// Buffer the JSON messages are built in; one allocation serves many messages.
    messages: BytesMut,
    /// Encoded audio shorter than a message, waiting for more (never more than `chunk_bytes`).
    /// Telnyx accepts no less than 20 ms per message.
    carry: Vec<u8>,
    /// The G.711 byte of silence, to pad a tail with.
    silence: u8,
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
    /// The socket is closed or broke.
    Closed,
    /// Telnyx did not take the data within the write timeout.
    Slow,
    /// The server is shutting down or the core ended the call while the write was blocked.
    Interrupted,
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
                tracing::warn!(call_id = %conn.call_id, "telnyx does not take output, closing the call");
                Err(WriteFailure::Slow)
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), WriteFailure> {
        let limit = self.conn.shared.config.write_timeout;
        self.write(message, limit, true).await
    }

    /// How long the goodbye may take: it is the last thing the call does, so it gets a short
    /// bound.
    fn farewell_limit(&self) -> Duration {
        let config = &self.conn.shared.config;
        config.write_timeout.min(config.close_grace)
    }

    /// The last word: a close frame. Returns whether the socket still answers.
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

    /// Asks the host to hang the call up (REST), unless Telnyx itself ended it with a `stop`.
    async fn hang_up(&self) {
        let conn = self.conn;
        if conn.peer_over.load(Ordering::Acquire) || conn.host_stopped() {
            return;
        }
        request_hangup(&conn.shared, &conn.telnyx, &conn.hangup, &conn.call_id).await;
    }

    /// What a failed write leads to.
    fn depart(failure: &WriteFailure, conn: &Conn) -> Departure {
        match failure {
            WriteFailure::Closed | WriteFailure::Slow => Departure::Broken,
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

    async fn run(mut self, outgoing: &mut channel::Receiver<WireOut>, ctrl: mpsc::Receiver<Ctrl>) {
        let conn = self.conn;
        let departure = self.serve_loop(outgoing, ctrl).await;
        // The connection ends on our initiative (or the socket broke): end the call too, before
        // the close handshake, so Telnyx cannot reconnect a stream for a call that is going away.
        self.hang_up().await;
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
        outgoing: &mut channel::Receiver<WireOut>,
        mut ctrl: mpsc::Receiver<Ctrl>,
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

    /// Tells the reader (and through it the engine) about a fault. Drops the text if the queue is
    /// full.
    fn notice(&self, message: String) {
        let _ = self.notices.try_send(message);
    }

    /// Barge-in: forget the marks in flight (Telnyx flushes and echoes them; those echoes are
    /// dropped) and the audio waiting for company, then tell Telnyx to stop and clear.
    async fn clear(&mut self, epoch: TurnEpoch) -> Result<(), WriteFailure> {
        self.min_epoch = self.min_epoch.max(epoch);
        self.carry.clear();
        let cut = self.conn.book().clear();
        let counters = self.conn.counters();
        Counters::add(&counters.marks_cut, u64::try_from(cut).unwrap_or(u64::MAX));
        Counters::add(&counters.clears_sent, 1);
        self.send(Message::Text(Utf8Bytes::from_static(protocol::CLEAR)))
            .await
    }

    /// An engine mark: Telnyx echoes it when the audio before it has played. The tail of the
    /// utterance that was waiting for company goes out first, padded to Telnyx's 20 ms minimum.
    async fn engine_mark(&mut self, id: String) -> Result<(), WriteFailure> {
        self.flush_carry().await?;
        let now = self.conn.shared.clock.now();
        let (name, evicted) = self.conn.book().engine_mark(id, now);
        self.send_mark(name, evicted).await
    }

    async fn send_mark(&mut self, name: MarkName, evicted: bool) -> Result<(), WriteFailure> {
        let counters = self.conn.counters();
        Counters::add(&counters.marks_sent, 1);
        if evicted {
            Counters::add(&counters.marks_evicted, 1);
        }
        let message = Utf8Bytes::from(protocol::mark(name));
        self.send(Message::Text(message)).await
    }

    /// A progress mark when enough audio went out since the last mark.
    async fn progress_if_due(&mut self) -> Result<(), WriteFailure> {
        let interval = self.conn.telnyx.config.mark_interval;
        if interval.is_zero() {
            return Ok(());
        }
        let due_bytes = u64::try_from(interval.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(BYTES_PER_MS);
        let now = self.conn.shared.clock.now();
        let marked = {
            let mut book = self.conn.book();
            (book.unmarked_bytes() >= due_bytes).then(|| book.progress_mark(now))
        };
        match marked {
            Some((name, evicted)) => self.send_mark(name, evicted).await,
            None => Ok(()),
        }
    }

    /// Sends the audio that was waiting for company, silence-padded to the 20 ms minimum.
    async fn flush_carry(&mut self) -> Result<(), WriteFailure> {
        if self.carry.is_empty() {
            return Ok(());
        }
        let mut tail = std::mem::take(&mut self.carry);
        if tail.len() < MIN_CHUNK_BYTES {
            tail.resize(MIN_CHUNK_BYTES, self.silence);
        }
        let sent = self.send_media(&tail).await;
        tail.clear();
        self.carry = tail;
        sent
    }

    /// Writes one `media` message and does the bookkeeping for it.
    async fn send_media(&mut self, part: &[u8]) -> Result<(), WriteFailure> {
        if self.messages.capacity() < 2 * 1024 {
            self.messages.reserve(MESSAGE_CHUNK);
        }
        write_media(part, &mut self.messages);
        let Ok(text) = Utf8Bytes::try_from(self.messages.split().freeze()) else {
            return Ok(());
        };
        self.send(Message::Text(text)).await?;
        let counters = self.conn.counters();
        Counters::add(&counters.media_out, 1);
        Counters::add(&counters.media_out_bytes, part.len() as u64);
        let unplayed = {
            let mut book = self.conn.book();
            book.audio_sent(part.len());
            book.unplayed_bytes()
        };
        Counters::max(&counters.max_unplayed_ms, unplayed / BYTES_PER_MS);
        self.progress_if_due().await
    }

    /// Writes one frame of bot audio: PCM16 8 kHz to G.711, in `chunk_bytes` pieces. A tail shorter
    /// than a piece waits for the next frame (or the next mark).
    async fn audio(&mut self, frame: &AudioFrame) -> Result<(), WriteFailure> {
        // Audio of an older epoch than the last clear never goes out.
        if frame.epoch().get() < self.min_epoch.get() {
            return Ok(());
        }
        if frame.sample_rate() != SampleRate::Hz8000 {
            // Resampling is DSP work for the audio layer, not for a transport task.
            if !self.rate_mismatch_reported {
                self.rate_mismatch_reported = true;
                self.notice(format!(
                    "bot audio is {} but a telnyx call is 8000 Hz; mismatched audio is dropped",
                    frame.sample_rate()
                ));
            }
            return Ok(());
        }
        let codec = self.conn.telnyx.config.codec.g711();
        let len = codec.encoded_len(frame.data().len());
        let source = frame.data();
        let encoded = scratch_bytes(&mut self.scratch, len, |out| {
            codec.encode(source, out).is_ok()
        });
        let Some(encoded) = encoded else {
            return Ok(());
        };
        let chunk = self.conn.telnyx.config.chunk_bytes.max(MIN_CHUNK_BYTES);
        let mut pending = std::mem::take(&mut self.carry);
        pending.extend_from_slice(&encoded);
        let mut sent = 0usize;
        let result = loop {
            let Some(part) = pending.get(sent..sent.saturating_add(chunk)) else {
                break Ok(());
            };
            if let Err(failure) = self.send_media(part).await {
                break Err(failure);
            }
            sent += chunk;
        };
        pending.drain(..sent.min(pending.len()));
        self.carry = pending;
        result
    }
}
