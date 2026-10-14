//! The task of one SIP call: RTP in and out, the dialog's events, and the engine's channel.
//!
//! See the module docs of [`super`] for the behaviour; this file is the implementation. The loop
//! has one timer that is re-armed every iteration for the earliest of: the next send slot, the
//! reorder buffer's deadline and the RTP silence deadline. All time comes from the injected
//! clock.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use bytes::{Bytes, BytesMut};
use rsipstack::dialog::dialog::{DialogState, DialogStateReceiver, TransactionHandle};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::{Header, HeadersExt, Method, Request, StatusCode};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::rtp::{
    DtmfDedupe, MAX_DATAGRAM, PTIME, Released, ReorderBuffer, RtpHeader, RtpSender,
    SAMPLES_PER_PACKET, TelephoneEvent, event_from_digit,
};
use super::sdp::{AudioParams, G711, Offer, SdpError, Security};
use super::srtp::{Remote, SrtpError, SrtpPolicy, SrtpSession};
use super::{
    DtmfRequest, DtmfSlot, SEND_QUEUE_PACKETS, SecureDialogGuard, Shared, Signaling, shared_timeout,
};
use crate::call::{CallEnds, WireEvent, WireOut};
use crate::channel;
use crate::event::CallInfo;

/// Initial capacity of the send queue (it grows to [`SEND_QUEUE_PACKETS`] at most).
const QUEUE_PREALLOC: usize = 64;

/// How long the old SSRC must have been silent before a new one is taken over.
const SSRC_SWITCH_AFTER: Duration = Duration::from_millis(500);

/// Size of the chunk the receive buffer is topped up with.
const RX_CHUNK: usize = 16 * 1024;

/// Smallest chunk the decode and encode scratch buffers grow by.
const SCRATCH_CHUNK: usize = 8 * 1024;

/// Largest silence frame served from the shared static buffer (one 20 ms packet is 320 bytes).
const SILENCE_MAX: usize = 4096;

/// Queue of DTMF requests per call.
const DTMF_QUEUE: usize = 8;

/// Volume of the DTMF events we send, in -dBm0 (RFC 4733 section 2.3.2 suggests around -10).
const DTMF_VOLUME: u8 = 10;

/// The end packet of a telephone-event is sent this many times (RFC 4733 section 2.5.1.4).
const DTMF_END_REPEATS: usize = 3;

/// Shortest and longest DTMF tone.
const DTMF_MIN: Duration = Duration::from_millis(40);
const DTMF_MAX: Duration = Duration::from_secs(5);

/// Which side placed the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    /// The peer called us.
    Inbound,
    /// We called the peer.
    Outbound,
}

/// What the task of a call is built from.
pub(super) struct CallMedia {
    pub(super) shared: Arc<Shared>,
    pub(super) permit: OwnedSemaphorePermit,
    pub(super) call_id: String,
    pub(super) socket: UdpSocket,
    pub(super) audio: AudioParams,
    pub(super) dialog: InviteDialog,
    pub(super) states: DialogStateReceiver,
    pub(super) ends: CallEnds,
    pub(super) local_rtp: SocketAddr,
    pub(super) session_id: u64,
    /// The call's SRTP state; `None` for plain RTP.
    pub(super) srtp: Option<SrtpSession>,
    /// The signaling transport of the dialog, taken from the real connection at setup.
    pub(super) signaling: Signaling,
    /// Registers a TLS dialog for the in-dialog checks; `None` for UDP and TCP dialogs.
    pub(super) secure_guard: Option<SecureDialogGuard>,
}

/// Builds the metadata of [`WireEvent::Started`].
pub(super) fn call_info(
    call_id: &str,
    origin: Origin,
    audio: &AudioParams,
    extra: BTreeMap<String, String>,
) -> CallInfo {
    let mut metadata = extra;
    metadata.insert(
        CallInfo::SAMPLE_RATE_KEY.to_owned(),
        SampleRate::Hz8000.hz().to_string(),
    );
    metadata.insert("transport".to_owned(), "sip".to_owned());
    metadata.insert(
        "encoding".to_owned(),
        match audio.codec {
            super::sdp::G711::Pcmu => "mulaw",
            super::sdp::G711::Pcma => "alaw",
        }
        .to_owned(),
    );
    metadata.insert("codec".to_owned(), audio.codec.name().to_owned());
    metadata.insert(
        "direction".to_owned(),
        match origin {
            Origin::Inbound => "inbound",
            Origin::Outbound => "outbound",
        }
        .to_owned(),
    );
    metadata.insert(
        "dtmf".to_owned(),
        if audio.dtmf_payload_type.is_some() {
            "rfc4733"
        } else {
            "none"
        }
        .to_owned(),
    );
    if let Some(remote) = audio.remote {
        metadata.insert("rtp_remote".to_owned(), remote.to_string());
    }
    CallInfo {
        call_id: call_id.to_owned(),
        metadata,
    }
}

/// The DTMF key an INFO request carries: `application/dtmf-relay` (`Signal=5`, `Duration=160`) or
/// `application/dtmf` (the bare key). The body formats are UNVERIFIED against a carrier.
pub(super) fn parse_info_dtmf(content_type: &str, body: &[u8]) -> Option<DtmfDigit> {
    let content_type = content_type.split(';').next().unwrap_or("").trim();
    let text = std::str::from_utf8(body).ok()?;
    let key = if content_type.eq_ignore_ascii_case("application/dtmf-relay") {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("signal")
                .then(|| value.trim())
        })?
    } else if content_type.eq_ignore_ascii_case("application/dtmf") {
        text.trim()
    } else {
        return None;
    };
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    DtmfDigit::try_from(c.to_ascii_uppercase()).ok()
}

/// Why the call task leaves its loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Departure {
    /// BYE from the peer, or the dialog ended otherwise.
    RemoteHangup,
    /// The engine hung up, or dropped its end of the call.
    EngineHangup,
    /// The server is shutting down.
    Shutdown,
    /// The peer sent no RTP for `rtp_timeout`.
    RtpTimeout,
    /// The engine took no event for `send_timeout`.
    Stalled,
    /// The RTP socket failed for good.
    SocketFailed,
    /// The dialog's event channel closed.
    DialogLost,
    /// More SRTP packets failed authentication within a second than `srtp_auth_failure_limit`
    /// allows: someone is guessing tags (the `_32` tag is 32 bits) or the keys are out of sync.
    SrtpFlood,
}

/// Counts the failed SRTP authentications of one call in one-second windows on the injected
/// clock.
#[derive(Debug, Default)]
struct AuthFailures {
    window_start: Option<Instant>,
    count: u32,
}

impl AuthFailures {
    /// Records one failure at `now`; true when the failures of the current second exceed `limit`
    /// (`0` means no limit).
    fn exceeds(&mut self, now: Instant, limit: u32) -> bool {
        let fresh = self
            .window_start
            .is_none_or(|start| now.saturating_duration_since(start) >= Duration::from_secs(1));
        if fresh {
            self.window_start = Some(now);
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        limit != 0 && self.count > limit
    }
}

/// An entry of the send queue.
enum Item {
    /// One packet of G.711 audio.
    Media(Bytes),
    /// One telephone-event packet.
    Event { event: TelephoneEvent, start: bool },
    /// A mark, resolved when the packets before it have been sent.
    Mark(String),
}

enum RxKind {
    Audio(G711),
    Event,
}

/// A received packet waiting in the reorder buffer.
struct RxPacket {
    kind: RxKind,
    timestamp: u32,
    payload: Bytes,
}

/// Removes the call's DTMF slot when the task ends.
struct Registration {
    shared: Arc<Shared>,
    call_id: String,
    owned: bool,
}

impl Registration {
    fn new(
        shared: &Arc<Shared>,
        call_id: &str,
        sender: mpsc::Sender<DtmfRequest>,
        supported: bool,
    ) -> Self {
        let mut slots = Shared::lock(&shared.dtmf);
        // First call with an id wins; a second call with the same Call-ID cannot send DTMF.
        let owned = !slots.contains_key(call_id);
        if owned {
            slots.insert(call_id.to_owned(), DtmfSlot { sender, supported });
        }
        Self {
            shared: Arc::clone(shared),
            call_id: call_id.to_owned(),
            owned,
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.owned {
            Shared::lock(&self.shared.dtmf).remove(&self.call_id);
        }
    }
}

/// Runs a call until it ends. Never panics and never touches another call.
pub(super) async fn run(media: CallMedia) {
    let CallMedia {
        shared,
        permit,
        call_id,
        socket,
        audio,
        dialog,
        mut states,
        ends,
        local_rtp,
        session_id,
        srtp,
        signaling,
        secure_guard,
    } = media;
    let _permit = permit;
    let (dtmf_tx, mut dtmf_rx) = mpsc::channel(DTMF_QUEUE);
    let _registration = Registration::new(
        &shared,
        &call_id,
        dtmf_tx,
        audio.dtmf_payload_type.is_some(),
    );
    let CallEnds {
        events,
        mut outgoing,
        ended,
    } = ends;
    let Ok(sender) = RtpSender::random() else {
        tracing::warn!(%call_id, "sip call: no randomness for the RTP stream, hanging up");
        let _ = dialog.hangup().await;
        shared.dialogs.remove_dialog(&dialog.id());
        return;
    };
    let now = shared.clock.now();
    let mut call = Call {
        reorder: ReorderBuffer::new(shared.config.reorder_depth, shared.config.reorder_wait),
        released: Vec::with_capacity(shared.config.reorder_depth + 2),
        dedupe: DtmfDedupe::default(),
        rx_ssrc: None,
        latch_until: shared
            .config
            .rtp_latch
            .then(|| deadline_after(now, shared.config.rtp_latch_window)),
        last_rx: now,
        last_audio_samples: SAMPLES_PER_PACKET,
        audio_seq: 0,
        decode_scratch: BytesMut::with_capacity(SCRATCH_CHUNK),
        sender,
        queue: VecDeque::with_capacity(QUEUE_PREALLOC),
        next_send: now,
        min_epoch: TurnEpoch::ZERO,
        encode_scratch: BytesMut::with_capacity(SCRATCH_CHUNK),
        packet_buf: Vec::with_capacity(MAX_DATAGRAM),
        rate_fault_reported: false,
        sdp_version: session_id,
        awaiting_ack_answer: None,
        srtp,
        signaling,
        _secure_guard: secure_guard,
        auth_failures: AuthFailures::default(),
        shared,
        call_id,
        dialog,
        audio,
        local_rtp,
        session_id,
        events,
        ended,
    };
    tracing::info!(call_id = %call.call_id, codec = call.audio.codec.name(), "sip call started");
    let departure = call
        .event_loop(&socket, &mut states, &mut outgoing, &mut dtmf_rx)
        .await;
    call.finish(departure).await;
    call.shared.dialogs.remove_dialog(&call.dialog.id());
    tracing::info!(call_id = %call.call_id, ?departure, "sip call ended");
}

/// The state of one running call.
struct Call {
    shared: Arc<Shared>,
    call_id: String,
    dialog: InviteDialog,
    audio: AudioParams,
    local_rtp: SocketAddr,
    session_id: u64,
    sdp_version: u64,
    /// CSeq of a body-less re-INVITE we answered with our offer: the peer's answer is in its ACK
    /// (RFC 3264 section 5, RFC 6337 section 3.1.2) and is applied when the dialog confirms.
    awaiting_ack_answer: Option<u32>,
    /// SRTP state of the call, `None` for plain RTP. The mode never changes during a call: a
    /// re-INVITE cannot turn security on or off.
    srtp: Option<SrtpSession>,
    /// The transport of the dialog, from the connection the call was set up on. Never read from
    /// a message: the `Via` of a request is written by its sender.
    signaling: Signaling,
    /// Keeps a TLS dialog registered while the call lives (dropped with the call).
    _secure_guard: Option<SecureDialogGuard>,
    /// Failed SRTP authentications in the current second.
    auth_failures: AuthFailures,
    events: channel::Sender<WireEvent>,
    ended: CancellationToken,
    // Receive side.
    reorder: ReorderBuffer<RxPacket>,
    released: Vec<Released<RxPacket>>,
    dedupe: DtmfDedupe,
    rx_ssrc: Option<u32>,
    /// While `Some(t)` and not yet past `t`, RTP from an address other than the SDP's may move
    /// the media destination (`rtp_latch`). Closed by the first valid packet from the SDP address
    /// and by the first latch.
    latch_until: Option<Instant>,
    last_rx: Instant,
    last_audio_samples: usize,
    audio_seq: u64,
    decode_scratch: BytesMut,
    // Send side.
    sender: RtpSender,
    queue: VecDeque<Item>,
    next_send: Instant,
    min_epoch: TurnEpoch,
    encode_scratch: BytesMut,
    packet_buf: Vec<u8>,
    rate_fault_reported: bool,
}

/// What woke the loop.
enum Step {
    Shutdown,
    Ended,
    State(Option<Box<DialogState>>),
    Timer,
    Out(Option<WireOut>),
    Dtmf(Option<DtmfRequest>),
    Packet(std::io::Result<(usize, SocketAddr)>),
}

impl Call {
    fn counters(&self) -> &super::Counters {
        &self.shared.counters
    }

    /// Whether the silence deadline applies: the peer is meant to send and to listen, and there
    /// is somewhere to receive from. A call on hold (the peer only sends, or nothing, or its
    /// address is 0.0.0.0) is legitimately silent for as long as the hold lasts.
    fn wants_audio_from_peer(&self) -> bool {
        self.audio.receive && self.audio.send && self.audio.remote.is_some()
    }

    /// The earliest moment something must happen without outside input.
    fn next_wake(&self, now: Instant) -> Instant {
        let mut wake = if self.wants_audio_from_peer() {
            deadline_after(self.last_rx, self.shared.config.rtp_timeout)
        } else {
            deadline_after(now, Duration::from_secs(86_400))
        };
        if let Some(deadline) = self.reorder.deadline() {
            wake = wake.min(deadline);
        }
        if matches!(
            self.queue.front(),
            Some(Item::Media(_) | Item::Event { .. })
        ) {
            wake = wake.min(self.next_send);
        }
        wake
    }

    async fn event_loop(
        &mut self,
        socket: &UdpSocket,
        states: &mut DialogStateReceiver,
        outgoing: &mut channel::Receiver<WireOut>,
        dtmf: &mut mpsc::Receiver<DtmfRequest>,
    ) -> Departure {
        let shared = Arc::clone(&self.shared);
        let ended = self.ended.clone();
        let shutdown = shared.cancel.cancelled();
        let ended_fut = ended.cancelled();
        let timer = shared
            .clock
            .sleep_until(deadline_after(shared.clock.now(), Duration::from_secs(1)));
        tokio::pin!(shutdown, ended_fut, timer);
        let mut rx_buf = BytesMut::with_capacity(RX_CHUNK);
        let mut dtmf_open = true;
        loop {
            let now = shared.clock.now();
            timer.as_mut().reset(self.next_wake(now));
            if rx_buf.capacity().saturating_sub(rx_buf.len()) < MAX_DATAGRAM {
                rx_buf.reserve(RX_CHUNK);
            }
            let step = tokio::select! {
                biased;
                () = &mut shutdown => Step::Shutdown,
                () = &mut ended_fut => Step::Ended,
                state = states.recv() => Step::State(state.map(Box::new)),
                () = &mut timer => Step::Timer,
                out = outgoing.recv() => Step::Out(out),
                request = dtmf.recv(), if dtmf_open => Step::Dtmf(request),
                received = socket.recv_buf_from(&mut rx_buf) => Step::Packet(received),
            };
            let result = match step {
                Step::Shutdown => Err(Departure::Shutdown),
                Step::Ended => Err(Departure::EngineHangup),
                Step::State(None) => Err(Departure::DialogLost),
                Step::State(Some(state)) => self.on_state(*state).await,
                Step::Timer => self.on_timer(socket).await,
                Step::Out(None | Some(WireOut::Hangup)) => Err(Departure::EngineHangup),
                Step::Out(Some(out)) => {
                    self.on_output(out);
                    self.pump(socket).await
                }
                Step::Dtmf(None) => {
                    dtmf_open = false;
                    Ok(())
                }
                Step::Dtmf(Some(request)) => {
                    self.enqueue_dtmf(request);
                    self.pump(socket).await
                }
                Step::Packet(Ok((_, from))) => {
                    let raw = rx_buf.split();
                    self.on_raw(raw, from).await
                }
                Step::Packet(Err(error)) => {
                    if is_icmp_noise(&error) {
                        Ok(())
                    } else {
                        tracing::warn!(call_id = %self.call_id, %error, "sip call: RTP socket failed");
                        Err(Departure::SocketFailed)
                    }
                }
            };
            if let Err(departure) = result {
                return departure;
            }
        }
    }

    // -- engine events ----------------------------------------------------------------------

    /// Hands one event to the engine, giving up after `send_timeout`, when the call is ended by
    /// the engine, or when the server shuts down.
    async fn deliver(&self, event: WireEvent) -> Result<(), Departure> {
        let limit = self.shared.config.send_timeout;
        tokio::select! {
            biased;
            () = self.shared.cancel.cancelled() => Err(Departure::Shutdown),
            () = self.ended.cancelled() => Err(Departure::EngineHangup),
            sent = shared_timeout(&*self.shared.clock, limit, self.events.send(event)) => match sent {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(Departure::EngineHangup),
                Err(_) => {
                    tracing::warn!(call_id = %self.call_id, "sip call: the engine takes no events, hanging up");
                    Err(Departure::Stalled)
                }
            },
        }
    }

    // -- receive ----------------------------------------------------------------------------

    /// A datagram off the RTP socket. On an SRTP call it is authenticated and decrypted first
    /// (a packet that fails changes nothing and is counted); the rest is the plain path.
    async fn on_raw(&mut self, mut raw: BytesMut, from: SocketAddr) -> Result<(), Departure> {
        if let Some(session) = self.srtp.as_mut() {
            match session.rx.unprotect(&mut raw) {
                Ok(len) => raw.truncate(len),
                Err(error) => {
                    let counters = &self.shared.counters;
                    counters.inc(&counters.srtp_packets_rejected);
                    if error == SrtpError::AuthFailed {
                        let now = self.shared.clock.now();
                        let limit = self.shared.config.srtp_auth_failure_limit;
                        if self.auth_failures.exceeds(now, limit) {
                            return Err(Departure::SrtpFlood);
                        }
                    }
                    return Ok(());
                }
            }
        }
        self.on_datagram(raw.freeze(), from).await
    }

    async fn on_datagram(&mut self, datagram: Bytes, from: SocketAddr) -> Result<(), Departure> {
        let shared = Arc::clone(&self.shared);
        let now = shared.clock.now();
        let counters = &shared.counters;
        if !self.audio.receive {
            return Ok(());
        }
        let (header, range) = match RtpHeader::parse(&datagram) {
            Ok(parsed) => parsed,
            Err(_) => {
                counters.inc(&counters.rtp_ignored);
                return Ok(());
            }
        };
        let kind = if let Some(codec) = self.audio.receive_codec(header.payload_type) {
            RxKind::Audio(codec)
        } else if Some(header.payload_type) == self.audio.dtmf_payload_type {
            RxKind::Event
        } else {
            counters.inc(&counters.rtp_ignored);
            return Ok(());
        };
        // Symmetric RTP: media comes from the address in the SDP. Another address may only move
        // the destination while the latch window is open, and only once the packet has passed
        // the stream checks below (a rejected packet changes nothing).
        let latching = self.audio.remote != Some(from);
        if latching && !self.latch_open(now) {
            counters.inc(&counters.rtp_foreign_source);
            return Ok(());
        }
        match self.rx_ssrc {
            None => self.rx_ssrc = Some(header.ssrc),
            Some(ssrc) if ssrc == header.ssrc => {}
            Some(_) => {
                if now.saturating_duration_since(self.last_rx) < SSRC_SWITCH_AFTER {
                    counters.inc(&counters.rtp_foreign_ssrc);
                    return Ok(());
                }
                // A new source (call transfer, media server change): start the stream over.
                self.rx_ssrc = Some(header.ssrc);
                self.restart_receive();
                self.drain_released().await?;
            }
        }
        self.last_rx = now;
        counters.inc(&counters.rtp_packets_in);
        let payload = datagram.slice(range);
        let packet = RxPacket {
            kind,
            timestamp: header.timestamp,
            payload,
        };
        let mut released = std::mem::take(&mut self.released);
        let before = self.reorder.stats();
        self.reorder
            .push(header.sequence, packet, now, &mut |r| released.push(r));
        let after = self.reorder.stats();
        let rejected =
            (after.late + after.duplicates).saturating_sub(before.late + before.duplicates);
        counters.add(&counters.rtp_late_or_duplicate, rejected);
        if rejected == 0 {
            if latching {
                // Follow the peer to where its media really comes from (NAT), once.
                self.audio.remote = Some(from);
            }
            self.latch_until = None;
        }
        self.released = released;
        self.drain_released().await
    }

    /// Whether RTP from an unexpected address may still move the media destination.
    fn latch_open(&self, now: Instant) -> bool {
        self.latch_until.is_some_and(|until| now <= until)
    }

    /// Forgets the receive stream's position: a new SSRC, a moved or resumed media stream. What
    /// the reorder buffer holds is released (not lost) and goes out at the next drain.
    fn restart_receive(&mut self) {
        self.dedupe.reset();
        let mut flushed = std::mem::take(&mut self.released);
        self.reorder.flush(&mut |r| flushed.push(r));
        self.released = flushed;
    }

    /// Delivers what the reorder buffer released.
    async fn drain_released(&mut self) -> Result<(), Departure> {
        let mut released = std::mem::take(&mut self.released);
        let mut result = Ok(());
        for item in released.drain(..) {
            if result.is_ok() {
                result = self.deliver_released(item).await;
            }
        }
        self.released = released;
        result
    }

    async fn deliver_released(&mut self, item: Released<RxPacket>) -> Result<(), Departure> {
        match item {
            Released::Packet(packet) => match packet.kind {
                RxKind::Audio(codec) => self.deliver_audio(codec, &packet.payload).await,
                RxKind::Event => self.deliver_event(&packet).await,
            },
            Released::Lost(count) => {
                let counters = self.counters();
                counters.add(&counters.rtp_lost, u64::from(count));
                if count > self.shared.config.max_conceal_packets {
                    return Ok(());
                }
                // Silence of the usual packet size keeps the engine's timeline without a hole.
                let samples = self.last_audio_samples;
                for _ in 0..count {
                    let silence = silence(samples * 2);
                    self.deliver_pcm(silence).await?;
                }
                Ok(())
            }
        }
    }

    async fn deliver_audio(&mut self, variant: G711, payload: &Bytes) -> Result<(), Departure> {
        let codec = variant.codec();
        let len = codec.decoded_len(payload.len());
        if len == 0 {
            return Ok(());
        }
        top_up(&mut self.decode_scratch, len);
        self.decode_scratch.resize(len, 0);
        if codec.decode(payload, self.decode_scratch.as_mut()).is_err() {
            self.decode_scratch.clear();
            return Ok(());
        }
        self.last_audio_samples = payload.len();
        let pcm = self.decode_scratch.split().freeze();
        self.deliver_pcm(pcm).await
    }

    async fn deliver_pcm(&mut self, pcm: Bytes) -> Result<(), Departure> {
        let seq = self.audio_seq;
        self.audio_seq += 1;
        let Ok(frame) = AudioFrame::new(
            pcm,
            SampleRate::Hz8000,
            self.shared.clock.now(),
            seq,
            TurnEpoch::ZERO,
        ) else {
            return Ok(());
        };
        self.deliver(WireEvent::Audio(frame)).await
    }

    async fn deliver_event(&mut self, packet: &RxPacket) -> Result<(), Departure> {
        let Some(event) = TelephoneEvent::parse(&packet.payload) else {
            return Ok(());
        };
        let Some(digit) = event.digit() else {
            return Ok(());
        };
        if !self.dedupe.is_new(packet.timestamp) {
            return Ok(());
        }
        self.report_dtmf(digit).await
    }

    async fn report_dtmf(&mut self, digit: DtmfDigit) -> Result<(), Departure> {
        let counters = self.counters();
        counters.inc(&counters.dtmf_in);
        let at = self.shared.clock.now();
        self.deliver(WireEvent::Dtmf { digit, at }).await
    }

    // -- send -------------------------------------------------------------------------------

    fn on_output(&mut self, out: WireOut) {
        match out {
            WireOut::Audio(frame) => self.enqueue_audio(&frame),
            WireOut::Clear { epoch } => self.clear(epoch),
            WireOut::Mark { id } => self.queue.push_back(Item::Mark(id)),
            WireOut::Hangup => {}
        }
    }

    fn drop_frame(&self) {
        let counters = self.counters();
        counters.inc(&counters.dropped_audio_frames);
    }

    fn enqueue_audio(&mut self, frame: &AudioFrame) {
        // Audio of an older epoch than the last clear never goes out.
        if frame.epoch().get() < self.min_epoch.get() {
            self.drop_frame();
            return;
        }
        if frame.sample_rate() != SampleRate::Hz8000 {
            // Resampling is DSP work for the audio layer, not for a transport task.
            if !self.rate_fault_reported {
                self.rate_fault_reported = true;
                tracing::warn!(call_id = %self.call_id, rate = %frame.sample_rate(), "sip call: bot audio is not 8 kHz, dropped");
            }
            self.drop_frame();
            return;
        }
        if !self.audio.send || self.audio.remote.is_none() {
            self.drop_frame();
            return;
        }
        let samples = frame.sample_count();
        if samples == 0 {
            return;
        }
        if self.queue.len() + samples.div_ceil(SAMPLES_PER_PACKET) > SEND_QUEUE_PACKETS {
            self.drop_frame();
            return;
        }
        let codec = self.audio.codec.codec();
        let encoded_len = codec.encoded_len(frame.data().len());
        top_up(&mut self.encode_scratch, encoded_len);
        self.encode_scratch.resize(encoded_len, 0);
        if codec
            .encode(frame.data(), self.encode_scratch.as_mut())
            .is_err()
        {
            self.encode_scratch.clear();
            self.drop_frame();
            return;
        }
        let mut encoded = self.encode_scratch.split().freeze();
        if self.queue.is_empty() {
            // An idle sender restarts its schedule now instead of catching up.
            let now = self.shared.clock.now();
            self.next_send = self.next_send.max(now);
        }
        while !encoded.is_empty() {
            let take = encoded.len().min(SAMPLES_PER_PACKET);
            self.queue.push_back(Item::Media(encoded.split_to(take)));
        }
    }

    /// Barge-in: drop the queued audio and marks (the core reports the marks as cut), keep a DTMF
    /// tone that is being sent, and start the next audio as a new talkspurt.
    fn clear(&mut self, epoch: TurnEpoch) {
        self.min_epoch = self.min_epoch.max(epoch);
        let mut dropped = 0u64;
        self.queue.retain(|item| match item {
            Item::Media(_) => {
                dropped += 1;
                false
            }
            Item::Mark(_) => false,
            Item::Event { .. } => true,
        });
        let counters = self.counters();
        counters.add(&counters.dropped_on_clear, dropped);
        self.sender.note_clear();
    }

    /// Counts a DTMF key that could not be sent ([`SipStats::dtmf_dropped`](super::SipStats)).
    fn drop_dtmf(&self, why: &'static str) {
        let counters = self.counters();
        counters.inc(&counters.dtmf_dropped);
        tracing::debug!(call_id = %self.call_id, why, "sip call: DTMF key dropped");
    }

    fn enqueue_dtmf(&mut self, request: DtmfRequest) {
        if !self.audio.send || self.audio.remote.is_none() {
            self.drop_dtmf("the call is on hold");
            return;
        }
        let duration = request.duration.clamp(DTMF_MIN, DTMF_MAX);
        let packets = u16::try_from(duration.as_millis().div_ceil(PTIME.as_millis())).unwrap_or(1);
        let packets = packets.max(1);
        let tone = usize::from(packets) - 1;
        if self.queue.len() + tone + DTMF_END_REPEATS > SEND_QUEUE_PACKETS {
            self.drop_dtmf("the send queue is full");
            return;
        }
        if self.queue.is_empty() {
            let now = self.shared.clock.now();
            self.next_send = self.next_send.max(now);
        }
        let counters = self.counters();
        counters.inc(&counters.dtmf_out);
        let code = event_from_digit(request.digit);
        let ticks_per_packet = u16::try_from(SAMPLES_PER_PACKET).unwrap_or(160);
        let mut first = true;
        for index in 1..packets {
            self.queue.push_back(Item::Event {
                event: TelephoneEvent {
                    event: code,
                    end: false,
                    volume: DTMF_VOLUME,
                    duration: ticks_per_packet.saturating_mul(index),
                },
                start: first,
            });
            first = false;
        }
        for _ in 0..DTMF_END_REPEATS {
            self.queue.push_back(Item::Event {
                event: TelephoneEvent {
                    event: code,
                    end: true,
                    volume: DTMF_VOLUME,
                    duration: ticks_per_packet.saturating_mul(packets),
                },
                start: first,
            });
            first = false;
        }
    }

    /// Sends the packet at the head of the queue when its slot has come, and resolves the marks
    /// in front of and behind it. One packet per call: a backlog is never sent as a burst.
    async fn pump(&mut self, socket: &UdpSocket) -> Result<(), Departure> {
        let mut sent = false;
        loop {
            match self.queue.front() {
                None => return Ok(()),
                Some(Item::Mark(_)) => {
                    if let Some(Item::Mark(id)) = self.queue.pop_front() {
                        self.deliver(WireEvent::MarkAck { id }).await?;
                    }
                }
                Some(_) => {
                    let now = self.shared.clock.now();
                    if sent || now < self.next_send {
                        return Ok(());
                    }
                    let Some(item) = self.queue.pop_front() else {
                        return Ok(());
                    };
                    self.transmit(socket, item, now).await;
                    sent = true;
                }
            }
        }
    }

    async fn transmit(&mut self, socket: &UdpSocket, item: Item, now: Instant) {
        let span = match item {
            Item::Media(payload) => {
                self.sender.audio_packet(
                    self.audio.payload_type,
                    &payload,
                    now,
                    &mut self.packet_buf,
                );
                Duration::from_micros(u64::try_from(payload.len()).unwrap_or(0) * 125)
            }
            Item::Event { event, start } => {
                let Some(payload_type) = self.audio.dtmf_payload_type else {
                    return;
                };
                self.sender
                    .event_packet(payload_type, event, start, now, &mut self.packet_buf);
                PTIME
            }
            Item::Mark(_) => return,
        };
        let counters = &self.shared.counters;
        let protected = match self.srtp.as_mut() {
            Some(session) => session.tx.protect(&mut self.packet_buf).is_ok(),
            None => true,
        };
        if !protected {
            counters.inc(&counters.rtp_send_errors);
        } else if let (true, Some(remote)) = (self.audio.send, self.audio.remote) {
            let limit = self.shared.config.send_timeout;
            let sent = shared_timeout(
                &*self.shared.clock,
                limit,
                socket.send_to(&self.packet_buf, remote),
            )
            .await;
            match sent {
                Ok(Ok(_)) => counters.inc(&counters.rtp_packets_out),
                // An ICMP "port unreachable" from an earlier packet; the peer may be starting.
                Ok(Err(error)) if is_icmp_noise(&error) => {}
                Ok(Err(_)) | Err(_) => counters.inc(&counters.rtp_send_errors),
            }
        }
        self.next_send = if now > self.next_send + PTIME {
            now + span
        } else {
            self.next_send + span
        };
    }

    async fn on_timer(&mut self, socket: &UdpSocket) -> Result<(), Departure> {
        let now = self.shared.clock.now();
        let mut released = std::mem::take(&mut self.released);
        self.reorder.poll(now, &mut |r| released.push(r));
        self.released = released;
        self.drain_released().await?;
        self.pump(socket).await?;
        if self.wants_audio_from_peer()
            && now >= deadline_after(self.last_rx, self.shared.config.rtp_timeout)
        {
            return Err(Departure::RtpTimeout);
        }
        Ok(())
    }

    // -- dialog events ----------------------------------------------------------------------

    async fn respond(
        &self,
        handle: &TransactionHandle,
        status: StatusCode,
        headers: Option<Vec<Header>>,
        body: Option<Vec<u8>>,
    ) {
        let limit = self.shared.config.hangup_timeout;
        let _ = shared_timeout(
            &*self.shared.clock,
            limit,
            handle.respond(status, headers, body),
        )
        .await;
    }

    async fn on_state(&mut self, state: DialogState) -> Result<(), Departure> {
        match state {
            DialogState::Terminated(..) => Err(Departure::RemoteHangup),
            DialogState::Info(_, request, handle) => self.on_info(&request, &handle).await,
            DialogState::Updated(_, request, handle) => self.on_reoffer(&request, &handle).await,
            DialogState::Options(_, _, handle) => {
                let allow = Header::Allow(rsipstack::sip::Allow::new(
                    "INVITE, ACK, CANCEL, BYE, OPTIONS, INFO, UPDATE",
                ));
                self.respond(&handle, StatusCode::OK, Some(vec![allow]), None)
                    .await;
                Ok(())
            }
            DialogState::Refer(_, _, handle)
            | DialogState::Notify(_, _, handle)
            | DialogState::Message(_, _, handle)
            | DialogState::Publish(_, _, handle) => {
                // REFER (transfer) is handled elsewhere; the rest is not used by a voice endpoint.
                self.respond(&handle, StatusCode::NotImplemented, None, None)
                    .await;
                Ok(())
            }
            DialogState::Confirmed(..) => self.on_ack_answer().await,
            _ => Ok(()),
        }
    }

    /// The dialog confirmed again: if that was the ACK to a body-less re-INVITE, its body is the
    /// peer's answer to the offer we put in the 200 (a moved media address, hold or resume).
    async fn on_ack_answer(&mut self) -> Result<(), Departure> {
        let Some(cseq) = self.awaiting_ack_answer else {
            return Ok(());
        };
        let Some(ack) = self.dialog.last_remote_ack() else {
            return Ok(());
        };
        // `Confirmed` is also notified after other requests; only the ACK of that INVITE counts.
        let acked = ack.cseq_header().ok().and_then(|c| c.seq().ok());
        if acked != Some(cseq) {
            return Ok(());
        }
        self.awaiting_ack_answer = None;
        if ack.body.is_empty() {
            return Ok(()); // no answer: the stream stays as it was
        }
        // The call's recorded transport and mode decide what is acceptable, and an SRTP call
        // only takes a secure answer; an ACK that came in off TLS on a TLS dialog has lost its
        // SDP already (`TlsOnlyAnswers`).
        match Offer::parse_secure(
            &ack.body,
            &self.shared.config.codecs,
            self.reoffer_security(),
        ) {
            Ok(answer) => {
                if !self.renegotiate(answer.srtp(), Remote::Answer) {
                    tracing::warn!(call_id = %self.call_id, "sip call: unusable SRTP answer in the ACK, media unchanged");
                    return Ok(());
                }
                // The same handling as an offer from the peer, but there is nothing to answer.
                let restart = self.adopt(answer.params());
                if restart {
                    self.rx_ssrc = None;
                    self.restart_receive();
                    self.drain_released().await?;
                }
            }
            Err(error) => {
                tracing::warn!(call_id = %self.call_id, %error, "sip call: unusable answer in the ACK, media unchanged");
            }
        }
        Ok(())
    }

    /// Takes over a stream the peer chose (its offer or its answer): the silence deadline starts
    /// over, whatever the hold was, and the latch window reopens. Returns whether the receive
    /// side must restart (a moved, held or resumed stream may come with a new SSRC and sequence
    /// space).
    fn adopt(&mut self, new: AudioParams) -> bool {
        let restart = new.remote != self.audio.remote
            || new.send != self.audio.send
            || new.receive != self.audio.receive
            || new.payload_type != self.audio.payload_type;
        self.audio = new;
        let now = self.shared.clock.now();
        self.last_rx = now;
        self.latch_until = self
            .shared
            .config
            .rtp_latch
            .then(|| deadline_after(now, self.shared.config.rtp_latch_window));
        restart
    }

    async fn on_info(
        &mut self,
        request: &Request,
        handle: &TransactionHandle,
    ) -> Result<(), Departure> {
        let content_type = request.headers.iter().find_map(|h| match h {
            Header::ContentType(c) => Some(c.value().to_owned()),
            _ => None,
        });
        let Some(content_type) = content_type else {
            self.respond(handle, StatusCode::UnsupportedMediaType, None, None)
                .await;
            return Ok(());
        };
        match parse_info_dtmf(&content_type, &request.body) {
            Some(digit) => {
                self.respond(handle, StatusCode::OK, None, None).await;
                self.report_dtmf(digit).await
            }
            None => {
                self.respond(handle, StatusCode::UnsupportedMediaType, None, None)
                    .await;
                Ok(())
            }
        }
    }

    /// A re-INVITE or UPDATE: hold and resume, a changed media address, a session refresh.
    async fn on_reoffer(
        &mut self,
        request: &Request,
        handle: &TransactionHandle,
    ) -> Result<(), Departure> {
        let sdp_type = || {
            vec![Header::ContentType(rsipstack::sip::ContentType::new(
                "application/sdp",
            ))]
        };
        if request.body.is_empty() {
            // No new offer: a re-INVITE puts ours in the 200, an UPDATE just succeeds.
            if request.method == Method::Invite {
                self.sdp_version += 1;
                self.awaiting_ack_answer = request.cseq_header().ok().and_then(|c| c.seq().ok());
                let answer = self.current_sdp();
                self.respond(
                    handle,
                    StatusCode::OK,
                    Some(sdp_type()),
                    Some(answer.into_bytes()),
                )
                .await;
            } else {
                self.respond(handle, StatusCode::OK, None, None).await;
            }
            return Ok(());
        }
        let security = self.reoffer_security();
        match Offer::parse_secure(&request.body, &self.shared.config.codecs, security) {
            Ok(offer) => {
                if !self.renegotiate(offer.srtp(), Remote::Offer) {
                    self.respond(handle, StatusCode::NotAcceptableHere, None, None)
                        .await;
                    return Ok(());
                }
                let restart = self.adopt(offer.params());
                self.sdp_version += 1;
                let local = self.srtp.as_ref().map(|session| &session.local);
                let answer =
                    offer.answer_with(self.local_rtp, self.session_id, self.sdp_version, local);
                self.respond(
                    handle,
                    StatusCode::OK,
                    Some(sdp_type()),
                    Some(answer.into_bytes()),
                )
                .await;
                if restart {
                    self.rx_ssrc = None;
                    self.restart_receive();
                    self.drain_released().await?;
                }
            }
            Err(SdpError::Malformed(_)) => {
                self.respond(handle, StatusCode::BadRequest, None, None)
                    .await;
            }
            Err(_) => {
                self.respond(handle, StatusCode::NotAcceptableHere, None, None)
                    .await;
            }
        }
        Ok(())
    }

    /// Our SDP for the current stream, for a re-INVITE that carried no offer.
    fn current_sdp(&self) -> String {
        super::sdp::describe(
            self.local_rtp,
            &self.audio,
            self.session_id,
            self.sdp_version,
            self.srtp.as_ref().map(|session| &session.local),
        )
    }

    /// What a re-offer, or the answer in an ACK, may use for media security. The call's mode is
    /// fixed at setup: an SRTP call takes only SRTP, and only on a dialog whose signaling is TLS.
    /// That is the transport recorded from the real connection when the call was set up, never
    /// the `Via` of the message (written by its sender); that an in-dialog request really came
    /// over TLS is checked before it gets here (`handle_other`, `TlsOnlyAnswers`). A plain call
    /// takes only plain RTP.
    fn reoffer_security(&self) -> Security {
        if self.srtp.is_none() {
            return Security::PLAIN;
        }
        Security {
            policy: SrtpPolicy::Required,
            secure_signaling: self.signaling.is_secure(),
        }
    }

    /// Applies the crypto line of a re-offer to the call's SRTP state. `false` when the call's
    /// mode and the line disagree (an SRTP call without a line cannot happen: the SDP parse
    /// already required one) or a key could not be made.
    fn renegotiate(&mut self, line: Option<&super::sdp::CryptoLine>, from: Remote) -> bool {
        match (self.srtp.as_mut(), line) {
            (None, None) => true,
            (Some(session), Some(line)) => session.renegotiate(line, from).is_ok(),
            _ => false,
        }
    }

    // -- the end ----------------------------------------------------------------------------

    /// Last events to the engine (when it is still listening) and the hang-up on the wire.
    async fn finish(&mut self, departure: Departure) {
        let counters = self.counters();
        match departure {
            Departure::RemoteHangup => {
                self.last_event(WireEvent::Hangup).await;
            }
            Departure::RtpTimeout => {
                counters.inc(&counters.rtp_timeouts);
                tracing::warn!(call_id = %self.call_id, "sip call: no RTP from the peer, hanging up");
                self.last_event(WireEvent::Fault {
                    message: "no RTP received from the peer".to_owned(),
                })
                .await;
                self.last_event(WireEvent::Hangup).await;
                self.bye().await;
            }
            Departure::SrtpFlood => {
                counters.inc(&counters.srtp_flood_hangups);
                tracing::warn!(
                    call_id = %self.call_id,
                    limit = self.shared.config.srtp_auth_failure_limit,
                    "sip call: too many SRTP authentication failures per second, hanging up"
                );
                self.last_event(WireEvent::Fault {
                    message: "SRTP authentication failures: the media is under attack or the keys are out of sync".to_owned(),
                })
                .await;
                self.last_event(WireEvent::Hangup).await;
                self.bye().await;
            }
            Departure::Shutdown
            | Departure::Stalled
            | Departure::SocketFailed
            | Departure::DialogLost => {
                self.last_event(WireEvent::Hangup).await;
                self.bye().await;
            }
            Departure::EngineHangup => self.bye().await,
        }
    }

    /// Tells the engine something at the end of the call; the engine may be gone, so the wait is
    /// bounded and ignored.
    async fn last_event(&self, event: WireEvent) {
        if self.ended.is_cancelled() {
            return;
        }
        let limit = self
            .shared
            .config
            .send_timeout
            .min(Duration::from_millis(500));
        let _ = shared_timeout(&*self.shared.clock, limit, self.events.send(event)).await;
    }

    /// BYE (confirmed dialog) or CANCEL (still early), bounded.
    async fn bye(&self) {
        if matches!(self.dialog.state(), DialogState::Terminated(..)) {
            return;
        }
        let limit = self.shared.config.hangup_timeout;
        match shared_timeout(&*self.shared.clock, limit, self.dialog.hangup()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(call_id = %self.call_id, %error, "sip call: hang-up failed");
            }
            Err(_) => {
                tracing::debug!(call_id = %self.call_id, "sip call: hang-up timed out");
            }
        }
    }
}

/// Makes sure `buf` has room for `need` more bytes. It only allocates when the spare capacity is
/// used up, and then a whole [`SCRATCH_CHUNK`] at once, so one allocation serves many frames
/// (reserving a chunk before every frame would reallocate each time while an earlier frame of
/// the same chunk is still alive in a queue).
fn top_up(buf: &mut BytesMut, need: usize) {
    if buf.capacity() - buf.len() < need {
        buf.reserve(need.max(SCRATCH_CHUNK));
    }
}

/// Zero bytes for concealment: a slice of one shared static buffer (no allocation) up to
/// [`SILENCE_MAX`] bytes, a fresh buffer beyond that (a packet larger than any real G.711 one).
fn silence(len: usize) -> Bytes {
    static ZEROS: [u8; SILENCE_MAX] = [0; SILENCE_MAX];
    match ZEROS.get(..len) {
        Some(zeros) => Bytes::from_static(zeros),
        None => Bytes::from(vec![0u8; len]),
    }
}

/// Errors that come from an ICMP "port unreachable" for an earlier datagram (Windows reports it
/// on the next receive as a connection reset). They say nothing about the socket's health.
fn is_icmp_noise(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset};
    matches!(error.kind(), ConnectionReset | ConnectionRefused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn auth_failures_are_counted_per_second_on_the_given_clock() {
        use brisvox_core::Clock as _;
        let start = brisvox_core::SimClock::new().now();
        let mut failures = AuthFailures::default();
        // 50 in one second are fine, the 51st is the flood; a new second starts over.
        for n in 0..50 {
            assert!(
                !failures.exceeds(start + Duration::from_millis(n * 10), 50),
                "{n}"
            );
        }
        assert!(failures.exceeds(start + Duration::from_millis(500), 50));
        let later = start + Duration::from_millis(1_600);
        assert!(!failures.exceeds(later, 50), "a new window starts over");
        assert_eq!(failures.count, 1);
        // Zero turns the limit off.
        for n in 0..1_000 {
            assert!(!failures.exceeds(later + Duration::from_millis(n % 900), 0));
        }
    }

    #[test]
    fn info_dtmf_bodies_are_read() {
        let relay = b"Signal=5\r\nDuration=160\r\n";
        assert_eq!(
            parse_info_dtmf("application/dtmf-relay", relay),
            Some(DtmfDigit::D5)
        );
        assert_eq!(
            parse_info_dtmf("application/dtmf-relay;charset=utf-8", b"signal = #\r\n"),
            Some(DtmfDigit::Pound)
        );
        assert_eq!(
            parse_info_dtmf("application/dtmf", b"*\r\n"),
            Some(DtmfDigit::Star)
        );
        assert_eq!(
            parse_info_dtmf("application/dtmf", b"a"),
            Some(DtmfDigit::A)
        );
    }

    #[test]
    fn other_info_bodies_are_not_dtmf() {
        assert_eq!(parse_info_dtmf("application/dtmf", b"55"), None);
        assert_eq!(parse_info_dtmf("application/dtmf", b""), None);
        assert_eq!(parse_info_dtmf("application/dtmf", b"x"), None);
        assert_eq!(
            parse_info_dtmf("application/dtmf-relay", b"Duration=160"),
            None
        );
        assert_eq!(parse_info_dtmf("text/plain", b"5"), None);
        assert_eq!(parse_info_dtmf("application/dtmf", &[0xff, 0xfe]), None);
    }

    #[test]
    fn call_info_describes_the_stream() {
        let audio = AudioParams {
            codec: super::super::sdp::G711::Pcma,
            payload_type: 8,
            dtmf_payload_type: Some(101),
            remote: Some("203.0.113.9:40000".parse().unwrap()),
            send: true,
            receive: true,
            alt: [None; super::super::sdp::MAX_ALT_FORMATS],
        };
        let info = call_info("abc", Origin::Inbound, &audio, BTreeMap::new());
        assert_eq!(info.call_id, "abc");
        assert_eq!(info.metadata["sample_rate"], "8000");
        assert_eq!(info.metadata["encoding"], "alaw");
        assert_eq!(info.metadata["direction"], "inbound");
        assert_eq!(info.metadata["dtmf"], "rfc4733");
        assert_eq!(info.metadata["transport"], "sip");
        let rates = info.sample_rate().unwrap().unwrap();
        assert_eq!(rates.input, SampleRate::Hz8000);
        assert_eq!(rates.output, SampleRate::Hz8000);
    }

    #[test]
    fn frames_that_are_still_alive_share_one_scratch_chunk() {
        // Twenty 320-byte frames fit one 8 KiB chunk. They are all kept alive (a queue, the
        // engine's buffers), which is when a `reserve` of a whole chunk before every frame would
        // allocate a fresh chunk for each of them.
        let mut scratch = BytesMut::with_capacity(SCRATCH_CHUNK);
        let mut frames = Vec::new();
        for _ in 0..20 {
            top_up(&mut scratch, 320);
            scratch.resize(320, 0);
            frames.push(scratch.split().freeze());
        }
        for pair in frames.windows(2) {
            assert_eq!(
                pair[1].as_ptr() as usize,
                pair[0].as_ptr() as usize + 320,
                "a frame was not carved from the same chunk as the one before it"
            );
        }
        // When the chunk is used up, the next one is a whole chunk again (not one per frame).
        for _ in 0..40 {
            top_up(&mut scratch, 320);
            scratch.resize(320, 0);
            frames.push(scratch.split().freeze());
        }
        let chunks = frames
            .windows(2)
            .filter(|p| p[1].as_ptr() as usize != p[0].as_ptr() as usize + 320)
            .count();
        assert!(chunks <= 3, "{chunks} chunk changes for 60 frames");
    }

    #[test]
    fn concealment_silence_shares_one_static_buffer() {
        let a = silence(320);
        let b = silence(320);
        assert_eq!(a.len(), 320);
        assert!(a.iter().all(|byte| *byte == 0));
        assert_eq!(a.as_ptr(), b.as_ptr(), "silence is allocated per frame");
        // Larger than the static buffer: still correct, just allocated.
        let big = silence(SILENCE_MAX + 1);
        assert_eq!(big.len(), SILENCE_MAX + 1);
        assert!(big.iter().all(|byte| *byte == 0));
    }
}
