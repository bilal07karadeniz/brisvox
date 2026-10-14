//! The SIP/RTP telephony transport: a SIP user agent that takes inbound calls from a
//! carrier or PBX and places outbound calls, with G.711 audio over RTP. Protocol facts and the
//! stack comparison: `docs/providers/sip.md` (fetched 2026-10-08).
//!
//! One [`SipServer`] owns the signaling endpoint (UDP, optionally TCP) and turns every call into a
//! [`CallLink`] for the engine, inbound through [`Transport::accept`] and
//! outbound through [`SipHandle::dial`]. Signaling (transactions, retransmission timers, dialogs,
//! digest authentication) is `rsipstack`; the media half is this module:
//!
//! - [`sdp`]: offer/answer for one `RTP/AVP` audio stream with PCMU/PCMA and RFC 4733
//!   `telephone-event`.
//! - [`rtp`]: header, telephone-event payload, a bounded reorder buffer and the sender state
//!   (sequence numbers, timestamps, marker bit).
//!
//! # One task per call
//! Each call runs one task that owns its UDP RTP socket, the reorder buffer, the paced send queue
//! and the SIP dialog's events. Its inputs are the RTP socket, the engine's [`WireOut`](crate::WireOut), DTMF
//! requests and the dialog; it ends on BYE (either side), the engine's hang-up, a silent RTP
//! stream, an engine that stops taking events, or server shutdown. A panic or error in one call
//! cannot touch another: every call has its own socket, queues and dialog.
//!
//! # Audio
//! Caller audio is G.711 at 8 kHz, decoded with [`brisvox_audio::Codec`] to PCM16 and delivered at
//! [`SampleRate::Hz8000`](brisvox_core::frame::SampleRate::Hz8000) (call metadata
//! `sample_rate=8000`, `encoding=mulaw|alaw`); the engine resamples. Bot audio must arrive at
//! 8 kHz (like the Twilio adapter, nothing is resampled here: other rates are dropped, counted in
//! [`SipStats::dropped_audio_frames`] and logged once per call as a warning; no fault event goes
//! to the engine), is encoded with the negotiated codec and sent in 20 ms
//! packets. A lost packet becomes up to [`SipConfig::max_conceal_packets`] frames of silence, so
//! the engine's audio timeline has no hole.
//!
//! # Receive path
//! RTP is accepted only from the address in the peer's SDP (symmetric RTP, RFC 4961: we also send
//! from the socket we receive on), unless [`SipConfig::rtp_latch`] allows following the peer to
//! the address it really sends from. Latching moves the destination only for a packet that passed
//! the payload-type, SSRC and sequence checks, only within [`SipConfig::rtp_latch_window`] of the
//! call being up (or of a re-INVITE), and only once. Packets pass a
//! [`ReorderBuffer`](rtp::ReorderBuffer) of [`SipConfig::reorder_depth`] packets that adds no
//! delay to a clean stream and gives up a gap after [`SipConfig::reorder_wait`]. The SSRC is
//! locked to the first one seen; a new SSRC (a transfer, a media server change) is taken over
//! only after the old one was silent for half a second, or at once when a re-INVITE moved,
//! held or resumed the media. On an outbound call every G.711 format the answer lists is
//! accepted for receiving (RFC 3264 section 7), and the first one is sent. The RTP silence
//! timeout ([`SipConfig::rtp_timeout`]) does not run while the call is on hold (no media
//! address, or the peer does not listen) and starts over when the media resumes.
//!
//! # Send path, barge-in and marks
//! The engine's pacer already releases audio at real time; the call task keeps a bounded queue
//! (at most [`SEND_QUEUE_PACKETS`] packets) and sends one packet per its duration on an absolute
//! schedule, so a burst from the engine cannot become a burst on the wire. The task always reads
//! the engine's channel, so a [`WireOut::Clear`](crate::WireOut::Clear) is never stuck behind a backlog; the price is
//! that a frame that does not fit the queue is dropped and counted
//! ([`SipStats::dropped_audio_frames`]) instead of making the engine wait. RTP has no clear and
//! no mark echo:
//!
//! - [`WireOut::Clear`](crate::WireOut::Clear) drops every packet and mark still in the queue and marks the next packet
//!   as a new talkspurt. Audio already sent plays out at the far end (the carrier's jitter
//!   buffer, which brisvox cannot observe).
//! - A [`WireOut::Mark`](crate::WireOut::Mark) is resolved ([`WireEvent::MarkAck`](crate::WireEvent::MarkAck)) when the RTP packets before it were
//!   sent. This is an approximation: the caller has not heard them yet by the network delay plus
//!   the far end's jitter buffer. No [`WireEvent::Lead`](crate::WireEvent::Lead) is reported, so the engine keeps its
//!   fixed barge-in margin (`docs/providers/sip.md` section 4, item 6).
//!
//! # DTMF
//! RFC 4733 `telephone-event` packets (the payload type comes from the peer's SDP, never
//! hard-coded) become [`WireEvent::Dtmf`](crate::WireEvent::Dtmf) once per key press, de-duplicated by RTP timestamp
//! (the end packet is sent three times). SIP INFO with `application/dtmf-relay` or
//! `application/dtmf` is accepted too (its body format is UNVERIFIED against a carrier: the
//! Cisco-style `Signal=5` and the bare-digit form are both read). Sending DTMF is
//! [`SipHandle::send_dtmf`]: the engine's `TransportCommand` has no DTMF command yet, so
//! it is a side door keyed by the call id (`CallInfo::call_id`, the SIP Call-ID). A key the
//! call cannot send (on hold, or the send queue is full) is dropped without an error to the
//! caller and counted ([`SipStats::dtmf_dropped`]).
//!
//! # Trust and limits
//! An open SIP port is scanned within minutes: set [`SipConfig::allowed_sources`] to the
//! carrier's signaling ranges (default-deny for everything else is enforced before any message is
//! parsed; it covers the answers to our own outbound calls too, so list the networks of the
//! hosts you dial as well) and/or [`SipConfig::inbound_auth`] for digest challenges. Over
//! [`SipConfig::max_calls`] or with a full accept queue an INVITE gets `503` with `Retry-After`;
//! so does one the host's admission gate refuses ([`SipServer::bind_gated`]; without
//! `Retry-After` when the host is draining).
//! An INVITE that requires an extension (`Require`) gets `420`. A datagram the stack cannot make
//! a transaction of (not SIP, or no Via/From tag/Call-ID/CSeq) is dropped without an answer
//! (RFC 3261 section 18.3 allows it); a request that parses and is unacceptable gets a 4xx.
//! Refusals send their answer and let go: the stack absorbs the ACK, so a flood of refused
//! INVITEs holds no handler slot. Every signaling send is bounded by
//! [`SipConfig::hangup_timeout`] and every in-dialog exchange by [`SipConfig::ack_timeout`], so a
//! TCP peer that stops reading cannot keep a slot for good.
//!
//! Credentials ([`Secret`](brisvox_core::Secret)) never reach logs, errors or `Debug` output of
//! this module, and neither do the values of [`DialRequest::headers`] in `Debug`. `rsipstack`
//! itself logs whole SIP messages at `info` level, and those carry the digest responses and any
//! extra header you sent (an `X-Telnyx-Token`): run with `rsipstack=warn` (for example
//! `RUST_LOG=info,rsipstack=warn`) in production. A library cannot set that filter for the
//! host subscriber (`brisvox_core::telemetry::sip_stack_log_allowed` is the filter; the server
//! binary applies it). On a secure call those messages carry the SDES SRTP master keys.
//!
//! # SIPS and SRTP
//! [`SipConfig::tls`] adds a TLS listener (`tls.rs`: TLS 1.2 and 1.3, `ring`; the accept loop,
//! the source ACL, connection limits and the handshake deadline are ours, the framing,
//! transactions and dialogs after the handshake are the stack's) and [`SipConfig::srtp`] the
//! media security policy: SDES-SRTP (RFC 4568) with `AES_CM_128_HMAC_SHA1_80` / `_32` (RFC 3711,
//! `srtp.rs`). Keys are only taken from, and only answered on, TLS signaling. The negotiation is
//! in `sdp.rs` ([`sdp::Security`], [`sdp::CryptoLine`]), the per-call state is
//! [`srtp::SrtpSession`] (`call.rs` protects every outgoing packet and authenticates, replay-checks
//! and decrypts every incoming one before the plain RTP path sees it). `docs/TELEPHONY.md`
//! section 8 has the design, the carriers checked and what is left.
//!
//! # Bounds
//! Every channel of this module is bounded, with one exception that comes from the stack:
//! `rsipstack` dialog-state channel is unbounded. It is bounded in practice: every state it
//! carries is made by a request that holds one of at most 512 handler slots until the call task
//! answers it, so the queue holds at most 512 entries plus the few lifecycle states of a call.
//!
//! # Not supported yet
//! DTLS-SRTP, AEAD and 256-bit SRTP suites, MKI, SRTCP and RTCP (none is sent; RTCP from the peer is
//! ignored), TLS certificate hot reload and an idle timeout for TLS connections, session timers (RFC 4028), early media on outbound calls, REFER (transfer; answered
//! `501`), an initial INVITE without an offer (delayed offer; answered `488`; a re-INVITE without an offer is
//! served: our offer goes in the 200 and the answer in the ACK is applied), REGISTER (this is a trunk
//! endpoint: carriers such as Twilio say not to register), DNS SRV lookup for outbound targets,
//! IPv6 media is untested. SIP transaction timers run on `rsipstack`'s own tokio timers, not on the
//! injected clock; everything in this module (timeouts, pacing, RTP timeout) uses the clock.

mod auth;
mod call;
mod inbound;
mod outbound;
pub mod rtp;
pub mod sdp;
pub mod srtp;
mod tls;
mod tls_relay;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_core::frame::DtmfDigit;
use rsipstack::EndpointBuilder;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::transport::{TransportLayer, udp::UdpConnection};
use tokio::net::UdpSocket;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::admission::SharedGate;
use crate::call::CallLink;
use crate::error::{Error, Result};
use crate::transport::Transport;

pub use crate::cidr::Cidr;
pub use auth::InboundAuth;
pub use outbound::{DialCredential, DialError, DialRequest};
pub use sdp::G711;
pub use srtp::SrtpPolicy;
pub use tls::SipTlsConfig;

/// Most RTP packets (20 ms each) a call queues for sending, audio and DTMF together. A frame (or
/// tone) that would not fit is dropped and counted; the engine's output channel is always read,
/// so a clear is never stuck behind a backlog.
pub const SEND_QUEUE_PACKETS: usize = 500;

/// Settings of a [`SipServer`].
#[derive(Debug, Clone)]
pub struct SipConfig {
    /// Where to listen for SIP over UDP (and TCP with [`tcp`](Self::tcp)). Port 0 picks a free
    /// port ([`SipHandle::local_addr`] tells which). Default `0.0.0.0:5060`; an unspecified
    /// address needs [`public_ip`](Self::public_ip).
    pub bind_addr: SocketAddr,
    /// Also listen on TCP, on the UDP port. Inbound calls over TCP are served; outbound targets
    /// choose their transport with `;transport=tcp` in the URI.
    pub tcp: bool,
    /// The address written into SDP, Contact and Via when it differs from `bind_addr` (a public
    /// IP behind NAT or a container bridge). Required when `bind_addr` is unspecified: it is not
    /// auto-detected.
    pub public_ip: Option<IpAddr>,
    /// The inclusive range of UDP ports for RTP, one port per call. `None` lets the OS pick a
    /// port per call. Firewalls and NAT must forward the whole range.
    pub rtp_ports: Option<(u16, u16)>,
    /// The G.711 variants this endpoint accepts and, for outbound calls, offers (in this order).
    pub codecs: Vec<G711>,
    /// Source networks allowed to send SIP. Empty allows every source; otherwise a datagram (or
    /// a TCP connection) from anywhere else is dropped before it is parsed. This holds for
    /// everything the endpoint receives, so it also drops the answers (100, 180, 200) of a
    /// callee you [`dial`](SipHandle::dial) whose address is outside these networks: include
    /// the hosts you call.
    pub allowed_sources: Vec<Cidr>,
    /// Digest credentials required of inbound INVITEs; `None` accepts any INVITE from an allowed
    /// source.
    pub inbound_auth: Option<InboundAuth>,
    /// Calls at once, inbound and outbound together.
    pub max_calls: usize,
    /// Inbound calls waiting for the engine to [`accept`](Transport::accept) them; with a full
    /// queue the next INVITE is refused with 503.
    pub accept_queue: usize,
    /// The bound on handing one event to the engine; an engine that takes nothing for this long
    /// ends the call. Also the bound on one RTP datagram send.
    pub send_timeout: Duration,
    /// How long to wait for the ACK of our 200 before giving up on the call.
    pub ack_timeout: Duration,
    /// A call whose peer sends no RTP for this long is hung up. It does not run while the call is
    /// on hold (no media address, or the peer only sends) and restarts when the media resumes.
    pub rtp_timeout: Duration,
    /// The bound on the BYE or CANCEL exchange when we hang up.
    pub hangup_timeout: Duration,
    /// Packets the reorder buffer holds while it waits for a missing one.
    pub reorder_depth: usize,
    /// The longest a packet waits behind a gap.
    pub reorder_wait: Duration,
    /// Lost packets turned into silence frames per gap; longer gaps are not filled.
    pub max_conceal_packets: u16,
    /// Send `180 Ringing` before answering an inbound call.
    pub ringing: bool,
    /// How long an inbound call rings (with `ringing`) before it is answered; the caller may
    /// cancel meanwhile. Zero answers at once.
    pub ring_time: Duration,
    /// Follow the peer's media to the address its RTP really comes from (NAT). Weakens the
    /// check that RTP comes from the address in the SDP: enable only for trusted peers. Only a
    /// packet that passes the stream checks (payload type, SSRC lock, sequence) moves the
    /// destination, only within [`rtp_latch_window`](Self::rtp_latch_window) of the start of the
    /// call (or of a re-INVITE), and only once: the first valid packet from the SDP address, or
    /// the first latch, closes the window.
    pub rtp_latch: bool,
    /// How long after the call is up (or a re-INVITE changed the media) latching can happen.
    pub rtp_latch_window: Duration,
    /// The `User-Agent` header.
    pub user_agent: String,
    /// The SIPS listener (SIP over TLS). `None`: UDP (and TCP) only. The listener applies
    /// [`allowed_sources`](Self::allowed_sources), the digest rules and everything else that
    /// holds for the other transports.
    pub tls: Option<SipTlsConfig>,
    /// SRTP (SDES, RFC 4568) for the media: [`SrtpPolicy::Off`] (plain RTP only),
    /// [`SrtpPolicy::Optional`] (SRTP when the peer offers it over TLS, plain RTP otherwise) or
    /// [`SrtpPolicy::Required`] (a call without SRTP is refused with 488). Keys travel in the SDP,
    /// so SRTP is only ever negotiated on a TLS signaling connection; it needs [`tls`](Self::tls).
    pub srtp: SrtpPolicy,
    /// Most SRTP packets of one call that may fail authentication within one second before the
    /// call is hung up (logged once, counted in `SipStats::srtp_flood_hangups`). A genuine peer
    /// with the right key never fails; a burst means someone is guessing tags (the `_32` tag is
    /// only 32 bits) or the two ends disagree on the keys. `0` turns the limit off. The cost of
    /// the limit is that whoever can send UDP to the call's RTP port can end the call; the
    /// default of 50 is above the 50 packets per second of one stream of an honest peer that
    /// lost its key for a moment. `BRISVOX_SIP_SRTP_AUTH_FAIL_LIMIT`.
    pub srtp_auth_failure_limit: u32,
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 5060)),
            tcp: false,
            public_ip: None,
            rtp_ports: None,
            codecs: vec![G711::Pcmu, G711::Pcma],
            allowed_sources: Vec::new(),
            inbound_auth: None,
            max_calls: 256,
            accept_queue: 16,
            send_timeout: Duration::from_secs(2),
            ack_timeout: Duration::from_secs(32),
            rtp_timeout: Duration::from_secs(60),
            hangup_timeout: Duration::from_secs(5),
            reorder_depth: 3,
            reorder_wait: Duration::from_millis(60),
            max_conceal_packets: 3,
            ringing: false,
            ring_time: Duration::ZERO,
            rtp_latch: false,
            rtp_latch_window: Duration::from_secs(5),
            user_agent: "brisvox".to_owned(),
            tls: None,
            srtp: SrtpPolicy::Off,
            srtp_auth_failure_limit: 50,
        }
    }
}

impl SipConfig {
    /// Checks the configuration; [`SipServer::bind`] calls it.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken: an unspecified `bind_addr` without
    /// `public_ip`; a zero, inverted or even-port-less RTP port range; no or duplicate codecs; zero `max_calls`,
    /// `accept_queue` or timeouts; `reorder_depth` outside 1 to 64; invalid `inbound_auth`; an
    /// unusable `tls` (certificate and key); `srtp` other than off without `tls`.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if self.bind_addr.ip().is_unspecified() && self.public_ip.is_none() {
            return bad("sip bind_addr is unspecified: set public_ip (the address to advertise)");
        }
        if let Some((low, high)) = self.rtp_ports
            && (low == 0 || low > high)
        {
            return bad("sip rtp_ports must be a non-empty range starting above 0");
        }
        if let Some((low, high)) = self.rtp_ports
            && even_bounds(low, high).is_none()
        {
            return bad(
                "sip rtp_ports must contain at least one even port (odd ports are left for RTCP)",
            );
        }
        if self.codecs.is_empty() {
            return bad("sip codecs must not be empty");
        }
        // There are two G.711 variants: more than two entries cannot be distinct.
        let repeats = match self.codecs.as_slice() {
            [a, b] => a == b,
            [_] => false,
            _ => true,
        };
        if repeats {
            return bad("sip codecs must list PCMU and/or PCMA once each");
        }
        if self.max_calls == 0 || self.max_calls > 100_000 {
            return bad("sip max_calls must be between 1 and 100000");
        }
        if self.accept_queue == 0 {
            return bad("sip accept_queue must be at least 1");
        }
        for (timeout, name) in [
            (self.send_timeout, "send_timeout"),
            (self.ack_timeout, "ack_timeout"),
            (self.rtp_timeout, "rtp_timeout"),
            (self.hangup_timeout, "hangup_timeout"),
            (self.reorder_wait, "reorder_wait"),
            (self.rtp_latch_window, "rtp_latch_window"),
        ] {
            if timeout.is_zero() {
                return Err(Error::Config(format!("sip {name} must be positive")));
            }
        }
        if !(1..=64).contains(&self.reorder_depth) {
            return bad("sip reorder_depth must be between 1 and 64");
        }
        if let Some(auth) = &self.inbound_auth {
            auth.validate()?;
        }
        if let Some(tls) = &self.tls {
            tls.validate()?;
            if tls.bind_addr.port() != 0 && tls.bind_addr.port() == self.bind_addr.port() {
                // With `tcp` both would want the same TCP port; a SIPS port equal to the SIP
                // port is a mistake either way.
                return bad("sip tls bind_addr port must differ from the sip port");
            }
        }
        if self.srtp != SrtpPolicy::Off && self.tls.is_none() {
            return bad(
                "sip srtp needs the TLS listener (tls): the SDES keys must not travel in cleartext",
            );
        }
        Ok(())
    }

    /// The address advertised in SDP, Contact and Via.
    pub(crate) fn advertised_ip(&self) -> IpAddr {
        self.public_ip.unwrap_or_else(|| self.bind_addr.ip())
    }
}

macro_rules! counters {
    ($($(#[doc = $doc:literal])+ $name:ident),+ $(,)?) => {
        /// The live counters (relaxed atomics).
        #[derive(Debug, Default)]
        pub(crate) struct Counters {
            $(pub(crate) $name: AtomicU64,)+
        }

        /// A snapshot of the counters of a [`SipServer`], summed over every call since it started.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        #[non_exhaustive]
        pub struct SipStats {
            $($(#[doc = $doc])+ pub $name: u64,)+
        }

        impl Counters {
            fn snapshot(&self) -> SipStats {
                SipStats { $($name: self.$name.load(Ordering::Relaxed),)+ }
            }
        }
    };
}

counters! {
    /// Inbound calls that were answered and handed to the engine.
    calls_inbound,
    /// Outbound calls that were answered and handed to the caller of `dial`.
    calls_outbound,
    /// INVITEs refused with 503 (call limit, accept queue full, shutting down).
    rejected_busy,
    /// INVITEs refused as malformed or unacceptable (400, 415, 488, 481, 405).
    rejected_invalid,
    /// INVITEs refused or challenged for missing or wrong credentials (401, 403).
    rejected_unauthorized,
    /// Inbound calls that were cancelled by the caller before they were answered.
    cancelled_before_answer,
    /// Calls given up because the ACK of our 200 never came.
    ack_timeouts,
    /// Calls hung up because the peer sent no RTP for `rtp_timeout`.
    rtp_timeouts,
    /// RTP packets accepted from the peer (audio and telephone-event).
    rtp_packets_in,
    /// RTP packets sent (audio and telephone-event).
    rtp_packets_out,
    /// RTP datagrams from an address other than the peer's SDP address.
    rtp_foreign_source,
    /// RTP packets with an SSRC other than the one locked to.
    rtp_foreign_ssrc,
    /// Datagrams on the RTP port that are not RTP, or have an unexpected payload type.
    rtp_ignored,
    /// RTP packets given up on (never arrived).
    rtp_lost,
    /// RTP packets that arrived too late or twice.
    rtp_late_or_duplicate,
    /// RTP datagrams that could not be sent.
    rtp_send_errors,
    /// DTMF key presses reported to the engine (RFC 4733 and INFO).
    dtmf_in,
    /// DTMF key presses sent to the peer.
    dtmf_out,
    /// DTMF keys accepted by `send_dtmf` that were not sent: the call was on hold (no media
    /// destination, or not sending) or the send queue was full.
    dtmf_dropped,
    /// Packets dropped from the send queue by a clear.
    dropped_on_clear,
    /// Bot audio frames dropped for a wrong sample rate, a stale epoch, a muted direction or a
    /// full queue.
    dropped_audio_frames,
    /// TLS connections that finished the handshake and were handed to the stack.
    tls_connections,
    /// TLS connections closed before the handshake: source not allowed, or over a connection limit.
    tls_rejected,
    /// TLS handshakes that failed or timed out.
    tls_handshake_failures,
    /// Calls whose media is SRTP (inbound and outbound).
    srtp_calls,
    /// INVITEs refused with 488 because of the SRTP policy: SRTP required and not offered, SDES
    /// keys offered on a connection that is not TLS, or no acceptable crypto line.
    rejected_insecure,
    /// SRTP packets dropped: wrong tag, replayed, too old or malformed.
    srtp_packets_rejected,
    /// In-dialog requests refused (488 or 403) or stripped of their SDP because they did not
    /// arrive on the TLS connection family of the dialog they claim to belong to.
    insecure_dialog_requests,
    /// Calls hung up because the peer's SRTP packets failed authentication too often
    /// (`SipConfig::srtp_auth_failure_limit`).
    srtp_flood_hangups,
    /// TLS connections closed for being idle longer than `SipTlsConfig::idle_timeout`.
    tls_idle_closed,
}

impl Counters {
    pub(crate) fn add(&self, counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn inc(&self, counter: &AtomicU64) {
        self.add(counter, 1);
    }
}

/// A request to send a DTMF key to the peer, handled by the call's task.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DtmfRequest {
    pub(crate) digit: DtmfDigit,
    pub(crate) duration: Duration,
}

/// The queue of DTMF requests of one call and whether the peer can take them.
pub(crate) struct DtmfSlot {
    pub(crate) sender: mpsc::Sender<DtmfRequest>,
    pub(crate) supported: bool,
}

/// Why [`SipHandle::send_dtmf`] did not queue the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DtmfError {
    /// No active call has this id.
    #[error("no such call")]
    UnknownCall,
    /// The peer's SDP has no `telephone-event` payload type.
    #[error("the peer does not support RFC 4733 telephone-event")]
    Unsupported,
    /// The call's DTMF queue is full or the call is ending.
    #[error("the DTMF queue is full")]
    Busy,
}

/// Remembers dial keys so a retried request never places two calls.
#[derive(Debug, Default)]
pub(crate) struct DialKeys {
    order: VecDeque<String>,
}

impl DialKeys {
    const REMEMBER: usize = 1024;

    /// Records `key`; `false` when it was already recorded.
    pub(crate) fn insert(&mut self, key: &str) -> bool {
        if self.order.iter().any(|k| k == key) {
            return false;
        }
        if self.order.len() >= Self::REMEMBER {
            self.order.pop_front();
        }
        self.order.push_back(key.to_owned());
        true
    }

    /// Forgets `key` (the call was never placed).
    pub(crate) fn forget(&mut self, key: &str) {
        self.order.retain(|k| k != key);
    }
}

/// The lowest and highest even port of `low..=high` (odd ports are left for RTCP), or `None`
/// when the range holds no even port (for example `20001..=20001`, or `65535..=65535`).
fn even_bounds(low: u16, high: u16) -> Option<(u16, u16)> {
    let first = if low.is_multiple_of(2) {
        low
    } else {
        low.checked_add(1)?
    };
    let last = if high.is_multiple_of(2) {
        high
    } else {
        high.checked_sub(1)?
    };
    (first <= last).then_some((first, last))
}

/// Hands out RTP sockets from the configured port range.
pub(crate) struct PortAllocator {
    bind_ip: IpAddr,
    range: Option<(u16, u16)>,
    next: Mutex<u16>,
}

impl PortAllocator {
    fn new(bind_ip: IpAddr, range: Option<(u16, u16)>) -> Self {
        Self {
            bind_ip,
            range,
            next: Mutex::new(range.map_or(0, |(low, _)| low)),
        }
    }

    /// Binds a socket on the next free even port of the range (odd ports are left for RTCP,
    /// which this endpoint does not run), or on a port the OS picks without a range.
    pub(crate) async fn bind(&self) -> std::io::Result<UdpSocket> {
        let Some((low, high)) = self.range else {
            return UdpSocket::bind(SocketAddr::new(self.bind_ip, 0)).await;
        };
        // Only even ports are used; a range without one cannot be configured (`validate`), but a
        // range is never trusted here: no clamp (it panics on an inverted pair), only comparisons.
        let Some((first, last)) = even_bounds(low, high) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "RTP port range contains no even port",
            ));
        };
        let slots = usize::from(last - first) / 2 + 1;
        let mut candidate = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            // The cursor can sit past the last even port (or below the first): wrap to the first.
            let start = if *next < first || *next > last {
                first
            } else {
                *next
            };
            *next = start;
            start
        };
        let mut last_error = None;
        for _ in 0..slots {
            if candidate < first || candidate > last {
                candidate = first;
            }
            match UdpSocket::bind(SocketAddr::new(self.bind_ip, candidate)).await {
                Ok(socket) => {
                    let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
                    *next = candidate.saturating_add(2);
                    return Ok(socket);
                }
                Err(error) => last_error = Some(error),
            }
            candidate = candidate.saturating_add(2);
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "RTP port range is empty")
        }))
    }
}

/// What every task of a server shares.
pub(crate) struct Shared {
    pub(crate) config: SipConfig,
    pub(crate) clock: SharedClock,
    /// Cancelled by `shutdown`: no new calls, running calls hang up.
    pub(crate) cancel: CancellationToken,
    /// Cancelled last: stops the signaling endpoint.
    pub(crate) endpoint_cancel: CancellationToken,
    pub(crate) dialogs: Arc<DialogLayer>,
    /// The dialogs whose signaling runs on TLS (see [`SecureDialogs`]).
    pub(crate) secure_dialogs: Arc<SecureDialogs>,
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) calls_tx: mpsc::Sender<CallLink>,
    /// Call tasks and request handlers.
    pub(crate) tracker: TaskTracker,
    pub(crate) counters: Arc<Counters>,
    pub(crate) dtmf: Mutex<HashMap<String, DtmfSlot>>,
    pub(crate) ports: PortAllocator,
    pub(crate) dial_keys: Mutex<DialKeys>,
    pub(crate) nonces: Mutex<auth::Nonces>,
    pub(crate) sip_addr: SocketAddr,
    /// The bound SIPS address, when the TLS listener runs.
    pub(crate) tls_addr: Option<SocketAddr>,
    /// What outbound TLS connections and the idle watch of the SIPS listener share.
    pub(crate) tls_runtime: Option<Arc<tls::TlsRuntime>>,
    pub(crate) session_ids: AtomicU64,
    /// In-dialog requests being handled; bounds the tasks a flood of requests can create.
    pub(crate) handlers: Arc<Semaphore>,
    /// The host's admission gate, asked for every new inbound INVITE ([`SipServer::bind_gated`]).
    pub(crate) gate: Option<SharedGate>,
}

impl Shared {
    pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh SDP session id.
    pub(crate) fn next_session_id(&self) -> u64 {
        self.session_ids.fetch_add(1, Ordering::Relaxed)
    }

    /// The `host:port` of our Contact (SIP) for the given advertised address and signaling
    /// transport: TLS has its own port.
    pub(crate) fn contact_addr(&self, signaling: Signaling) -> SocketAddr {
        let port = match (signaling, self.tls_addr) {
            (Signaling::Tls, Some(tls)) => tls.port(),
            _ => self.sip_addr.port(),
        };
        SocketAddr::new(self.config.advertised_ip(), port)
    }

    /// Our Contact URI. A call set up over TCP or TLS names that transport in it
    /// (`;transport=tcp`, `;transport=tls`), or uses the `sips:` scheme when the request that
    /// started the dialog did (`sips`): the peer's in-dialog requests (BYE, re-INVITE, INFO) go
    /// to the Contact, and a TCP- or TLS-only trunk does not take them over UDP (RFC 3261
    /// section 12.1.1 and 8.1.1.8).
    pub(crate) fn contact_uri(
        &self,
        signaling: Signaling,
        sips: bool,
    ) -> Option<rsipstack::sip::Uri> {
        let addr = self.contact_addr(signaling);
        let text = match (signaling, sips) {
            (Signaling::Tls, true) => format!("sips:brisvox@{addr}"),
            (Signaling::Tls, false) => format!("sip:brisvox@{addr};transport=tls"),
            (Signaling::Tcp, _) => format!("sip:brisvox@{addr};transport=tcp"),
            (Signaling::Udp, _) => format!("sip:brisvox@{addr}"),
        };
        rsipstack::sip::Uri::try_from(text.as_str()).ok()
    }
}

/// The transport a dialog's signaling runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signaling {
    Udp,
    Tcp,
    Tls,
}

impl Signaling {
    pub(crate) fn of(transport: Option<&rsipstack::sip::Transport>) -> Self {
        match transport {
            Some(rsipstack::sip::Transport::Tcp) => Self::Tcp,
            Some(rsipstack::sip::Transport::Tls) => Self::Tls,
            _ => Self::Udp,
        }
    }

    /// Whether SDES keys may travel on this transport (RFC 4568 section 7.1: only with
    /// confidentiality and integrity for the signaling, that is TLS).
    pub(crate) const fn is_secure(self) -> bool {
        matches!(self, Self::Tls)
    }
}

/// The dialogs whose signaling runs on TLS, recorded at call setup from the real connection (the
/// `Via` of a message is written by whoever sent it and proves nothing about the connection).
///
/// An in-dialog request for one of these must arrive on a TLS connection too. Without that, an
/// attacker who knows the Call-ID and both tags (a spoofed UDP datagram from an allowed network
/// is enough) could write `Via: SIP/2.0/TLS` and have this endpoint treat a cleartext re-INVITE
/// as secure: our SDES key would go out in the clear answer and the media would follow the
/// attacker's `c=` line. Two checks use this set: `handle_other` (the request is refused with 488
/// or 403 before the dialog sees it) and [`TlsOnlyAnswers`] (the ACK of a re-INVITE, which the
/// stack hands straight to the re-INVITE's transaction, loses its SDP).
///
/// The set holds one entry per live TLS call, so it is bounded by `max_calls`.
#[derive(Debug, Default)]
pub(crate) struct SecureDialogs {
    ids: Mutex<HashSet<DialogId>>,
}

impl SecureDialogs {
    pub(crate) fn contains(&self, id: &DialogId) -> bool {
        Shared::lock(&self.ids).contains(id)
    }

    /// Whether `request` belongs to a registered dialog (the id as the receiving side of the
    /// request sees it).
    fn claims(&self, request: &rsipstack::sip::Request) -> bool {
        DialogId::try_from((
            request,
            rsipstack::transaction::key::TransactionRole::Server,
        ))
        .is_ok_and(|id| self.contains(&id))
    }
}

/// Keeps a dialog in [`SecureDialogs`] for as long as it lives.
#[derive(Debug)]
pub(crate) struct SecureDialogGuard {
    registry: Arc<SecureDialogs>,
    id: DialogId,
}

impl SecureDialogGuard {
    /// Registers `id`.
    pub(crate) fn new(registry: &Arc<SecureDialogs>, id: DialogId) -> Self {
        Shared::lock(&registry.ids).insert(id.clone());
        Self {
            registry: Arc::clone(registry),
            id,
        }
    }
}

impl Drop for SecureDialogGuard {
    fn drop(&mut self) {
        Shared::lock(&self.registry.ids).remove(&self.id);
    }
}

/// Whether a message came in over TLS, by the connection's own address type.
pub(crate) fn is_tls(addr: Option<&rsipstack::transport::SipAddr>) -> bool {
    addr.is_some_and(|addr| addr.r#type == Some(rsipstack::sip::Transport::Tls))
}

/// Takes the SDP out of an ACK that did not arrive over TLS on a TLS dialog. The ACK of a
/// re-INVITE is routed by the stack to the re-INVITE's own transaction without going through
/// `handle_other`, and the connection it came on is not passed on, so the one place that still
/// sees the transport of the sender is the message inspector.
#[derive(Debug)]
pub(crate) struct TlsOnlyAnswers {
    pub(crate) dialogs: Arc<SecureDialogs>,
    pub(crate) refused: Arc<Counters>,
}

impl rsipstack::transaction::endpoint::MessageInspector for TlsOnlyAnswers {
    fn before_send(
        &self,
        msg: rsipstack::sip::SipMessage,
        _dest: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        msg
    }

    fn after_received(
        &self,
        msg: rsipstack::sip::SipMessage,
        from: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        match msg {
            rsipstack::sip::SipMessage::Request(mut request)
                if request.method == rsipstack::sip::Method::Ack
                    && !request.body.is_empty()
                    && !is_tls(from)
                    && self.dialogs.claims(&request) =>
            {
                tracing::warn!("sip: an ACK with SDP on a TLS dialog arrived off TLS, SDP dropped");
                self.refused.inc(&self.refused.insecure_dialog_requests);
                request.body.clear();
                rsipstack::sip::SipMessage::Request(request)
            }
            other => other,
        }
    }
}

/// Most in-dialog requests handled at once (see [`Shared::handlers`]).
const MAX_HANDLERS: usize = 512;

/// A cheap, cloneable handle on a running [`SipServer`]: place calls, send DTMF, read counters.
#[derive(Clone)]
pub struct SipHandle {
    pub(crate) shared: Arc<Shared>,
}

impl std::fmt::Debug for SipHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipHandle")
            .field("sip_addr", &self.shared.sip_addr)
            .field("active_calls", &self.active_calls())
            .finish()
    }
}

impl SipHandle {
    /// The address the SIP socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.sip_addr
    }

    /// The address of the SIPS (TLS) listener, when [`SipConfig::tls`] is set (the port the OS
    /// picked for port 0).
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.shared.tls_addr
    }

    /// Calls in progress, inbound and outbound (from the INVITE to the end of the call task).
    pub fn active_calls(&self) -> usize {
        self.shared
            .config
            .max_calls
            .saturating_sub(self.shared.permits.available_permits())
    }

    /// The counters, summed over every call since the server started.
    pub fn stats(&self) -> SipStats {
        self.shared.counters.snapshot()
    }

    /// Sends a DTMF key to the peer of call `call_id` (`CallInfo::call_id`) as RFC 4733
    /// telephone-event packets lasting `duration` (clamped to 40 ms to 5 s), queued behind the
    /// audio already waiting to go out.
    ///
    /// # Errors
    /// [`DtmfError`] when the call is unknown, the peer has no telephone-event, or the queue is
    /// full.
    pub fn send_dtmf(
        &self,
        call_id: &str,
        digit: DtmfDigit,
        duration: Duration,
    ) -> Result<(), DtmfError> {
        let sender = {
            let slots = Shared::lock(&self.shared.dtmf);
            let slot = slots.get(call_id).ok_or(DtmfError::UnknownCall)?;
            if !slot.supported {
                return Err(DtmfError::Unsupported);
            }
            slot.sender.clone()
        };
        sender
            .try_send(DtmfRequest { digit, duration })
            .map_err(|_| DtmfError::Busy)
    }

    /// Places an outbound call and returns its [`CallLink`] once the callee answered (and the
    /// ACK was sent).
    ///
    /// See [`DialRequest`] for the target, credentials and timeouts. `cancel` abandons the call:
    /// before the answer a CANCEL is sent.
    ///
    /// # Errors
    /// [`DialError`]: invalid request, call limit, a duplicate dial key, the callee's rejection
    /// (with its SIP status), ring timeout, cancellation, an SDP answer that cannot be used, or an
    /// I/O failure.
    pub async fn dial(
        &self,
        request: DialRequest,
        cancel: &CancellationToken,
    ) -> Result<CallLink, DialError> {
        outbound::dial(&self.shared, request, cancel).await
    }
}

/// A SIP user agent that yields one call per accepted INVITE.
///
/// [`handle`](Self::handle) gives the cloneable [`SipHandle`] for dialing and DTMF while this
/// value is busy accepting. Dropping the server stops everything without waiting;
/// [`shutdown`](Self::shutdown) hangs up the running calls first.
pub struct SipServer {
    handle: SipHandle,
    calls: mpsc::Receiver<CallLink>,
    infra: TaskTracker,
}

impl Drop for SipServer {
    fn drop(&mut self) {
        // No BYE: the tasks stop at once. `shutdown` is the graceful way.
        self.handle.shared.cancel.cancel();
        self.handle.shared.endpoint_cancel.cancel();
    }
}

impl std::fmt::Debug for SipServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipServer")
            .field("handle", &self.handle)
            .finish()
    }
}

/// Attempts to find a port free for both UDP and TCP when the OS picks it (see `SipServer::bind`).
const PORT_ATTEMPTS: u32 = 64;
/// Jump between candidate ports: larger than the 100-port blocks Windows reserves.
const PORT_JUMP: u16 = 127;
/// First port of the IANA dynamic range the candidates stay in.
const DYNAMIC_FIRST: u16 = 49_152;

/// The next candidate port after `port` failed: `PORT_JUMP` further, wrapping inside the
/// dynamic range.
fn next_port(port: u16) -> u16 {
    let span = u32::from(u16::MAX - DYNAMIC_FIRST) + 1;
    let offset = u32::from(port.saturating_sub(DYNAMIC_FIRST));
    let next = (offset + u32::from(PORT_JUMP)) % span;
    DYNAMIC_FIRST.saturating_add(u16::try_from(next).unwrap_or(0))
}

impl SipServer {
    /// Binds the signaling socket and starts the endpoint. The server is a child of `parent`:
    /// cancelling it stops the server at once (no BYE is sent; use [`shutdown`](Self::shutdown)
    /// for a graceful stop).
    ///
    /// # Errors
    /// [`Error::Config`] for an invalid configuration, [`Error::Io`] or [`Error::Sip`] when the
    /// address cannot be bound.
    pub async fn bind(
        config: SipConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::bind_gated(config, clock, parent, None).await
    }

    /// [`bind`](Self::bind) with the host's admission gate ([`crate::admission`]): every new
    /// inbound INVITE asks `gate` first, together with the server's own call limit and accept
    /// queue. A refusal is answered `503` before any other work, with `Retry-After` when the
    /// host is busy and without it when the host is shutting down (the caller should try another
    /// instance). The ticket of an admitted INVITE is held through the answer and the ACK and
    /// travels in the call's [`CallLink`] ([`CallLink::take_ticket`]); an INVITE that does not
    /// become a call (refused later, cancelled, no ACK) drops it, which gives the reservation
    /// back. Outbound calls ([`SipHandle::dial`]) do not ask the gate.
    ///
    /// # Errors
    /// As [`bind`](Self::bind).
    pub async fn bind_gated(
        config: SipConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        config.validate()?;
        let cancel = parent.child_token();
        let endpoint_cancel = parent.child_token();
        let sip_error = |e: rsipstack::Error| Error::Sip(e.to_string());

        let transport_layer = TransportLayer::new(endpoint_cancel.child_token());
        let advertised = config.public_ip;
        // The advertised port is the bound one, known after the bind: create the socket first
        // without `external`, then, for a NAT address, once more with it.
        // With TCP on a port the OS picks, the same number must be free for TCP too. The OS hands
        // out ephemeral ports almost sequentially and Windows reserves blocks of 100 ports per
        // protocol (`netsh interface ipv4 show excludedportrange`), so retrying with the next
        // ephemeral port can stay inside a reserved block: after a failure, jump by
        // PORT_JUMP (> 100) inside the dynamic range and bind that port explicitly.
        let pick_port = config.tcp && config.bind_addr.port() == 0;
        let mut attempts = if pick_port { PORT_ATTEMPTS } else { 1 };
        let mut candidate = config.bind_addr;
        let (probe, sip_addr) = loop {
            let probe = match UdpConnection::create_connection(
                candidate,
                None,
                Some(endpoint_cancel.child_token()),
            )
            .await
            {
                Ok(probe) => probe,
                Err(error) if attempts > 1 && candidate.port() != 0 => {
                    tracing::debug!(%error, port = candidate.port(), "sip: UDP port refused, trying another");
                    attempts -= 1;
                    candidate.set_port(next_port(candidate.port()));
                    continue;
                }
                Err(error) => return Err(sip_error(error)),
            };
            let sip_addr = probe.get_addr().get_socketaddr().map_err(sip_error)?;
            if config.tcp {
                match tokio::net::TcpListener::bind(SocketAddr::new(
                    candidate.ip(),
                    sip_addr.port(),
                ))
                .await
                {
                    Ok(listener) => drop(listener),
                    Err(error) if attempts > 1 => {
                        tracing::debug!(%error, port = sip_addr.port(), "sip: TCP port refused, trying another");
                        attempts -= 1;
                        candidate.set_port(next_port(sip_addr.port()));
                        continue;
                    }
                    Err(error) => return Err(Error::Io(error)),
                }
            }
            break (probe, sip_addr);
        };
        // `sip_addr` is what rsipstack reports for the socket: for an unspecified bind address it
        // is the address of the first network interface, not the address bound. Sockets are
        // always (re)created on the configured bind address with the port that was chosen.
        let bound = SocketAddr::new(config.bind_addr.ip(), sip_addr.port());
        let udp = match advertised {
            Some(ip) if ip != sip_addr.ip() => {
                drop(probe);
                UdpConnection::create_connection(
                    bound,
                    Some(SocketAddr::new(ip, sip_addr.port())),
                    Some(endpoint_cancel.child_token()),
                )
                .await
                .map_err(sip_error)?
            }
            _ => probe,
        };
        transport_layer.add_transport(udp.into());
        if config.tcp {
            let tcp = rsipstack::transport::TcpListenerConnection::new(
                bound,
                advertised.map(|ip| SocketAddr::new(ip, sip_addr.port())),
            )
            .await
            .map_err(sip_error)?;
            transport_layer.add_transport(tcp.into());
        }
        // SIPS: the listener socket is bound here (a port in use is an error of `bind`); the
        // accept loop starts below, once the shared state exists.
        let tls_socket = match &config.tls {
            Some(tls) => {
                // The stack's TLS client (outbound calls) builds its config from the process
                // default crypto provider: make sure it is `ring`, like everywhere else here.
                // An error only says a provider was installed already.
                let _ = rustls::crypto::ring::default_provider().install_default();
                transport_layer.set_tls_config(tls.stack_config());
                let listener = tokio::net::TcpListener::bind(tls.bind_addr)
                    .await
                    .map_err(Error::Io)?;
                let bound = listener.local_addr().map_err(Error::Io)?;
                Some((listener, bound))
            }
            None => None,
        };
        if let Some((_, bound)) = &tls_socket {
            // The stack writes the `Via` of a request for a transport from the addresses of its
            // listeners: without a TLS entry an INVITE to a TLS callee would name UDP in its Via
            // and the answers would be routed to the UDP socket. Our accept loop is not one of
            // its listeners, so a placeholder carries the address (an in-memory channel nobody
            // writes to or reads from; the stack logs one warning at start that it is not a
            // listener it can serve).
            let (unused_tx, unused_rx) = mpsc::unbounded_channel();
            let advertised = SocketAddr::new(config.advertised_ip(), bound.port());
            let placeholder = rsipstack::transport::channel::ChannelConnection::create_connection(
                unused_rx,
                unused_tx,
                rsipstack::transport::SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Tls),
                    addr: advertised.into(),
                },
                Some(endpoint_cancel.child_token()),
            )
            .await
            .map_err(sip_error)?;
            transport_layer.add_transport(placeholder.into());
        }
        let tls_layer = tls_socket.as_ref().map(|_| transport_layer.clone());
        let (tls_runtime, tls_handoff) = match config.tls.as_ref() {
            Some(tls) if tls_socket.is_some() => {
                let (runtime, handoff) = tls::TlsRuntime::bind(tls).await?;
                (Some(runtime), Some(handoff))
            }
            _ => (None, None),
        };
        if !config.allowed_sources.is_empty() {
            let allowed = config.allowed_sources.clone();
            transport_layer.set_whitelist(move |ip: IpAddr| {
                let allowed = allowed.clone();
                async move { allowed.iter().any(|net| net.contains(ip)) }
            });
        }

        let secure_dialogs = Arc::new(SecureDialogs::default());
        let counters = Arc::new(Counters::default());
        let endpoint = EndpointBuilder::new()
            .with_user_agent(&config.user_agent)
            .with_inspector(Box::new(TlsOnlyAnswers {
                dialogs: Arc::clone(&secure_dialogs),
                refused: Arc::clone(&counters),
            }))
            .with_transport_layer(transport_layer)
            .with_cancel_token(endpoint_cancel.clone())
            .with_allows(vec![
                rsipstack::sip::Method::Invite,
                rsipstack::sip::Method::Ack,
                rsipstack::sip::Method::Cancel,
                rsipstack::sip::Method::Bye,
                rsipstack::sip::Method::Options,
                rsipstack::sip::Method::Info,
                rsipstack::sip::Method::Update,
            ])
            .build();
        let incoming = endpoint.incoming_transactions().map_err(sip_error)?;
        let dialogs = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        let (calls_tx, calls_rx) = mpsc::channel(config.accept_queue);
        let ports = PortAllocator::new(config.bind_addr.ip(), config.rtp_ports);
        let session_seed = clock
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_secs());
        let shared = Arc::new(Shared {
            permits: Arc::new(Semaphore::new(config.max_calls)),
            config,
            clock,
            cancel: cancel.clone(),
            endpoint_cancel: endpoint_cancel.clone(),
            dialogs,
            secure_dialogs,
            calls_tx,
            tracker: TaskTracker::new(),
            counters,
            dtmf: Mutex::new(HashMap::new()),
            ports,
            dial_keys: Mutex::new(DialKeys::default()),
            nonces: Mutex::new(auth::Nonces::default()),
            sip_addr,
            tls_addr: tls_socket.as_ref().map(|(_, bound)| *bound),
            tls_runtime: tls_runtime.clone(),
            session_ids: AtomicU64::new(session_seed),
            handlers: Arc::new(Semaphore::new(MAX_HANDLERS)),
            gate,
        });

        let infra = TaskTracker::new();
        let serving = endpoint.inner.clone();
        infra.spawn(async move {
            if let Err(error) = serving.serve().await {
                tracing::warn!(%error, "sip endpoint stopped");
            }
        });
        infra.spawn(inbound::dispatch(Arc::clone(&shared), incoming));
        if let (Some((listener, bound)), Some(layer), Some(tls), Some(runtime), Some(handoff)) = (
            tls_socket,
            tls_layer,
            shared.config.tls.clone(),
            tls_runtime,
            tls_handoff,
        ) {
            let advertised = SocketAddr::new(shared.config.advertised_ip(), bound.port());
            let listener = tls::Listener {
                listener,
                config: tls,
                advertised,
                layer,
                runtime,
                handoff,
                shared: Arc::clone(&shared),
            };
            let tasks = infra.clone();
            infra.spawn(listener.run(tasks));
            tracing::info!(addr = %bound, srtp = ?shared.config.srtp, "sip tls listener started");
        }
        // Keeps the endpoint (and with it the sockets) owned by the tasks above alive.
        drop(endpoint);
        tracing::info!(addr = %sip_addr, tcp = shared.config.tcp, "sip server started");

        Ok(Self {
            handle: SipHandle { shared },
            calls: calls_rx,
            infra,
        })
    }

    /// The cloneable handle for [`SipHandle::dial`] and [`SipHandle::send_dtmf`].
    pub fn handle(&self) -> SipHandle {
        self.handle.clone()
    }

    /// The address the SIP socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.handle.local_addr()
    }

    /// Calls in progress.
    pub fn active_calls(&self) -> usize {
        self.handle.active_calls()
    }

    /// The counters, summed over every call since the server started.
    pub fn stats(&self) -> SipStats {
        self.handle.stats()
    }

    /// Refuses new calls, hangs up the running ones (BYE or CANCEL, each bounded by
    /// `hangup_timeout`), then stops the signaling endpoint and waits for all tasks.
    ///
    /// # Errors
    /// [`Error::Core`] (timeout) if calls were still running after `timeout`; the endpoint is
    /// stopped anyway and the calls finish on their own bounded waits.
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        let shared = &self.handle.shared;
        shared.cancel.cancel();
        shared.tracker.close();
        let drained = shared_timeout(&*shared.clock, timeout, shared.tracker.wait()).await;
        shared.endpoint_cancel.cancel();
        self.infra.close();
        let _ = shared_timeout(&*shared.clock, timeout, self.infra.wait()).await;
        drained.map_err(|_| {
            Error::Core(brisvox_core::Error::Timeout {
                what: "sip shutdown",
                after: timeout,
            })
        })
    }
}

/// `tokio::time::timeout` on the injected clock.
pub(crate) async fn shared_timeout<F: std::future::Future>(
    clock: &dyn brisvox_core::Clock,
    after: Duration,
    future: F,
) -> std::result::Result<F::Output, Elapsed> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = clock.sleep(after) => Err(Elapsed),
    }
}

/// A deadline passed ([`shared_timeout`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Elapsed;

impl Transport for SipServer {
    /// Waits for the next inbound call that was answered and acknowledged.
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// and no queued call is left.
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        let shared = Arc::clone(&self.handle.shared);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(brisvox_core::Error::Cancelled.into()),
            link = self.calls.recv() => Ok(link),
            () = shared.cancel.cancelled() => Ok(self.calls.try_recv().ok()),
        }
    }
}

#[cfg(test)]
mod port_tests {
    use super::{DYNAMIC_FIRST, PORT_ATTEMPTS, next_port};

    #[test]
    fn the_next_port_jumps_out_of_a_reserved_block_and_stays_dynamic() {
        assert_eq!(next_port(55_445), 55_572);
        assert!(next_port(55_445) > 55_544, "clears a 100-port block");
        assert!(next_port(65_500) >= DYNAMIC_FIRST);
        assert!(next_port(1_000) >= DYNAMIC_FIRST);
        let mut port = 50_000;
        for _ in 0..PORT_ATTEMPTS {
            port = next_port(port);
            assert!(port >= DYNAMIC_FIRST);
        }
    }
}

#[cfg(test)]
mod rtp_port_tests {
    use super::{PortAllocator, SipConfig, even_bounds};
    use std::net::{IpAddr, Ipv4Addr};

    /// An even base port whose neighbours were free a moment ago.
    async fn free_even_base() -> u16 {
        for _ in 0..50 {
            let Ok(probe) = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await else {
                continue;
            };
            let Ok(addr) = probe.local_addr() else {
                continue;
            };
            let base = addr.port() & !1;
            if (20_000..65_000).contains(&base) {
                return base;
            }
        }
        40_000
    }

    fn config_with(range: (u16, u16)) -> SipConfig {
        SipConfig {
            public_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            rtp_ports: Some(range),
            ..SipConfig::default()
        }
    }

    fn allocator(range: (u16, u16)) -> PortAllocator {
        PortAllocator::new(IpAddr::V4(Ipv4Addr::LOCALHOST), Some(range))
    }

    #[test]
    fn even_bounds_round_inwards_and_reject_ranges_without_an_even_port() {
        assert_eq!(even_bounds(20_000, 20_399), Some((20_000, 20_398)));
        assert_eq!(even_bounds(20_001, 20_010), Some((20_002, 20_010)));
        assert_eq!(even_bounds(20_002, 20_002), Some((20_002, 20_002)));
        assert_eq!(even_bounds(20_001, 20_001), None);
        assert_eq!(even_bounds(20_001, 20_002), Some((20_002, 20_002)));
        assert_eq!(even_bounds(65_535, 65_535), None);
        assert_eq!(even_bounds(65_534, 65_535), Some((65_534, 65_534)));
        assert_eq!(even_bounds(5, 4), None);
    }

    #[test]
    fn a_range_without_an_even_port_is_a_config_error() {
        for range in [(20_001, 20_001), (65_535, 65_535), (7, 7)] {
            assert!(config_with(range).validate().is_err(), "{range:?}");
        }
        assert!(config_with((20_001, 20_002)).validate().is_ok());
        assert!(config_with((20_000, 20_000)).validate().is_ok());
    }

    #[tokio::test]
    async fn binding_in_a_range_without_an_even_port_errors_instead_of_panicking() {
        // `validate` rejects this range; the allocator must still not panic if it sees it.
        let result = allocator((20_001, 20_001)).bind().await;
        assert!(matches!(
            result.map_err(|e| e.kind()),
            Err(std::io::ErrorKind::AddrInUse)
        ));
    }

    #[tokio::test]
    async fn the_rotation_only_ever_binds_even_ports_inside_an_odd_high_range() {
        let base = free_even_base().await;
        // Two even slots (base, base + 2) and an odd high end (base + 3) that must stay unused.
        let ports = allocator((base, base + 3));
        for round in 0..8 {
            let Ok(socket) = ports.bind().await else {
                // A foreign process may hold both ports for a moment; not what is tested here.
                continue;
            };
            let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
            assert_eq!(port % 2, 0, "round {round}: odd port {port}");
            assert!((base..=base + 2).contains(&port), "round {round}: {port}");
        }
    }

    #[tokio::test]
    async fn released_ports_are_reused_after_the_cursor_wrapped_and_exhaustion_errors() {
        let base = free_even_base().await;
        let ports = allocator((base + 1, base + 5)); // even ports base + 2 and base + 4
        let first = ports.bind().await;
        let second = ports.bind().await;
        let (Ok(first), Ok(second)) = (first, second) else {
            return; // a foreign process holds one of the ports
        };
        let a = first.local_addr().map(|a| a.port()).unwrap_or(0);
        let b = second.local_addr().map(|a| a.port()).unwrap_or(0);
        assert_ne!(a, b);
        assert!([a, b].iter().all(|p| *p == base + 2 || *p == base + 4));
        // Every slot is held: the range is exhausted.
        assert!(ports.bind().await.is_err());
        // Releasing one makes it available again even though the cursor is past the end.
        drop(first);
        // The OS may take a moment to release the port of a dropped socket.
        let mut third = None;
        for _ in 0..100 {
            if let Ok(socket) = ports.bind().await {
                third = Some(socket);
                break;
            }
            brisvox_core::SystemClock::shared()
                .sleep(std::time::Duration::from_millis(10))
                .await;
        }
        let third = third.map(|s| s.local_addr().map(|a| a.port()).unwrap_or(0));
        assert_eq!(third, Some(a));
        drop(second);
    }
}
