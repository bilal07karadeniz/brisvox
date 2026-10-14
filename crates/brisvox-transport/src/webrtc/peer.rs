//! One WebRTC call: a single task that drives one sans-IO `str0m` `Rtc`.
//!
//! ```text
//!   UDP reader --Datagram--> inbound queue --+
//!   engine --WireOut--------> outgoing queue +--> select! --> Rtc::handle_input / poll_output
//!   timers (Rtc, marks, connect, disconnect) +                    |        |
//!                                                  UDP try_send_to        Event::MediaData
//!                                                                         --> MediaPath --> WireEvent
//! ```
//!
//! The loop is the standard `str0m` one: drain `poll_output` until it returns a timeout, wait for
//! the next input or that timeout, feed it back. Time is the injected clock's `Instant`
//! converted to `std`. Nothing in the loop waits on the network: sends are `try_send_to`, and
//! the only waits are the select (all branches cancellable), a codec job (bounded by
//! `codec_timeout`) and a control event for the engine (bounded by `send_timeout`).
//!
//! The call is created when the browser's offer is accepted; the engine hears about it
//! (`CallLink` into the accept queue, `WireEvent::Started`) only when ICE and DTLS completed.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt as _;
use str0m::media::{Frequency, MediaData, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use brisvox_core::TurnEpoch;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::AudioFrame;
use brisvox_inference::pool::PoolHandle;

use super::demux::{Datagram, is_stun};
use super::http::timeout;
use super::media::MediaPath;
use super::relay::Relay;
use super::tcp::TcpPeers;
use super::{Counters, Shared};
use crate::auth::SessionClaims;
use crate::call::{CallEnds, WireEvent, WireOut, call_pair};
use crate::channel;
use crate::event::CallInfo;

/// Samples (48 kHz RTP ticks) in one 20 ms packet.
const FRAME_TICKS: u64 = 960;

/// A pause in the outgoing audio longer than this (60 ms of RTP ticks) makes the next packet
/// jump to the wall-clock position and carry the marker bit.
const GAP_TOLERANCE_TICKS: u64 = 2_880;

/// Marks the adapter remembers as pending (the core gives up on older ones at 256 too).
const MAX_PENDING_MARKS: usize = 256;

/// Rounds of `poll_output` before the loop gives up on a `Rtc` that never settles.
const MAX_DRAIN_STEPS: usize = 10_000;

/// Datagrams the loop handles in a row before it yields to the runtime, so a flood on one call
/// cannot hold a worker thread (timers are checked at the top of every round regardless).
const MAX_DATAGRAMS_PER_TURN: u32 = 32;

/// Ticks of the 48 kHz RTP clock in `duration`.
fn ticks(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros())
        .unwrap_or(u64::MAX)
        .saturating_mul(48)
        / 1_000
}

/// A token bucket for the audio packets a browser may send: `per_sec` packets per second on
/// average, with a burst of one second's worth. Driven by the injected clock's instants.
///
/// Counted in micro-tokens (one packet costs `PACKET`) so refilling needs no floating point.
#[derive(Debug)]
pub(crate) struct PacketBudget {
    micro_tokens: u64,
    capacity: u64,
    per_sec: u64,
    last: Instant,
}

impl PacketBudget {
    const PACKET: u64 = 1_000_000;

    /// A full bucket at `now`.
    pub(crate) fn new(per_sec: u32, now: Instant) -> Self {
        let per_sec = u64::from(per_sec);
        let capacity = per_sec.saturating_mul(Self::PACKET);
        Self {
            micro_tokens: capacity,
            capacity,
            per_sec,
            last: now,
        }
    }

    /// Takes one packet's worth at `now`; `false` when the browser is over its rate.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        if now > self.last {
            let elapsed_us =
                u64::try_from(now.duration_since(self.last).as_micros()).unwrap_or(u64::MAX);
            // One second adds `per_sec` packets: one microsecond adds `per_sec` micro-tokens.
            self.micro_tokens = self
                .micro_tokens
                .saturating_add(elapsed_us.saturating_mul(self.per_sec))
                .min(self.capacity);
            self.last = now;
        }
        match self.micro_tokens.checked_sub(Self::PACKET) {
            Some(left) => {
                self.micro_tokens = left;
                true
            }
            None => false,
        }
    }
}

/// How many Opus frames the decoder should conceal before the packet at `ts`: the frames the
/// timestamps say are missing since `next` (the end of the previous packet), but only when the
/// sequence numbers show a loss (`contiguous == false`). A timestamp jump over contiguous
/// sequence numbers is silence suppression (DTX) and is not concealed. At most `max` frames.
fn lost_frames(next: Option<u64>, ts: u64, contiguous: bool, max: u32) -> u32 {
    match next {
        Some(next) if !contiguous && ts > next => u32::try_from((ts - next) / FRAME_TICKS)
            .unwrap_or(u32::MAX)
            .min(max),
        _ => 0,
    }
}

/// The RTP timestamp and marker bit of the next outgoing packet, given the stream's position
/// (`next`, `None` before the first packet), whether a talkspurt is pending (`marker`, set at
/// the start and by a clear) and the wall-clock position `wall` on the 48 kHz timeline.
/// Returns `(timestamp, marker_bit, marker_pending_after)`.
///
/// While audio keeps coming (`wall` within [`GAP_TOLERANCE_TICKS`] of `next`) the timestamp
/// continues and the marker bit is set only if one was pending. After a longer pause the
/// timestamp jumps to the wall-clock position, rounded down to a whole packet and never
/// backwards, and the packet carries the marker bit.
fn plan_packet(next: Option<u64>, marker: bool, wall: u64) -> (u64, bool, bool) {
    match next {
        Some(next) if wall <= next.saturating_add(GAP_TOLERANCE_TICKS) => (next, marker, false),
        previous => {
            let resume = wall / FRAME_TICKS * FRAME_TICKS;
            (resume.max(previous.unwrap_or(0)), true, false)
        }
    }
}

/// Why a call ended.
#[derive(Debug)]
pub(crate) enum End {
    /// The server is shutting down.
    Cancelled,
    /// The client sent `DELETE`.
    ClientHangup,
    /// The browser closed the connection (DTLS close).
    PeerClosed,
    /// ICE stayed disconnected beyond the grace period, or the `Rtc` died.
    PeerGone,
    /// The engine sent `WireOut::Hangup`.
    CoreHangup,
    /// The engine ended the call (`ended` token) or dropped its link.
    CoreGone,
    /// ICE and DTLS did not complete in `connect_timeout`.
    ConnectTimeout,
    /// The engine did not take a control event in `send_timeout`, or the accept queue stayed full.
    Overloaded,
    /// The session token's `max_s` passed.
    MaxDuration,
    /// An error from `str0m`.
    Failed(String),
}

impl std::fmt::Display for End {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("server shutdown"),
            Self::ClientHangup => f.write_str("client hung up (DELETE)"),
            Self::PeerClosed => f.write_str("browser closed the connection"),
            Self::PeerGone => f.write_str("connection lost"),
            Self::CoreHangup => f.write_str("engine hung up"),
            Self::CoreGone => f.write_str("engine ended the call"),
            Self::ConnectTimeout => f.write_str("ICE/DTLS did not complete in time"),
            Self::Overloaded => f.write_str("engine did not take events in time"),
            Self::MaxDuration => f.write_str("the session token's call duration cap passed"),
            Self::Failed(message) => write!(f, "failed: {message}"),
        }
    }
}

/// What an encode or flush job returns: the Opus packets, or why there are none.
type Encoded = Result<Vec<Arc<[u8]>>, crate::Error>;

/// A codec job did not run (the pool refused it, timed out or died). The frame is dropped.
pub(crate) struct Dropped;

/// Runs codec work for one call: on the inference pool when there is one, otherwise inline.
struct MediaRunner {
    path: Arc<Mutex<MediaPath>>,
    pool: Option<PoolHandle>,
    timeout: Duration,
}

fn lock(path: &Mutex<MediaPath>) -> std::sync::MutexGuard<'_, MediaPath> {
    // A job that panicked left the codecs in some state; they are re-synchronised by the next
    // packet (decoder) or the next clear (encoder), so a poisoned lock is still usable.
    path.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MediaRunner {
    async fn run<T, F>(&self, job: F) -> Result<T, Dropped>
    where
        T: Send + 'static,
        F: FnOnce(&mut MediaPath) -> T + Send + 'static,
    {
        match &self.pool {
            None => Ok(job(&mut lock(&self.path))),
            Some(pool) => {
                let path = Arc::clone(&self.path);
                pool.run(self.timeout, move || job(&mut lock(&path)))
                    .await
                    .map(|done| done.value)
                    .map_err(|_| Dropped)
            }
        }
    }

    /// Runs a cheap job (a reset) on the calling task.
    fn now<T>(&self, job: impl FnOnce(&mut MediaPath) -> T) -> T {
        job(&mut lock(&self.path))
    }
}

/// Removes a call from the registry and frees its slot when it ends, however it ends.
pub(crate) struct SessionGuard {
    shared: Arc<Shared>,
    id: String,
    client: IpAddr,
    _permit: OwnedSemaphorePermit,
}

impl SessionGuard {
    /// Counts the call against `client`'s limit. `None` when that address is at its limit.
    pub(crate) fn reserve(
        shared: &Arc<Shared>,
        id: String,
        permit: OwnedSemaphorePermit,
        client: IpAddr,
    ) -> Option<Self> {
        let reserved = shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reserve_ip(client, shared.config.max_calls_per_ip);
        if !reserved {
            return None;
        }
        shared.stats.sessions_active.fetch_add(1, Ordering::Relaxed);
        Some(Self {
            shared: Arc::clone(shared),
            id,
            client,
            _permit: permit,
        })
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        {
            let mut registry = self
                .shared
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let dropped = registry.remove(&self.id);
            registry.release_ip(self.client);
            // Under the registry lock: `WebRtcServer::stats` adds the live queues' drops to
            // this total under the same lock, so a call is never counted twice or not at all.
            Counters::add(&self.shared.stats.datagrams_dropped_overflow, dropped);
        }
        self.shared
            .stats
            .sessions_active
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// What [`Peer::new`] takes.
pub(crate) struct PeerParts {
    pub(crate) shared: Arc<Shared>,
    pub(crate) rtc: Rtc,
    pub(crate) mid: Mid,
    pub(crate) pt: Pt,
    pub(crate) can_send: bool,
    pub(crate) path: MediaPath,
    pub(crate) inbound: channel::Receiver<Datagram>,
    pub(crate) hangup: CancellationToken,
    /// Shared with the registry entry: set once ICE and DTLS completed (see [`Peer::connected`]).
    pub(crate) connected: Arc<AtomicBool>,
    pub(crate) cancel: CancellationToken,
    pub(crate) created: Instant,
    /// The handshake-pool slot, released when the media path is up (or the call ends).
    pub(crate) handshake: Option<OwnedSemaphorePermit>,
    /// What the session token that opened the call said, if there was one.
    pub(crate) claims: Option<SessionClaims>,
    /// The call's TURN relay, when it has one. Dropping the peer releases it.
    pub(crate) relay: Option<Relay>,
}

/// The state of one call.
pub(crate) struct Peer {
    shared: Arc<Shared>,
    rtc: Rtc,
    mid: Mid,
    pt: Pt,
    can_send: bool,
    media: MediaRunner,
    inbound: channel::Receiver<Datagram>,
    hangup: CancellationToken,
    /// Set once ICE and DTLS completed; the `DELETE` handler reads it to decide on a refund.
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
    created: Instant,
    /// Slot in the handshake pool; dropped when ICE and DTLS complete.
    handshake: Option<OwnedSemaphorePermit>,
    claims: Option<SessionClaims>,
    relay: Option<Relay>,
    /// Set once the engine has the call.
    events: Option<channel::Sender<WireEvent>>,
    outgoing: Option<channel::Receiver<WireOut>>,
    ended: Option<CancellationToken>,
    disconnected_at: Option<Instant>,
    last_source: Option<SocketAddr>,
    /// ICE-TCP connections of this call (authenticated and pending ones, capped apart).
    tcp_peers: TcpPeers,
    // Inbound audio.
    rx_seq: u64,
    rx_next_ts: Option<u64>,
    rx_budget: PacketBudget,
    // Outbound audio.
    epoch: TurnEpoch,
    tx_origin: Instant,
    tx_next_ts: Option<u64>,
    tx_marker: bool,
    marks: VecDeque<(Instant, String)>,
    reported_rate: bool,
    reported_receive_only: bool,
}

impl Peer {
    pub(crate) fn new(parts: PeerParts) -> Self {
        let PeerParts {
            shared,
            rtc,
            mid,
            pt,
            can_send,
            path,
            inbound,
            hangup,
            connected,
            cancel,
            created,
            handshake,
            claims,
            relay,
        } = parts;
        let rx_budget = PacketBudget::new(shared.config.max_audio_packets_per_sec, created);
        let media = MediaRunner {
            path: Arc::new(Mutex::new(path)),
            pool: shared.options.codec_pool.clone(),
            timeout: shared.config.codec_timeout,
        };
        Self {
            shared,
            rtc,
            mid,
            pt,
            can_send,
            media,
            inbound,
            hangup,
            connected,
            cancel,
            created,
            handshake,
            claims,
            relay,
            events: None,
            outgoing: None,
            ended: None,
            disconnected_at: None,
            last_source: None,
            tcp_peers: TcpPeers::default(),
            rx_seq: 0,
            rx_next_ts: None,
            rx_budget,
            epoch: TurnEpoch::ZERO,
            tx_origin: created,
            tx_next_ts: None,
            tx_marker: true,
            marks: VecDeque::new(),
            reported_rate: false,
            reported_receive_only: false,
        }
    }

    /// Runs the call to its end; a panic inside ends this call only.
    pub(crate) async fn run_guarded(self, guard: SessionGuard) {
        let shared = Arc::clone(&self.shared);
        match AssertUnwindSafe(self.run()).catch_unwind().await {
            Ok(end) => tracing::debug!(reason = %end, "webrtc call ended"),
            Err(_) => {
                Counters::bump(&shared.stats.call_panics);
                tracing::error!("webrtc call panicked");
            }
        }
        drop(guard);
    }

    async fn run(mut self) -> End {
        let end = self.run_loop().await;
        self.finish(&end).await;
        end
    }

    async fn run_loop(&mut self) -> End {
        let clock = Arc::clone(&self.shared.clock);
        let sleep = clock.sleep(Duration::from_secs(3_600));
        tokio::pin!(sleep);
        let mut datagram_run = 0u32;
        loop {
            let rtc_timeout = match self.drain().await {
                Ok(at) => at,
                Err(end) => return end,
            };
            if !self.rtc.is_alive() {
                return End::PeerGone;
            }
            let now = clock.now();
            let deadline = self.wakeup(Instant::from_std(rtc_timeout));
            if deadline <= now {
                // Due already: skip the timer (it would round up to its next tick). Every round
                // passes here first, so a steady stream of datagrams cannot starve the timers
                // (mark acknowledgements, the connect deadline, the disconnect grace).
                if let Err(end) = self.on_tick(now).await {
                    return end;
                }
                tokio::task::yield_now().await;
                continue;
            }
            sleep.as_mut().reset(deadline);
            let step = tokio::select! {
                biased;
                () = self.cancel.cancelled() => Step::Cancelled,
                () = self.hangup.cancelled() => Step::Hangup,
                () = wait_cancelled(self.ended.as_ref()) => Step::CoreEnded,
                datagram = self.inbound.recv() => Step::Datagram(datagram),
                out = recv_out(self.outgoing.as_mut()) => Step::Out(out),
                () = &mut sleep => Step::Tick,
            };
            if matches!(step, Step::Datagram(_)) {
                datagram_run += 1;
                if datagram_run >= MAX_DATAGRAMS_PER_TURN {
                    datagram_run = 0;
                    tokio::task::yield_now().await;
                }
            } else {
                datagram_run = 0;
            }
            let result = match step {
                Step::Cancelled => return End::Cancelled,
                Step::Hangup => return End::ClientHangup,
                Step::CoreEnded | Step::Out(None) => return End::CoreGone,
                Step::Datagram(None) => return End::Cancelled,
                Step::Datagram(Some(datagram)) => self.on_datagram(datagram),
                Step::Out(Some(out)) => self.on_wire_out(out).await,
                Step::Tick => self.on_tick(clock.now()).await,
            };
            if let Err(end) = result {
                return end;
            }
        }
    }

    /// The call-duration cap of the session token (`max_s`), counted from the offer.
    fn max_call_duration(&self) -> Option<Duration> {
        self.claims.as_ref().and_then(SessionClaims::max_duration)
    }

    /// The earliest of the `Rtc`'s timeout and ours.
    fn wakeup(&self, rtc: Instant) -> Instant {
        let config = &self.shared.config;
        let mut at = rtc;
        if self.events.is_none() {
            at = at.min(deadline_after(self.created, config.connect_timeout));
        }
        if let Some(limit) = self.max_call_duration() {
            at = at.min(deadline_after(self.created, limit));
        }
        if let Some((due, _)) = self.marks.front() {
            at = at.min(*due);
        }
        if let Some(since) = self.disconnected_at {
            at = at.min(deadline_after(since, config.disconnect_grace));
        }
        at
    }

    async fn on_tick(&mut self, now: Instant) -> Result<(), End> {
        let config = &self.shared.config;
        self.rtc
            .handle_input(Input::Timeout(now.into_std()))
            .map_err(|e| End::Failed(e.to_string()))?;
        if self.events.is_none() && now >= deadline_after(self.created, config.connect_timeout) {
            return Err(End::ConnectTimeout);
        }
        if let Some(since) = self.disconnected_at
            && now >= deadline_after(since, config.disconnect_grace)
        {
            return Err(End::PeerGone);
        }
        if let Some(limit) = self.max_call_duration()
            && now >= deadline_after(self.created, limit)
        {
            return Err(End::MaxDuration);
        }
        while self.marks.front().is_some_and(|(due, _)| *due <= now) {
            if let Some((_, id)) = self.marks.pop_front() {
                self.deliver(WireEvent::MarkAck { id }).await?;
            }
        }
        Ok(())
    }

    fn on_datagram(&mut self, datagram: Datagram) -> Result<(), End> {
        let stats = &self.shared.stats;
        // The local address the packet arrived on is the advertised candidate of its protocol
        // (`str0m` matches it against its local candidates), or the TURN relayed address when
        // the packet came through the call's relay.
        let local = datagram.local.or(match datagram.proto {
            Protocol::Tcp => self.shared.tcp_candidate,
            _ => Some(self.shared.candidate),
        });
        let Some(local) = local else {
            Counters::bump(&stats.datagrams_rejected);
            return Ok(());
        };
        let Ok(receive) = Receive::new(datagram.proto, datagram.source, local, &datagram.data)
        else {
            Counters::bump(&stats.datagrams_rejected);
            return Ok(());
        };
        let input = Input::Receive(datagram.received.into_std(), receive);
        if !self.rtc.accepts(&input) {
            Counters::bump(&stats.datagrams_rejected);
            return Ok(());
        }
        if let Some(writer) = &datagram.reply {
            // `accepts` checked only the ufrag of a STUN request (which anyone who has seen the
            // cleartext offer knows); anything else is accepted only from a source the ICE
            // agent already authenticated.
            let authenticated = !is_stun(&datagram.data);
            self.tcp_peers
                .remember(datagram.source, writer, authenticated);
        }
        self.last_source = Some(datagram.source);
        self.rtc
            .handle_input(input)
            .map_err(|e| End::Failed(e.to_string()))
    }

    /// Polls the `Rtc` until it asks for a timeout; sends what it transmits and handles what it
    /// reports.
    async fn drain(&mut self) -> Result<std::time::Instant, End> {
        for _ in 0..MAX_DRAIN_STEPS {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Ok(at),
                Ok(Output::Transmit(transmit)) => self.transmit(&transmit),
                Ok(Output::Event(event)) => self.on_event(event).await?,
                Err(error) => return Err(End::Failed(error.to_string())),
            }
        }
        Err(End::Failed("the rtc did not settle".to_owned()))
    }

    /// Sends one packet over the ICE-TCP connection of `destination`: queued as a frame, never
    /// waiting (a full queue drops its oldest frame, a gone connection forgets the writer).
    fn transmit_tcp(&mut self, transmit: &Transmit) {
        let stats = &self.shared.stats;
        match self
            .tcp_peers
            .send(transmit.destination, &transmit.contents)
        {
            Some(true) => Counters::bump(&stats.tcp_frames_out),
            Some(false) | None => Counters::bump(&stats.datagrams_out_dropped),
        }
    }

    /// Sends one packet out through the call's TURN relay: queued, never waiting (a full queue
    /// drops its oldest packet; an ended allocation drops the packet).
    fn transmit_relay(&self, relay: &Relay, transmit: &Transmit) {
        let stats = &self.shared.stats;
        if !relay.send(transmit.destination, &transmit.contents) {
            Counters::bump(&stats.relay_packets_dropped);
        }
    }

    fn transmit(&mut self, transmit: &Transmit) {
        if let Some(relay) = &self.relay
            && transmit.proto == Protocol::Udp
            && transmit.source == relay.relayed()
        {
            return self.transmit_relay(relay, transmit);
        }
        match transmit.proto {
            Protocol::Tcp => return self.transmit_tcp(transmit),
            Protocol::Udp => {}
            // Never produced for the candidates this transport advertises.
            _ => return Counters::bump(&self.shared.stats.datagrams_out_dropped),
        }
        let stats = &self.shared.stats;
        match self
            .shared
            .socket
            .try_send_to(&transmit.contents, transmit.destination)
        {
            Ok(_) => Counters::bump(&stats.datagrams_out),
            // The socket buffer is full or the destination is unreachable: UDP, so the datagram
            // is lost; media is real time and retransmission is the protocol's job.
            Err(_) => Counters::bump(&stats.datagrams_out_dropped),
        }
    }

    async fn on_event(&mut self, event: Event) -> Result<(), End> {
        match event {
            Event::Connected => self.connected().await,
            Event::IceConnectionStateChange(state) => {
                match state {
                    IceConnectionState::Disconnected => {
                        if self.disconnected_at.is_none() {
                            self.disconnected_at = Some(self.shared.clock.now());
                        }
                    }
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        self.disconnected_at = None;
                    }
                    _ => {}
                }
                Ok(())
            }
            Event::MediaData(data) => self.on_media(data).await,
            Event::Closed => Err(End::PeerClosed),
            _ => Ok(()),
        }
    }

    /// ICE and DTLS are up: hand the call to the engine.
    async fn connected(&mut self) -> Result<(), End> {
        if self.events.is_some() {
            return Ok(());
        }
        let config = &self.shared.config;
        // From here a `DELETE` no longer gives the session token back: the call is the browser's.
        self.connected.store(true, Ordering::Release);
        // ICE and DTLS are done: the call no longer counts against the handshake pool.
        self.handshake = None;
        let (link, ends) = call_pair();
        let CallEnds {
            events,
            outgoing,
            ended,
        } = ends;
        // Never wait here: this task also answers the browser's connectivity checks. A full
        // accept queue means the engine is not taking calls, so the call ends at once.
        if self.shared.calls.try_send(link).is_err() {
            return Err(End::Overloaded);
        }
        let number = self.shared.next_call.fetch_add(1, Ordering::Relaxed);
        let mut info = CallInfo {
            call_id: format!("webrtc-{number}"),
            ..CallInfo::default()
        };
        let mut set = |key: &str, value: String| {
            info.metadata.insert(key.to_owned(), value);
        };
        set("transport", "webrtc".to_owned());
        set("encoding", "opus".to_owned());
        set(
            CallInfo::SAMPLE_RATE_KEY,
            config.input_rate.hz().to_string(),
        );
        set(
            CallInfo::OUTPUT_SAMPLE_RATE_KEY,
            config.output_rate.hz().to_string(),
        );
        if let Some(source) = self.last_source {
            set("peer_addr", source.to_string());
        }
        if let Some(claims) = &self.claims {
            claims.add_metadata(&mut info.metadata);
        }
        self.events = Some(events);
        self.outgoing = Some(outgoing);
        self.ended = Some(ended);
        self.tx_origin = self.shared.clock.now();
        Counters::bump(&self.shared.stats.calls_started);
        self.deliver(WireEvent::Started(info)).await
    }

    /// Decodes one Opus frame from the browser and hands the PCM to the engine.
    async fn on_media(&mut self, data: MediaData) -> Result<(), End> {
        if data.mid != self.mid || self.events.is_none() {
            return Ok(());
        }
        let stats = &self.shared.stats;
        let config = &self.shared.config;
        // A browser sends about 50 packets a second. The rest is dropped before it costs a codec
        // job, so one client cannot keep a pool worker (or this task) busy, or flood the engine
        // with audio faster than real time.
        if !self.rx_budget.allow(self.shared.clock.now()) {
            Counters::bump(&stats.audio_packets_rate_limited);
            return Ok(());
        }
        let ts = if data.time.denom() == 48_000 {
            data.time.numer()
        } else {
            data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer()
        };
        // A sequence gap (`contiguous == false`) is packet loss: conceal what the timestamps say
        // is missing, up to a limit. A timestamp jump without a gap is silence suppression.
        let lost = lost_frames(
            self.rx_next_ts,
            ts,
            data.contiguous,
            config.max_concealed_frames,
        );
        let packet = data.data;
        let decoded = self
            .media
            .run(move |path| {
                let mut out = Vec::new();
                path.decode(&packet, lost, &mut out).map(|n| (n, out))
            })
            .await;
        let (samples, frames) = match decoded {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                Counters::bump(&stats.decode_errors);
                tracing::debug!(%error, "webrtc opus packet did not decode");
                return Ok(());
            }
            Err(Dropped) => {
                Counters::bump(&stats.codec_dropped);
                return Ok(());
            }
        };
        Counters::add(&stats.frames_concealed, u64::from(lost));
        self.rx_next_ts = Some(ts.saturating_add(u64::try_from(samples).unwrap_or(0)));
        for pcm in frames {
            self.send_audio(pcm).await?;
        }
        Ok(())
    }

    async fn send_audio(&mut self, pcm: Bytes) -> Result<(), End> {
        let frame = AudioFrame::new(
            pcm,
            self.shared.config.input_rate,
            self.shared.clock.now(),
            self.rx_seq,
            TurnEpoch::ZERO,
        );
        let Ok(frame) = frame else {
            return Ok(());
        };
        self.rx_seq = self.rx_seq.wrapping_add(1);
        Counters::bump(&self.shared.stats.audio_frames_in);
        // Input audio is drop-oldest: this never waits for the engine.
        let Some(events) = &self.events else {
            return Ok(());
        };
        events
            .send(WireEvent::Audio(frame))
            .await
            .map_err(|_| End::CoreGone)
    }

    async fn on_wire_out(&mut self, out: WireOut) -> Result<(), End> {
        match out {
            WireOut::Audio(frame) => self.on_bot_audio(frame).await,
            WireOut::Clear { epoch } => {
                // Dropping what is buffered here is all a transport can do: packets already on
                // the wire play out in the browser.
                self.epoch = self.epoch.max(epoch);
                self.marks.clear();
                self.media.now(MediaPath::clear_output);
                self.tx_marker = true;
                Ok(())
            }
            WireOut::Mark { id } => self.on_mark(id).await,
            WireOut::Hangup => Err(End::CoreHangup),
        }
    }

    async fn on_bot_audio(&mut self, frame: AudioFrame) -> Result<(), End> {
        let stats = &self.shared.stats;
        if frame.epoch().is_stale(self.epoch) {
            return Ok(());
        }
        if frame.sample_rate() != self.shared.config.output_rate {
            Counters::bump(&stats.audio_out_dropped);
            if !self.reported_rate {
                self.reported_rate = true;
                let message = format!(
                    "output_rate_mismatch: bot audio at {} Hz, the call takes {} Hz",
                    frame.sample_rate().hz(),
                    self.shared.config.output_rate.hz()
                );
                self.deliver(WireEvent::Fault { message }).await?;
            }
            return Ok(());
        }
        if !self.can_send {
            Counters::bump(&stats.audio_out_dropped);
            if !self.reported_receive_only {
                self.reported_receive_only = true;
                let message =
                    "the browser offered sendonly: it cannot receive the bot's audio".to_owned();
                self.deliver(WireEvent::Fault { message }).await?;
            }
            return Ok(());
        }
        Counters::bump(&stats.audio_frames_out);
        let pcm = frame.into_data();
        let encoded = self
            .media
            .run(move |path| {
                let mut out = Vec::new();
                path.encode(&pcm, &mut out).map(|()| out)
            })
            .await;
        self.send_packets(encoded)
    }

    async fn on_mark(&mut self, id: String) -> Result<(), End> {
        // The audio ahead of the mark must be on the wire before the mark counts.
        let flushed = self
            .media
            .run(|path| {
                let mut out = Vec::new();
                path.flush(&mut out).map(|()| out)
            })
            .await;
        self.send_packets(flushed)?;
        if self.marks.len() >= MAX_PENDING_MARKS {
            self.marks.pop_front();
        }
        let due = deadline_after(self.shared.clock.now(), self.shared.config.mark_delay);
        self.marks.push_back((due, id));
        Ok(())
    }

    fn send_packets(&mut self, encoded: Result<Encoded, Dropped>) -> Result<(), End> {
        let stats = &self.shared.stats;
        match encoded {
            Ok(Ok(packets)) => {
                for packet in packets {
                    self.write_packet(packet);
                }
            }
            Ok(Err(error)) => {
                Counters::bump(&stats.encode_errors);
                tracing::debug!(%error, "webrtc opus frame did not encode");
            }
            Err(Dropped) => Counters::bump(&stats.codec_dropped),
        }
        Ok(())
    }

    /// Writes one Opus packet to RTP at the stream's position on the 48 kHz timeline.
    fn write_packet(&mut self, payload: Arc<[u8]>) {
        let now = self.shared.clock.now();
        let wall = ticks(now.saturating_duration_since(self.tx_origin));
        let (timestamp, marker, marker_after) = plan_packet(self.tx_next_ts, self.tx_marker, wall);
        self.tx_marker = marker_after;
        self.tx_next_ts = Some(timestamp.saturating_add(FRAME_TICKS));
        let Some(writer) = self.rtc.writer(self.mid) else {
            return;
        };
        let written = writer.start_of_talkspurt(marker).write(
            self.pt,
            now.into_std(),
            MediaTime::new(timestamp, Frequency::FORTY_EIGHT_KHZ),
            payload,
        );
        match written {
            Ok(()) => Counters::bump(&self.shared.stats.rtp_packets_out),
            Err(error) => tracing::debug!(%error, "webrtc rtp write failed"),
        }
    }

    /// Sends a control event to the engine, waiting at most `send_timeout`.
    async fn deliver(&self, event: WireEvent) -> Result<(), End> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        match timeout(
            &*self.shared.clock,
            self.shared.config.send_timeout,
            events.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(End::CoreGone),
            Err(_) => Err(End::Overloaded),
        }
    }

    /// Tells the engine and the browser how the call ended.
    async fn finish(&mut self, end: &End) {
        if matches!(end, End::ClientHangup | End::PeerClosed | End::MaxDuration) {
            let _ = self.deliver(WireEvent::Hangup).await;
        }
        if matches!(
            end,
            End::Cancelled
                | End::ClientHangup
                | End::CoreHangup
                | End::CoreGone
                | End::Overloaded
                | End::MaxDuration
        ) {
            self.close_rtc();
        }
        // Dropping the call's ends tells the engine the wire is gone.
        self.events = None;
        self.outgoing = None;
        self.ended = None;
    }

    /// Starts the close (DTLS close_notify, RTCP BYE) and sends what that produces.
    fn close_rtc(&mut self) {
        if self.rtc.close().is_err() {
            return;
        }
        for _ in 0..64 {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => self.transmit(&transmit),
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }
    }
}

/// What woke the select.
enum Step {
    Cancelled,
    Hangup,
    CoreEnded,
    Datagram(Option<Datagram>),
    Out(Option<WireOut>),
    Tick,
}

/// Waits for `token`, forever if there is none.
async fn wait_cancelled(token: Option<&CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

/// The next output item, waiting forever if the call is not with the engine yet.
async fn recv_out(outgoing: Option<&mut channel::Receiver<WireOut>>) -> Option<WireOut> {
    match outgoing {
        Some(outgoing) => outgoing.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    //! The adapter's own handling of the wire commands, driven directly: the transport core's
    //! pacer and mark ledger already hide most mistakes here from an end-to-end test (a mark
    //! acknowledged too early or after a clear is dropped or ignored by the core), so these
    //! assert on the adapter's state and on the events it sends.

    use std::time::Duration;

    use brisvox_core::frame::SampleRate;
    use brisvox_core::{SystemClock, TurnEpoch};
    use str0m::media::{Direction, MediaKind};
    use str0m::{Candidate, Rtc};

    use super::*;
    use crate::call::CallLink;
    use crate::transport::Transport;
    use crate::webrtc::signaling::build_peer;
    use crate::webrtc::{WebRtcConfig, WebRtcServer};

    struct Fixture {
        peer: Peer,
        link: CallLink,
        _server: WebRtcServer,
        _inbound: channel::Sender<Datagram>,
    }

    async fn fixture(config: WebRtcConfig) -> Fixture {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let mut server = WebRtcServer::bind(config, clock.clone(), &parent)
            .await
            .unwrap();
        let mut browser = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .build(clock.now().into_std());
        browser.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
        let mut change = browser.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, _pending) = change.apply().unwrap();
        let shared = Arc::clone(&server.shared);
        let setup = build_peer(&shared, &offer.to_sdp_string(), clock.now().into_std()).unwrap();
        let (inbound_tx, inbound_rx) = channel::bounded(8);
        let mut peer = Peer::new(PeerParts {
            shared: Arc::clone(&shared),
            rtc: setup.rtc,
            mid: setup.mid,
            pt: setup.pt,
            can_send: setup.can_send,
            path: setup.path,
            inbound: inbound_rx,
            hangup: CancellationToken::new(),
            connected: Arc::new(AtomicBool::new(false)),
            cancel: shared.cancel.child_token(),
            created: clock.now(),
            handshake: None,
            claims: None,
            relay: None,
        });
        peer.connected().await.unwrap();
        let mut link = server.accept(&parent).await.unwrap().unwrap();
        assert!(matches!(
            link.events.recv().await,
            Some(WireEvent::Started(_))
        ));
        Fixture {
            peer,
            link,
            _server: server,
            _inbound: inbound_tx,
        }
    }

    fn config(mark_delay: Duration) -> WebRtcConfig {
        WebRtcConfig {
            http_addr: ([127, 0, 0, 1], 0).into(),
            udp_addr: ([127, 0, 0, 1], 0).into(),
            mark_delay,
            ..WebRtcConfig::default()
        }
    }

    /// `millis` of a 500 Hz tone at the engine's output rate (24 kHz).
    fn bot_audio(millis: usize, epoch: TurnEpoch) -> AudioFrame {
        let samples = 24 * millis;
        let pcm: Vec<u8> = (0..samples)
            .flat_map(|n| {
                let phase = 2.0 * std::f32::consts::PI * 500.0 * n as f32 / 24_000.0;
                ((phase.sin() * 8_000.0) as i16).to_le_bytes()
            })
            .collect();
        AudioFrame::new(
            Bytes::from(pcm),
            SampleRate::Hz24000,
            SystemClock::shared().now(),
            0,
            epoch,
        )
        .unwrap()
    }

    /// No event reaches the engine within `wait`.
    async fn nothing_arrives(link: &mut CallLink, wait: Duration) -> bool {
        tokio::time::timeout(wait, link.events.recv())
            .await
            .is_err()
    }

    fn buffered(peer: &Peer) -> usize {
        peer.media.now(|path| path.buffered_output())
    }

    fn frames_out(peer: &Peer) -> u64 {
        peer.shared.stats.audio_frames_out.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn a_mark_flushes_the_partial_frame_first_and_is_acknowledged_mark_delay_later() {
        let mark_delay = Duration::from_millis(300);
        let mut f = fixture(config(mark_delay)).await;
        let clock = SystemClock::shared();
        // 30 ms is one whole 20 ms packet and a 10 ms remainder that stays in the encoder.
        f.peer
            .on_wire_out(WireOut::Audio(bot_audio(30, TurnEpoch::ZERO)))
            .await
            .unwrap();
        let after_audio = f.peer.tx_next_ts.unwrap();
        assert!(buffered(&f.peer) > 0, "a remainder waits for its frame");

        let before_mark = clock.now();
        f.peer
            .on_wire_out(WireOut::Mark { id: "m".to_owned() })
            .await
            .unwrap();
        assert_eq!(
            f.peer.tx_next_ts.unwrap(),
            after_audio + FRAME_TICKS,
            "the remainder was sent (padded to a packet) before the mark counts"
        );
        assert_eq!(buffered(&f.peer), 0);
        let due = f.peer.marks.front().unwrap().0;
        assert!(
            due >= deadline_after(before_mark, mark_delay),
            "the mark is due {:?} after the flush, not at once",
            due.saturating_duration_since(before_mark)
        );

        // Not acknowledged before it is due, acknowledged when it is.
        f.peer.on_tick(clock.now()).await.unwrap();
        assert!(nothing_arrives(&mut f.link, Duration::from_millis(50)).await);
        f.peer.on_tick(due).await.unwrap();
        assert!(matches!(
            f.link.events.recv().await,
            Some(WireEvent::MarkAck { id }) if id == "m"
        ));
    }

    #[tokio::test]
    async fn a_clear_drops_the_tail_the_pending_marks_and_stale_audio() {
        let mut f = fixture(config(Duration::from_millis(300))).await;
        let clock = SystemClock::shared();
        let epoch1 = TurnEpoch::ZERO.next();
        f.peer
            .on_wire_out(WireOut::Audio(bot_audio(30, TurnEpoch::ZERO)))
            .await
            .unwrap();
        f.peer
            .on_wire_out(WireOut::Mark {
                id: "old".to_owned(),
            })
            .await
            .unwrap();
        // More audio after the mark leaves a remainder in the encoder again.
        f.peer
            .on_wire_out(WireOut::Audio(bot_audio(30, TurnEpoch::ZERO)))
            .await
            .unwrap();
        assert!(buffered(&f.peer) > 0);
        let sent_before = f.peer.tx_next_ts;
        let frames_before = frames_out(&f.peer);

        f.peer
            .on_wire_out(WireOut::Clear { epoch: epoch1 })
            .await
            .unwrap();
        assert!(f.peer.marks.is_empty(), "pending marks are forgotten");
        assert_eq!(f.peer.epoch, epoch1);
        assert!(f.peer.tx_marker, "the next packet starts a talkspurt");
        assert_eq!(
            buffered(&f.peer),
            0,
            "the encoder and resampler were cleared"
        );

        // The cleared mark is never acknowledged, even long after its due time.
        f.peer
            .on_tick(deadline_after(clock.now(), Duration::from_secs(10)))
            .await
            .unwrap();
        assert!(nothing_arrives(&mut f.link, Duration::from_millis(50)).await);

        // A mark right after the clear has no pre-clear tail to flush.
        f.peer
            .on_wire_out(WireOut::Mark {
                id: "new".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            f.peer.tx_next_ts, sent_before,
            "a stale tail leaked into the new epoch"
        );

        // Audio of the old epoch is refused, audio of the new epoch is taken.
        f.peer
            .on_wire_out(WireOut::Audio(bot_audio(20, TurnEpoch::ZERO)))
            .await
            .unwrap();
        assert_eq!(
            frames_out(&f.peer),
            frames_before,
            "a stale-epoch frame was encoded"
        );
        f.peer
            .on_wire_out(WireOut::Audio(bot_audio(20, epoch1)))
            .await
            .unwrap();
        assert_eq!(frames_out(&f.peer), frames_before + 1);
    }

    #[tokio::test]
    async fn the_disconnect_grace_and_the_connect_deadline_end_a_call() {
        let grace = Duration::from_millis(80);
        let connect = Duration::from_millis(200);
        let mut f = fixture(WebRtcConfig {
            disconnect_grace: grace,
            connect_timeout: connect,
            ..config(Duration::from_millis(100))
        })
        .await;
        let clock = SystemClock::shared();
        // Connected (the engine has the call): the connect deadline no longer applies.
        assert!(f.peer.events.is_some());
        f.peer
            .on_tick(deadline_after(clock.now(), Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(f.peer.disconnected_at.is_none());

        let disconnected = || Event::IceConnectionStateChange(IceConnectionState::Disconnected);
        f.peer.on_event(disconnected()).await.unwrap();
        let since = f.peer.disconnected_at.expect("the disconnect was noted");
        let far = deadline_after(clock.now(), Duration::from_secs(3_600));
        assert_eq!(
            f.peer.wakeup(far),
            deadline_after(since, grace),
            "the loop wakes up for the grace deadline"
        );
        // Within the grace the call survives, and a recovery cancels the deadline.
        f.peer.on_tick(since).await.unwrap();
        f.peer
            .on_event(Event::IceConnectionStateChange(
                IceConnectionState::Connected,
            ))
            .await
            .unwrap();
        assert!(f.peer.disconnected_at.is_none());
        f.peer.on_event(disconnected()).await.unwrap();
        let since = f.peer.disconnected_at.unwrap();
        assert!(matches!(
            f.peer.on_tick(deadline_after(since, grace)).await,
            Err(End::PeerGone)
        ));

        // A call the engine has not got yet is abandoned at the connect deadline.
        f.peer.events = None;
        f.peer.disconnected_at = None;
        let created = f.peer.created;
        assert!(f.peer.on_tick(created).await.is_ok());
        assert!(matches!(
            f.peer.on_tick(deadline_after(created, connect)).await,
            Err(End::ConnectTimeout)
        ));
    }
    #[test]
    fn the_packet_budget_allows_the_rate_and_a_one_second_burst_then_refills() {
        let t0 = SystemClock::shared().now();
        let mut budget = PacketBudget::new(100, t0);
        // A full bucket is one second of packets at once, then nothing.
        let burst = (0..500).filter(|_| budget.allow(t0)).count();
        assert_eq!(burst, 100);
        assert!(!budget.allow(t0));
        // 10 ms later one packet has come back (100 per second), not two.
        let t1 = deadline_after(t0, Duration::from_millis(10));
        assert!(budget.allow(t1));
        assert!(!budget.allow(t1));
        // A real-time 50 packets per second stream is never refused, however long it runs.
        let mut at = t1;
        for _ in 0..1_000 {
            at = deadline_after(at, Duration::from_millis(20));
            assert!(budget.allow(at));
        }
        // The bucket never holds more than one second, however long the silence.
        let later = deadline_after(at, Duration::from_secs(3_600));
        assert_eq!((0..500).filter(|_| budget.allow(later)).count(), 100);
        // Time never runs backwards into the bucket.
        assert!(!budget.allow(t0));
    }

    #[test]
    fn a_flood_gets_at_most_the_rate_through() {
        let t0 = SystemClock::shared().now();
        let mut budget = PacketBudget::new(200, t0);
        // 10 000 packets a second for 2 seconds.
        let mut accepted = 0;
        let mut at = t0;
        for _ in 0..20_000 {
            at = deadline_after(at, Duration::from_micros(100));
            if budget.allow(at) {
                accepted += 1;
            }
        }
        assert!(
            (500..=620).contains(&accepted),
            "200 burst + 200 per second for 2 s is about 600, got {accepted}"
        );
    }

    #[test]
    fn loss_is_concealed_from_the_timestamp_gap_only_when_the_sequence_has_a_gap() {
        // Packets 960 ticks apart, one missing in between: 1 frame lost.
        assert_eq!(lost_frames(Some(1_000), 1_000 + 960, false, 3), 1);
        assert_eq!(lost_frames(Some(1_000), 1_000 + 3 * 960, false, 6), 3);
        // The cap.
        assert_eq!(lost_frames(Some(1_000), 1_000 + 10 * 960, false, 3), 3);
        // No gap in the sequence numbers: a timestamp jump is silence suppression (DTX).
        assert_eq!(lost_frames(Some(1_000), 1_000 + 10 * 960, true, 3), 0);
        // Nothing before the first packet, nothing for a packet that is not ahead.
        assert_eq!(lost_frames(None, 9_999, false, 3), 0);
        assert_eq!(lost_frames(Some(5_000), 5_000, false, 3), 0);
        assert_eq!(lost_frames(Some(5_000), 4_000, false, 3), 0);
        // Less than a whole frame of difference is no lost frame.
        assert_eq!(lost_frames(Some(1_000), 1_000 + 959, false, 3), 0);
    }

    #[test]
    fn the_outgoing_timeline_continues_while_audio_flows_and_jumps_after_a_pause() {
        // First packet: the wall-clock position (rounded to a packet), marker set.
        let (ts, marker, pending) = plan_packet(None, true, 10 * FRAME_TICKS + 5);
        assert_eq!((ts, marker, pending), (10 * FRAME_TICKS, true, false));
        // Contiguous audio, even a little late (inside the 60 ms tolerance): same timeline.
        let next = ts + FRAME_TICKS;
        assert_eq!(
            plan_packet(Some(next), false, next + GAP_TOLERANCE_TICKS),
            (next, false, false)
        );
        // A pending marker (after a clear) rides on the next packet without a jump.
        assert_eq!(plan_packet(Some(next), true, next), (next, true, false));
        // A pause beyond the tolerance: jump to the wall-clock position, marker set.
        let wall = next + GAP_TOLERANCE_TICKS + 1;
        let (jumped, marker, pending) = plan_packet(Some(next), false, wall);
        assert_eq!(jumped, wall / FRAME_TICKS * FRAME_TICKS);
        assert!(jumped > next && marker && !pending);
        // A long pause jumps by about its length.
        let (jumped, _, _) = plan_packet(Some(next), false, next + 48_000);
        assert!(jumped >= next + 47_000, "{jumped}");
        // Audio sent faster than real time (the wall clock is behind the stream): the timeline
        // continues and never goes backwards.
        let ahead = 100 * FRAME_TICKS;
        let (kept, marker, _) = plan_packet(Some(ahead), false, ahead - 90 * FRAME_TICKS);
        assert_eq!((kept, marker), (ahead, false));
        let (kept, marker, _) = plan_packet(Some(ahead), false, ahead + GAP_TOLERANCE_TICKS + 1);
        assert!(kept >= ahead && marker);
    }
}
