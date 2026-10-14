//! SIPS: the TLS listener of the SIP transport.
//!
//! `rsipstack` has a TLS listener of its own, but it does a TLS handshake without a deadline and
//! without a connection limit: a client that opens sockets and stays silent holds a task and a
//! file descriptor each, for good. This module does the accepting instead and hands each
//! finished TLS stream to the stack as a connection (`TlsConnection::from_server_stream`), so
//! everything after the handshake (framing, transactions, dialogs, digest, the same call
//! handling) is the code the UDP and TCP paths use.
//!
//! What the accept loop enforces, in this order, before any SIP is read:
//! 1. the source ACL ([`SipConfig::allowed_sources`](super::SipConfig::allowed_sources)): a
//!    connection from anywhere else is closed without a handshake;
//! 2. [`SipTlsConfig::max_connections`] and [`SipTlsConfig::max_connections_per_ip`]: over either
//!    the connection is closed at once (counted in `SipStats::tls_rejected`);
//! 3. [`SipTlsConfig::handshake_timeout`] for the handshake (on the injected clock).
//!
//! A connection holds its slots until the stack lets go of it (the peer closed, an error, the
//! server stopped, or [`SipTlsConfig::idle_timeout`] passed without a single byte in either
//! direction; a CRLF keepalive counts). `rsipstack` reports no activity, so the bytes are
//! counted one level down: see [`tls_relay`](super::tls_relay). Set `allowed_sources` to the
//! carrier's networks on a public address all the same.
//!
//! Outbound calls to a `sips:` callee through an explicit proxy address
//! ([`DialRequest::destination`](super::DialRequest::destination)) connect here too
//! ([`TlsRuntime::connect_named`]): the stack would verify the certificate against the proxy's
//! IP address, which public CAs do not certify, so this module makes the connection and verifies
//! the callee's domain name, then hands the verified connection to the stack.
//!
//! The protocol versions are rustls' safe defaults with the `ring` provider: TLS 1.2 and 1.3,
//! no older. No client certificate is asked for (carriers authenticate with the source network
//! and digest, and the certificate pinned by the carrier is ours). The private key is a
//! [`Secret`] and is never printed.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::Secret;
use rsipstack::transport::{SipAddr, SipConnection, TransportLayer, tls::TlsConnection};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::tls_relay::{Activity, LoopbackPairs, relay, watch_idle};
use super::{Shared, shared_timeout};
use crate::error::{Error, Result};

/// Settings of the SIPS listener ([`SipConfig::tls`](super::SipConfig::tls)).
#[derive(Clone)]
pub struct SipTlsConfig {
    /// Where to listen for SIP over TLS. Port 0 picks a free port
    /// ([`SipHandle::tls_addr`](super::SipHandle::tls_addr) tells which). The conventional port is
    /// 5061.
    pub bind_addr: SocketAddr,
    /// The server certificate chain in PEM, leaf first. The carrier must trust it: a public CA
    /// for a public trunk (Twilio, Telnyx, ...), your own CA for a private one.
    pub certificate_chain_pem: String,
    /// The private key of the leaf certificate in PEM (PKCS#8, PKCS#1 or SEC1). Never printed.
    pub private_key_pem: Secret<String>,
    /// PEM bundle of the CAs that outbound TLS calls verify the callee's certificate against.
    /// Without it, calls to `sips:` / `;transport=tls` targets fail at the handshake: the stack
    /// has no built-in roots. Inbound TLS does not use it.
    pub client_ca_pem: Option<String>,
    /// The longest a client may take for the TLS handshake.
    pub handshake_timeout: Duration,
    /// TLS connections at once, handshaking and established.
    pub max_connections: usize,
    /// TLS connections at once from one source address.
    pub max_connections_per_ip: usize,
    /// An established connection that carries no byte in either direction for this long is
    /// closed (counted in `SipStats::tls_idle_closed`). CRLF keepalives (RFC 5626) are bytes, so
    /// a peer that keeps its connection alive that way is never cut. `Duration::ZERO` turns the
    /// check off. `BRISVOX_SIP_TLS_IDLE_TIMEOUT`.
    pub idle_timeout: Duration,
}

impl std::fmt::Debug for SipTlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipTlsConfig")
            .field("bind_addr", &self.bind_addr)
            .field("certificate_chain_pem", &"[certificate]")
            .field("private_key_pem", &self.private_key_pem)
            .field(
                "client_ca_pem",
                &self.client_ca_pem.as_ref().map(|_| "[CA]"),
            )
            .field("handshake_timeout", &self.handshake_timeout)
            .field("max_connections", &self.max_connections)
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("idle_timeout", &self.idle_timeout)
            .finish()
    }
}

impl SipTlsConfig {
    /// A configuration for `certificate_chain_pem` and `private_key_pem` on the default SIPS port
    /// 5061 of all interfaces, with a 10 s handshake deadline, 1024 / 32 connections and a 5
    /// minute idle timeout.
    pub fn new(
        certificate_chain_pem: impl Into<String>,
        private_key_pem: impl Into<String>,
    ) -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 5061)),
            certificate_chain_pem: certificate_chain_pem.into(),
            private_key_pem: Secret::new(private_key_pem.into()),
            client_ca_pem: None,
            handshake_timeout: Duration::from_secs(10),
            max_connections: 1024,
            max_connections_per_ip: 32,
            idle_timeout: Duration::from_secs(300),
        }
    }

    /// Checks the configuration: positive limits and deadline, and a certificate chain with a key
    /// that belongs to it (parsed and tested by rustls).
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken. The messages never contain the key.
    pub fn validate(&self) -> Result<()> {
        if self.handshake_timeout.is_zero() {
            return Err(Error::Config(
                "sip tls handshake_timeout must be positive".to_owned(),
            ));
        }
        if self.max_connections == 0 || self.max_connections_per_ip == 0 {
            return Err(Error::Config(
                "sip tls max_connections and max_connections_per_ip must be at least 1".to_owned(),
            ));
        }
        self.server_config().map(drop)?;
        if let Some(ca) = &self.client_ca_pem
            && CertificateDer::pem_slice_iter(ca.as_bytes())
                .next()
                .is_none()
        {
            return Err(Error::Config(
                "sip tls client_ca_pem holds no certificate".to_owned(),
            ));
        }
        Ok(())
    }

    /// The rustls server configuration: TLS 1.2 and 1.3, `ring`, no client certificate.
    pub(crate) fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let bad = |what: String| Error::Config(format!("sip tls: {what}"));
        let chain = CertificateDer::pem_slice_iter(self.certificate_chain_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| bad(format!("certificate chain is not valid PEM: {e}")))?;
        if chain.is_empty() {
            return Err(bad("certificate chain holds no certificate".to_owned()));
        }
        let key = PrivateKeyDer::from_pem_slice(self.private_key_pem.expose().as_bytes())
            .map_err(|e| bad(format!("private key is not valid PEM: {e}")))?;
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| bad(format!("certificate and key do not fit: {e}")))?;
        Ok(Arc::new(config))
    }

    /// The trust roots for outbound TLS, as the stack wants them (PEM bytes).
    pub(crate) fn stack_config(&self) -> rsipstack::transport::TlsConfig {
        rsipstack::transport::TlsConfig {
            ca_certs: self
                .client_ca_pem
                .as_ref()
                .map(|pem| pem.as_bytes().to_vec()),
            ..rsipstack::transport::TlsConfig::default()
        }
    }
}

/// Who is connected: the total and the count per source address.
struct Limits {
    total: Arc<Semaphore>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    max_per_ip: usize,
}

/// The slots one connection holds; released on drop.
struct Slot {
    _permit: OwnedSemaphorePermit,
    limits: Arc<Limits>,
    ip: IpAddr,
}

impl Limits {
    fn take(self: &Arc<Self>, ip: IpAddr) -> Option<Slot> {
        {
            let mut per_ip = self.per_ip.lock().unwrap_or_else(PoisonError::into_inner);
            let count = per_ip.entry(ip).or_insert(0);
            if *count >= self.max_per_ip {
                return None;
            }
            *count += 1;
        }
        match Arc::clone(&self.total).try_acquire_owned() {
            Ok(permit) => Some(Slot {
                _permit: permit,
                limits: Arc::clone(self),
                ip,
            }),
            Err(_) => {
                self.release(ip);
                None
            }
        }
    }

    fn release(&self, ip: IpAddr) {
        let mut per_ip = self.per_ip.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = per_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_ip.remove(&ip);
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.limits.release(self.ip);
    }
}

/// A finished TLS connection for the accept loop to register with the stack. The accept loop is
/// the only owner of a clone of the transport layer (dropping one cancels the whole stack), so
/// every connection, inbound or outbound, reaches the layer through it.
pub(crate) struct Handoff {
    pub(crate) connection: SipConnection,
    /// Told when the connection is registered (an outbound call looks it up right after).
    pub(crate) registered: Option<oneshot::Sender<()>>,
}

/// Outbound TLS connections verified by name: the address, the name verified and the
/// connection's own token (cancelled when the stack lets go of it).
type Verified = HashMap<SocketAddr, (String, CancellationToken)>;

/// Most distinct proxy addresses with a verified outbound connection kept at once.
const MAX_VERIFIED: usize = 64;

/// What the SIPS listener, the idle watch and outbound TLS connections share.
pub(crate) struct TlsRuntime {
    pairs: LoopbackPairs,
    handoff: mpsc::Sender<Handoff>,
    client: Arc<rustls::ClientConfig>,
    verified: Mutex<Verified>,
    idle_timeout: Duration,
    handshake_timeout: Duration,
}

/// Finished connections waiting for the accept loop to register them with the stack. Bounded;
/// when it is full the sender waits (the loop drains it in microseconds, and a connection task
/// is cancelled with the endpoint), nothing is dropped.
const HANDOFF_QUEUE: usize = 64;

impl TlsRuntime {
    /// Binds the loopback listener of the relay and builds the client configuration (the trust
    /// roots of [`SipTlsConfig::client_ca_pem`], `ring`, TLS 1.2 and 1.3, no client certificate).
    ///
    /// # Errors
    /// [`Error::Io`] when the loopback listener cannot be bound, [`Error::Config`] when the
    /// client configuration cannot be made.
    pub(crate) async fn bind(
        config: &SipTlsConfig,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Handoff>)> {
        let pairs = LoopbackPairs::bind().await.map_err(Error::Io)?;
        let mut roots = rustls::RootCertStore::empty();
        if let Some(pem) = &config.client_ca_pem {
            for cert in CertificateDer::pem_slice_iter(pem.as_bytes()).flatten() {
                // A certificate the store refuses is skipped, like a stray block in a bundle.
                let _ = roots.add(cert);
            }
        }
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Config(format!("sip tls client: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let (handoff, handoff_rx) = mpsc::channel(HANDOFF_QUEUE);
        let runtime = Self {
            pairs,
            handoff,
            client: Arc::new(client),
            verified: Mutex::new(HashMap::new()),
            idle_timeout: config.idle_timeout,
            handshake_timeout: config.handshake_timeout,
        };
        Ok((Arc::new(runtime), handoff_rx))
    }

    /// Makes sure the stack has a TLS connection to `destination` whose certificate was verified
    /// for the server name `name` (the callee's domain), and returns once the stack can find it.
    /// A live connection verified for the same name is reused. A call through an explicit proxy
    /// address needs this: left to itself the stack names the proxy's IP address in the
    /// handshake.
    ///
    /// # Errors
    /// The reason in words (no route, handshake failure, certificate not trusted for `name`,
    /// timeout, too many distinct destinations).
    pub(crate) async fn connect_named(
        &self,
        shared: &Arc<Shared>,
        destination: SocketAddr,
        name: &str,
    ) -> std::result::Result<(), String> {
        let wanted = name.to_ascii_lowercase();
        {
            let mut verified = self.verified.lock().unwrap_or_else(PoisonError::into_inner);
            verified.retain(|_, (_, token)| !token.is_cancelled());
            if verified
                .get(&destination)
                .is_some_and(|(known, _)| *known == wanted)
            {
                return Ok(());
            }
            if verified.len() >= MAX_VERIFIED && !verified.contains_key(&destination) {
                return Err("too many distinct TLS destinations".to_owned());
            }
        }
        let server_name =
            ServerName::try_from(wanted.clone()).map_err(|_| "invalid server name".to_owned())?;
        let relay_cancel = CancellationToken::new();
        let stop_relay = relay_cancel.clone().drop_guard();
        let activity = Arc::new(Activity::new(shared.clock.now()));
        let setup = async {
            let real = TcpStream::connect(destination)
                .await
                .map_err(|e| e.to_string())?;
            let _ = real.set_nodelay(true);
            let (near, far) = self.pairs.pair().await.map_err(|e| e.to_string())?;
            shared.tracker.spawn(relay(
                real,
                far,
                Arc::clone(&activity),
                Arc::clone(&shared.clock),
                relay_cancel.clone(),
            ));
            TlsConnector::from(Arc::clone(&self.client))
                .connect(server_name, near)
                .await
                .map_err(|e| e.to_string())
        };
        let tls = shared_timeout(&*shared.clock, self.handshake_timeout, setup)
            .await
            .map_err(|_| "connect timed out".to_owned())??;
        let token = CancellationToken::new();
        let remote = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Tls),
            addr: destination.into(),
        };
        let connection = TlsConnection::from_client_stream(tls, remote, Some(token.clone()))
            .await
            .map_err(|e| e.to_string())?;
        let (registered_tx, registered_rx) = oneshot::channel();
        let handed = Handoff {
            connection: SipConnection::Tls(connection),
            registered: Some(registered_tx),
        };
        let limit = self.handshake_timeout;
        shared_timeout(&*shared.clock, limit, async {
            self.handoff
                .send(handed)
                .await
                .map_err(|_| "the SIP stack is stopping".to_owned())?;
            registered_rx
                .await
                .map_err(|_| "the SIP stack is stopping".to_owned())
        })
        .await
        .map_err(|_| "registering the connection timed out".to_owned())??;
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(destination, (wanted, token.clone()));
        // The connection now outlives this call: the idle watch ends it and the relay.
        let (watched, keep_relay) = (Arc::clone(shared), relay_cancel.clone());
        let (idle_timeout, activity) = (self.idle_timeout, activity);
        shared.tracker.spawn(async move {
            let idle = watch_idle(&*watched.clock, &activity, idle_timeout, &token);
            let expired = tokio::select! {
                biased;
                () = watched.endpoint_cancel.cancelled() => false,
                expired = idle => expired,
            };
            if expired {
                let counters = &watched.counters;
                counters.inc(&counters.tls_idle_closed);
                tracing::debug!(%destination, "sip tls: idle outbound connection closed");
            }
            keep_relay.cancel();
        });
        // The relay belongs to the connection now, not to this call.
        let _ = stop_relay.disarm();
        Ok(())
    }
}

/// What the accept loop needs.
pub(crate) struct Listener {
    pub(crate) listener: TcpListener,
    pub(crate) config: SipTlsConfig,
    /// The address written into Via / Contact for connections of this listener.
    pub(crate) advertised: SocketAddr,
    /// The only owner of a clone of the transport layer: dropping a `TransportLayer` cancels the
    /// whole stack, so connection tasks never hold one; they hand their connection to this loop.
    pub(crate) layer: TransportLayer,
    pub(crate) runtime: Arc<TlsRuntime>,
    pub(crate) handoff: mpsc::Receiver<Handoff>,
    pub(crate) shared: Arc<Shared>,
}

impl Listener {
    /// Accepts until the endpoint stops. Every connection is its own tracked task.
    pub(crate) async fn run(self, tasks: TaskTracker) {
        let Self {
            listener,
            config,
            advertised,
            layer,
            runtime,
            handoff: mut handoff_rx,
            shared,
        } = self;
        let cancel = shared.endpoint_cancel.clone();
        let acceptor = match config.server_config() {
            Ok(server) => TlsAcceptor::from(server),
            Err(error) => {
                tracing::error!(%error, "sip tls: no usable server configuration, listener not started");
                cancel.cancelled().await;
                return;
            }
        };
        let limits = Arc::new(Limits {
            total: Arc::new(Semaphore::new(config.max_connections)),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip: config.max_connections_per_ip,
        });
        let handshake_timeout = config.handshake_timeout;
        let idle_timeout = config.idle_timeout;
        loop {
            let accepted = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                Some(finished) = handoff_rx.recv() => {
                    layer.add_connection(finished.connection);
                    if let Some(registered) = finished.registered {
                        let _ = registered.send(());
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let counters = &shared.counters;
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    // Out of descriptors or a reset before accept: back off, never spin.
                    tracing::warn!(%error, "sip tls: accept failed");
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        () = shared.clock.sleep(Duration::from_millis(100)) => {}
                    }
                    continue;
                }
            };
            let ip = peer.ip();
            let allowed = &shared.config.allowed_sources;
            if !allowed.is_empty() && !allowed.iter().any(|net| net.contains(ip)) {
                counters.inc(&counters.tls_rejected);
                continue;
            }
            let Some(slot) = limits.take(ip) else {
                counters.inc(&counters.tls_rejected);
                tracing::debug!(%peer, "sip tls: connection limit reached, closing");
                continue;
            };
            let _ = stream.set_nodelay(true);
            let acceptor = acceptor.clone();
            let shared = Arc::clone(&shared);
            let runtime = Arc::clone(&runtime);
            let handoff = runtime.handoff.clone();
            let relays = tasks.clone();
            tasks.spawn(async move {
                let counters = &shared.counters;
                // The TLS endpoint talks to a loopback socket and a relay task carries the bytes
                // to the peer's socket, noting the time of each (`tls_relay`). The relay ends
                // with this task, whatever way it ends.
                let relay_cancel = CancellationToken::new();
                let _stop_relay = relay_cancel.clone().drop_guard();
                let activity = Arc::new(Activity::new(shared.clock.now()));
                let handshake = async {
                    let (near, far) = runtime.pairs.pair().await.map_err(|e| e.to_string())?;
                    relays.spawn(relay(
                        stream,
                        far,
                        Arc::clone(&activity),
                        Arc::clone(&shared.clock),
                        relay_cancel.clone(),
                    ));
                    acceptor.accept(near).await.map_err(|e| e.to_string())
                };
                let handshake = shared_timeout(&*shared.clock, handshake_timeout, handshake);
                let tls = tokio::select! {
                    biased;
                    () = shared.endpoint_cancel.cancelled() => return,
                    done = handshake => done,
                };
                let tls = match tls {
                    Ok(Ok(tls)) => tls,
                    Ok(Err(error)) => {
                        counters.inc(&counters.tls_handshake_failures);
                        tracing::debug!(%peer, %error, "sip tls: handshake failed");
                        return;
                    }
                    Err(_) => {
                        counters.inc(&counters.tls_handshake_failures);
                        tracing::debug!(%peer, "sip tls: handshake timed out");
                        return;
                    }
                };
                let token = CancellationToken::new();
                let local = SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Tls),
                    addr: advertised.into(),
                };
                let remote = SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Tls),
                    addr: peer.into(),
                };
                let connection = match TlsConnection::from_server_stream(
                    tls,
                    local,
                    remote,
                    Some(token.clone()),
                )
                .await
                {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::debug!(%peer, %error, "sip tls: connection not usable");
                        return;
                    }
                };
                let handed = tokio::select! {
                    biased;
                    () = shared.endpoint_cancel.cancelled() => return,
                    sent = handoff.send(Handoff {
                        connection: SipConnection::Tls(connection),
                        registered: None,
                    }) => sent,
                };
                if handed.is_err() {
                    return;
                }
                counters.inc(&counters.tls_connections);
                // The stack cancels the token when the connection ends (peer closed, error or
                // endpoint stop); until then the slot stays taken, unless the connection is idle
                // for too long: then the relay ends, the stack sees the stream close and lets go.
                let idle = watch_idle(&*shared.clock, &activity, idle_timeout, &token);
                let expired = tokio::select! {
                    biased;
                    () = shared.endpoint_cancel.cancelled() => false,
                    expired = idle => expired,
                };
                if expired {
                    counters.inc(&counters.tls_idle_closed);
                    tracing::debug!(%peer, "sip tls: idle connection closed");
                    relay_cancel.cancel();
                    // The slot is held until the stack has noticed (bounded).
                    tokio::select! {
                        () = token.cancelled() => {}
                        () = shared.endpoint_cancel.cancelled() => {}
                        () = shared.clock.sleep(Duration::from_secs(5)) => {}
                    }
                }
                drop(slot);
            });
        }
        drop(layer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_count_per_address_and_in_total() {
        let limits = Arc::new(Limits {
            total: Arc::new(Semaphore::new(3)),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip: 2,
        });
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        let first = limits.take(a).unwrap();
        let second = limits.take(a).unwrap();
        assert!(limits.take(a).is_none(), "per-address limit");
        let third = limits.take(b).unwrap();
        assert!(limits.take(b).is_none(), "total limit");
        drop(first);
        assert!(limits.take(b).is_some(), "a freed slot is reusable");
        drop((second, third));
        let map = limits.per_ip.lock().unwrap();
        assert!(
            map.len() <= 1,
            "counts of addresses without connections are dropped"
        );
    }

    #[test]
    fn the_debug_output_hides_the_key() {
        let config = SipTlsConfig::new("CERT", "-----BEGIN PRIVATE KEY-----secret");
        let text = format!("{config:?}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("CERT"), "{text}");
        assert!(text.contains("REDACTED"));
    }

    #[test]
    fn bad_pem_is_refused_without_echoing_it() {
        let config = SipTlsConfig::new(
            "not a certificate",
            "-----BEGIN PRIVATE KEY-----\nhunter2\n",
        );
        let Err(Error::Config(message)) = config.validate() else {
            panic!("expected a config error");
        };
        assert!(!message.contains("hunter2"), "{message}");
    }
}
