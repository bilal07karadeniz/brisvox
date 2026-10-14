//! ICE-TCP, passive side (review 2026-10-13 section 3a): the way in for browsers whose
//! network blocks UDP, and for hosts that expose a TCP port but no UDP (a TCP proxy).
//!
//! The server advertises a second host candidate, `typ host tcptype passive`, next to the UDP
//! one ([`WebRtcConfig::tcp_addr`](super::WebRtcConfig::tcp_addr)). A browser that cannot use UDP
//! opens a TCP connection to it (RFC 6544) and speaks the same STUN, DTLS and SRTP over it, each
//! packet framed as RFC 4571 says: a 2-byte big-endian length, then the packet. The candidate has
//! a lower priority than the UDP one (`str0m`: host/UDP 126, host/TCP 90), so UDP wins whenever
//! it works.
//!
//! # Routing
//! One listener serves all calls. A connection starts unrouted: the first STUN binding request
//! on it names the call (`USERNAME` = `<our ufrag>:<theirs>`, the same lookup the UDP socket
//! uses, [`stun_ufrag`](super::demux::stun_ufrag)) and **pins** the connection to that call.
//! From then on every frame goes to the call's inbound queue (drop-oldest, the queue UDP uses)
//! as a [`Datagram`] with `proto: Tcp` and a handle to the connection's writer; the call's
//! `str0m` checks `MESSAGE-INTEGRITY` like it does for UDP. The call sends its replies to that
//! handle.
//!
//! # Bounds
//! - Connections: [`WebRtcConfig::max_tcp_connections`](super::WebRtcConfig::max_tcp_connections);
//!   the next ones are closed at once, unanswered.
//! - Per source IP:
//!   [`WebRtcConfig::max_tcp_connections_per_ip`](super::WebRtcConfig::max_tcp_connections_per_ip)
//!   (default 16, 0 = off); the next ones from that address are closed at once, unanswered.
//! - Unrouted: a connection that has not named a live call within
//!   [`WebRtcConfig::tcp_idle_timeout`](super::WebRtcConfig::tcp_idle_timeout) of being accepted
//!   is closed. The deadline is absolute: junk frames do not extend it.
//! - Pinned: closed after [`PINNED_IDLE`] without a complete frame (a live call exchanges STUN
//!   checks and media more often), when the call ends, or at shutdown.
//! - Frames: a length above
//!   [`WebRtcConfig::tcp_max_frame`](super::WebRtcConfig::tcp_max_frame) closes the connection;
//!   a frame is read into an exactly sized buffer, and the read buffer is a fixed
//!   [`READ_BUFFER`] bytes per connection.
//! - Outbound: a bounded queue per connection of
//!   [`WebRtcConfig::tcp_queue`](super::WebRtcConfig::tcp_queue) frames, **drop-oldest** (media is
//!   real time; a frame is only ever dropped whole, so the byte stream stays framed). The call's
//!   task never waits on a socket: it queues and goes on. A writer stuck longer than the
//!   configured write timeout closes the connection.
//!
//! Per outbound packet one buffer is allocated (the 2-byte header and the packet, contiguous);
//! after a stalled write the frames that queued up (at most 16) are coalesced into one write; a
//! write that blocks has a deadline of [`WebRtcConfig::write_timeout`](super::WebRtcConfig::write_timeout)
//! from the moment it first blocks (no timer for a write the socket takes at once). This path
//! is opt-in and a fallback. TCP head-of-line blocking makes it worse than UDP for
//! real-time audio, which is why it is the second choice.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt as _;
use str0m::net::Protocol;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::demux::{Datagram, stun_ufrag};
use super::http::write_bounded;
use super::{Counters, Shared};
use crate::channel::{self, Overflow, Overflowing};

/// Time a pinned connection may stay silent (no complete frame) before it is closed.
pub(crate) const PINNED_IDLE: Duration = Duration::from_secs(30);

/// Size of the per-connection read buffer.
pub(crate) const READ_BUFFER: usize = 4096;

/// Pause after a failed `accept` (descriptor exhaustion), so the loop does not spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Queued frames one write carries at most.
const BATCH_FRAMES: usize = 16;

/// Time a closing writer may take to flush the TLS-less stream's shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

/// One framed packet (the 2-byte length is already in front of it) waiting for the socket.
pub(crate) struct TcpOut(Bytes);

impl Overflowing for TcpOut {
    fn overflow(&self) -> Overflow {
        Overflow::DropOldest
    }
}

/// The write side of one connection, as the call's task sees it.
pub(crate) struct TcpWriter {
    queue: channel::Sender<TcpOut>,
}

impl TcpWriter {
    /// Queues `packet` (one STUN, DTLS or SRTP packet) as a frame. Never waits: a full queue
    /// evicts its oldest frame. `false` when the connection is gone or the packet cannot be
    /// framed (more than 65535 bytes).
    pub(crate) fn send(&self, packet: &[u8]) -> bool {
        use futures::FutureExt as _;
        let Ok(length) = u16::try_from(packet.len()) else {
            return false;
        };
        let mut framed = Vec::with_capacity(packet.len().saturating_add(2));
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(packet);
        // `DropOldest` never pends, so `now_or_never` always completes; `Err` means the writer
        // (and with it the connection) is gone.
        matches!(
            self.queue.send(TcpOut(Bytes::from(framed))).now_or_never(),
            Some(Ok(()))
        )
    }
}

/// Open connections per source IP. An entry exists only while it counts at least one, so the map
/// never holds more entries than there are open connections (themselves bounded by
/// `max_tcp_connections`).
#[derive(Default)]
pub(super) struct IpCounts(Mutex<HashMap<IpAddr, usize>>);

/// One connection's count; gives it back on drop.
pub(super) struct IpSlot {
    counts: Arc<IpCounts>,
    ip: IpAddr,
}

impl IpCounts {
    /// Counts one more connection from `ip`, unless it already holds `limit` (`0`: no limit).
    pub(super) fn acquire(self: &Arc<Self>, ip: IpAddr, limit: usize) -> Option<IpSlot> {
        let mut map = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = map.entry(ip).or_insert(0);
        if limit != 0 && *count >= limit {
            return None;
        }
        *count = count.saturating_add(1);
        Some(IpSlot {
            counts: Arc::clone(self),
            ip,
        })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut map = self
            .counts
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// Accepts ICE-TCP connections until cancelled.
pub(crate) async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let slots = Arc::new(Semaphore::new(shared.config.max_tcp_connections));
    let per_ip = Arc::new(IpCounts::default());
    loop {
        let accepted = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, remote)) => {
                let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                    // Over the limit: closed without a word (dropping the stream resets it).
                    Counters::bump(&shared.stats.tcp_connections_refused);
                    continue;
                };
                let Some(ip_slot) =
                    per_ip.acquire(remote.ip(), shared.config.max_tcp_connections_per_ip)
                else {
                    Counters::bump(&shared.stats.tcp_connections_refused);
                    continue;
                };
                Counters::bump(&shared.stats.tcp_connections);
                let connection_shared = Arc::clone(&shared);
                shared.tracker.spawn(async move {
                    Counters::bump(&connection_shared.stats.tcp_connections_active);
                    serve_connection(stream, remote, &connection_shared).await;
                    connection_shared
                        .stats
                        .tcp_connections_active
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    drop(ip_slot);
                    drop(slot);
                });
            }
            Err(error) => {
                tracing::warn!(%error, "webrtc ice-tcp accept failed");
                tokio::select! {
                    biased;
                    () = shared.cancel.cancelled() => return,
                    () = shared.clock.sleep(ACCEPT_ERROR_BACKOFF) => {}
                }
            }
        }
    }
}

/// Authenticated ICE-TCP connections one call remembers (a browser opens one, maybe a retry or
/// two).
const MAX_TCP_PEERS: usize = 4;

/// ICE-TCP connections that sent a STUN request with the right ufrag but are not yet known to
/// have passed the integrity check. Counted apart from [`MAX_TCP_PEERS`].
const MAX_PENDING_TCP_PEERS: usize = 4;

/// One ICE-TCP connection of a call.
struct TcpPeer {
    addr: SocketAddr,
    writer: Arc<TcpWriter>,
    /// `str0m` has answered it, or took non-STUN traffic from it: only the owner of the ICE
    /// password gets either. An unauthenticated entry is evicted by newer unauthenticated ones
    /// only.
    authenticated: bool,
}

/// The ICE-TCP connections a call replies on, oldest first, in two classes with separate caps.
///
/// `accepts` of `str0m` checks only the `ufrag` of a STUN request, and that is in the cleartext
/// offer/answer: anyone who saw it can open connections that get their packets into the call
/// (the `MESSAGE-INTEGRITY` check that follows fails, silently). Such a connection must never
/// push the browser's own out, so a connection is **pending** until `str0m` has answered it
/// (it answers a binding request only after the integrity check) or has taken non-STUN traffic
/// from it (only from a source it has authenticated), and only then **authenticated**. Pending
/// entries evict only pending entries.
#[derive(Default)]
pub(crate) struct TcpPeers {
    list: Vec<TcpPeer>,
}

impl TcpPeers {
    /// Remembers the writer of a connection the call just heard from. `authenticated`: the packet
    /// was not STUN (see the type docs).
    pub(crate) fn remember(
        &mut self,
        source: SocketAddr,
        writer: &Arc<TcpWriter>,
        authenticated: bool,
    ) {
        if let Some(entry) = self.list.iter_mut().find(|peer| peer.addr == source) {
            if !Arc::ptr_eq(&entry.writer, writer) {
                entry.writer = Arc::clone(writer);
            }
            entry.authenticated |= authenticated;
            return;
        }
        let in_class = self
            .list
            .iter()
            .filter(|peer| peer.authenticated == authenticated)
            .count();
        let cap = if authenticated {
            MAX_TCP_PEERS
        } else {
            MAX_PENDING_TCP_PEERS
        };
        if in_class >= cap
            && let Some(oldest) = self
                .list
                .iter()
                .position(|peer| peer.authenticated == authenticated)
        {
            self.list.remove(oldest);
        }
        self.list.push(TcpPeer {
            addr: source,
            writer: Arc::clone(writer),
            authenticated,
        });
    }

    /// Queues `packet` for `destination` (never waits). `None`: no such connection. `Some(false)`:
    /// the connection is gone (and forgotten). A packet `str0m` sends to a pending connection
    /// proves it authenticated, so it is promoted.
    pub(crate) fn send(&mut self, destination: SocketAddr, packet: &[u8]) -> Option<bool> {
        let index = self.list.iter().position(|peer| peer.addr == destination)?;
        let sent = match self.list.get_mut(index) {
            Some(peer) => {
                peer.authenticated = true;
                peer.writer.send(packet)
            }
            None => false,
        };
        if !sent {
            self.list.remove(index);
        }
        Some(sent)
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<(SocketAddr, bool)> {
        self.list
            .iter()
            .map(|peer| (peer.addr, peer.authenticated))
            .collect()
    }
}

/// Why reading a frame ended the connection.
enum FrameError {
    /// The peer closed the connection or the socket failed.
    Gone,
    /// The announced length is above the limit.
    TooLarge,
}

/// Reads one RFC 4571 frame. A zero-length frame yields an empty buffer.
async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
    max_frame: usize,
) -> Result<Bytes, FrameError> {
    let mut header = [0u8; 2];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|_| FrameError::Gone)?;
    let length = usize::from(u16::from_be_bytes(header));
    if length > max_frame {
        return Err(FrameError::TooLarge);
    }
    let mut frame = vec![0u8; length];
    reader
        .read_exact(&mut frame)
        .await
        .map_err(|_| FrameError::Gone)?;
    Ok(Bytes::from(frame))
}

/// A call this connection is pinned to.
struct Pin {
    queue: channel::Sender<Datagram>,
    ended: CancellationToken,
}

async fn wait_ended(pin: Option<&Pin>) {
    match pin {
        Some(pin) => pin.ended.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Serves one connection until it closes, idles out, errs or its call ends.
async fn serve_connection(stream: TcpStream, remote: SocketAddr, shared: &Arc<Shared>) {
    let config = &shared.config;
    let _ = stream.set_nodelay(true);
    let (read_half, write_half) = stream.into_split();
    let closed = shared.cancel.child_token();
    let (queue, outbound) = channel::bounded::<TcpOut>(config.tcp_queue);
    let writer = Arc::new(TcpWriter { queue });
    shared.tracker.spawn(write_loop(
        write_half,
        outbound,
        closed.clone(),
        Arc::clone(shared),
    ));

    let mut reader = BufReader::with_capacity(READ_BUFFER, read_half);
    let mut pin: Option<Pin> = None;
    let mut deadline = shared.clock.now() + config.tcp_idle_timeout;
    loop {
        let frame = tokio::select! {
            biased;
            () = closed.cancelled() => break,
            () = wait_ended(pin.as_ref()) => break,
            () = shared.clock.sleep_until(deadline) => {
                if pin.is_none() {
                    Counters::bump(&shared.stats.tcp_unrouted_closed);
                }
                break;
            }
            frame = read_frame(&mut reader, config.tcp_max_frame) => frame,
        };
        let data = match frame {
            Ok(data) => data,
            Err(FrameError::Gone) => break,
            Err(FrameError::TooLarge) => {
                Counters::bump(&shared.stats.tcp_protocol_errors);
                tracing::debug!(%remote, "webrtc ice-tcp frame above the limit; closing");
                break;
            }
        };
        let received = shared.clock.now();
        Counters::bump(&shared.stats.tcp_frames_in);
        if pin.is_none() {
            // Unrouted: only a STUN request naming a live call counts (and ends the idle
            // deadline's absolute phase); everything else is dropped.
            let found = stun_ufrag(&data).and_then(|ufrag| {
                shared
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .by_ufrag(ufrag)
            });
            match found {
                Some((queue, ended)) => pin = Some(Pin { queue, ended }),
                None => {
                    Counters::bump(&shared.stats.datagrams_unrouted);
                    continue;
                }
            }
        }
        deadline = received + PINNED_IDLE;
        if let Some(pin) = &pin {
            let datagram = Datagram {
                proto: Protocol::Tcp,
                source: remote,
                received,
                data,
                reply: Some(Arc::clone(&writer)),
                local: None,
            };
            // `DropOldest` never waits. An error means the call's task is gone.
            if pin.queue.send(datagram).await.is_err() {
                break;
            }
        }
    }
    closed.cancel();
}

/// Writes queued frames to the socket until the connection closes.
async fn write_loop(
    mut socket: OwnedWriteHalf,
    mut outbound: channel::Receiver<TcpOut>,
    closed: CancellationToken,
    shared: Arc<Shared>,
) {
    let write_timeout = shared.config.write_timeout;
    // Coalescing buffer for a backlog: reused, grown once.
    let mut batch: Vec<u8> = Vec::new();
    loop {
        let item = tokio::select! {
            biased;
            () = closed.cancelled() => break,
            item = outbound.recv() => item,
        };
        let Some(TcpOut(first)) = item else { break };
        // A lone frame is written as it is. When more are already queued (the previous write
        // blocked), up to `BATCH_FRAMES` go out in one write: one syscall instead of one per
        // frame, which is how a backlog is worked off.
        let second = outbound.recv().now_or_never().flatten();
        let written = tokio::select! {
            biased;
            () = closed.cancelled() => break,
            written = async {
                let clock = &*shared.clock;
                match second {
                    None => write_bounded(clock, write_timeout, &mut socket, &first).await,
                    Some(TcpOut(second)) => {
                        batch.clear();
                        batch.extend_from_slice(&first);
                        batch.extend_from_slice(&second);
                        for _ in 2..BATCH_FRAMES {
                            let Some(Some(TcpOut(more))) = outbound.recv().now_or_never() else {
                                break;
                            };
                            batch.extend_from_slice(&more);
                        }
                        write_bounded(clock, write_timeout, &mut socket, &batch).await
                    }
                }
            } => matches!(written, Ok(Ok(()))),
        };
        if !written {
            break;
        }
    }
    // Frames still queued or evicted are lost with the connection: count the evictions.
    let stats = outbound.stats();
    Counters::add(
        &shared.stats.tcp_frames_out_dropped,
        stats.dropped_oldest.saturating_add(stats.dropped_newest),
    );
    closed.cancel();
    drop(outbound);
    tokio::select! {
        biased;
        _ = socket.shutdown() => {}
        () = shared.clock.sleep(SHUTDOWN_GRACE) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_a_big_endian_length() {
        let (queue, mut rx) = channel::bounded::<TcpOut>(4);
        let writer = TcpWriter { queue };
        assert!(writer.send(&[1, 2, 3]));
        let framed = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(rx.recv())
            .unwrap();
        assert_eq!(&framed.0[..], &[0, 3, 1, 2, 3]);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_frame_and_never_waits() {
        let (queue, mut rx) = channel::bounded::<TcpOut>(2);
        let writer = TcpWriter { queue };
        for n in 0..5u8 {
            assert!(writer.send(&[n]));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let first = runtime.block_on(rx.recv()).unwrap();
        let second = runtime.block_on(rx.recv()).unwrap();
        // The queue held two frames; the newest two survive.
        assert_eq!(&first.0[..], &[0, 1, 3]);
        assert_eq!(&second.0[..], &[0, 1, 4]);
        assert_eq!(rx.stats().dropped_oldest, 3);
    }

    #[test]
    fn sending_to_a_gone_connection_and_oversize_packets_report_false() {
        let (queue, rx) = channel::bounded::<TcpOut>(2);
        let writer = TcpWriter { queue };
        assert!(!writer.send(&vec![0u8; 70_000]));
        drop(rx);
        assert!(!writer.send(&[1]));
    }

    fn writer() -> (Arc<TcpWriter>, channel::Receiver<TcpOut>) {
        let (queue, rx) = channel::bounded::<TcpOut>(8);
        (Arc::new(TcpWriter { queue }), rx)
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn unauthenticated_connections_never_evict_an_authenticated_one() {
        let mut peers = TcpPeers::default();
        let (real, _rx) = writer();
        // The browser: answered by str0m (pending, then promoted by the first reply).
        peers.remember(addr(1), &real, false);
        assert_eq!(peers.send(addr(1), b"binding response"), Some(true));
        // Attackers with the right ufrag and a bad integrity: far more than the caps.
        let kept: Vec<_> = (0..20)
            .map(|n| {
                let (attacker, rx) = writer();
                peers.remember(addr(100 + n), &attacker, false);
                (attacker, rx)
            })
            .collect();
        let snapshot = peers.snapshot();
        assert!(snapshot.contains(&(addr(1), true)), "{snapshot:?}");
        assert_eq!(
            snapshot
                .iter()
                .filter(|(_, authenticated)| !authenticated)
                .count(),
            MAX_PENDING_TCP_PEERS,
            "{snapshot:?}"
        );
        // The browser can still be replied to.
        assert_eq!(peers.send(addr(1), b"media"), Some(true));
        // Nothing was ever sent to an attacker.
        assert_eq!(peers.send(addr(100), b"x"), None, "the oldest were evicted");
        drop(kept);
    }

    #[test]
    fn non_stun_traffic_authenticates_and_authenticated_entries_have_their_own_cap() {
        let mut peers = TcpPeers::default();
        let keep: Vec<_> = (0..6)
            .map(|n| {
                let (w, rx) = writer();
                peers.remember(addr(n), &w, true);
                (w, rx)
            })
            .collect();
        let snapshot = peers.snapshot();
        assert_eq!(snapshot.len(), MAX_TCP_PEERS);
        assert!(snapshot.iter().all(|(_, authenticated)| *authenticated));
        // The oldest authenticated entries made room; the newest survive.
        assert_eq!(snapshot.last().map(|(a, _)| *a), Some(addr(5)));
        // Later non-STUN traffic from a pending source promotes it in place.
        let (late, _rx) = writer();
        peers.remember(addr(50), &late, false);
        peers.remember(addr(50), &late, true);
        assert!(peers.snapshot().contains(&(addr(50), true)));
        drop(keep);
    }

    #[test]
    fn a_gone_connection_is_forgotten_on_send() {
        let mut peers = TcpPeers::default();
        let (w, rx) = writer();
        peers.remember(addr(1), &w, true);
        drop(rx);
        assert_eq!(peers.send(addr(1), b"x"), Some(false));
        assert_eq!(peers.send(addr(1), b"x"), None);
        assert!(peers.snapshot().is_empty());
    }

    #[test]
    fn the_per_ip_counter_refuses_over_the_limit_and_gives_back_on_drop() {
        let counts = Arc::new(IpCounts::default());
        let one = IpAddr::from([192, 0, 2, 1]);
        let two = IpAddr::from([192, 0, 2, 2]);
        let a = counts.acquire(one, 2).unwrap();
        let b = counts.acquire(one, 2).unwrap();
        assert!(counts.acquire(one, 2).is_none());
        // Another address has its own budget; limit 0 means none.
        let c = counts.acquire(two, 2).unwrap();
        let many: Vec<_> = (0..50).map(|_| counts.acquire(two, 0).unwrap()).collect();
        drop(a);
        let again = counts.acquire(one, 2).unwrap();
        assert!(counts.acquire(one, 2).is_none());
        drop((again, b, c, many));
        // Entries vanish with their last connection (the map stays bounded by open connections).
        assert!(counts.0.lock().unwrap().is_empty());
    }
}
