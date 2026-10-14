//! The websocket transport: a generic protocol adapter and a reference client.
//!
//! [`WebSocketServer`] listens on a TCP address, upgrades each connection to a websocket and turns
//! it into a [`CallLink`] (the [`Transport`] trait). Binary frames are audio,
//! text frames are JSON control messages ([`protocol`], `docs/WEBSOCKET_PROTOCOL.md`). The first
//! message must be `start`, which negotiates the sample rate and encoding for the whole call.
//!
//! # Tasks and ownership
//! The server owns one accept task. Each connection runs in one task that drives a reader and a
//! writer side concurrently, so a client that does not take output cannot stop the engine from
//! hearing the client, and the other way round. All tasks are tracked: [`WebSocketServer::shutdown`]
//! cancels them and waits.
//!
//! # Bounded everything
//! - Calls: at most [`ServerConfig::max_calls`] connections are served at once (connections still in
//!   the handshake count). The next ones are answered with HTTP 503 and `Retry-After`.
//! - Admission gate (optional, [`WebSocketServer::serve_gated`]): a connection within
//!   `max_calls` is also admitted by the host's
//!   [`AdmissionGate`](crate::admission::AdmissionGate) before the upgrade; a refusal
//!   is answered with HTTP 503 (`Retry-After: 1` when busy, none when shutting down), and the
//!   gate's ticket travels with the call ([`CallLink::take_ticket`]); see
//!   [`crate::admission`].
//! - Rejections: at most 32 over-limit connections are answered at once; past that they are
//!   dropped without an answer.
//! - Frames: [`ServerConfig::max_audio_bytes`] / [`ServerConfig::max_control_bytes`]; larger ones
//!   close the connection with `frame_too_large`.
//! - Input to the engine: the call's `WireEvent` queue (input audio is dropped oldest-first when
//!   the engine falls behind; control events wait up to [`ServerConfig::send_timeout`], then the
//!   call is closed as `overloaded`).
//! - Output to the client: the call's `WireOut` queue (8 frames, lossless, the engine's output
//!   waits) and the socket. A write that takes longer than [`ServerConfig::write_timeout`] closes
//!   the call (`slow_client`): playout is real time, so a client that falls behind is lost, not
//!   buffered without bound.
//!
//! # Keepalive
//! The server pings every [`ServerConfig::ping_interval`]. Any frame from the client counts as
//! life; none for [`ServerConfig::ping_timeout`] closes the call (`ping_timeout`).
//!
//! # Time
//! Every timer and every timeout of the server (pings, idle, handshake, start, writes, hand-offs,
//! the close handshake) runs on the injected [`Clock`], so a simulated clock drives all of them.
//! Only the TCP and websocket I/O itself is real.
//!
//! The reference client is in [`client`]; the `ws_client` binary wraps it.

pub mod client;
pub(crate) mod connection;
pub mod protocol;
pub(crate) mod upgrade_auth;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use brisvox_core::{Clock, SharedClock};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::admission::{AdmissionTicket, Refusal, SharedGate};
use crate::auth::Authenticator;
use crate::call::CallLink;
use crate::error::{Error, Result};
use crate::transport::Transport;

pub use protocol::{ClientMessage, Encoding, ProtocolError, ServerMessage, Start};

/// Over-limit connections answered with HTTP 503 at the same time.
const REJECT_SLOTS: usize = 32;

/// Shortest ping interval a configuration may ask for.
pub(crate) const MIN_PING_INTERVAL: Duration = Duration::from_millis(10);

/// Smallest `max_audio_bytes`: one 16-bit sample.
const MIN_AUDIO_BYTES: usize = 2;

/// Smallest `max_control_bytes`: room for a `start` with a short call id and a few metadata
/// entries, and for a mark echo of the longest allowed id.
const MIN_CONTROL_BYTES: usize = 512;

/// A deadline of [`timeout`] passed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Elapsed;

/// Runs `future`, giving up after `after` on `clock` (the injected clock, not `tokio::time`, so a
/// simulated clock drives every timeout). `future` is dropped when the deadline wins.
pub(crate) async fn timeout<F: Future>(
    clock: &dyn Clock,
    after: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = clock.sleep(after) => Err(Elapsed),
    }
}

/// Pause after a failed `accept` (descriptor exhaustion), so the loop does not spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// The future that serves one admitted connection (see [`ServeFn`]).
pub(crate) type ServeFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What an admitted connection holds while it runs: its call permit and, behind a gate, the
/// host's ticket, which goes into the call's link when the call is handed over.
pub(crate) struct Admitted {
    _permit: Option<OwnedSemaphorePermit>,
    ticket: Option<AdmissionTicket>,
    /// A slot of the host's handshake pool, held until the call has sent a valid `start`
    /// (only the WebRTC listener's fallback uses it).
    handshake: Option<OwnedSemaphorePermit>,
}

impl Admitted {
    /// An admission the host made itself (it holds the call's own slot): optionally a
    /// `handshake` slot that is freed by [`finish_handshake`](Self::finish_handshake).
    #[cfg(feature = "webrtc")]
    pub(crate) fn hosted(handshake: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            _permit: None,
            ticket: None,
            handshake,
        }
    }

    /// Frees the handshake slot, if any: the call has passed its opening.
    pub(crate) fn finish_handshake(&mut self) {
        self.handshake = None;
    }

    /// Moves the gate's ticket (if any) into `link`, right before the link is handed over.
    pub(crate) fn attach(&mut self, link: &mut CallLink) {
        link.set_ticket(self.ticket.take());
    }
}

/// Serves one admitted TCP connection to its end, holding the call permit (and the gate's
/// ticket, see [`Admitted`]) while it runs. The
/// accept loop, the limits, the call queue and the shutdown are the same for every protocol; this
/// is the one part that differs: the generic websocket protocol or Twilio Media Streams.
pub(crate) type ServeFn =
    Arc<dyn Fn(TcpStream, SocketAddr, Admitted, Arc<Shared>) -> ServeFuture + Send + Sync>;

/// Tuning of a [`WebSocketServer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// Address to listen on. Port 0 picks a free port ([`WebSocketServer::local_addr`]).
    pub bind_addr: SocketAddr,
    /// Most connections served at once, handshake included. At least 1.
    pub max_calls: usize,
    /// From TCP accept to the end of the websocket upgrade.
    pub handshake_timeout: Duration,
    /// From the upgrade to a valid `start` message.
    pub start_timeout: Duration,
    /// Interval of the server's pings.
    pub ping_interval: Duration,
    /// No frame of any kind from the client for this long ends the call. Must be longer than
    /// `ping_interval` (a healthy but silent client is only heard from after its pong).
    pub ping_timeout: Duration,
    /// Longest a single write to the client may take.
    pub write_timeout: Duration,
    /// Longest a control event may wait for the engine's queue, and longest the engine may take
    /// to accept a new call from the accept queue.
    pub send_timeout: Duration,
    /// How long the two sides of a closing connection wait for each other and for the peer's
    /// close frame.
    pub close_grace: Duration,
    /// Calls waiting for [`Transport::accept`]. At least 1. Full: new calls wait up to
    /// `send_timeout`, then are refused with `server_busy`.
    pub accept_queue: usize,
    /// Largest binary frame accepted, in bytes. At least 2. Output is cut into frames of at most
    /// this size.
    pub max_audio_bytes: usize,
    /// Largest text frame accepted, in bytes. At least 512.
    pub max_control_bytes: usize,
}

/// The largest [`ServerConfig::max_calls`] and [`ServerConfig::accept_queue`]: far above what one
/// process serves, far below what the server's semaphore and channel accept (a larger value would
/// panic there instead of failing the configuration).
pub const MAX_CALLS: usize = 1_000_000;

impl ServerConfig {
    /// Checks the configuration; [`WebSocketServer::bind`] and [`WebSocketServer::serve`] call it.
    ///
    /// The rules: `max_calls` and `accept_queue` at least 1 and at most [`MAX_CALLS`]; every
    /// timeout and `close_grace` above zero; `ping_interval` at least 10 ms and shorter than `ping_timeout`; `max_audio_bytes` at
    /// least 2 and `max_control_bytes` at least 512.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first rule that is broken.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if self.max_calls == 0 {
            return bad("max_calls must be at least 1");
        }
        if self.max_calls > MAX_CALLS {
            return bad("max_calls must be at most 1000000");
        }
        if self.accept_queue == 0 {
            return bad("accept_queue must be at least 1");
        }
        if self.accept_queue > MAX_CALLS {
            return bad("accept_queue must be at most 1000000");
        }
        let timeouts = [
            ("handshake_timeout", self.handshake_timeout),
            ("start_timeout", self.start_timeout),
            ("ping_timeout", self.ping_timeout),
            ("write_timeout", self.write_timeout),
            ("send_timeout", self.send_timeout),
            ("close_grace", self.close_grace),
        ];
        if let Some((name, _)) = timeouts.iter().find(|(_, d)| d.is_zero()) {
            return bad(&format!("{name} must be longer than zero"));
        }
        if self.ping_interval < MIN_PING_INTERVAL {
            return bad("ping_interval must be at least 10 ms");
        }
        if self.ping_timeout <= self.ping_interval {
            return bad("ping_timeout must be longer than ping_interval");
        }
        if self.max_audio_bytes < MIN_AUDIO_BYTES {
            return bad("max_audio_bytes must be at least 2");
        }
        if self.max_control_bytes < MIN_CONTROL_BYTES {
            return bad("max_control_bytes must be at least 512");
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            max_calls: 256,
            handshake_timeout: Duration::from_secs(5),
            start_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(10),
            ping_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(5),
            send_timeout: Duration::from_secs(5),
            close_grace: Duration::from_secs(2),
            accept_queue: 16,
            max_audio_bytes: 64 * 1024,
            max_control_bytes: 16 * 1024,
        }
    }
}

/// How a protocol serves a connection beyond the call limit (or refused by the host's gate) that
/// may need no capacity: a stream that continues a call whose session is already admitted (Twilio's
/// stream restarts). The protocol only knows after the upgrade, so such a connection
/// is upgraded and looked at before it is turned away. Bounded by the same reject slots as the
/// 503 answers, so a flood of over-limit connections cannot hold more than [`REJECT_SLOTS`] of
/// them.
pub(crate) struct Overflow {
    /// Whether some call waits for a continuation right now. Cheap; when `false` an over-limit
    /// connection gets the 503 at once.
    pub(crate) wanted: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Serves the connection: hands it to its call, or refuses it itself.
    pub(crate) serve: OverflowFn,
}

/// Serves one over-limit connection (see [`Overflow`]); the permit is its reject slot.
pub(crate) type OverflowFn = Arc<
    dyn Fn(TcpStream, SocketAddr, OwnedSemaphorePermit, Arc<Shared>) -> ServeFuture + Send + Sync,
>;

/// State shared by the accept loop and every connection.
pub(crate) struct Shared {
    pub(crate) clock: SharedClock,
    pub(crate) config: ServerConfig,
    pub(crate) calls: mpsc::Sender<CallLink>,
    pub(crate) next_call: AtomicU64,
    pub(crate) cancel: CancellationToken,
    /// The host's admission gate, if any.
    pub(crate) gate: Option<SharedGate>,
}

impl Shared {
    /// The state of a host that serves upgraded connections itself (the WebRTC signaling
    /// listener): calls go to `calls`, `cancel` is the host's shutdown, there is no gate
    /// (the host admits connections before the upgrade).
    #[cfg(feature = "webrtc")]
    pub(crate) fn hosted(
        clock: SharedClock,
        config: ServerConfig,
        calls: mpsc::Sender<CallLink>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            clock,
            config,
            calls,
            next_call: AtomicU64::new(1),
            cancel,
            gate: None,
        }
    }
}

/// A websocket server that yields one call per connection.
///
/// Dropping it cancels its tasks without waiting; [`shutdown`](Self::shutdown) waits for them.
pub struct WebSocketServer {
    local_addr: SocketAddr,
    calls: mpsc::Receiver<CallLink>,
    cancel: CancellationToken,
    tracker: TaskTracker,
    permits: Arc<Semaphore>,
    max_calls: usize,
    clock: SharedClock,
}

impl WebSocketServer {
    /// Binds `config.bind_addr` and starts accepting connections. The server is a child of
    /// `parent`: cancelling it stops the server.
    ///
    /// # Errors
    /// [`Error::Config`] if `config` is invalid ([`ServerConfig::validate`]), [`Error::Io`] if
    /// the address cannot be bound.
    pub async fn bind(
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(config.bind_addr).await?;
        Self::serve(listener, config, clock, parent)
    }

    /// Starts accepting connections on an already bound `listener` (`config.bind_addr` is
    /// ignored). Must run inside a tokio runtime.
    ///
    /// # Errors
    /// [`Error::Config`] if `config` is invalid ([`ServerConfig::validate`]), [`Error::Io`] if
    /// the listener's address cannot be read.
    pub fn serve(
        listener: TcpListener,
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
    ) -> Result<Self> {
        Self::serve_gated(listener, config, clock, parent, None)
    }

    /// [`serve`](Self::serve) behind the host's admission `gate` (none: like `serve`): every
    /// connection within `max_calls` is admitted by the gate before the upgrade, refused with
    /// HTTP 503 otherwise, and its ticket is in the call's link ([`CallLink::take_ticket`]).
    /// See [`crate::admission`].
    ///
    /// # Errors
    /// As [`serve`](Self::serve).
    pub fn serve_gated(
        listener: TcpListener,
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        let handler: ServeFn = Arc::new(|stream, peer, admitted, shared| {
            Box::pin(connection::serve(stream, peer, admitted, shared))
        });
        Self::serve_with(listener, config, clock, parent, handler, gate)
    }

    /// [`serve_gated`](Self::serve_gated) where the upgrade request must carry a credential
    /// `auth` accepts (`Authorization: Bearer`, the `brisvox-token.<token>` subprotocol, or
    /// `?token=`; see [`crate::auth`]): a missing one is answered with HTTP 401, an invalid one
    /// with 403, an oversize one with 413. The session token's claims (`session_id`,
    /// `session.agent`, `session.meta.*`) are in the call's metadata. An `auth` that requires
    /// nothing behaves like [`serve_gated`](Self::serve_gated).
    ///
    /// # Errors
    /// As [`serve`](Self::serve).
    pub fn serve_authenticated(
        listener: TcpListener,
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        gate: Option<SharedGate>,
        auth: Authenticator,
    ) -> Result<Self> {
        let auth = auth.required().then(|| Arc::new(auth));
        let handler: ServeFn = Arc::new(move |stream, peer, admitted, shared| {
            Box::pin(connection::serve_with_auth(
                stream,
                peer,
                admitted,
                shared,
                auth.clone(),
            ))
        });
        Self::serve_with(listener, config, clock, parent, handler, gate)
    }

    /// [`serve_gated`](Self::serve_gated) with another protocol on the connections (`handler`).
    /// Same accept loop, limits, gate, call queue and shutdown.
    pub(crate) fn serve_with(
        listener: TcpListener,
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        handler: ServeFn,
        gate: Option<SharedGate>,
    ) -> Result<Self> {
        Self::serve_with_overflow(listener, config, clock, parent, handler, gate, None)
    }

    /// [`serve_with`](Self::serve_with) where the protocol may also serve over-limit connections
    /// that need no capacity (see [`Overflow`]).
    pub(crate) fn serve_with_overflow(
        listener: TcpListener,
        config: ServerConfig,
        clock: SharedClock,
        parent: &CancellationToken,
        handler: ServeFn,
        gate: Option<SharedGate>,
        overflow: Option<Overflow>,
    ) -> Result<Self> {
        config.validate()?;
        let local_addr = listener.local_addr()?;
        let max_calls = config.max_calls;
        let cancel = parent.child_token();
        let tracker = TaskTracker::new();
        let permits = Arc::new(Semaphore::new(max_calls));
        let (calls_tx, calls_rx) = mpsc::channel(config.accept_queue);
        let shared = Arc::new(Shared {
            clock: clock.clone(),
            config,
            calls: calls_tx,
            next_call: AtomicU64::new(1),
            cancel: cancel.clone(),
            gate,
        });
        tracker.spawn(accept_loop(
            listener,
            shared,
            Arc::clone(&permits),
            tracker.clone(),
            handler,
            overflow,
        ));
        Ok(Self {
            local_addr,
            calls: calls_rx,
            cancel,
            tracker,
            permits,
            max_calls,
            clock,
        })
    }

    /// The address the server listens on.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Connections being served right now (handshake and closing included).
    pub fn active_calls(&self) -> usize {
        self.max_calls
            .saturating_sub(self.permits.available_permits())
    }

    /// Stops accepting, tells every call the server is shutting down (`stop` with reason
    /// `shutdown`, then a close) and waits for all tasks to end.
    ///
    /// # Errors
    /// [`Error::Core`] (timeout) if tasks were still running after `timeout`; they stay
    /// cancelled and finish on their own bounded waits.
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.cancel.cancel();
        self.tracker.close();
        self::timeout(&*self.clock, timeout, self.tracker.wait())
            .await
            .map_err(|_| {
                Error::Core(brisvox_core::Error::Timeout {
                    what: "websocket shutdown",
                    after: timeout,
                })
            })
    }
}

impl Drop for WebSocketServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Transport for WebSocketServer {
    /// Waits for the next call that completed the upgrade and sent a valid `start`.
    ///
    /// # Errors
    /// [`Error::Core`] (cancelled) if `cancel` fires. `Ok(None)` once the server has shut down
    /// and no queued call is left.
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(brisvox_core::Error::Cancelled.into()),
            link = self.calls.recv() => Ok(link),
        }
    }
}

/// Accepts TCP connections until cancelled.
async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    permits: Arc<Semaphore>,
    tracker: TaskTracker,
    handler: ServeFn,
    overflow: Option<Overflow>,
) {
    let reject_slots = Arc::new(Semaphore::new(REJECT_SLOTS));
    loop {
        let accepted = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => match admit(&permits, shared.gate.as_ref()) {
                Ok(admitted) => {
                    tracker.spawn(handler(stream, peer, admitted, Arc::clone(&shared)));
                }
                Err(refusal) => match Arc::clone(&reject_slots).try_acquire_owned() {
                    Ok(slot) => {
                        let continuation = match &overflow {
                            Some(overflow)
                                if refusal != Refusal::ShuttingDown && (overflow.wanted)() =>
                            {
                                Some(overflow)
                            }
                            _ => None,
                        };
                        if let Some(overflow) = continuation {
                            tracker.spawn((overflow.serve)(
                                stream,
                                peer,
                                slot,
                                Arc::clone(&shared),
                            ));
                        } else {
                            tracker.spawn(connection::reject_busy(
                                stream,
                                slot,
                                refusal,
                                Arc::clone(&shared),
                            ));
                        }
                    }
                    Err(_) => drop_connection(stream),
                },
            },
            Err(error) => {
                tracing::warn!(%error, "websocket accept failed");
                tokio::select! {
                    biased;
                    () = shared.cancel.cancelled() => return,
                    () = shared.clock.sleep(ACCEPT_ERROR_BACKOFF) => {}
                }
            }
        }
    }
}

/// Admits one connection: a call permit first (the server's own limit), then the host's gate.
/// A refused connection holds neither.
fn admit(permits: &Arc<Semaphore>, gate: Option<&SharedGate>) -> Result<Admitted, Refusal> {
    let permit = Arc::clone(permits)
        .try_acquire_owned()
        .map_err(|_| Refusal::Busy)?;
    let ticket = match gate {
        Some(gate) => Some(gate.try_admit()?),
        None => None,
    };
    Ok(Admitted {
        _permit: Some(permit),
        ticket,
        handshake: None,
    })
}

/// Closes a connection nobody has capacity for.
fn drop_connection(stream: TcpStream) {
    drop(stream);
}
