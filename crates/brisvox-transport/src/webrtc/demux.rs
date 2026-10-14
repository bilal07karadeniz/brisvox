//! The shared UDP socket: which call does a datagram belong to?
//!
//! All calls share one UDP port. A STUN binding request carries a `USERNAME` attribute that
//! starts with the call's local ICE ufrag (RFC 8445: `<receiver ufrag>:<sender ufrag>`); that
//! ufrag decides the call and (re)binds the request's source address to it. Every other datagram
//! (RTP, DTLS, a STUN request without a known ufrag) follows its source address. Everything
//! from an unknown address is dropped and counted: a datagram flood from strangers costs one
//! map lookup each.
//!
//! The ufrag wins over a known source address, so a client that reuses its 5-tuple for a new
//! call (a kiosk with a fixed port, a quick reconnect) reaches the new call while the old one is
//! still alive. Routing by ufrag is safe because the call's `str0m` still checks the request's
//! `MESSAGE-INTEGRITY` with the ICE password: someone who only knows a ufrag (it is in the
//! answer, i.e. known to the call's own client) cannot connect to the call. What a spoofed
//! source address can do is send the victim's address to the attacker's own call until the
//! victim's next connectivity check (STUN checks repeat every few seconds) binds it back; this
//! needs off-path source spoofing and the exact victim ip:port.
//!
//! Each call has a bounded inbound queue ([`INBOUND_CLASS`]): when the call's task falls
//! behind, the oldest datagram is dropped (UDP is lossy by nature and old media is worthless).
//! The queue is bounded in count and, because every datagram is copied into an exactly sized
//! buffer, in bytes (a queued datagram never pins a larger receive buffer).

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};

use bytes::Bytes;
use str0m::net::Protocol;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::limits::rate_key;
use super::tcp::TcpWriter;
use super::{Counters, Shared};
use crate::auth::SessionClaims;
use crate::channel::{self, Overflow, Overflowing};

/// Largest datagram handed to a call (above any UDP payload str0m sends: MTU 1200 target).
/// Larger ones are dropped and counted as unrouted.
const MAX_DATAGRAM: usize = 2048;

/// Size of the receive buffer: the largest UDP payload, so an oversized datagram is read whole
/// (and dropped) instead of being reported as a socket error. One buffer per server, reused.
const RECV_BUFFER: usize = 64 * 1024;

/// Source addresses remembered per call (a client behind a changing NAT, or several candidate
/// pairs, uses a few). The least recently used one is forgotten first: every datagram from a
/// source refreshes it, so the source carrying media is never the one evicted.
const MAX_ADDRS_PER_CALL: usize = 8;

/// Pause after a failed `recv_from`, so the loop does not spin on a persistent error.
const RECV_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// One received datagram (a UDP datagram, or one RFC 4571 frame of an ICE-TCP connection).
pub(crate) struct Datagram {
    pub(crate) proto: Protocol,
    pub(crate) source: SocketAddr,
    pub(crate) received: Instant,
    pub(crate) data: Bytes,
    /// For [`Protocol::Tcp`]: where replies to `source` go (the connection's writer queue).
    pub(crate) reply: Option<Arc<TcpWriter>>,
    /// The local address the packet arrived on, when it is not the advertised candidate of its
    /// protocol: the TURN relayed address of a call (`relay` module).
    pub(crate) local: Option<SocketAddr>,
}

impl Datagram {
    /// A datagram read from the UDP socket.
    pub(crate) fn udp(source: SocketAddr, received: Instant, data: Bytes) -> Self {
        Self {
            proto: Protocol::Udp,
            source,
            received,
            data,
            reply: None,
            local: None,
        }
    }
}

impl std::fmt::Debug for Datagram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Datagram")
            .field("proto", &self.proto)
            .field("source", &self.source)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Eviction class of datagrams (they are the only item type of their queue).
pub(crate) const INBOUND_CLASS: u8 = 0;

impl Overflowing for Datagram {
    fn overflow(&self) -> Overflow {
        Overflow::DropOldest
    }

    fn class(&self) -> u8 {
        INBOUND_CLASS
    }
}

struct Entry {
    hangup: CancellationToken,
    /// Set by the call's task when ICE and DTLS completed; read by the `DELETE` handler to decide
    /// whether the session token may be given back.
    connected: Arc<AtomicBool>,
    /// The session token's claims (none for a static or open listener).
    claims: Option<SessionClaims>,
    /// Cancelled when the call is removed from the registry (its ICE-TCP connections close).
    ended: CancellationToken,
    queue: channel::Sender<Datagram>,
    ufrag: String,
    addrs: VecDeque<SocketAddr>,
}

/// The result of [`Registry::hangup`].
pub(crate) struct Hangup {
    /// Ends the call as a client hang-up.
    pub(crate) token: CancellationToken,
    /// The session claims of a call that has not reached ICE+DTLS completion (none once it
    /// connected, or without a session token).
    pub(crate) unconnected_claims: Option<SessionClaims>,
}

/// Live calls and the routes to them.
#[derive(Default)]
pub(crate) struct Registry {
    sessions: HashMap<String, Entry>,
    by_ufrag: HashMap<String, String>,
    by_addr: HashMap<SocketAddr, String>,
    /// Calls per signaling client address (reserved when an offer arrives, released when the
    /// call ends), keyed like the offer limiter ([`rate_key`]: IPv4-mapped as IPv4, IPv6 by its
    /// `/64`), so a client cannot multiply its cap with the addresses of its own prefix.
    per_ip: HashMap<IpAddr, usize>,
}

impl Registry {
    /// Counts one more call for `ip`; `false` (nothing counted) when `limit` is reached.
    pub(crate) fn reserve_ip(&mut self, ip: IpAddr, limit: Option<usize>) -> bool {
        let ip = rate_key(ip);
        let count = self.per_ip.entry(ip).or_insert(0);
        if limit.is_some_and(|limit| *count >= limit) {
            if *count == 0 {
                self.per_ip.remove(&ip);
            }
            return false;
        }
        *count += 1;
        true
    }

    /// Takes back one call counted by [`reserve_ip`](Self::reserve_ip).
    pub(crate) fn release_ip(&mut self, ip: IpAddr) {
        let ip = rate_key(ip);
        if let Some(count) = self.per_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_ip.remove(&ip);
            }
        }
    }

    /// Adds a call. `ufrag` is its local ICE username fragment.
    pub(crate) fn insert(
        &mut self,
        id: String,
        ufrag: String,
        hangup: CancellationToken,
        connected: Arc<AtomicBool>,
        claims: Option<SessionClaims>,
        queue: channel::Sender<Datagram>,
    ) {
        self.by_ufrag.insert(ufrag.clone(), id.clone());
        self.sessions.insert(
            id,
            Entry {
                hangup,
                connected,
                claims,
                ended: CancellationToken::new(),
                queue,
                ufrag,
                addrs: VecDeque::new(),
            },
        );
    }

    /// Removes a call and every route to it. Returns the datagrams its queue dropped, so the
    /// caller can fold them into the lifetime counter under the same lock that stops
    /// [`live_overflow`](Self::live_overflow) from counting them (no double count in `stats()`).
    pub(crate) fn remove(&mut self, id: &str) -> u64 {
        let Some(entry) = self.sessions.remove(id) else {
            return 0;
        };
        self.by_ufrag.remove(&entry.ufrag);
        entry.ended.cancel();
        for addr in &entry.addrs {
            if self.by_addr.get(addr).is_some_and(|owner| owner == id) {
                self.by_addr.remove(addr);
            }
        }
        let stats = entry.queue.stats();
        stats.dropped_oldest.saturating_add(stats.dropped_newest)
    }

    /// What a client hang-up (WHIP `DELETE`) of call `id` needs: the token that ends the call
    /// and, when the call never connected, the session claims whose token may be given back.
    pub(crate) fn hangup(&self, id: &str) -> Option<Hangup> {
        self.sessions.get(id).map(|entry| Hangup {
            token: entry.hangup.clone(),
            unconnected_claims: if entry.connected.load(Ordering::Acquire) {
                None
            } else {
                entry.claims.clone()
            },
        })
    }

    /// The call whose local ICE ufrag is `ufrag`: its inbound queue and the token that is
    /// cancelled when it ends. An ICE-TCP connection pins itself to the call this names; unlike
    /// a UDP source, a connection needs no address binding (it is the route).
    pub(crate) fn by_ufrag(
        &self,
        ufrag: &str,
    ) -> Option<(channel::Sender<Datagram>, CancellationToken)> {
        let entry = self.sessions.get(self.by_ufrag.get(ufrag)?)?;
        Some((entry.queue.clone(), entry.ended.clone()))
    }

    /// Calls registered now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }

    /// The queue a datagram from `source` belongs to. A STUN `ufrag` naming a live call decides
    /// the call and binds `source` to it (taking the source over from another call if it was
    /// bound there); without a known ufrag a known source keeps its call.
    pub(crate) fn route(
        &mut self,
        source: SocketAddr,
        ufrag: Option<&str>,
    ) -> Option<channel::Sender<Datagram>> {
        if let Some(named) = ufrag.and_then(|ufrag| self.by_ufrag.get(ufrag))
            && self.by_addr.get(&source) != Some(named)
        {
            let named = named.clone();
            return self.bind(source, &named);
        }
        // The common path (RTP, DTLS, repeated checks): no allocation.
        let owner = self.by_addr.get(&source)?;
        let entry = self.sessions.get_mut(owner)?;
        touch(&mut entry.addrs, source);
        Some(entry.queue.clone())
    }

    /// Binds `source` to call `id`, forgetting a binding to another call and, past the limit,
    /// the call's least recently used source.
    fn bind(&mut self, source: SocketAddr, id: &str) -> Option<channel::Sender<Datagram>> {
        if !self.sessions.contains_key(id) {
            return None;
        }
        if let Some(previous) = self.by_addr.insert(source, id.to_owned())
            && previous != id
            && let Some(old) = self.sessions.get_mut(&previous)
        {
            old.addrs.retain(|addr| *addr != source);
        }
        let entry = self.sessions.get_mut(id)?;
        touch(&mut entry.addrs, source);
        while entry.addrs.len() > MAX_ADDRS_PER_CALL {
            if let Some(forgotten) = entry.addrs.pop_front() {
                self.by_addr.remove(&forgotten);
            }
        }
        Some(entry.queue.clone())
    }

    /// Datagrams dropped from the queues of the calls alive now.
    pub(crate) fn live_overflow(&self) -> u64 {
        self.sessions
            .values()
            .map(|entry| {
                let stats = entry.queue.stats();
                stats.dropped_oldest.saturating_add(stats.dropped_newest)
            })
            .fold(0, u64::saturating_add)
    }
}

/// Moves `source` to the most recently used end of `addrs` (adds it if it is not there).
fn touch(addrs: &mut VecDeque<SocketAddr>, source: SocketAddr) {
    if addrs.back() == Some(&source) {
        return;
    }
    if let Some(position) = addrs.iter().position(|addr| *addr == source) {
        addrs.remove(position);
    }
    addrs.push_back(source);
}

/// Whether `datagram` looks like STUN rather than DTLS or RTP/RTCP: the first byte is 0 to 3
/// (RFC 7983 section 7; DTLS is 20 to 63, RTP/RTCP 128 to 191).
pub(crate) fn is_stun(datagram: &[u8]) -> bool {
    datagram.first().is_some_and(|byte| *byte <= 3)
}

/// The local ICE ufrag a STUN request carries: the part of `USERNAME` before the colon.
///
/// `None` if `datagram` is not a STUN message with a `USERNAME` attribute (RFC 8489: 20-byte
/// header with the magic cookie `0x2112A442` at offset 4; attributes are type, length, value,
/// padded to 4 bytes).
pub(crate) fn stun_ufrag(datagram: &[u8]) -> Option<&str> {
    let header = datagram.get(..20)?;
    // The two most significant bits of a STUN message are zero (RFC 8489 section 5), which also
    // tells it apart from DTLS (20..=63) and RTP/RTCP (128..=191).
    if header.first()? & 0xC0 != 0 || header.get(4..8)? != [0x21, 0x12, 0xA4, 0x42] {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([*header.get(2)?, *header.get(3)?]));
    let mut attributes = datagram.get(20..20usize.checked_add(length)?)?;
    while attributes.len() >= 4 {
        let kind = u16::from_be_bytes([*attributes.first()?, *attributes.get(1)?]);
        let size = usize::from(u16::from_be_bytes([
            *attributes.get(2)?,
            *attributes.get(3)?,
        ]));
        let value = attributes.get(4..4usize.checked_add(size)?)?;
        if kind == 0x0006 {
            let username = std::str::from_utf8(value).ok()?;
            return username.split(':').next();
        }
        let padded = size.checked_add(3)? & !3;
        attributes = attributes.get(4usize.checked_add(padded)?..)?;
    }
    None
}

/// Reads the shared UDP socket until cancelled and routes every datagram to its call.
pub(crate) async fn udp_loop(shared: Arc<Shared>) {
    let mut buffer = vec![0u8; RECV_BUFFER];
    loop {
        let received = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            received = shared.socket.recv_from(&mut buffer) => received,
        };
        match received {
            Ok((length, source)) => {
                let received = shared.clock.now();
                // An exactly sized copy (about 100 to 1200 bytes): a queued datagram must not
                // keep a big shared receive buffer alive, or a stalled call would pin up to
                // queue-length times the buffer size.
                let data = match buffer.get(..length) {
                    Some(datagram) if length <= MAX_DATAGRAM => Bytes::copy_from_slice(datagram),
                    _ => {
                        Counters::bump(&shared.stats.datagrams_in);
                        Counters::bump(&shared.stats.datagrams_unrouted);
                        continue;
                    }
                };
                route(&shared, source, received, data).await;
            }
            Err(error) => {
                // A reset (Windows reports ICMP "port unreachable" of an earlier send here) is
                // not a socket failure.
                if error.kind() != std::io::ErrorKind::ConnectionReset {
                    tracing::warn!(%error, "webrtc udp receive failed");
                    tokio::select! {
                        biased;
                        () = shared.cancel.cancelled() => return,
                        () = shared.clock.sleep(RECV_ERROR_BACKOFF) => {}
                    }
                }
            }
        }
    }
}

async fn route(shared: &Shared, source: SocketAddr, received: Instant, data: Bytes) {
    Counters::bump(&shared.stats.datagrams_in);
    let ufrag = stun_ufrag(&data);
    let queue = {
        let mut registry = shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        registry.route(source, ufrag)
    };
    match queue {
        // `DropOldest` never waits, so the reader is never held up by a slow call.
        Some(queue) => {
            let _ = queue.send(Datagram::udp(source, received, data)).await;
        }
        None => Counters::bump(&shared.stats.datagrams_unrouted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STUN binding request with the given USERNAME (and a trailing padded attribute).
    fn binding_request(username: &str) -> Vec<u8> {
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&0x0006u16.to_be_bytes());
        attrs.extend_from_slice(&u16::try_from(username.len()).unwrap().to_be_bytes());
        attrs.extend_from_slice(username.as_bytes());
        while attrs.len() % 4 != 0 {
            attrs.push(0);
        }
        // PRIORITY, to prove attributes after USERNAME do not matter.
        attrs.extend_from_slice(&0x0024u16.to_be_bytes());
        attrs.extend_from_slice(&4u16.to_be_bytes());
        attrs.extend_from_slice(&[0, 0, 0, 1]);
        let mut message = Vec::new();
        message.extend_from_slice(&0x0001u16.to_be_bytes());
        message.extend_from_slice(&u16::try_from(attrs.len()).unwrap().to_be_bytes());
        message.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        message.extend_from_slice(&[7; 12]);
        message.extend_from_slice(&attrs);
        message
    }

    #[test]
    fn the_local_ufrag_is_read_from_a_binding_request() {
        assert_eq!(stun_ufrag(&binding_request("abcd:wxyz")), Some("abcd"));
        // Padding: a 5-byte username is followed by 3 bytes of padding.
        assert_eq!(stun_ufrag(&binding_request("abcde:q")), Some("abcde"));
    }

    #[test]
    fn anything_else_has_no_ufrag() {
        assert_eq!(stun_ufrag(&[]), None);
        assert_eq!(stun_ufrag(&[0u8; 19]), None);
        // RTP (first bits 10), DTLS (content type 22) and a STUN header with a wrong cookie.
        let mut rtp = vec![0x80, 111, 0, 1];
        rtp.resize(40, 0);
        assert_eq!(stun_ufrag(&rtp), None);
        let mut dtls = vec![22, 0xfe, 0xfd];
        dtls.resize(40, 0);
        assert_eq!(stun_ufrag(&dtls), None);
        let mut cookie = binding_request("a:b");
        cookie[4] = 0;
        assert_eq!(stun_ufrag(&cookie), None);
        // Length claiming more than the datagram holds, and an attribute running off the end.
        let mut long = binding_request("a:b");
        long[3] = 200;
        assert_eq!(stun_ufrag(&long), None);
        let mut cut = binding_request("a:b");
        cut.truncate(24);
        cut[3] = 4;
        assert_eq!(stun_ufrag(&cut), None);
        // No USERNAME at all.
        let mut bare = Vec::new();
        bare.extend_from_slice(&0x0001u16.to_be_bytes());
        bare.extend_from_slice(&0u16.to_be_bytes());
        bare.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        bare.extend_from_slice(&[0; 12]);
        assert_eq!(stun_ufrag(&bare), None);
    }

    fn call(registry: &mut Registry, id: &str, ufrag: &str) -> channel::Receiver<Datagram> {
        let (tx, rx) = channel::bounded(4);
        registry.insert(
            id.to_owned(),
            ufrag.to_owned(),
            CancellationToken::new(),
            Arc::new(AtomicBool::new(false)),
            None,
            tx,
        );
        rx
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn a_stun_ufrag_binds_a_source_and_later_datagrams_follow_it() {
        let mut registry = Registry::default();
        let _a = call(&mut registry, "a", "ufragA");
        let _b = call(&mut registry, "b", "ufragB");
        assert!(registry.route(addr(1), None).is_none(), "unknown source");
        assert!(
            registry.route(addr(1), Some("nobody")).is_none(),
            "unknown ufrag"
        );
        assert!(registry.route(addr(1), Some("ufragA")).is_some());
        // Now source 1 needs no ufrag.
        assert!(registry.route(addr(1), None).is_some());
        assert!(registry.route(addr(2), None).is_none());
    }

    #[test]
    fn removing_a_call_removes_its_routes() {
        let mut registry = Registry::default();
        let _a = call(&mut registry, "a", "ufragA");
        assert!(registry.route(addr(1), Some("ufragA")).is_some());
        assert_eq!(registry.len(), 1);
        registry.remove("a");
        assert_eq!(registry.len(), 0);
        assert!(registry.route(addr(1), None).is_none());
        assert!(registry.route(addr(1), Some("ufragA")).is_none());
        registry.remove("a");
    }

    #[test]
    fn a_call_remembers_a_bounded_number_of_sources_and_a_source_can_move() {
        let mut registry = Registry::default();
        let _a = call(&mut registry, "a", "ufragA");
        let _b = call(&mut registry, "b", "ufragB");
        for port in 1..=20 {
            assert!(registry.route(addr(port), Some("ufragA")).is_some());
        }
        // Only the newest MAX_ADDRS_PER_CALL sources still route.
        assert!(registry.route(addr(1), None).is_none());
        assert!(registry.route(addr(20), None).is_some());
        assert!(registry.by_addr.len() <= MAX_ADDRS_PER_CALL);
    }

    #[test]
    fn calls_per_address_are_counted_and_limited() {
        let mut registry = Registry::default();
        let ip: IpAddr = [10, 0, 0, 1].into();
        let other: IpAddr = [10, 0, 0, 2].into();
        assert!(registry.reserve_ip(ip, Some(2)));
        assert!(registry.reserve_ip(ip, Some(2)));
        assert!(
            !registry.reserve_ip(ip, Some(2)),
            "third call from one address"
        );
        assert!(
            registry.reserve_ip(other, Some(2)),
            "another address is separate"
        );
        registry.release_ip(ip);
        assert!(registry.reserve_ip(ip, Some(2)));
        // No limit: always admitted, and the count still balances.
        for _ in 0..100 {
            assert!(registry.reserve_ip(other, None));
        }
        for _ in 0..101 {
            registry.release_ip(other);
        }
        registry.release_ip(other);
        assert!(!registry.per_ip.contains_key(&other));
    }

    #[test]
    fn the_call_cap_is_keyed_like_the_offer_limiter() {
        let mut registry = Registry::default();
        // Addresses of one IPv6 /64 share a cap.
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let elsewhere: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(registry.reserve_ip(a, Some(2)));
        assert!(registry.reserve_ip(b, Some(2)));
        assert!(!registry.reserve_ip(a, Some(2)));
        assert!(!registry.reserve_ip("2001:db8:1:2:1:2:3:4".parse().unwrap(), Some(2)));
        assert!(registry.reserve_ip(elsewhere, Some(2)), "another /64");
        // The release from any address of the prefix frees a slot of the prefix.
        registry.release_ip(b);
        assert!(registry.reserve_ip(a, Some(2)));
        // IPv4-mapped IPv6 counts as the IPv4 address.
        let v4: IpAddr = [198, 51, 100, 1].into();
        let mapped: IpAddr = "::ffff:198.51.100.1".parse().unwrap();
        assert!(registry.reserve_ip(v4, Some(1)));
        assert!(!registry.reserve_ip(mapped, Some(1)));
        registry.release_ip(mapped);
        assert!(registry.reserve_ip(v4, Some(1)));
        assert_eq!(registry.per_ip.len(), 3);
    }

    fn owner(registry: &Registry, source: SocketAddr) -> Option<&str> {
        registry.by_addr.get(&source).map(String::as_str)
    }

    #[test]
    fn a_stun_ufrag_moves_a_source_to_the_call_it_names() {
        // Two calls from one client 5-tuple (a fixed source port, or a quick reconnect): the new
        // call's STUN request carries the new call's ufrag and must reach the new call although
        // the old one is still alive and owns the source.
        let mut registry = Registry::default();
        let _a = call(&mut registry, "a", "ufragA");
        let _b = call(&mut registry, "b", "ufragB");
        registry.route(addr(5), Some("ufragA")).unwrap();
        assert_eq!(owner(&registry, addr(5)), Some("a"));
        registry.route(addr(5), Some("ufragB")).unwrap();
        assert_eq!(owner(&registry, addr(5)), Some("b"));
        // The old call no longer lists the source (so its removal cannot unroute call b).
        assert!(registry.sessions["a"].addrs.is_empty());
        registry.remove("a");
        assert_eq!(owner(&registry, addr(5)), Some("b"));
        assert!(registry.route(addr(5), None).is_some());
        // Unrelated traffic without a ufrag, and an unknown ufrag, keep the current owner.
        registry.route(addr(5), Some("stale")).unwrap();
        assert_eq!(owner(&registry, addr(5)), Some("b"));
    }

    #[tokio::test]
    async fn the_datagram_reaches_the_queue_of_the_named_call() {
        let mut registry = Registry::default();
        let mut a = call(&mut registry, "a", "ufragA");
        let mut b = call(&mut registry, "b", "ufragB");
        let probe = |n: u8| {
            Datagram::udp(
                addr(5),
                brisvox_core::SystemClock::shared().now(),
                Bytes::from(vec![n]),
            )
        };
        registry
            .route(addr(5), Some("ufragA"))
            .unwrap()
            .send(probe(1))
            .await
            .unwrap();
        registry
            .route(addr(5), Some("ufragB"))
            .unwrap()
            .send(probe(2))
            .await
            .unwrap();
        registry
            .route(addr(5), None)
            .unwrap()
            .send(probe(3))
            .await
            .unwrap();
        assert_eq!(a.recv().await.unwrap().data[..], [1]);
        assert_eq!(b.recv().await.unwrap().data[..], [2]);
        assert_eq!(b.recv().await.unwrap().data[..], [3]);
    }

    #[test]
    fn a_source_that_carries_media_is_never_the_one_evicted() {
        // A browser with many local addresses (VPN, docker, IPv6 temporaries) sends STUN checks
        // from more than MAX_ADDRS_PER_CALL sources; the nominated one keeps sending RTP.
        let mut registry = Registry::default();
        let _a = call(&mut registry, "a", "ufragA");
        let nominated = addr(1000);
        registry.route(nominated, Some("ufragA")).unwrap();
        for port in 1..=(2 * MAX_ADDRS_PER_CALL as u16) {
            assert!(registry.route(addr(port), Some("ufragA")).is_some());
            // Media from the nominated source in between: refreshes its position.
            assert!(registry.route(nominated, None).is_some());
        }
        assert!(
            registry.route(nominated, None).is_some(),
            "the media source was evicted"
        );
        assert!(registry.by_addr.len() <= MAX_ADDRS_PER_CALL);
        assert_eq!(registry.sessions["a"].addrs.len(), registry.by_addr.len());
    }

    #[tokio::test]
    async fn queue_overflow_is_counted_while_the_call_is_alive_and_after_it_ends() {
        let mut registry = Registry::default();
        let (tx, _rx) = channel::bounded(4);
        registry.insert(
            "a".to_owned(),
            "ufragA".to_owned(),
            CancellationToken::new(),
            Arc::new(AtomicBool::new(false)),
            None,
            tx,
        );
        for n in 0..10u8 {
            let queue = registry.route(addr(7), Some("ufragA")).unwrap();
            queue
                .send(Datagram::udp(
                    addr(7),
                    brisvox_core::SystemClock::shared().now(),
                    Bytes::from(vec![n]),
                ))
                .await
                .unwrap();
        }
        // 10 sent into a queue of 4 that nobody reads: 6 evicted, visible without ending the call.
        assert_eq!(registry.live_overflow(), 6);
        assert_eq!(registry.remove("a"), 6);
        assert_eq!(registry.live_overflow(), 0);
    }
}
