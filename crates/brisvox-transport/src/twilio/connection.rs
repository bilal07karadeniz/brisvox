//! One Twilio Media Streams connection: upgrade (path and signature check), `connected` /
//! `start`, then a reader and a writer side that run concurrently inside the connection's single
//! task. The structure and the shutdown protocol are those of the generic websocket connection
//! (`websocket/connection.rs`); only the wire format differs.
//!
//! ```text
//!   Twilio --media/dtmf/mark/stop--> reader --WireEvent--> core (CallLink)
//!   Twilio <--media/mark/clear------ writer <--WireOut---- core
//!                  reader --Ctrl::Close--> writer
//!                  reader <--fault text--- writer
//!                  reader, writer share the MarkBook
//! ```
//!
//! Twilio accepts only `media`, `mark` and `clear` from the server, so there is no error or stop
//! message to send: a problem on the connection is reported to the engine as a fault event, and
//! a fatal one closes the socket with a close code and reason. Closing the socket is also how
//! the call leg is hung up.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use super::marks::{Counters, Echo, MarkBook};
use super::protocol::{
    BASE64, Inbound, MULAW_ENCODING, MarkName, Outbound, SAMPLE_RATE_HZ, StartInfo, parse,
};
use super::registry::{Claim, Handoff, Parked, RESUME_PARAMETER, Registry, Slot};
use super::stream_token::StreamTokenBook;
use super::{TwilioConfig, signature};
use crate::call::{CallEnds, WireEvent, WireOut, call_pair};
use crate::channel;
use crate::event::CallInfo;
use crate::telephony::{Ws, blame, close_reason, refusal, refuse};
use crate::websocket::connection::{scratch_bytes, ws_config};
use crate::websocket::{Admitted, MIN_PING_INTERVAL, Shared, timeout};

use base64::Engine as _;

/// Capacity of the reader-to-writer queue of close requests (the first one wins).
const CTRL_CAPACITY: usize = 2;

/// Capacity of the writer-to-reader queue of fault texts. A full queue drops the new text (the
/// fault is still counted).
const NOTICE_CAPACITY: usize = 8;

/// Non-fatal faults tolerated on one call before it is closed.
const MAX_FAULTS: u32 = 16;

/// Allocation unit of the outgoing message buffer: one allocation serves many messages (a 20 ms
/// `media` message is about 250 bytes).
const MESSAGE_CHUNK: usize = 16 * 1024;

/// Mu-law bytes per millisecond at 8 kHz.
const BYTES_PER_MS: u64 = 8;

/// Microseconds of audio in one mu-law byte at 8 kHz.
const MICROS_PER_BYTE: u64 = 1_000_000 / 8_000;

/// State the upgrade check and every connection share.
pub(crate) struct TwilioShared {
    pub(crate) config: TwilioConfig,
    pub(crate) counters: Arc<Counters>,
    /// The URL forms a signature is checked against (empty when there is no auth token).
    pub(crate) signed_urls: Vec<String>,
    /// The calls streamed right now, for streams that end on purpose and come back.
    pub(crate) registry: Arc<Registry>,
    /// Per-call stream URLs (`stream_secret`): verifies the URL's segment and remembers the
    /// ones used. `None`: only the fixed URL exists.
    pub(crate) tokens: Option<StreamTokenBook>,
}

/// Which route an upgrade request asked for.
enum Route<'a> {
    /// Exactly the configured path.
    Static,
    /// The configured path and one more segment: a per-call stream URL.
    PerCall(&'a str),
    Unknown,
}

impl TwilioShared {
    fn route<'a>(&self, path: &'a str) -> Route<'a> {
        let wanted = self.config.path.trim_end_matches('/');
        let trimmed = path.trim_end_matches('/');
        if trimmed == wanted {
            return Route::Static;
        }
        match trimmed
            .strip_prefix(wanted)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(segment) if !segment.is_empty() && !segment.contains('/') => {
                Route::PerCall(segment)
            }
            _ => Route::Unknown,
        }
    }

    /// The upgrade check: path, then the per-call segment when `stream_secret` is configured,
    /// then the signature when an auth token is configured. `Ok(Some(sid))` is the call SID a
    /// per-call URL was minted for (the `start` that follows must carry it).
    fn check(&self, request: &Request) -> Result<Option<String>, Box<ErrorResponse>> {
        let path = request.uri().path();
        let header = request
            .headers()
            .get("x-twilio-signature")
            .and_then(|value| value.to_str().ok());
        match self.route(path) {
            Route::Unknown => {
                tracing::debug!(path, "twilio upgrade refused: unknown path");
                Err(refusal(StatusCode::NOT_FOUND, "not found"))
            }
            Route::Static => {
                if self.tokens.is_some() && !self.config.allow_static_url {
                    tracing::debug!("twilio upgrade refused: the per-call stream URL is required");
                    return Err(refusal(StatusCode::FORBIDDEN, "stream token required"));
                }
                self.check_signature(header, &self.signed_urls)?;
                Ok(None)
            }
            Route::PerCall(segment) => {
                let Some(tokens) = &self.tokens else {
                    tracing::debug!(path, "twilio upgrade refused: unknown path");
                    return Err(refusal(StatusCode::NOT_FOUND, "not found"));
                };
                let checked = tokens.check(segment).map_err(|reason| {
                    tracing::debug!(?reason, "twilio upgrade refused: stream token");
                    refusal(StatusCode::FORBIDDEN, "invalid stream token")
                })?;
                // The signature covers the whole per-call URL, as Twilio signs it.
                if let (Some(_), Some(public)) = (&self.config.auth_token, &self.config.public_url)
                {
                    let url = format!("{}/{segment}", public.trim_end_matches('/'));
                    self.check_signature(header, &signature::candidate_urls(&url))?;
                }
                // Used up only after the signature held: an attacker holding a URL but no
                // signature cannot burn a legitimate call's token.
                let sid = tokens.consume(checked).map_err(|reason| {
                    tracing::debug!(?reason, "twilio upgrade refused: stream token");
                    refusal(StatusCode::FORBIDDEN, "invalid stream token")
                })?;
                Ok(Some(sid))
            }
        }
    }

    fn check_signature(
        &self,
        header: Option<&str>,
        urls: &[String],
    ) -> Result<(), Box<ErrorResponse>> {
        let Some(token) = &self.config.auth_token else {
            return Ok(());
        };
        match header {
            None => {
                tracing::debug!("twilio upgrade refused: no X-Twilio-Signature");
                Err(refusal(StatusCode::FORBIDDEN, "signature required"))
            }
            Some(header) if signature::verify(token.expose(), urls, header) => Ok(()),
            Some(_) => {
                tracing::debug!("twilio upgrade refused: signature does not match");
                Err(refusal(StatusCode::FORBIDDEN, "invalid signature"))
            }
        }
    }
}

/// The upgrade callback: the answer is the unchanged response, or a refusal. The call SID of a
/// per-call URL is left in `bound` for the connection to compare with its `start`.
struct Upgrade {
    shared: Arc<TwilioShared>,
    bound: Arc<Mutex<Option<String>>>,
}

impl Callback for Upgrade {
    #[allow(clippy::result_large_err)] // the error type is fixed by tungstenite's `Callback`
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        match self.shared.check(request) {
            Ok(sid) => {
                *self.bound.lock().unwrap_or_else(PoisonError::into_inner) = sid;
                Ok(response)
            }
            Err(refusal) => Err(*refusal),
        }
    }
}

/// What the stream's first messages turned out to be.
enum Opening {
    Start(Box<StartInfo>, Option<(Option<String>, Option<String>)>),
    /// Twilio went away before starting.
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

/// Waits for `start` (ignoring a leading `connected`, and anything unknown).
async fn read_start(ws: &mut Ws, shared: &Shared) -> Opening {
    let config = &shared.config;
    let read = async {
        let mut connected = None;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > config.max_control_bytes {
                        return refuse_with("control message too large", CloseCode::Size);
                    }
                    match parse(text.as_str()) {
                        Ok(parsed) => match parsed.message {
                            Inbound::Connected { protocol, version } => {
                                connected = Some((protocol, version));
                            }
                            Inbound::Start(info) => return Opening::Start(info, connected),
                            Inbound::Stop => return Opening::Gone,
                            Inbound::Media { .. } | Inbound::Dtmf(_) | Inbound::Mark { .. } => {
                                return refuse_with("start expected first", CloseCode::Policy);
                            }
                            Inbound::Unknown(_) => {}
                        },
                        Err(error) => {
                            tracing::debug!(%error, "twilio message before start is invalid");
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

/// Checks the media format of a `start`. Twilio documents it as always mu-law, 8 kHz, mono; a
/// stream that says otherwise is refused (the adapter converts only that format). Absent fields
/// are taken as the documented values.
fn check_format(start: &StartInfo) -> Result<(), &'static str> {
    if let Some(encoding) = &start.encoding
        && !encoding.eq_ignore_ascii_case(MULAW_ENCODING)
    {
        return Err("unsupported media encoding");
    }
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
    Ok(())
}

/// What a connection's opening turned out to be: the upgraded socket and its `start`.
struct Opened {
    ws: Ws,
    start: Box<StartInfo>,
    connected: Option<(Option<String>, Option<String>)>,
}

/// Upgrades the connection, reads and checks its `start`. `None` when the connection ended or
/// was refused on the way (it has been answered).
async fn open(
    stream: TcpStream,
    peer: SocketAddr,
    shared: &Shared,
    twilio: &Arc<TwilioShared>,
) -> Option<Opened> {
    let _ = stream.set_nodelay(true);
    let bound = Arc::new(Mutex::new(None));
    let handshake = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        Upgrade {
            shared: Arc::clone(twilio),
            bound: Arc::clone(&bound),
        },
        Some(ws_config(&shared.config)),
    );
    let mut ws = tokio::select! {
        biased;
        () = shared.cancel.cancelled() => return None,
        handshake = timeout(&*shared.clock, shared.config.handshake_timeout, handshake) => match handshake {
            Ok(Ok(ws)) => ws,
            Ok(Err(error)) => {
                tracing::debug!(%peer, %error, "twilio handshake failed");
                return None;
            }
            Err(_) => {
                tracing::debug!(%peer, "twilio handshake timed out");
                return None;
            }
        },
    };
    let (start, connected) = match read_start(&mut ws, shared).await {
        Opening::Start(start, connected) => (start, connected),
        Opening::Gone => return None,
        Opening::Refuse { reason, close } => {
            tracing::debug!(%peer, reason, "twilio start refused");
            refuse(&mut ws, shared, &reason, close).await;
            return None;
        }
    };
    if let Err(reason) = check_format(&start) {
        tracing::debug!(%peer, reason, "twilio start refused");
        refuse(&mut ws, shared, reason, CloseCode::Policy).await;
        return None;
    }
    // A per-call URL was minted for one call: another call's `start` on it is refused.
    let wanted_sid = bound.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(sid) = wanted_sid
        && sid != start.call_sid
    {
        tracing::debug!(%peer, "twilio start refused: the call is not the one the URL was issued for");
        refuse(
            &mut ws,
            shared,
            "stream URL is for another call",
            CloseCode::Policy,
        )
        .await;
        return None;
    }
    Some(Opened {
        ws,
        start,
        connected,
    })
}

/// Serves a connection the server has no capacity for (the call limit or the host's gate
/// refused it), because it may continue a call whose session is parked: that session keeps its
/// own call permit and ticket, so its next stream takes no capacity. A stream that is not a
/// continuation is answered like any over-limit connection, closed with a policy code. Holds
/// one reject slot for as long as it runs.
pub(crate) async fn serve_overflow(
    stream: TcpStream,
    peer: SocketAddr,
    _slot: OwnedSemaphorePermit,
    shared: Arc<Shared>,
    twilio: Arc<TwilioShared>,
) {
    let Some(Opened { mut ws, start, .. }) = open(stream, peer, &shared, &twilio).await else {
        return;
    };
    if start.custom_parameters.contains_key(RESUME_PARAMETER) {
        resume_call(ws, start, &shared, &twilio, peer).await;
    } else {
        tracing::debug!(%peer, "twilio stream beyond the call limit refused");
        refuse(&mut ws, &shared, "server busy", CloseCode::Again).await;
    }
}

/// Serves one admitted connection to its end. Holds the call permit (and the gate's ticket, until
/// it goes into the call's link) for as long as it runs.
pub(crate) async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    mut admitted: Admitted,
    shared: Arc<Shared>,
    twilio: Arc<TwilioShared>,
) {
    let Some(Opened {
        mut ws,
        start,
        connected,
    }) = open(stream, peer, &shared, &twilio).await
    else {
        return;
    };

    if start.custom_parameters.contains_key(RESUME_PARAMETER) {
        resume_call(ws, start, &shared, &twilio, peer).await;
        return;
    }

    let call_id = if start.call_sid.is_empty() {
        start.stream_sid.clone()
    } else {
        start.call_sid.clone()
    };
    let mut metadata = std::collections::BTreeMap::new();
    for (name, value) in &start.custom_parameters {
        metadata.insert(format!("custom.{name}"), value.clone());
    }
    metadata.insert("transport".to_owned(), "twilio".to_owned());
    metadata.insert(
        CallInfo::SAMPLE_RATE_KEY.to_owned(),
        SAMPLE_RATE_HZ.to_string(),
    );
    metadata.insert("encoding".to_owned(), "mulaw".to_owned());
    metadata.insert("stream_sid".to_owned(), start.stream_sid.clone());
    metadata.insert("call_sid".to_owned(), start.call_sid.clone());
    if let Some(account) = &start.account_sid {
        metadata.insert("account_sid".to_owned(), account.clone());
    }
    if !start.tracks.is_empty() {
        metadata.insert("tracks".to_owned(), start.tracks.join(","));
    }
    if let Some((protocol, version)) = connected {
        if let Some(protocol) = protocol {
            metadata.insert("twilio_protocol".to_owned(), protocol);
        }
        if let Some(version) = version {
            metadata.insert("twilio_version".to_owned(), version);
        }
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
            "engine is not taking calls",
            CloseCode::Again,
        )
        .await;
        return;
    }
    Counters::add(&twilio.counters.calls, 1);
    tracing::info!(%call_id, %peer, "twilio call started");

    let claim = twilio.registry.claim(&start.call_sid);
    let mut session = Session {
        events: Some(events),
        outgoing,
        held: VecDeque::new(),
        ended,
    };
    let mut socket = (ws, start.stream_sid.clone());
    loop {
        let (ws, stream_sid) = socket;
        let parked = run_socket(
            ws,
            &stream_sid,
            &call_id,
            &shared,
            &twilio,
            claim.as_ref().map(Claim::slot),
            &mut session,
        )
        .await;
        let (Some(linger), Some(claim)) = (parked, &claim) else {
            break;
        };
        Counters::add(&twilio.counters.sessions_parked, 1);
        tracing::info!(%call_id, ?linger, "twilio stream ended on purpose, waiting for the call to come back");
        match wait_for_stream(&shared, claim, linger, &mut session).await {
            Some(handoff) => {
                Counters::add(&twilio.counters.streams_resumed, 1);
                tracing::info!(%call_id, stream_sid = %handoff.start.stream_sid, "twilio call resumed on a new stream");
                socket = (handoff.ws, handoff.start.stream_sid.clone());
            }
            None => {
                tracing::info!(%call_id, "twilio call not resumed");
                break;
            }
        }
    }
    tracing::info!(%call_id, "twilio call ended");
}

/// The engine's side of a call, which outlives the sockets of a call that is resumed.
struct Session {
    /// `None` once the reader stopped feeding the core (the core then sees the call end).
    events: Option<channel::Sender<WireEvent>>,
    outgoing: channel::Receiver<WireOut>,
    /// What the engine sent while the call had no stream, for the next one (at most
    /// [`MAX_HELD_OUT`] items, the oldest dropped first).
    held: VecDeque<WireOut>,
    /// Cancelled by the core once it ended the call.
    ended: CancellationToken,
}

/// Most output items (20 ms audio frames, marks) kept for a call between two streams: a minute
/// of audio. A stream that is away longer loses the oldest.
const MAX_HELD_OUT: usize = 3_000;

/// Runs one socket of a call to its end. Returns how long the session should wait for a new
/// stream when the end of this one was announced by the host (`StreamResumer::expect_resume`)
/// and the engine's session is still up; `None` when the call is over.
async fn run_socket(
    ws: Ws,
    stream_sid: &str,
    call_id: &str,
    shared: &Arc<Shared>,
    twilio: &Arc<TwilioShared>,
    slot: Option<Arc<Slot>>,
    session: &mut Session,
) -> Option<Duration> {
    let (sink, stream) = ws.split();
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_CAPACITY);
    let (notice_tx, notice_rx) = mpsc::channel(NOTICE_CAPACITY);
    let conn = Conn {
        shared: Arc::clone(shared),
        twilio: Arc::clone(twilio),
        call_id: call_id.to_owned(),
        outbound: Outbound::new(stream_sid),
        stream_sid: stream_sid.to_owned(),
        book: Mutex::new(MarkBook::default()),
        ended: session.ended.clone(),
        reader_done: CancellationToken::new(),
        writer_done: CancellationToken::new(),
        slot,
        park_linger: Mutex::new(None),
    };
    let reader = async {
        Reader {
            conn: &conn,
            events: &mut session.events,
            ctrl: ctrl_tx,
            seq: 0,
            faults: 0,
            last_chunk: None,
            mulaw: Vec::new(),
            scratch: BytesMut::new(),
        }
        .run(stream, notice_rx)
        .await;
        conn.reader_done.cancel();
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
            engine_done: false,
        }
        .run(&mut session.outgoing, &mut session.held, ctrl_rx)
        .await
    };
    let ((), engine_done) = tokio::join!(reader, writer);
    let linger = conn.park_linger();
    let over = engine_done
        || shared.cancel.is_cancelled()
        || session.ended.is_cancelled()
        || session.events.is_none();
    if over { None } else { linger }
}

/// Waits for the continuation of a parked call: a stream with the `resume` parameter, handed
/// over by the connection that accepted it. What the engine sends meanwhile is held for the new
/// stream (a reply that starts while the stream is being restarted is not lost); a barge-in
/// clear drops it. `None` when the engine ended the call, the server stops, or `linger` passed.
async fn wait_for_stream(
    shared: &Shared,
    claim: &Claim,
    linger: Duration,
    session: &mut Session,
) -> Option<Handoff> {
    let mut resumed = match claim.park() {
        Parked::Ready(handoff) => return Some(*handoff),
        Parked::Wait(resumed) => resumed,
    };
    let expiry = shared.clock.sleep(linger);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return None,
            () = session.ended.cancelled() => return None,
            handoff = &mut resumed => return handoff.ok(),
            out = session.outgoing.recv() => match out {
                None | Some(WireOut::Hangup) => return None,
                Some(WireOut::Clear { .. }) => session.held.clear(),
                Some(out) => {
                    if session.held.len() >= MAX_HELD_OUT {
                        session.held.pop_front();
                    }
                    session.held.push_back(out);
                }
            },
            () = &mut expiry => return None,
        }
    }
}

/// A stream that carries the `resume` parameter: the call came back after the host ended its
/// stream on purpose. Hands the socket to the call's parked session (this connection is done
/// either way); a stream nobody expects is refused.
async fn resume_call(
    ws: Ws,
    start: Box<StartInfo>,
    shared: &Shared,
    twilio: &TwilioShared,
    peer: SocketAddr,
) {
    let call_sid = start.call_sid.clone();
    let now = shared.clock.now();
    match twilio
        .registry
        .resume(&call_sid, Handoff { ws, start }, now)
    {
        Ok(()) => {}
        Err(mut handoff) => {
            tracing::debug!(%peer, "twilio resume refused: no call is waiting for it");
            refuse(
                &mut handoff.ws,
                shared,
                "no call to resume",
                CloseCode::Policy,
            )
            .await;
        }
    }
}

/// What both sides of a connection share.
struct Conn {
    shared: Arc<Shared>,
    twilio: Arc<TwilioShared>,
    call_id: String,
    stream_sid: String,
    outbound: Outbound,
    book: Mutex<MarkBook>,
    /// Cancelled by the core once it ended the call.
    ended: CancellationToken,
    /// Cancelled when the reader returns.
    reader_done: CancellationToken,
    /// Cancelled when the writer has said its last word (the close frame, or a broken socket).
    /// Until then the reader of a closing connection does not read: reading a close frame makes
    /// the websocket layer refuse every later write, so the farewell would be lost.
    writer_done: CancellationToken,
    /// The call's entry in the registry (none for a call without a `callSid`).
    slot: Option<Arc<Slot>>,
    /// Set by the reader when this socket ended on purpose (announced by the host): how long the
    /// session waits for the call to come back. The reader then keeps the engine's event queue.
    park_linger: Mutex<Option<Duration>>,
}

impl Conn {
    fn book(&self) -> MutexGuard<'_, MarkBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn counters(&self) -> &Counters {
        &self.twilio.counters
    }

    /// Whether the end of this stream is the host's doing (announced, within the window): then
    /// the engine is not told the caller hung up and the session is parked. Remembers the answer.
    fn ended_on_purpose(&self) -> bool {
        let Some(slot) = &self.slot else {
            return false;
        };
        match slot.take_expected(self.shared.clock.now()) {
            Some(linger) => {
                *self
                    .park_linger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(linger);
                true
            }
            None => self.parking(),
        }
    }

    /// Whether this stream's session is being parked.
    fn parking(&self) -> bool {
        self.park_linger().is_some()
    }

    fn park_linger(&self) -> Option<Duration> {
        *self
            .park_linger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
    /// `None` once the reader stopped feeding the core (the core then sees the call end). Kept
    /// when the stream ended on purpose, for the call's next stream.
    events: &'a mut Option<channel::Sender<WireEvent>>,
    ctrl: mpsc::Sender<Ctrl>,
    seq: u64,
    faults: u32,
    /// `media.chunk` of the last inbound media message, to count lost audio.
    last_chunk: Option<u64>,
    /// Buffer the base64 payload is decoded into (grown once, reused).
    mulaw: Vec<u8>,
    /// Buffer the mu-law decoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
}

impl Reader<'_> {
    /// Reads until the stream ends. Unless the end was the host's doing (the session is parked
    /// for the call's next stream), the engine's queue is closed on the way out, so the core
    /// sees the call end.
    async fn run(mut self, stream: SplitStream<Ws>, notices: mpsc::Receiver<String>) {
        self.read(stream, notices).await;
        if !self.conn.parking() {
            self.finish_input();
        }
    }

    async fn read(&mut self, mut stream: SplitStream<Ws>, mut notices: mpsc::Receiver<String>) {
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
                        self.fatal("no frame from Twilio in time", CloseCode::Policy).await;
                        draining = true;
                        drain.as_mut().reset(deadline_after(clock.now(), config.close_grace));
                        continue;
                    }
                    idle.as_mut().reset(due);
                }
                () = conn.writer_done.cancelled(), if !may_read => {}
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
                    None => {
                        // A vanished socket is the host's doing when it announced the end.
                        conn.ended_on_purpose();
                        return;
                    }
                    Some(Err(error)) => {
                        if !draining && !conn.ended_on_purpose() && let Some((reason, close)) = blame(&error) {
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
            Message::Text(text) => self.control(text.as_str()).await,
            Message::Close(_) => {
                // A close frame is a clean hang-up; a vanished socket is not (see `run`). Unless
                // the host ended the stream on purpose (see `registry`).
                if !self.stream_ended_on_purpose() {
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
        if text.len() > max {
            self.fatal("control message too large", CloseCode::Size)
                .await;
            return Flow::Drain;
        }
        let parsed = match parse(text) {
            Ok(parsed) => parsed,
            Err(error) => return self.fault(error.to_string()).await,
        };
        if let Some(sid) = &parsed.stream_sid
            && sid.as_ref() != self.conn.stream_sid
        {
            // Never act on another stream's messages.
            Counters::add(&self.conn.counters().foreign_stream_messages, 1);
            return Flow::Continue;
        }
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
            Inbound::Dtmf(digit) => {
                let at = self.conn.shared.clock.now();
                self.flow(WireEvent::Dtmf { digit, at }).await
            }
            Inbound::Mark { name } => self.mark(&name).await,
            Inbound::Stop => {
                if !self.stream_ended_on_purpose() {
                    self.deliver(WireEvent::Hangup).await;
                    self.finish_input();
                }
                Flow::Drain
            }
        }
    }

    /// Caller audio: base64 mu-law to PCM16 at 8 kHz.
    async fn media(&mut self, chunk: Option<u64>, payload: &str) -> Flow {
        if let Some(chunk) = chunk {
            if let Some(last) = self.last_chunk
                && chunk > last.saturating_add(1)
            {
                let lost = chunk - last - 1;
                Counters::add(&self.conn.counters().inbound_chunks_lost, lost);
                tracing::debug!(call_id = %self.conn.call_id, lost, "twilio dropped inbound audio");
            }
            self.last_chunk = Some(chunk);
        }
        let max_mulaw = self.conn.shared.config.max_audio_bytes / AudioFrame::BYTES_PER_SAMPLE;
        // Decoded size of the base64 text, an upper bound.
        if payload.len() / 4 * 3 > max_mulaw.saturating_add(3) {
            return self
                .fault(format!("media payload exceeds {max_mulaw} bytes"))
                .await;
        }
        if payload.is_empty() {
            return Flow::Continue;
        }
        let needed = base64::decoded_len_estimate(payload.len());
        if self.mulaw.len() < needed {
            self.mulaw.resize(needed, 0);
        }
        let Ok(count) = BASE64.decode_slice(payload.as_bytes(), &mut self.mulaw) else {
            return self
                .fault("media payload is not valid base64".to_owned())
                .await;
        };
        let Some(mulaw) = self.mulaw.get(..count) else {
            return Flow::Continue;
        };
        if mulaw.is_empty() {
            return Flow::Continue;
        }
        let len = Codec::Ulaw.decoded_len(mulaw.len());
        let decoded = scratch_bytes(&mut self.scratch, len, |out| {
            Codec::Ulaw.decode(mulaw, out).is_ok()
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
        let Some(events) = self.events.as_ref() else {
            return Flow::Drain;
        };
        match events.send(WireEvent::Audio(frame)).await {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Drain,
        }
    }

    /// Twilio echoes a mark: played, or flushed by our `clear`.
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
        let Some(events) = self.events.as_ref() else {
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
        tracing::debug!(call_id = %self.conn.call_id, reason, "twilio call closed by protocol fault");
        // Only the first request matters; a full queue means one is already pending.
        let _ = self.ctrl.try_send(Ctrl::Close { close, reason });
        Counters::add(&self.conn.counters().faults, 1);
        self.deliver(WireEvent::Fault {
            message: reason.to_owned(),
        })
        .await;
        self.finish_input();
    }

    /// Whether the host announced the end of this stream. Then the session is parked for the
    /// call's next stream: the engine is told nothing and the writer is asked to close the
    /// socket (the stream is over, and the next one comes on a new socket).
    fn stream_ended_on_purpose(&mut self) -> bool {
        if !self.conn.ended_on_purpose() {
            return false;
        }
        let _ = self.ctrl.try_send(Ctrl::Close {
            close: CloseCode::Normal,
            reason: "stream restarted",
        });
        true
    }

    /// Stops feeding the core: dropping the queue's sender is how the core learns Twilio is
    /// gone (a hang-up queued before stays readable).
    fn finish_input(&mut self) {
        *self.events = None;
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
    /// Buffer the mu-law encoder writes into (see [`scratch_bytes`]).
    scratch: BytesMut,
    /// Buffer the JSON messages are built in; one allocation serves many messages.
    messages: BytesMut,
    /// The engine ended the call or dropped its end (the call is over, whatever the socket did).
    engine_done: bool,
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
    /// Twilio did not take the data within the write timeout.
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

/// The next output item: what was held for this stream first, then the engine's queue.
async fn next_out(
    held: &mut VecDeque<WireOut>,
    outgoing: &mut channel::Receiver<WireOut>,
) -> Option<WireOut> {
    match held.pop_front() {
        Some(out) => Some(out),
        None => outgoing.recv().await,
    }
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
                tracing::warn!(call_id = %conn.call_id, "twilio does not take output, closing the call");
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

    /// Closing the socket is the hang-up: Twilio ends the stream (and continues the TwiML after
    /// `<Connect>`). Returns whether the socket still answers.
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

    /// Writes until the socket is done. Returns whether the engine ended the call.
    async fn run(
        mut self,
        outgoing: &mut channel::Receiver<WireOut>,
        held: &mut VecDeque<WireOut>,
        ctrl: mpsc::Receiver<Ctrl>,
    ) -> bool {
        let conn = self.conn;
        let departure = self.serve_loop(outgoing, held, ctrl).await;
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
        self.engine_done
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
        held: &mut VecDeque<WireOut>,
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
                out = next_out(held, outgoing) => Step::Out(out),
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
                Step::Ended | Step::Out(None | Some(WireOut::Hangup)) => {
                    self.engine_done = true;
                    return Departure::Farewell {
                        reason: "call ended",
                        close: CloseCode::Normal,
                    };
                }
                Step::GraceOver => {
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

    /// Barge-in: forget the marks in flight (Twilio flushes and echoes them; those echoes are
    /// dropped), then tell Twilio to empty its buffer.
    async fn clear(&mut self, epoch: TurnEpoch) -> Result<(), WriteFailure> {
        self.min_epoch = self.min_epoch.max(epoch);
        let cut = self.conn.book().clear();
        let counters = self.conn.counters();
        Counters::add(&counters.marks_cut, u64::try_from(cut).unwrap_or(u64::MAX));
        Counters::add(&counters.clears_sent, 1);
        let message = Utf8Bytes::from(self.conn.outbound.clear());
        self.send(Message::Text(message)).await
    }

    /// An engine mark: Twilio echoes it when the audio before it has played.
    async fn engine_mark(&mut self, id: String) -> Result<(), WriteFailure> {
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
        let message = Utf8Bytes::from(self.conn.outbound.mark(name));
        self.send(Message::Text(message)).await
    }

    /// A progress mark when enough audio went out since the last mark.
    async fn progress_if_due(&mut self) -> Result<(), WriteFailure> {
        let interval = self.conn.twilio.config.mark_interval;
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

    /// Writes one frame of bot audio: PCM16 8 kHz to mu-law, in `chunk_bytes` pieces.
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
                    "bot audio is {} but a twilio call is 8000 Hz; mismatched audio is dropped",
                    frame.sample_rate()
                ));
            }
            return Ok(());
        }
        let len = Codec::Ulaw.encoded_len(frame.data().len());
        let source = frame.data();
        let encoded = scratch_bytes(&mut self.scratch, len, |out| {
            Codec::Ulaw.encode(source, out).is_ok()
        });
        let Some(mut mulaw) = encoded else {
            return Ok(());
        };
        let chunk = self.conn.twilio.config.chunk_bytes.max(1);
        while !mulaw.is_empty() {
            let take = mulaw.len().min(chunk);
            let part = mulaw.split_to(take);
            if self.messages.capacity() < 2 * 1024 {
                self.messages.reserve(MESSAGE_CHUNK);
            }
            self.conn.outbound.write_media(&part, &mut self.messages);
            let text = Utf8Bytes::try_from(self.messages.split().freeze());
            let Ok(text) = text else {
                continue;
            };
            self.send(Message::Text(text)).await?;
            let counters = self.conn.counters();
            Counters::add(&counters.media_out, 1);
            Counters::add(&counters.media_out_bytes, take as u64);
            let unplayed = {
                let mut book = self.conn.book();
                book.audio_sent(take);
                book.unplayed_bytes()
            };
            Counters::max(&counters.max_unplayed_ms, unplayed / BYTES_PER_MS);
            self.progress_if_due().await?;
        }
        Ok(())
    }
}
