//! Server-side TURN client (review 2026-10-13 section 3c-iii): a media path for hosts
//! without UDP ingress (Railway: HTTP plus one TCP proxy port).
//!
//! The server stays ICE-lite and answers with a **host-type candidate whose address is a TURN
//! relayed address**: `str0m` in ICE-lite mode refuses relayed local candidates, but it does not
//! care where a host candidate's address lives. Every packet the browser sends there reaches the
//! server through a TURN allocation the server holds on a managed TURN service (Cloudflare,
//! Twilio NTS or coturn), reached over **TCP or TLS** (`turns:...:443?transport=tcp`), which is the
//! one thing such a host can do outbound.
//!
//! ```text
//!   browser --UDP--> TURN relayed address ==Data indication / ChannelData==> TLS:443 --> this call
//!   browser <--UDP-- TURN relayed address <==Send indication / ChannelData== TLS:443 <-- this call
//! ```
//!
//! # One allocation per call
//! Each call opens its own TCP/TLS connection and allocation (RFC 8656 allocations are per
//! 5-tuple, so a shared one would need a demultiplexer, put every call behind one connection's
//! head-of-line blocking and make a single bad frame everybody's problem). The price is one TLS
//! connection and one allocation per call at the TURN provider; calls are isolated.
//!
//! # Call setup (before the answer is sent)
//! 1. credentials from the same [`TurnService`](super::turn_creds) that serves the browser;
//! 2. connect (DNS, TCP, TLS), `Allocate` (the long-term credential dance is the client crate's);
//! 3. `CreatePermission` for **every IP in the offer's candidates** (relay, srflx, host; same
//!    address family as the allocation; at most [`RelayConfig::max_permissions`]) *before* the
//!    answer goes out: the browser is the controlling agent and sends the first connectivity
//!    check, and a TURN server silently drops data from peers without a permission;
//! 4. the relayed address is added to the call's `Rtc` as a host candidate and goes into the SDP.
//!
//! All of it is bounded by [`RelayConfig::setup_timeout`] (default 3 s). On failure the call is
//! answered with its normal candidates (logged, counted in `relay_setup_failures`), or refused
//! with `503` when [`RelayConfig::only`] says there are no other candidates (the single-use
//! session token of such a refusal is refunded: the call never started).
//!
//! A setup that ends in an error releases its half-open allocation with a `Refresh` of lifetime 0
//! from a tracked task of its own ([`RelayConfig::release_timeout`]), so the release is not
//! cut short by the setup budget. A setup that is **cut off by the setup timeout** has no such
//! chance: its connection is closed without a `Refresh`, and the allocation is freed by the TURN
//! server when it notices the closed connection (coturn does; an allocation belongs to the
//! 5-tuple of its TCP connection, RFC 8656) or, failing that, when its lifetime (default 10
//! minutes) runs out unrefreshed. At most one such allocation per timed-out setup.
//!
//! ## Permission bounds
//! The permissions are created for the offer's candidate IPs, which the *client* supplies. They
//! are bounded: at most [`RelayConfig::max_permissions`] (default 8), only IPs of the
//! allocation's address family, and never an unspecified, multicast, broadcast, loopback or
//! link-local address (those last two only with [`RelayConfig::allow_local_peers`], for tests).
//! A permission only lets the TURN server pass packets from that IP to the relayed address; it
//! costs a permission on the operator's TURN quota, nothing else, and the server's `str0m`
//! only ever sends to peers whose ICE check it verified (a permission for an address the server
//! never talks to is inert).
//!
//! A peer address outside the offer's candidates (a peer-reflexive address behind a different
//! NAT mapping *and* a different IP) has no permission and is dropped by the TURN server; a
//! permission can only be added on demand after a packet has arrived, which it never does. The
//! bundled pages send non-trickle offers (gathering complete), so the offer holds the candidates.
//!
//! ## Trust in the TURN URLs
//! The server opens an **outbound connection to whatever host the TURN URL names**. The URLs
//! come from the operator's configuration (coturn) or from the TURN provider's API answer
//! (Cloudflare, Twilio) over TLS; they are trusted. A hostile or compromised provider response
//! could point the connection at an internal host or port (an SSRF: the connection speaks TLS
//! or STUN-framed TCP, and nothing of the answer is returned to the caller). Operators who
//! cannot trust the provider path should restrict the server's egress.
//!
//! DNS for the TURN host runs on tokio's blocking pool, so concurrent lookups are capped at
//! [`DNS_MAX_CONCURRENT`] (an extra call waits for a slot inside its setup budget) and a
//! resolved address is cached for [`DNS_CACHE_TTL`] per host: a burst of calls costs one lookup.
//! Every resolved address (IPv4 first, at most [`MAX_CONNECT_ADDRS`]) is tried in turn within the
//! budget.
//!
//! # Steady state
//! One task per call owns the connection and the sans-IO client ([`turn_client_proto`]); it
//! unwraps Data indications and ChannelData into [`Datagram`]s with `local` set to the relayed
//! address (the call's inbound queue, drop-oldest, the queue UDP uses), and wraps the call's
//! outgoing packets (a bounded drop-oldest queue; nothing ever waits on this socket) in
//! ChannelData (a channel is bound on the first packet to a peer; up to [`MAX_CHANNELS`] peers,
//! then Send indications). The client crate refreshes the allocation, permissions and channels on
//! the injected clock. When the call ends (or the server shuts down) the task sends `Refresh`
//! with lifetime 0 and closes, waiting at most [`RelayConfig::release_timeout`].
//!
//! # Bounds and failure
//! - Frames from the TURN server are split off the byte stream here, before the client crate
//!   sees them: a frame that is not STUN or ChannelData, a bad length, or one above
//!   [`RelayConfig::max_frame`] ends *this* allocation (`relay_protocol_errors`); the call goes
//!   on over whatever other path it has, and no other call is touched.
//! - A write that takes longer than [`RelayConfig::write_timeout`] ends the allocation. The
//!   task does not observe a stop request while such a write is blocked: it ends within
//!   `write_timeout` at the latest.
//! - Both queues hold [`RelayConfig::queue`] packets (default 20, 400 ms of audio: older audio
//!   is worthless to a live call, and a deep queue only turns a slow connection into latency);
//!   per outbound packet one buffer is copied (this path is a fallback, like ICE-TCP) and up to
//!   [`WRITE_BATCH`] queued packets go out in one write. The time a packet waited in the queue
//!   and slow writes are counted in `WebRtcStats` (`relay_queue_dwell_max_us`,
//!   `relay_queue_slow`, `relay_write_max_us`, `relay_slow_writes`).
//!
//! # Secrets and logs
//! The credentials are never logged by this module or put in an error, and [`Target`] redacts
//! the password in its `Debug` output. The client crate's own `Debug` output
//! (`TurnCredentials`, the client, its config) *does* hold the password in clear text, so none
//! of it is ever formatted here. The crates' `tracing` output (`turn_client_proto`, `stun_proto`,
//! `turn_types`) was checked with a log-capture test at `trace` over a whole call: it never
//! contained the password or the shared secret, but at `trace` it logs every STUN message it
//! *receives* in full (nonce, realm, the response's `MESSAGE-INTEGRITY` HMAC), and at `info`
//! it logs on every poll. That is credential-adjacent noise: the server's log filter
//! drops those targets below `warn` **whatever `RUST_LOG` says**
//! ([`turn_client_log_allowed`](brisvox_core::telemetry::turn_client_log_allowed); the opt-out is
//! `BRISVOX_LOG_TURN_CLIENT=1`). **Embedders that install their own subscriber must apply the
//! same filter** (re-exported as `brisvox_transport::webrtc::turn_client_log_allowed`).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::{Clock, Secret, SharedClock};
use bytes::Bytes;
use futures::FutureExt as _;
use rustls::pki_types::{CertificateDer, ServerName};
use str0m::net::Protocol;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;
use tokio_util::sync::{CancellationToken, DropGuard};
use turn_client_proto::api::{
    TurnClientApi as _, TurnConfig as ProtoConfig, TurnEvent, TurnPollRet, TurnRecvRet,
};
use turn_client_proto::prelude::DelayedTransmitBuild as _;
use turn_client_proto::stun::Instant as ProtoInstant;
use turn_client_proto::stun::agent::Transmit as ProtoTransmit;
use turn_client_proto::stun::auth::Feature;
use turn_client_proto::tcp::TurnClientTcp;
use turn_client_proto::types::{TransportType, TurnCredentials};

use super::demux::Datagram;
use super::http::{timeout, write_bounded};
use super::turn_creds::IceServer;
use super::{Counters, Shared};
use crate::channel::{self, Overflow, Overflowing};
use crate::error::{Error, Result};

/// Peers a call binds a channel for; further peers are served with Send indications (a browser
/// has one or two candidate pairs in play, so this is a bound, not a limit that matters).
pub(crate) const MAX_CHANNELS: usize = 8;

/// Bytes read from the TURN connection at once.
const READ_BUFFER: usize = 4096;

/// Rounds of "poll the client, write what it produced" before the task yields back to the select
/// (each round continues only if something was written, so this is a safety bound).
const MAX_FLUSH_ROUNDS: usize = 16;

/// Most TURN-host lookups running at once (they occupy tokio's blocking pool).
pub(crate) const DNS_MAX_CONCURRENT: usize = 8;

/// How long a resolved TURN host is remembered.
pub(crate) const DNS_CACHE_TTL: Duration = Duration::from_secs(60);

/// Hosts remembered at once (TURN URLs come from configuration or the provider: a handful).
const DNS_CACHE_ENTRIES: usize = 16;

/// Resolved addresses tried per call.
pub(crate) const MAX_CONNECT_ADDRS: usize = 4;

/// Queued packets one write carries at most.
pub(crate) const WRITE_BATCH: usize = 16;

/// A packet that waited longer than this in the relay queue counts in `relay_queue_slow`.
const SLOW_DWELL: Duration = Duration::from_millis(40);

/// A write that took longer than this counts in `relay_slow_writes`.
const SLOW_WRITE: Duration = Duration::from_millis(20);

/// Default port of a `turn:` URL (RFC 7064).
const DEFAULT_TURN_PORT: u16 = 3478;
/// Default port of a `turns:` URL (RFC 7064).
const DEFAULT_TURNS_PORT: u16 = 5349;

/// Tuning of the server-side TURN client (`WebRtcConfig::turn_relay`). It uses the TURN provider
/// configured in [`WebRtcConfig::turn`](super::WebRtcConfig::turn): the credentials the browser
/// gets also open the server's own allocation.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// There is no usable direct address: the answer carries the relayed candidate (and the
    /// ICE-TCP one, if configured) but no UDP host candidate, `candidate_ip` is not required, and
    /// a failed allocation refuses the offer (`503`) instead of answering with candidates that
    /// cannot work. `false`: the relayed candidate is added next to the direct ones (a lower
    /// priority, so a working direct path wins) and a failed allocation only costs the fallback.
    pub only: bool,
    /// Longest the whole setup may take (credentials, DNS, TCP, TLS, `Allocate`,
    /// `CreatePermission`s) before the call is answered without a relay. Default 3 s.
    pub setup_timeout: Duration,
    /// Offer IPs a permission is created for, 1 to 32. Default 8.
    pub max_permissions: usize,
    /// Packets queued per direction (drop-oldest). 8 to 4096. Default 20 (400 ms of 20 ms audio
    /// frames: a slow TURN connection costs the oldest audio, never a growing delay).
    pub queue: usize,
    /// Largest frame accepted from the TURN server, in bytes (a STUN message or ChannelData
    /// with padding). 512 to 65535. Default 4096.
    pub max_frame: usize,
    /// Longest a write to the TURN server may take before the allocation is dropped. Default 2 s.
    pub write_timeout: Duration,
    /// Longest the `Refresh` with lifetime 0 at the end of a call waits for its answer.
    /// Default 1 s.
    pub release_timeout: Duration,
    /// Extra trust anchors for the TLS connection to the TURN server (DER certificates), on top
    /// of the compiled-in webpki roots: a private CA, or a test certificate. Default none.
    pub extra_roots: Vec<Vec<u8>>,
    /// Also create permissions for loopback and link-local candidate IPs of the offer. Such a
    /// peer cannot be reached through a TURN server on the internet; it only matters when the
    /// TURN server runs on the same host (tests). Default `false`.
    pub allow_local_peers: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            only: false,
            setup_timeout: Duration::from_secs(3),
            max_permissions: 8,
            queue: 20,
            max_frame: 4096,
            write_timeout: Duration::from_secs(2),
            release_timeout: Duration::from_secs(1),
            extra_roots: Vec::new(),
            allow_local_peers: false,
        }
    }
}

impl RelayConfig {
    /// Checks the configuration.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first broken rule.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(format!("webrtc turn_relay: {what}")));
        if self.setup_timeout.is_zero()
            || self.write_timeout.is_zero()
            || self.release_timeout.is_zero()
        {
            return bad(
                "setup_timeout, write_timeout and release_timeout must be longer than zero",
            );
        }
        if !(1..=32).contains(&self.max_permissions) {
            return bad("max_permissions must be 1 to 32");
        }
        if !(8..=4_096).contains(&self.queue) {
            return bad("queue must be 8 to 4096");
        }
        if !(512..=65_535).contains(&self.max_frame) {
            return bad("max_frame must be 512 to 65535");
        }
        Ok(())
    }
}

/// Why a relay could not be set up. The text never holds a URL's credentials.
#[derive(Debug)]
pub(crate) struct RelayError(String);

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn fail<T>(what: impl Into<String>) -> std::result::Result<T, RelayError> {
    Err(RelayError(what.into()))
}

/// The TURN client of one server: its configuration and the TLS settings, built once.
pub(crate) struct RelayService {
    config: RelayConfig,
    tls: TlsConnector,
    /// Lookups running at once ([`DNS_MAX_CONCURRENT`] permits).
    dns_slots: Arc<Semaphore>,
    /// Resolved TURN hosts: `host -> (valid until, addresses)`.
    dns_cache: Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>,
}

impl RelayService {
    /// Builds the service (the TLS configuration with the `ring` provider and the webpki roots).
    pub(crate) fn new(config: RelayConfig) -> Result<Self> {
        config.validate()?;
        let mut roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for der in &config.extra_roots {
            roots
                .add(CertificateDer::from(der.clone()))
                .map_err(|e| Error::Config(format!("webrtc turn_relay: extra root: {e}")))?;
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Config(format!("webrtc turn_relay: TLS setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            config,
            tls: TlsConnector::from(Arc::new(tls)),
            dns_slots: Arc::new(Semaphore::new(DNS_MAX_CONCURRENT)),
            dns_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Whether there is no direct address to advertise ([`RelayConfig::only`]).
    pub(crate) fn only(&self) -> bool {
        self.config.only
    }

    /// Opens the relay of the call `sid` whose offer is `offer`: connection, allocation and the
    /// permissions for the offer's peers, all within [`RelayConfig::setup_timeout`]. The task
    /// that serves it starts here, feeding `inbound` (the call's datagram queue) and ending when
    /// the returned [`Relay`] is dropped.
    pub(crate) async fn open(
        &self,
        shared: &Arc<Shared>,
        sid: &str,
        offer: &str,
        inbound: channel::Sender<Datagram>,
    ) -> std::result::Result<Relay, RelayError> {
        let peers = offer_peer_ips(offer, self.config.allow_local_peers);
        if peers.is_empty() {
            return fail(
                "the offer has no candidate with an IP address (mDNS-only or empty: the page \
                 must wait for ICE gathering to complete)",
            );
        }
        let setup = self.open_inner(shared, sid, peers, inbound);
        match timeout(&*shared.clock, self.config.setup_timeout, setup).await {
            Ok(result) => result,
            Err(_) => fail("timed out"),
        }
    }

    async fn open_inner(
        &self,
        shared: &Arc<Shared>,
        sid: &str,
        peers: Vec<IpAddr>,
        inbound: channel::Sender<Datagram>,
    ) -> std::result::Result<Relay, RelayError> {
        let Some(turn) = &shared.turn else {
            return fail("no TURN provider configured");
        };
        let servers = turn.servers_bounded(sid, self.config.setup_timeout).await;
        let Some(target) = pick_target(&servers) else {
            return fail("the TURN provider offers no turn:/turns: URL over TCP with credentials");
        };
        let (stream, local, remote) = self.connect(shared, &target).await?;
        let mut proto = ProtoConfig::new(TurnCredentials::new(
            &target.username,
            target.password.expose(),
        ));
        proto.set_allocation_transport(TransportType::Udp);
        // RFC 8489 USERHASH hides the user name from eavesdroppers when a server supports it;
        // coturn and the managed services identify the REST credential by USERNAME, and a
        // TLS connection already hides it.
        proto.set_anonymous_username(Feature::Disabled);
        let (reader, writer) = tokio::io::split(stream);
        let origin = shared.clock.now();
        let mut session = Session {
            shared: Arc::clone(shared),
            client: TurnClientTcp::allocate(local, remote, proto),
            reader,
            writer,
            framer: Framer::new(self.config.max_frame),
            rbuf: vec![0; READ_BUFFER].into_boxed_slice(),
            wbuf: Vec::with_capacity(2048),
            origin,
            local,
            remote,
            inbound,
            relayed: None,
            alloc_failed: false,
            perms_pending: 0,
            perms_ok: 0,
            bound: HashSet::new(),
            write_timeout: self.config.write_timeout,
        };
        let relayed = match session.setup(&peers, self.config.max_permissions).await {
            Ok(relayed) => relayed,
            Err(error) => {
                // A half-open allocation (created but a permission failed) is released, from a
                // task of its own: the setup budget this future runs under must not cut the
                // `Refresh` short, and the answer must not wait for it.
                let release_timeout = self.config.release_timeout;
                shared.tracker.spawn(async move {
                    session.release(release_timeout).await;
                });
                return Err(error);
            }
        };
        Counters::bump(&shared.stats.relay_allocations);
        let (out_tx, out_rx) = channel::bounded(self.config.queue);
        let stop = shared.cancel.child_token();
        let guard = stop.clone().drop_guard();
        let release_timeout = self.config.release_timeout;
        let task_shared = Arc::clone(shared);
        shared.tracker.spawn(async move {
            Counters::bump(&task_shared.stats.relay_active);
            let result = AssertUnwindSafe(session.run(stop, out_rx, release_timeout))
                .catch_unwind()
                .await;
            if result.is_err() {
                tracing::error!("webrtc turn relay task panicked");
                Counters::bump(&task_shared.stats.relay_lost);
            }
            task_shared
                .stats
                .relay_active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
        Ok(Relay {
            relayed,
            out: out_tx,
            clock: Arc::clone(&shared.clock),
            _stop: guard,
        })
    }

    /// The addresses of `host`, IPv4 first: a literal as is, else from the cache or one lookup.
    ///
    /// Lookups run on tokio's blocking pool and cannot be cancelled, so each one is a task of its
    /// own that owns its [`DNS_MAX_CONCURRENT`] slot until the resolver returns, even when the
    /// call that asked has given up (the setup timeout). A call waits for a slot inside its
    /// setup budget.
    async fn resolve(
        &self,
        clock: &dyn Clock,
        host: &str,
    ) -> std::result::Result<Vec<IpAddr>, RelayError> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        if let Some(ips) = self.cached(clock.now(), host) {
            return Ok(ips);
        }
        let Ok(slot) = Arc::clone(&self.dns_slots).acquire_owned().await else {
            return fail("DNS is shut down");
        };
        // Another call may have resolved the host while this one waited for a slot.
        if let Some(ips) = self.cached(clock.now(), host) {
            return Ok(ips);
        }
        let name = host.to_owned();
        // Untracked on purpose: the resolver call is not cancellable, and a server shutdown must
        // not wait for it; the slot (not the task) is the bound.
        let lookup = tokio::spawn(async move {
            let _slot = slot;
            tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map(|found| {
                    let mut ips: Vec<IpAddr> = Vec::new();
                    for addr in found {
                        if !ips.contains(&addr.ip()) {
                            ips.push(addr.ip());
                        }
                    }
                    ips.sort_by_key(|ip| !ip.is_ipv4());
                    ips
                })
        });
        let ips = match lookup.await {
            Ok(Ok(ips)) => ips,
            Ok(Err(e)) => return fail(format!("DNS lookup failed: {}", e.kind())),
            Err(_) => return fail("DNS lookup task failed"),
        };
        if ips.is_empty() {
            return fail("DNS lookup returned no address");
        }
        let now = clock.now();
        let mut cache = self
            .dns_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= DNS_CACHE_ENTRIES {
            cache.retain(|_, (until, _)| *until > now);
        }
        if cache.len() < DNS_CACHE_ENTRIES
            && let Some(until) = now.checked_add(DNS_CACHE_TTL)
        {
            cache.insert(host.to_owned(), (until, ips.clone()));
        }
        Ok(ips)
    }

    fn cached(&self, now: Instant, host: &str) -> Option<Vec<IpAddr>> {
        let cache = self
            .dns_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(host)
            .filter(|(until, _)| *until > now)
            .map(|(_, ips)| ips.clone())
    }

    /// Forgets a host whose addresses did not connect (they may be stale).
    fn forget(&self, host: &str) {
        self.dns_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(host);
    }

    /// DNS, TCP and (for `turns:`) TLS to the TURN server. Resolved addresses are tried in
    /// turn, IPv4 first, at most [`MAX_CONNECT_ADDRS`]; all but the last get a third of the
    /// setup budget each, so one blackholed address cannot eat the budget of the others.
    async fn connect(
        &self,
        shared: &Shared,
        target: &Target,
    ) -> std::result::Result<(Box<dyn Io>, SocketAddr, SocketAddr), RelayError> {
        let clock = &*shared.clock;
        let ips = self.resolve(clock, &target.host).await?;
        let tried = ips.len().min(MAX_CONNECT_ADDRS);
        let mut failure = String::new();
        let mut connected = None;
        for (index, ip) in ips.iter().take(tried).enumerate() {
            let remote = SocketAddr::new(*ip, target.port);
            let last = index.saturating_add(1) >= tried;
            let share = if last {
                self.config.setup_timeout
            } else {
                self.config.setup_timeout / 3
            };
            match timeout(clock, share, TcpStream::connect(remote)).await {
                Ok(Ok(tcp)) => {
                    connected = Some((tcp, remote));
                    break;
                }
                Ok(Err(e)) => failure = format!("TCP connect failed: {}", e.kind()),
                Err(_) => failure = "TCP connect timed out".to_owned(),
            }
        }
        let Some((tcp, remote)) = connected else {
            self.forget(&target.host);
            return fail(failure);
        };
        // Small STUN and media packets must not wait for Nagle.
        let _ = tcp.set_nodelay(true);
        let local = match tcp.local_addr() {
            Ok(local) => local,
            Err(e) => return fail(format!("no local address: {}", e.kind())),
        };
        if !target.tls {
            return Ok((Box::new(tcp), local, remote));
        }
        let Ok(name) = ServerName::try_from(target.host.clone()) else {
            return fail("the TURN host is not a valid TLS server name");
        };
        match self.tls.connect(name, tcp).await {
            Ok(tls) => Ok((Box::new(tls), local, remote)),
            Err(e) => fail(format!("TLS handshake failed: {e}")),
        }
    }
}

/// Relay-only mode, and the relay could not be set up: the offer is refused (`503`).
#[derive(Debug)]
pub(crate) struct RelayUnavailable;

/// Opens the relay of the call `sid` for `offer`, when the server has the feature on.
///
/// - `Ok(Some(relay))`: the relayed address is ready; the permissions for the offer's peers exist.
/// - `Ok(None)`: no relay: not configured, the offer is malformed (the normal path refuses it with
///   `400`), or the setup failed and the call still has its direct candidates (logged, counted).
/// - `Err(RelayUnavailable)`: the setup failed in relay-only mode, where nothing else could work.
pub(crate) async fn open_for_offer(
    shared: &Arc<Shared>,
    sid: &str,
    offer: &str,
    inbound: &channel::Sender<Datagram>,
) -> std::result::Result<Option<Relay>, RelayUnavailable> {
    let Some(service) = &shared.relay else {
        return Ok(None);
    };
    // Do not spend a TURN allocation on an offer that is going to be refused anyway.
    if super::signaling::check_offer_shape(offer).is_err() {
        return Ok(None);
    }
    match service.open(shared, sid, offer, inbound.clone()).await {
        Ok(relay) => Ok(Some(relay)),
        Err(error) => {
            Counters::bump(&shared.stats.relay_setup_failures);
            if service.only() {
                tracing::warn!(%error, "webrtc turn relay setup failed; the offer is refused (relay-only)");
                Err(RelayUnavailable)
            } else {
                tracing::warn!(%error, "webrtc turn relay setup failed; the call goes on with its direct candidates");
                Ok(None)
            }
        }
    }
}

/// Anything a TURN connection can be: a TCP stream or a TLS stream over one.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// One outbound packet: where to (the browser's address) and what.
pub(crate) struct RelayOut {
    to: SocketAddr,
    data: Bytes,
    /// When the call's task queued it (injected clock): the queue dwell is measured against it.
    queued: Instant,
}

impl Overflowing for RelayOut {
    fn overflow(&self) -> Overflow {
        Overflow::DropOldest
    }
}

/// The relay of one call, as the call's task sees it. Dropping it ends the allocation (the
/// serving task releases it with a `Refresh` of lifetime 0).
pub(crate) struct Relay {
    relayed: SocketAddr,
    out: channel::Sender<RelayOut>,
    clock: SharedClock,
    _stop: DropGuard,
}

impl Relay {
    /// The relayed transport address: the call's host candidate.
    pub(crate) fn relayed(&self) -> SocketAddr {
        self.relayed
    }

    /// Queues `packet` for `to` through the relay. Never waits: a full queue evicts its oldest
    /// packet. `false` when the allocation has ended.
    pub(crate) fn send(&self, to: SocketAddr, packet: &[u8]) -> bool {
        let item = RelayOut {
            to,
            data: Bytes::copy_from_slice(packet),
            queued: self.clock.now(),
        };
        // `DropOldest` never pends: `now_or_never` always completes; `Err` means the task is gone.
        matches!(self.out.send(item).now_or_never(), Some(Ok(())))
    }
}

/// A TURN server to connect to, from one of the provider's URLs. `Debug` prints the password as
/// `[REDACTED]` ([`Secret`]).
#[derive(Debug)]
struct Target {
    host: String,
    port: u16,
    tls: bool,
    username: String,
    password: Secret<String>,
}

/// The best `turn:`/`turns:` URL over TCP among `servers`: TLS on port 443 (works through
/// firewalls that allow HTTPS), then any TLS port, then plain TCP. UDP-only URLs (and `turns:`
/// with `transport=udp`, i.e. DTLS) are skipped; so are servers without credentials.
fn pick_target(servers: &[IceServer]) -> Option<Target> {
    let mut best: Option<(u8, Target)> = None;
    for server in servers {
        let (Some(username), Some(credential)) = (&server.username, &server.credential) else {
            continue;
        };
        for url in &server.urls {
            let Some((rank, host, port, tls)) = parse_turn_url(url) else {
                continue;
            };
            if best.as_ref().is_none_or(|(have, _)| rank < *have) {
                best = Some((
                    rank,
                    Target {
                        host,
                        port,
                        tls,
                        username: username.clone(),
                        password: Secret::new(credential.expose().clone()),
                    },
                ));
            }
        }
    }
    best.map(|(_, target)| target)
}

/// `(rank, host, port, tls)` of a TURN URL usable over TCP (lower rank is better), else `None`.
fn parse_turn_url(url: &str) -> Option<(u8, String, u16, bool)> {
    let (scheme, rest) = url.split_once(':')?;
    let tls = match scheme {
        "turns" => true,
        "turn" => false,
        _ => return None,
    };
    let (host_port, query) = rest.split_once('?').unwrap_or((rest, ""));
    let host_port = host_port.trim_start_matches("//");
    let transport = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("transport="));
    match (transport, tls) {
        (Some(t), _) if t.eq_ignore_ascii_case("tcp") => {}
        // `turns:` without a transport is TLS over TCP (RFC 7064); `turn:` without one is UDP.
        (None, true) => {}
        _ => return None,
    }
    let default_port = if tls {
        DEFAULT_TURNS_PORT
    } else {
        DEFAULT_TURN_PORT
    };
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        (host, port)
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (host_port, default_port),
        }
    };
    if host.is_empty() || port == 0 {
        return None;
    }
    let rank = match (tls, port) {
        (true, 443) => 0,
        (true, _) => 1,
        (false, _) => 2,
    };
    Some((rank, host.to_owned(), port, tls))
}

/// The IPs of the UDP candidates in an SDP offer, relay and server-reflexive first (the addresses
/// a browser's packets are most likely to come from), then host, without duplicates. Names
/// (mDNS) and unusable addresses are skipped: unspecified, multicast, broadcast, and (unless
/// `allow_local`) loopback and link-local ones, for which a permission on a TURN server on the
/// internet is meaningless (and a request the operator's quota pays for).
pub(crate) fn offer_peer_ips(sdp: &str, allow_local: bool) -> Vec<IpAddr> {
    let mut found: Vec<(u8, IpAddr)> = Vec::new();
    for line in sdp.lines() {
        let Some(rest) = line.trim().strip_prefix("a=candidate:") else {
            continue;
        };
        // foundation component transport priority address port typ <type> ...
        let mut fields = rest.split(' ');
        let transport = fields.nth(2);
        let address = fields.nth(1);
        let kind = fields.nth(2);
        if !transport.is_some_and(|t| t.eq_ignore_ascii_case("udp")) {
            continue;
        }
        let Some(ip) = address.and_then(|a| a.parse::<IpAddr>().ok()) else {
            continue;
        };
        if !usable_peer(ip, allow_local) {
            continue;
        }
        let rank = match kind {
            Some("relay") => 0,
            Some("srflx") => 1,
            Some("prflx") => 2,
            _ => 3,
        };
        if !found.iter().any(|(_, seen)| *seen == ip) {
            found.push((rank, ip));
        }
    }
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, ip)| ip).collect()
}

/// `duration` in whole microseconds, saturating.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Whether a permission for the candidate address `ip` makes sense.
fn usable_peer(ip: IpAddr, allow_local: bool) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let (broadcast, local) = match ip {
        IpAddr::V4(v4) => (v4.is_broadcast(), v4.is_loopback() || v4.is_link_local()),
        IpAddr::V6(v6) => (false, v6.is_loopback() || v6.is_unicast_link_local()),
    };
    !broadcast && (allow_local || !local)
}

/// Splits the TURN connection's byte stream into frames (RFC 8656 section 3.1 / 12.5): a STUN
/// message (20-byte header, length in bytes 2..4, a multiple of 4) or ChannelData (4-byte header,
/// length in bytes 2..4, padded to a multiple of 4 on a stream). Anything else, or a frame above
/// the limit, is a protocol error. Bounded: never holds more than one frame plus one read.
struct Framer {
    buf: Vec<u8>,
    max: usize,
}

/// Why a TURN connection ended.
#[derive(Debug)]
enum Fail {
    /// Our side asked for the end (call over, server shutdown, call task gone).
    Stopped,
    /// The TURN server closed the connection, or the allocation expired.
    Closed,
    /// A socket error.
    Io(std::io::ErrorKind),
    /// A write did not finish in time.
    Timeout,
    /// Garbage or an oversize frame from the TURN server.
    Protocol(&'static str),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("stopped"),
            Self::Closed => f.write_str("the TURN server closed the allocation"),
            Self::Io(kind) => write!(f, "socket error: {kind}"),
            Self::Timeout => f.write_str("a write to the TURN server timed out"),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
        }
    }
}

impl Framer {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::with_capacity(READ_BUFFER),
            max,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The length of the first complete frame, `None` while it is incomplete.
    fn frame_len(&self) -> std::result::Result<Option<usize>, Fail> {
        let Ok([first, _, high, low]) = <[u8; 4]>::try_from(self.buf.get(..4).unwrap_or_default())
        else {
            return Ok(None);
        };
        let length = usize::from(u16::from_be_bytes([high, low]));
        let total = match first >> 6 {
            0 => {
                if length % 4 != 0 {
                    return Err(Fail::Protocol("STUN length is not a multiple of 4"));
                }
                if let Some(cookie) = self.buf.get(4..8)
                    && cookie != [0x21, 0x12, 0xA4, 0x42]
                {
                    return Err(Fail::Protocol("no STUN magic cookie"));
                }
                20usize.saturating_add(length)
            }
            1 => 4usize.saturating_add(length).saturating_add(3) & !3,
            _ => return Err(Fail::Protocol("neither STUN nor ChannelData")),
        };
        if total > self.max {
            return Err(Fail::Protocol("frame above the size limit"));
        }
        Ok((self.buf.len() >= total).then_some(total))
    }

    /// The part of the first frame (`total` bytes with its padding) the client crate should see:
    /// ChannelData without the stream padding (RFC 8656 section 12.5 requires the padding on
    /// TCP and TLS; the crate's parser expects `4 + length` bytes and would take the padding for
    /// the start of the next frame).
    fn unpadded(&self, total: usize) -> usize {
        match self.buf.get(..4) {
            Some(&[first, _, high, low]) if first >> 6 == 1 => {
                total.min(4usize.saturating_add(usize::from(u16::from_be_bytes([high, low]))))
            }
            _ => total,
        }
    }
}

/// One call's connection to the TURN server and the sans-IO client on it.
struct Session {
    shared: Arc<Shared>,
    client: TurnClientTcp,
    reader: ReadHalf<Box<dyn Io>>,
    writer: WriteHalf<Box<dyn Io>>,
    framer: Framer,
    rbuf: Box<[u8]>,
    wbuf: Vec<u8>,
    /// The origin of the client's time line: `ProtoInstant` is nanoseconds since this instant of
    /// the injected clock.
    origin: Instant,
    local: SocketAddr,
    remote: SocketAddr,
    inbound: channel::Sender<Datagram>,
    relayed: Option<SocketAddr>,
    alloc_failed: bool,
    perms_pending: usize,
    perms_ok: usize,
    /// Peers a channel is bound (or being bound) for; at most [`MAX_CHANNELS`].
    bound: HashSet<SocketAddr>,
    write_timeout: Duration,
}

impl Session {
    fn now(&self) -> ProtoInstant {
        let nanos = self
            .shared
            .clock
            .now()
            .saturating_duration_since(self.origin)
            .as_nanos();
        ProtoInstant::from_nanos(i64::try_from(nanos).unwrap_or(i64::MAX))
    }

    /// The clock instant of a client-time `at`.
    fn deadline(&self, at: ProtoInstant) -> Instant {
        let nanos = u64::try_from(at.as_nanos()).unwrap_or(0);
        brisvox_core::clock::deadline_after(self.origin, Duration::from_nanos(nanos))
    }

    /// Writes what the client has to send. `true` if anything was written.
    async fn flush(&mut self) -> std::result::Result<bool, Fail> {
        let now = self.now();
        while let Some(transmit) = self.client.poll_transmit(now) {
            self.wbuf.extend_from_slice(transmit.data.as_ref());
        }
        self.write_out().await
    }

    /// Writes `wbuf` (bounded by the write timeout). `true` if it held anything.
    async fn write_out(&mut self) -> std::result::Result<bool, Fail> {
        if self.wbuf.is_empty() {
            return Ok(false);
        }
        let clock = &*self.shared.clock;
        let started = clock.now();
        // No timer unless the socket would block (a media packet usually goes at once).
        let written = write_bounded(clock, self.write_timeout, &mut self.writer, &self.wbuf).await;
        let took = clock.now().saturating_duration_since(started);
        let stats = &self.shared.stats;
        Counters::max(&stats.relay_write_max_us, micros(took));
        if took > SLOW_WRITE {
            Counters::bump(&stats.relay_slow_writes);
        }
        self.wbuf.clear();
        match written {
            Ok(Ok(())) => Ok(true),
            Ok(Err(e)) => Err(Fail::Io(e.kind())),
            Err(_) => Err(Fail::Timeout),
        }
    }

    /// Progresses the client's timers and writes what falls due; returns when it wants to be
    /// woken next.
    async fn cycle(&mut self) -> std::result::Result<Instant, Fail> {
        for _ in 0..MAX_FLUSH_ROUNDS {
            let now = self.now();
            let wake = match self.client.poll(now) {
                TurnPollRet::WaitUntil(at) => self.deadline(at),
                TurnPollRet::Closed => return Err(Fail::Closed),
                // TCP allocations (RFC 6062) are never requested.
                TurnPollRet::AllocateTcpSocket { .. } | TurnPollRet::TcpClose { .. } => {
                    self.shared.clock.now()
                }
            };
            self.drain_events();
            if !self.flush().await? {
                return Ok(wake);
            }
        }
        Ok(self.shared.clock.now())
    }

    fn drain_events(&mut self) {
        while let Some(event) = self.client.poll_event() {
            match event {
                TurnEvent::AllocationCreated(TransportType::Udp, addr) => {
                    self.relayed = Some(addr);
                }
                TurnEvent::AllocationCreateFailed(_) => self.alloc_failed = true,
                TurnEvent::PermissionCreated(..) => {
                    self.perms_pending = self.perms_pending.saturating_sub(1);
                    self.perms_ok += 1;
                }
                TurnEvent::PermissionCreateFailed(..) => {
                    self.perms_pending = self.perms_pending.saturating_sub(1);
                    Counters::bump(&self.shared.stats.relay_permission_failures);
                }
                _ => {}
            }
        }
    }

    /// Allocation and permissions; returns the relayed address once at least one permission
    /// exists.
    async fn setup(
        &mut self,
        peers: &[IpAddr],
        max_permissions: usize,
    ) -> std::result::Result<SocketAddr, RelayError> {
        let mut requested = false;
        loop {
            let wake = self
                .cycle()
                .await
                .map_err(|e| RelayError(format!("allocation: {e}")))?;
            if self.alloc_failed {
                return fail("the TURN server refused the allocation");
            }
            if let Some(relayed) = self.relayed {
                if !requested {
                    requested = true;
                    let now = self.now();
                    for ip in peers
                        .iter()
                        .filter(|ip| ip.is_ipv4() == relayed.is_ipv4())
                        .take(max_permissions)
                    {
                        if self
                            .client
                            .create_permission(TransportType::Udp, *ip, now)
                            .is_ok()
                        {
                            self.perms_pending += 1;
                        }
                    }
                    if self.perms_pending == 0 {
                        return fail("no offer candidate has the relay's address family");
                    }
                    // Send the requests before waiting for their answers.
                    continue;
                }
                if self.perms_pending == 0 {
                    if self.perms_ok == 0 {
                        return fail("the TURN server refused every permission");
                    }
                    return Ok(relayed);
                }
            }
            // The client may schedule its next request for later (a retry after a challenge).
            let clock = Arc::clone(&self.shared.clock);
            let read = tokio::select! {
                read = self.reader.read(&mut self.rbuf) => read
                    .map_err(|e| RelayError(format!("read failed: {}", e.kind())))?,
                () = clock.sleep_until(wake) => continue,
            };
            if read == 0 {
                return fail("the TURN server closed the connection");
            }
            self.input(read)
                .map_err(|e| RelayError(format!("TURN server: {e}")))?;
        }
    }

    /// Feeds `read` bytes of `rbuf` through the framer and the client; peer data goes to the
    /// call's queue.
    fn input(&mut self, read: usize) -> std::result::Result<(), Fail> {
        self.framer.push(self.rbuf.get(..read).unwrap_or_default());
        while let Some(total) = self.framer.frame_len()? {
            let now = self.now();
            let Some(frame) = self.framer.buf.get(..self.framer.unpadded(total)) else {
                break;
            };
            let received = ProtoTransmit::new(frame, TransportType::Tcp, self.remote, self.local);
            match self.client.recv(received, now) {
                TurnRecvRet::PeerData(data) => {
                    deliver(
                        &self.shared,
                        &self.inbound,
                        self.relayed,
                        data.peer,
                        data.data(),
                    )?;
                }
                // A response to nothing we asked (a late answer to a retried request).
                TurnRecvRet::Ignored(_) => tracing::debug!("webrtc turn relay: unexpected frame"),
                TurnRecvRet::Handled | TurnRecvRet::PeerIcmp { .. } => {}
            }
            while let Some(data) = self.client.poll_recv(now) {
                deliver(
                    &self.shared,
                    &self.inbound,
                    self.relayed,
                    data.peer,
                    data.data(),
                )?;
            }
            self.framer.buf.drain(..total);
        }
        self.drain_events();
        Ok(())
    }

    /// Wraps a packet for `to` (ChannelData once a channel is bound, else a Send indication) and
    /// queues it in `wbuf`.
    fn queue_packet(&mut self, packet: RelayOut) {
        let dwell = self
            .shared
            .clock
            .now()
            .saturating_duration_since(packet.queued);
        Counters::max(&self.shared.stats.relay_queue_dwell_max_us, micros(dwell));
        if dwell > SLOW_DWELL {
            Counters::bump(&self.shared.stats.relay_queue_slow);
        }
        let now = self.now();
        if !self.bound.contains(&packet.to) && self.bound.len() < MAX_CHANNELS {
            self.bound.insert(packet.to);
            // Failure (a channel for this peer cannot be made now) leaves Send indications.
            let _ = self.client.bind_channel(TransportType::Udp, packet.to, now);
        }
        let stats = &self.shared.stats;
        match self
            .client
            .send_to(TransportType::Udp, packet.to, packet.data, now)
        {
            Ok(Some(build)) => {
                let length = build.data.len();
                let start = self.wbuf.len();
                self.wbuf.resize(start.saturating_add(length), 0);
                let written = self
                    .wbuf
                    .get_mut(start..)
                    .map_or(0, |room| build.data.write_into(room));
                let end = start.saturating_add(written);
                self.wbuf.truncate(end);
                // ChannelData must be padded to a multiple of 4 on TCP and TLS (RFC 8656
                // section 12.5); the client crate writes it unpadded.
                if self.wbuf.get(start).is_some_and(|first| first >> 6 == 1) {
                    self.wbuf
                        .resize(end.saturating_add(written.wrapping_neg() & 3), 0);
                }
                Counters::bump(&stats.relay_packets_out);
            }
            Ok(None) => {}
            Err(_) => Counters::bump(&stats.relay_packets_dropped),
        }
    }

    /// Serves the call until it ends or the connection fails.
    async fn run(
        mut self,
        stop: CancellationToken,
        mut out: channel::Receiver<RelayOut>,
        release_timeout: Duration,
    ) {
        let clock = Arc::clone(&self.shared.clock);
        let sleep = clock.sleep(Duration::from_secs(3_600));
        tokio::pin!(sleep);
        // The wake time `sleep` is armed for (`None` once it has fired): rearmed only when the
        // client's next wake changes, not after every packet.
        let mut armed: Option<Instant> = None;
        let end = loop {
            let wake = match self.cycle().await {
                Ok(wake) => wake,
                Err(fail) => break fail,
            };
            if armed != Some(wake) {
                sleep.as_mut().reset(wake);
                armed = Some(wake);
            }
            let step = tokio::select! {
                biased;
                () = stop.cancelled() => break Fail::Stopped,
                read = self.reader.read(&mut self.rbuf) => match read {
                    Ok(0) => break Fail::Closed,
                    Ok(n) => self.input(n),
                    Err(e) => break Fail::Io(e.kind()),
                },
                packet = out.recv() => match packet {
                    Some(packet) => {
                        self.queue_packet(packet);
                        // After a backlog (a stalled write) the queued packets go out in one
                        // write instead of one write each.
                        for _ in 1..WRITE_BATCH {
                            let Some(Some(more)) = out.recv().now_or_never() else {
                                break;
                            };
                            self.queue_packet(more);
                        }
                        Ok(())
                    }
                    None => break Fail::Stopped,
                },
                () = &mut sleep => {
                    armed = None;
                    Ok(())
                }
            };
            if let Err(fail) = step {
                break fail;
            }
        };
        match &end {
            Fail::Stopped => {
                // Packets queued before the end (the DTLS close_notify, RTCP BYE) still go out.
                while let Some(Some(packet)) = out.recv().now_or_never() {
                    self.queue_packet(packet);
                }
                let _ = self.write_out().await;
            }
            other => {
                Counters::bump(&self.shared.stats.relay_lost);
                if matches!(other, Fail::Protocol(_)) {
                    Counters::bump(&self.shared.stats.relay_protocol_errors);
                }
                tracing::warn!(reason = %other, "webrtc turn relay ended; the call continues without it");
            }
        }
        let evicted = out.stats();
        Counters::add(
            &self.shared.stats.relay_packets_dropped,
            evicted
                .dropped_oldest
                .saturating_add(evicted.dropped_newest),
        );
        if matches!(end, Fail::Stopped) {
            self.release(release_timeout).await;
        }
    }

    /// Best effort: `Refresh` with lifetime 0, wait (bounded) for the answer, close.
    async fn release(&mut self, within: Duration) {
        let clock = Arc::clone(&self.shared.clock);
        let now = self.now();
        if self.client.delete(now).is_err() {
            return;
        }
        let finish = async {
            loop {
                // The TURN client sends at most one request per 50 ms: a release right behind
                // another request (a hang-up just after the setup) is held back until `wake`,
                // so the wait for the answer must also wake for the client's own timer, or the
                // Refresh would only go out when `within` has run out.
                let Ok(wake) = self.cycle().await else {
                    return;
                };
                tokio::select! {
                    read = self.reader.read(&mut self.rbuf) => match read {
                        Ok(n) if n > 0 => {
                            if self.input(n).is_err() {
                                return;
                            }
                            // The answer to the Refresh closes the allocation in the client.
                            if matches!(self.client.poll(self.now()), TurnPollRet::Closed) {
                                return;
                            }
                        }
                        _ => return,
                    },
                    () = clock.sleep_until(wake) => {}
                }
            }
        };
        let _ = timeout(&*clock, within, finish).await;
        let _ = timeout(&*clock, within, self.writer.shutdown()).await;
    }
}

/// Hands a datagram the TURN server relayed to the call's inbound queue (drop-oldest). `Err`
/// when the call's queue is gone (the call ended).
fn deliver(
    shared: &Shared,
    inbound: &channel::Sender<Datagram>,
    relayed: Option<SocketAddr>,
    peer: SocketAddr,
    data: &[u8],
) -> std::result::Result<(), Fail> {
    let Some(relayed) = relayed else {
        return Ok(());
    };
    let datagram = Datagram {
        proto: Protocol::Udp,
        source: peer,
        received: shared.clock.now(),
        data: Bytes::copy_from_slice(data),
        reply: None,
        local: Some(relayed),
    };
    match inbound.send(datagram).now_or_never() {
        Some(Ok(())) => {
            Counters::bump(&shared.stats.relay_packets_in);
            Ok(())
        }
        _ => Err(Fail::Stopped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(urls: &[&str]) -> IceServer {
        IceServer {
            urls: urls.iter().map(|u| (*u).to_owned()).collect(),
            username: Some("user".to_owned()),
            credential: Some(Secret::new("pass".to_owned())),
        }
    }

    #[test]
    fn turn_urls_are_parsed_and_ranked() {
        assert_eq!(
            parse_turn_url("turns:turn.example.com:443?transport=tcp"),
            Some((0, "turn.example.com".to_owned(), 443, true))
        );
        assert_eq!(
            parse_turn_url("turns:turn.example.com"),
            Some((1, "turn.example.com".to_owned(), 5349, true))
        );
        assert_eq!(
            parse_turn_url("turn:10.0.0.1:80?transport=tcp"),
            Some((2, "10.0.0.1".to_owned(), 80, false))
        );
        assert_eq!(
            parse_turn_url("turns:[2001:db8::1]:443?transport=tcp"),
            Some((0, "2001:db8::1".to_owned(), 443, true))
        );
        // UDP (and DTLS) and non-TURN URLs cannot be used over a TCP connection.
        assert_eq!(parse_turn_url("turn:turn.example.com:3478"), None);
        assert_eq!(
            parse_turn_url("turn:turn.example.com:3478?transport=udp"),
            None
        );
        assert_eq!(
            parse_turn_url("turns:turn.example.com:5349?transport=udp"),
            None
        );
        assert_eq!(parse_turn_url("stun:stun.example.com:3478"), None);
        assert_eq!(parse_turn_url("turns::443?transport=tcp"), None);
        assert_eq!(parse_turn_url("turns:host:0?transport=tcp"), None);
        assert_eq!(parse_turn_url("turns:host:notaport"), None);
    }

    #[test]
    fn the_best_tcp_url_with_credentials_is_picked() {
        let servers = [
            server(&[
                "turn:turn.example.com:3478?transport=udp",
                "turn:turn.example.com:80?transport=tcp",
                "turns:turn.example.com:5349?transport=tcp",
                "turns:turn.example.com:443?transport=tcp",
            ]),
            IceServer {
                urls: vec!["turns:other.example.com:443?transport=tcp".to_owned()],
                username: None,
                credential: None,
            },
        ];
        let target = pick_target(&servers).unwrap();
        assert_eq!(target.host, "turn.example.com");
        assert_eq!(target.port, 443);
        assert!(target.tls);
        assert_eq!(target.username, "user");
        assert!(pick_target(&[server(&["turn:x:3478?transport=udp"])]).is_none());
        assert!(pick_target(&[]).is_none());
    }

    #[test]
    fn offer_candidates_become_peer_ips_relay_first_without_names_or_tcp() {
        let sdp = "v=0\r\n\
a=candidate:1 1 udp 2113937151 0a1b2c3d-4e5f-6789-abcd-ef0123456789.local 54321 typ host\r\n\
a=candidate:2 1 udp 2113937151 192.168.1.5 54322 typ host\r\n\
a=candidate:3 1 udp 1677729535 203.0.113.7 54323 typ srflx raddr 192.168.1.5 rport 54322\r\n\
a=candidate:4 1 udp 41885439 198.51.100.9 60000 typ relay raddr 203.0.113.7 rport 54323\r\n\
a=candidate:5 1 tcp 1518280447 192.168.1.5 9 typ host tcptype active\r\n\
a=candidate:6 1 udp 1677729535 203.0.113.7 54324 typ srflx\r\n\
a=candidate:7 1 udp 2113937151 2001:db8::1 54325 typ host\r\n\
a=candidate:8 1 udp 1 0.0.0.0 9 typ host\r\n";
        let ips: Vec<String> = offer_peer_ips(sdp, false)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            ips,
            ["198.51.100.9", "203.0.113.7", "192.168.1.5", "2001:db8::1"]
        );
        assert!(offer_peer_ips("v=0\r\n", false).is_empty());
    }

    #[test]
    fn loopback_link_local_broadcast_and_unspecified_peers_get_no_permission() {
        let sdp = "v=0\r\n\
a=candidate:1 1 udp 2113937151 127.0.0.1 1 typ host\r\n\
a=candidate:2 1 udp 2113937151 127.8.9.10 2 typ host\r\n\
a=candidate:3 1 udp 2113937151 ::1 3 typ host\r\n\
a=candidate:4 1 udp 2113937151 169.254.1.2 4 typ host\r\n\
a=candidate:5 1 udp 2113937151 fe80::1 5 typ host\r\n\
a=candidate:6 1 udp 2113937151 255.255.255.255 6 typ host\r\n\
a=candidate:7 1 udp 2113937151 :: 7 typ host\r\n\
a=candidate:8 1 udp 2113937151 224.0.0.251 8 typ host\r\n\
a=candidate:9 1 udp 2113937151 ff02::fb 9 typ host\r\n\
a=candidate:10 1 udp 2113937151 10.0.0.7 10 typ host\r\n\
a=candidate:11 1 udp 2113937151 203.0.113.9 11 typ srflx\r\n";
        let ips = |allow_local| -> Vec<String> {
            offer_peer_ips(sdp, allow_local)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        // Private (RFC 1918) addresses stay: a LAN peer is a real candidate.
        assert_eq!(ips(false), ["203.0.113.9", "10.0.0.7"]);
        // Tests (a TURN server on the same host) may ask for the local ones; never the others.
        assert_eq!(
            ips(true),
            [
                "203.0.113.9",
                "127.0.0.1",
                "127.8.9.10",
                "::1",
                "169.254.1.2",
                "fe80::1",
                "10.0.0.7"
            ]
        );
    }

    #[tokio::test]
    async fn a_resolved_host_is_cached_for_a_while_and_the_slot_comes_back() {
        let clock = brisvox_core::SystemClock::shared();
        let service = RelayService::new(RelayConfig::default()).unwrap();
        assert!(service.cached(clock.now(), "localhost").is_none());
        let first = service.resolve(&*clock, "localhost").await.unwrap();
        assert!(!first.is_empty());
        assert!(first.iter().all(|ip| ip.is_loopback()), "{first:?}");
        // The second answer comes from the cache; it is gone once the TTL has passed.
        assert_eq!(service.cached(clock.now(), "localhost"), Some(first));
        let later = clock.now() + DNS_CACHE_TTL + Duration::from_secs(1);
        assert!(service.cached(later, "localhost").is_none());
        service.forget("localhost");
        assert!(service.cached(clock.now(), "localhost").is_none());
        // An IP literal needs no lookup (and no slot).
        let literal = service.resolve(&*clock, "192.0.2.1").await.unwrap();
        assert_eq!(literal, [IpAddr::from([192, 0, 2, 1])]);
        assert_eq!(service.dns_slots.available_permits(), DNS_MAX_CONCURRENT);
    }

    #[test]
    fn the_debug_output_of_a_target_hides_the_password() {
        let target = pick_target(&[server(&["turns:turn.example.com:443?transport=tcp"])]).unwrap();
        assert_eq!(target.password.expose(), "pass");
        let shown = format!("{target:?}");
        assert!(!shown.contains("\"pass\""), "{shown}");
        assert!(shown.contains("REDACTED"), "{shown}");
    }

    #[test]
    fn channel_data_with_a_channel_number_outside_the_valid_range_is_framed_not_judged() {
        // The framer only splits; a channel number outside 0x4000..=0x7FFF is the client
        // crate's to refuse (it is not a bound channel). 0x4000 and 0x7FFF are accepted as
        // ChannelData by their top bits, 0x8000 and above are "neither STUN nor ChannelData".
        for (first, expect_ok) in [(0x40u8, true), (0x7F, true), (0x80, false), (0xC0, false)] {
            let mut framer = Framer::new(4096);
            framer.push(&[first, 0xFF, 0x00, 0x01, 9, 0, 0, 0]);
            let result = framer.frame_len();
            assert_eq!(result.is_ok(), expect_ok, "{first:#x}: {result:?}");
            if expect_ok {
                assert_eq!(result.unwrap(), Some(8));
            }
        }
    }

    #[test]
    fn a_channel_data_frame_split_inside_its_padding_waits_for_the_padding() {
        let mut framer = Framer::new(4096);
        // 5 bytes of data, 3 of padding: the frame is not complete until the last padding byte.
        framer.push(&[0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5]);
        assert_eq!(framer.frame_len().unwrap(), None);
        framer.push(&[0]);
        assert_eq!(framer.frame_len().unwrap(), None);
        framer.push(&[0]);
        assert_eq!(framer.frame_len().unwrap(), None);
        framer.push(&[0]);
        let total = framer.frame_len().unwrap().unwrap();
        assert_eq!((total, framer.unpadded(total)), (12, 9));
        // The next frame starts right after the padding, even when it arrived in the same read.
        framer.buf.drain(..total);
        framer.push(&stun_header(0));
        assert_eq!(framer.frame_len().unwrap(), Some(20));
    }

    #[test]
    fn a_zero_length_channel_data_frame_is_four_bytes() {
        let mut framer = Framer::new(4096);
        framer.push(&[0x40, 0x00, 0x00, 0x00]);
        assert_eq!(framer.frame_len().unwrap(), Some(4));
    }

    fn stun_header(length: u16) -> Vec<u8> {
        let mut frame = vec![0x00, 0x01];
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        frame.extend_from_slice(&[7; 12]);
        frame
    }

    #[test]
    fn the_framer_splits_stun_and_padded_channel_data_across_reads() {
        let mut framer = Framer::new(4096);
        // A 4-byte STUN attribute, then ChannelData with 5 bytes of data (3 bytes of padding).
        let mut stream = stun_header(4);
        stream.extend_from_slice(&[0, 0, 0, 0]);
        stream.extend_from_slice(&[0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0]);
        let total = stream.len();
        // Byte by byte: no frame until it is complete.
        let mut frames = Vec::new();
        for byte in &stream {
            framer.push(&[*byte]);
            while let Some(len) = framer.frame_len().unwrap() {
                frames.push(len);
                framer.buf.drain(..len);
            }
        }
        assert_eq!(frames, [24, 12]);
        assert_eq!(frames.iter().sum::<usize>(), total);
        assert!(framer.buf.is_empty());
    }

    #[test]
    fn channel_data_loses_its_stream_padding_before_the_client_crate_sees_it() {
        let mut framer = Framer::new(4096);
        // 5 bytes of data, 3 of padding, then a STUN message.
        framer.push(&[0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0]);
        framer.push(&stun_header(0));
        let first = framer.frame_len().unwrap().unwrap();
        assert_eq!((first, framer.unpadded(first)), (12, 9));
        framer.buf.drain(..first);
        let second = framer.frame_len().unwrap().unwrap();
        assert_eq!((second, framer.unpadded(second)), (20, 20));
    }

    #[test]
    fn the_framer_refuses_garbage_bad_lengths_and_oversize_frames() {
        let check = |bytes: &[u8], max: usize| {
            let mut framer = Framer::new(max);
            framer.push(bytes);
            framer.frame_len()
        };
        // Not STUN, not ChannelData (top bits 10 and 11).
        assert!(matches!(
            check(&[0x80, 0, 0, 4, 0, 0, 0, 0], 4096),
            Err(Fail::Protocol(_))
        ));
        assert!(matches!(
            check(&[0xC0, 0, 0, 4, 0, 0, 0, 0], 4096),
            Err(Fail::Protocol(_))
        ));
        // A STUN length that is not a multiple of 4, and a bad magic cookie.
        assert!(matches!(
            check(&stun_header(3), 4096),
            Err(Fail::Protocol(_))
        ));
        let mut cookie = stun_header(0);
        cookie[4] = 0;
        assert!(matches!(check(&cookie, 4096), Err(Fail::Protocol(_))));
        // Oversize, seen from the first four bytes (nothing is buffered for it).
        assert!(matches!(
            check(&[0x40, 0, 0xFF, 0xF0], 4096),
            Err(Fail::Protocol(_))
        ));
        assert!(matches!(
            check(&stun_header(8192), 4096),
            Err(Fail::Protocol(_))
        ));
        // Less than a header is just incomplete.
        assert!(matches!(check(&[0x40, 0, 0], 4096), Ok(None)));
        assert!(matches!(check(&[], 4096), Ok(None)));
    }

    #[test]
    fn the_relay_configuration_is_validated() {
        RelayConfig::default().validate().unwrap();
        for (config, needle) in [
            (
                RelayConfig {
                    setup_timeout: Duration::ZERO,
                    ..RelayConfig::default()
                },
                "setup_timeout",
            ),
            (
                RelayConfig {
                    max_permissions: 0,
                    ..RelayConfig::default()
                },
                "max_permissions",
            ),
            (
                RelayConfig {
                    queue: 1,
                    ..RelayConfig::default()
                },
                "queue",
            ),
            (
                RelayConfig {
                    max_frame: 100,
                    ..RelayConfig::default()
                },
                "max_frame",
            ),
        ] {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
    }
}
