//! Server-side TURN client tests: a server that can only be reached through a TURN
//! relay (no direct candidate in the answer) completes WebRTC calls.
//!
//! The TURN server is a real RFC 8656 implementation (`turn-server-proto`, the sans-IO server of
//! the project that also makes the client crate) wrapped in an in-process TCP (or TLS) listener
//! with one UDP socket per allocation on 127.0.0.1. It enforces long-term credentials (checked
//! against the coturn REST formula the server uses), permissions, channels and lifetimes, so a
//! missing `CreatePermission` or `Refresh` shows up as dropped packets or a vanished allocation.
//! The browser is a second `str0m` peer whose UDP socket can reach the relayed address only.
//! Real sockets on 127.0.0.1; the server's clock is a stepped one (real timers plus an offset
//! the test adds), which is how refreshes are driven across minutes of virtual time.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines,
    clippy::disallowed_methods
)] // test helpers (see clippy.toml); real sockets need real waits

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{Clock, Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_transport::auth::{Audience, SessionSecrets, TokenSpec, mint_token};
use brisvox_transport::webrtc::opus::{
    FRAME_SAMPLES, LibOpusFactory, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusFactory, OpusSettings,
};
use brisvox_transport::webrtc::turn_creds::coturn_password;
use brisvox_transport::webrtc::{
    RelayConfig, TurnConfig, TurnProvider, WebRtcConfig, WebRtcServer,
};
use brisvox_transport::{
    SessionConfig, Transport, TransportCommand, TransportEvent, TransportSession, UtteranceId,
};
use bytes::Bytes;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use turn_server_proto::api::{SocketAllocateError, TurnServerApi, TurnServerPollRet};
use turn_server_proto::server::TurnServer;
use turn_server_proto::stun::Instant as ProtoInstant;
use turn_server_proto::stun::agent::Transmit as ProtoTransmit;
use turn_server_proto::types::{AddressFamily, TransportType};

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(8);

/// The coturn `static-auth-secret` the server and the mock TURN server share.
const SECRET: &str = "relay-test-secret";

// ---------------------------------------------------------------------------------------------
// A clock the test steps: real timers, plus an offset the test adds to move "now" forward

#[derive(Debug)]
struct StepClock {
    real: SharedClock,
    offset_ms: AtomicU64,
}

impl StepClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemClock::shared(),
            offset_ms: AtomicU64::new(0),
        })
    }

    fn advance(&self, by: Duration) {
        self.offset_ms
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }

    fn offset(&self) -> Duration {
        Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

impl Clock for StepClock {
    fn now(&self) -> tokio::time::Instant {
        self.real.now() + self.offset()
    }

    fn wall(&self) -> std::time::SystemTime {
        self.real.wall() + self.offset()
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> tokio::time::Sleep {
        let real = deadline
            .checked_sub(self.offset())
            .unwrap_or_else(|| self.real.now());
        self.real.sleep_until(real)
    }
}

// ---------------------------------------------------------------------------------------------
// STUN pieces the mock needs to log what the client does

#[derive(Debug, Default)]
struct StunInfo {
    method: u16,
    class: u16,
    username: Option<String>,
    lifetime: Option<u32>,
    peers: Vec<IpAddr>,
}

fn parse_stun(frame: &[u8]) -> Option<StunInfo> {
    if frame.len() < 20 || frame[0] & 0xC0 != 0 {
        return None;
    }
    let kind = u16::from_be_bytes([frame[0], frame[1]]);
    let mut info = StunInfo {
        method: (kind & 0x000F) | ((kind & 0x00E0) >> 1) | ((kind & 0x3E00) >> 2),
        class: ((kind & 0x0100) >> 7) | ((kind & 0x0010) >> 4),
        ..StunInfo::default()
    };
    let length = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
    let mut rest = frame.get(20..20 + length)?;
    while rest.len() >= 4 {
        let attr = u16::from_be_bytes([rest[0], rest[1]]);
        let size = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let value = rest.get(4..4 + size)?;
        match attr {
            0x0006 => info.username = String::from_utf8(value.to_vec()).ok(),
            0x000D if size == 4 => {
                info.lifetime = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
            }
            // XOR-PEER-ADDRESS, IPv4 only (the mock relays IPv4 only).
            0x0012 if size == 8 && value[1] == 1 => {
                let xored = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                info.peers
                    .push(IpAddr::from((xored ^ 0x2112_A442).to_be_bytes()));
            }
            _ => {}
        }
        rest = rest.get(4 + ((size + 3) & !3)..).unwrap_or_default();
    }
    Some(info)
}

const ALLOCATE: u16 = 3;
const REFRESH: u16 = 4;
const SEND: u16 = 6;
const CREATE_PERMISSION: u16 = 8;
const CHANNEL_BIND: u16 = 9;

/// Splits one frame (STUN message, or ChannelData with its stream padding) off `acc`.
fn take_frame(acc: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ()> {
    if acc.len() < 4 {
        return Ok(None);
    }
    let length = usize::from(u16::from_be_bytes([acc[2], acc[3]]));
    let total = match acc[0] >> 6 {
        0 => 20 + length,
        1 => (4 + length + 3) & !3,
        _ => return Err(()),
    };
    if acc.len() < total {
        return Ok(None);
    }
    Ok(Some(acc.drain(..total).collect()))
}

// ---------------------------------------------------------------------------------------------
// The TURN server

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What the mock saw, in order.
#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Connection(usize),
    Allocate,
    Permission(IpAddr),
    ChannelBind,
    Refresh(u32),
    SendIndication,
    /// ChannelData from the client (a packet towards the browser).
    ChannelDataIn,
    /// A UDP datagram that reached a relayed address: from whom, and whether the TURN server
    /// forwarded it to the client (it does not without a permission).
    PeerPacket {
        from: SocketAddr,
        forwarded: bool,
    },
    /// An allocation was deleted (expired or refreshed to lifetime 0).
    AllocationClosed,
    Garbage,
}

#[derive(Default)]
struct MockState {
    events: Mutex<Vec<Ev>>,
    injectors: Mutex<Vec<mpsc::UnboundedSender<Vec<u8>>>>,
    connections: AtomicUsize,
    /// The USERNAMEs the clients authenticated with.
    usernames: Mutex<Vec<String>>,
}

impl MockState {
    fn log(&self, event: Ev) {
        self.events.lock().unwrap().push(event);
    }
}

struct MockOptions {
    /// The secret the credentials are checked against.
    secret: String,
    /// Accept connections and never answer.
    silent: bool,
    /// Terminate TLS with this configuration.
    tls: Option<Arc<rustls::ServerConfig>>,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            secret: SECRET.to_owned(),
            silent: false,
            tls: None,
        }
    }
}

struct MockTurn {
    addr: SocketAddr,
    state: Arc<MockState>,
    stop: CancellationToken,
}

impl MockTurn {
    async fn start(clock: SharedClock, options: MockOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(MockState::default());
        let stop = CancellationToken::new();
        let shared = Arc::new(MockShared {
            state: Arc::clone(&state),
            origin: clock.now(),
            clock,
            options,
            stop: stop.clone(),
        });
        tokio::spawn(async move {
            loop {
                let (tcp, client) = tokio::select! {
                    () = shared.stop.cancelled() => return,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let (tx, rx) = mpsc::unbounded_channel();
                shared.state.injectors.lock().unwrap().push(tx);
                let number = shared.state.connections.fetch_add(1, Ordering::SeqCst);
                shared.state.log(Ev::Connection(number));
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    let listen = tcp.local_addr().unwrap();
                    let stream: Box<dyn Io> = match &shared.options.tls {
                        Some(config) => {
                            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::clone(config));
                            match acceptor.accept(tcp).await {
                                Ok(tls) => Box::new(tls),
                                Err(_) => return,
                            }
                        }
                        None => Box::new(tcp),
                    };
                    Connection::new(stream, client, listen, shared, rx)
                        .serve()
                        .await;
                });
            }
        });
        Self { addr, state, stop }
    }

    fn url(&self, scheme: &str, host: &str) -> String {
        format!("{scheme}:{host}:{}?transport=tcp", self.addr.port())
    }

    fn events(&self) -> Vec<Ev> {
        self.state.events.lock().unwrap().clone()
    }

    fn count(&self, pick: impl Fn(&Ev) -> bool) -> usize {
        self.events().iter().filter(|e| pick(e)).count()
    }

    fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Writes raw bytes to the TCP connection number `n` (the client sees them as TURN traffic).
    fn inject(&self, n: usize, bytes: Vec<u8>) {
        self.state.injectors.lock().unwrap()[n].send(bytes).unwrap();
    }

    async fn until(&self, what: &str, mut ready: impl FnMut(&[Ev]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if ready(&self.events()) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                self.events()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for MockTurn {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct MockShared {
    state: Arc<MockState>,
    clock: SharedClock,
    origin: tokio::time::Instant,
    options: MockOptions,
    stop: CancellationToken,
}

/// One client connection: its own sans-IO TURN server, its relay sockets.
struct Connection {
    stream: Box<dyn Io>,
    client: SocketAddr,
    listen: SocketAddr,
    server: TurnServer,
    shared: Arc<MockShared>,
    inject: mpsc::UnboundedReceiver<Vec<u8>>,
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    udp_tx: mpsc::Sender<(SocketAddr, SocketAddr, Vec<u8>)>,
    udp_rx: mpsc::Receiver<(SocketAddr, SocketAddr, Vec<u8>)>,
}

impl Connection {
    fn new(
        stream: Box<dyn Io>,
        client: SocketAddr,
        listen: SocketAddr,
        shared: Arc<MockShared>,
        inject: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Self {
        let (udp_tx, udp_rx) = mpsc::channel(1024);
        Self {
            stream,
            client,
            listen,
            server: TurnServer::new(TransportType::Tcp, listen, "brisvox-test".to_owned()),
            shared,
            inject,
            sockets: HashMap::new(),
            udp_tx,
            udp_rx,
        }
    }

    fn now(&self) -> ProtoInstant {
        let elapsed = self
            .shared
            .clock
            .now()
            .saturating_duration_since(self.shared.origin);
        ProtoInstant::from_nanos(i64::try_from(elapsed.as_nanos()).unwrap())
    }

    async fn serve(mut self) {
        let mut accumulated: Vec<u8> = Vec::new();
        let mut buffer = vec![0u8; 4096];
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                () = self.shared.stop.cancelled() => return,
                read = self.stream.read(&mut buffer) => {
                    let n = match read {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    if self.shared.options.silent {
                        continue;
                    }
                    accumulated.extend_from_slice(&buffer[..n]);
                    loop {
                        match take_frame(&mut accumulated) {
                            Ok(Some(frame)) => {
                                if !self.client_frame(frame).await {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(()) => {
                                self.shared.state.log(Ev::Garbage);
                                return;
                            }
                        }
                    }
                }
                Some((relayed, from, data)) = self.udp_rx.recv() => {
                    let now = self.now();
                    let forwarded = self
                        .server
                        .recv(ProtoTransmit::new(data, TransportType::Udp, from, relayed), now)
                        .map(|build| build.build());
                    self.shared.state.log(Ev::PeerPacket { from, forwarded: forwarded.is_some() });
                    if let Some(transmit) = forwarded
                        && !self.send(transmit).await
                    {
                        return;
                    }
                }
                Some(raw) = self.inject.recv() => {
                    if self.stream.write_all(&raw).await.is_err() {
                        return;
                    }
                }
                _ = tick.tick() => {}
            }
            if !self.pump().await {
                return;
            }
        }
    }

    /// Handles one frame from the client.
    async fn client_frame(&mut self, mut frame: Vec<u8>) -> bool {
        let state = Arc::clone(&self.shared.state);
        if frame[0] >> 6 == 1 {
            state.log(Ev::ChannelDataIn);
            // RFC 8656 section 12.5: over a stream the client pads ChannelData to a multiple of
            // 4 bytes; the padding is not part of the message (and the server crate expects none).
            // The client must have padded: check it, then strip it.
            let length = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
            assert_eq!(frame.len(), (4 + length + 3) & !3);
            frame.truncate(4 + length);
        } else if let Some(info) = parse_stun(&frame) {
            // coturn REST credentials: the password follows from the user name and the secret.
            if let Some(username) = &info.username {
                {
                    let mut seen = self.shared.state.usernames.lock().unwrap();
                    if !seen.contains(username) {
                        seen.push(username.clone());
                    }
                }
                let password = coturn_password(&self.shared.options.secret, username);
                self.server.add_user(username.clone(), password);
            }
            if info.class == 0 || info.class == 1 {
                match info.method {
                    ALLOCATE if info.username.is_some() => {}
                    REFRESH => state.log(Ev::Refresh(info.lifetime.unwrap_or(600))),
                    CREATE_PERMISSION => {
                        for ip in &info.peers {
                            state.log(Ev::Permission(*ip));
                        }
                    }
                    CHANNEL_BIND => state.log(Ev::ChannelBind),
                    SEND => state.log(Ev::SendIndication),
                    _ => {}
                }
            }
        }
        let now = self.now();
        let reply = self
            .server
            .recv(
                ProtoTransmit::new(frame, TransportType::Tcp, self.client, self.listen),
                now,
            )
            .map(|build| build.build());
        match reply {
            Some(transmit) => self.send(transmit).await,
            None => true,
        }
    }

    async fn send(&mut self, transmit: ProtoTransmit<Vec<u8>>) -> bool {
        if transmit.transport == TransportType::Tcp {
            let mut data = transmit.data;
            // The server crate writes ChannelData unpadded; a real server pads it on a stream.
            if data.first().is_some_and(|first| first >> 6 == 1) {
                data.resize(data.len().div_ceil(4) * 4, 0);
            }
            self.stream.write_all(&data).await.is_ok()
        } else {
            if let Some(socket) = self.sockets.get(&transmit.from) {
                let _ = socket.send_to(&transmit.data, transmit.to).await;
            }
            true
        }
    }

    /// Lets the TURN server make progress: allocation sockets, closes, queued replies.
    async fn pump(&mut self) -> bool {
        for _ in 0..64 {
            let now = self.now();
            match self.server.poll(now) {
                TurnServerPollRet::AllocateSocket {
                    transport,
                    listen_addr,
                    client_addr,
                    allocation_transport,
                    family,
                } => {
                    let result = if family == AddressFamily::IPV4 {
                        match UdpSocket::bind("127.0.0.1:0").await {
                            Ok(socket) => {
                                let relayed = socket.local_addr().unwrap();
                                let socket = Arc::new(socket);
                                self.sockets.insert(relayed, Arc::clone(&socket));
                                let tx = self.udp_tx.clone();
                                tokio::spawn(async move {
                                    let mut buffer = vec![0u8; 4096];
                                    while let Ok((n, from)) = socket.recv_from(&mut buffer).await {
                                        if tx
                                            .send((relayed, from, buffer[..n].to_vec()))
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                });
                                self.shared.state.log(Ev::Allocate);
                                Ok(relayed)
                            }
                            Err(_) => Err(SocketAllocateError::InsufficientCapacity),
                        }
                    } else {
                        Err(SocketAllocateError::AddressFamilyNotSupported)
                    };
                    self.server.allocated_socket(
                        transport,
                        listen_addr,
                        client_addr,
                        allocation_transport,
                        family,
                        result,
                        now,
                    );
                }
                TurnServerPollRet::SocketClose {
                    transport: TransportType::Udp,
                    listen_addr,
                } => {
                    self.sockets.remove(&listen_addr);
                    self.shared.state.log(Ev::AllocationClosed);
                }
                TurnServerPollRet::WaitUntil(_) => break,
                _ => {}
            }
        }
        let now = self.now();
        while let Some(transmit) = self.server.poll_transmit(now) {
            if !self.send(transmit).await {
                return false;
            }
        }
        true
    }
}

fn tls_material() -> (Arc<rustls::ServerConfig>, Vec<u8>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .unwrap();
    (Arc::new(config), cert.to_vec())
}

// ---------------------------------------------------------------------------------------------
// A raw HTTP/1.1 client

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

async fn http(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> HttpResponse {
    http_with(addr, method, path, &[], body).await
}

async fn http_with(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() && !headers.iter().any(|(name, _)| *name == "Content-Type") {
        request.push_str("Content-Type: application/sdp\r\n");
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes()).await.unwrap();
    let _ = stream.write_all(body).await;
    let mut raw = Vec::new();
    let read = tokio::time::timeout(WAIT, stream.read_to_end(&mut raw))
        .await
        .expect("response timed out");
    if !raw.starts_with(b"HTTP/") {
        read.unwrap();
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("no header end");
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    HttpResponse {
        status,
        headers,
        body: body.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// The browser: a str0m peer with a full ICE agent and a UDP socket

#[derive(Default)]
struct BrowserState {
    connected: bool,
    talking: bool,
    received: Vec<i16>,
}

/// What the browser's offer carries as candidates.
#[derive(Clone)]
enum Cands {
    /// Its real `127.0.0.1` host candidate.
    Real,
    /// Host candidates with these addresses instead (the browser still sends from 127.0.0.1).
    Ips(Vec<IpAddr>),
    /// No candidate line at all.
    Nothing,
}

struct Offer {
    rtc: Rtc,
    pending: SdpPendingOffer,
    mid: Mid,
    socket: UdpSocket,
    local: SocketAddr,
    sdp: String,
}

fn crypto() -> Arc<str0m::crypto::CryptoProvider> {
    Arc::new(str0m::crypto::from_feature_flags())
}

fn make_offer(cands: &Cands) -> Offer {
    let clock = SystemClock::shared();
    let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    std_socket.set_nonblocking(true).unwrap();
    let local = std_socket.local_addr().unwrap();
    let socket = UdpSocket::from_std(std_socket).unwrap();
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto())
        .clear_codecs()
        .enable_opus(true, false)
        .build(clock.now().into_std());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut change = rtc.sdp_api();
    let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = change.apply().unwrap();
    let mut sdp = offer.to_sdp_string();
    if !matches!(cands, Cands::Real) {
        sdp = sdp
            .split_inclusive('\n')
            .filter(|line| {
                !line.starts_with("a=candidate:") && !line.starts_with("a=end-of-candidates")
            })
            .collect();
    }
    if let Cands::Ips(ips) = cands {
        for (n, ip) in ips.iter().enumerate() {
            sdp = sdp.replacen(
                "a=ice-ufrag",
                &format!(
                    "a=candidate:{} 1 udp 2113937151 {ip} {} typ host generation 0\r\na=ice-ufrag",
                    n + 1,
                    54_000 + n
                ),
                1,
            );
        }
    }
    Offer {
        rtc,
        pending,
        mid,
        socket,
        local,
        sdp,
    }
}

struct Browser {
    state: Arc<Mutex<BrowserState>>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    session_url: String,
    answer: String,
    http_addr: SocketAddr,
}

impl Offer {
    async fn post(
        self,
        http_addr: SocketAddr,
        start_driver: bool,
    ) -> Result<Browser, HttpResponse> {
        let response = http(http_addr, "POST", "/whip", self.sdp.as_bytes()).await;
        if response.status != 201 {
            return Err(response);
        }
        let session_url = response.headers["location"].clone();
        let answer = response.body;
        let Self {
            mut rtc,
            pending,
            mid,
            socket,
            local,
            ..
        } = self;
        rtc.sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap())
            .unwrap();
        let state = Arc::new(Mutex::new(BrowserState::default()));
        let stop = CancellationToken::new();
        let task = start_driver.then(|| {
            let driver = BrowserDriver {
                rtc,
                socket,
                local,
                mid,
                state: Arc::clone(&state),
                clock: SystemClock::shared(),
            };
            tokio::spawn(driver.run(stop.clone()))
        });
        Ok(Browser {
            state,
            stop,
            task,
            session_url,
            answer,
            http_addr,
        })
    }
}

impl Browser {
    fn with<T>(&self, f: impl FnOnce(&mut BrowserState) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    async fn until(&self, what: &str, mut ready: impl FnMut(&BrowserState) -> bool) {
        let clock = SystemClock::shared();
        let deadline = clock.now() + WAIT;
        loop {
            if self.with(|s| ready(s)) {
                return;
            }
            assert!(clock.now() < deadline, "timed out waiting for {what}");
            clock.sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_connected(&self) {
        self.until("the browser to connect", |s| s.connected).await;
    }

    fn connected(&self) -> bool {
        self.with(|s| s.connected)
    }

    fn talk(&self, on: bool) {
        self.with(|s| s.talking = on);
    }

    fn received_samples(&self) -> usize {
        self.with(|s| s.received.len())
    }

    /// The candidate lines of the answer: `(priority, address, rest)`.
    fn answer_candidates(&self) -> Vec<(u32, SocketAddr)> {
        self.answer
            .lines()
            .filter_map(|line| line.trim().strip_prefix("a=candidate:"))
            .map(|rest| {
                let parts: Vec<&str> = rest.split(' ').collect();
                let prio: u32 = parts[3].parse().unwrap();
                let addr: SocketAddr = format!("{}:{}", parts[4], parts[5]).parse().unwrap();
                (prio, addr)
            })
            .collect()
    }

    async fn delete(&self) -> HttpResponse {
        http(self.http_addr, "DELETE", &self.session_url, &[]).await
    }

    async fn stop(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct BrowserDriver {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    mid: Mid,
    state: Arc<Mutex<BrowserState>>,
    clock: SharedClock,
}

impl BrowserDriver {
    async fn run(mut self, stop: CancellationToken) {
        let mut encoder = LibOpusFactory.encoder(&OpusSettings::default()).unwrap();
        let mut decoder = LibOpusFactory.decoder().unwrap();
        let pt: Pt = self
            .rtc
            .codec_config()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
            .unwrap();
        let origin = self.clock.now();
        let mut next_frame = 0u64;
        let mut tone_phase = 0usize;
        let mut buf = vec![0u8; 2048];
        let mut pcm = vec![0i16; MAX_PACKET_SAMPLES];
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        loop {
            let timeout = loop {
                match self.rtc.poll_output() {
                    Ok(Output::Timeout(at)) => break at,
                    Ok(Output::Transmit(t)) => {
                        let _ = self.socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(Event::Connected)) => {
                        self.state.lock().unwrap().connected = true;
                    }
                    Ok(Output::Event(Event::MediaData(data))) => {
                        if let Ok(n) = decoder.decode(&data.data, &mut pcm) {
                            self.state
                                .lock()
                                .unwrap()
                                .received
                                .extend_from_slice(&pcm[..n]);
                        }
                    }
                    Ok(Output::Event(_)) => {}
                    Err(_) => return,
                }
            };
            if !self.rtc.is_alive() {
                return;
            }
            let frame_due = origin + Duration::from_millis(20 * next_frame);
            let wake = tokio::time::Instant::from_std(timeout).min(frame_due);
            let now = self.clock.now();
            let input = if wake <= now {
                None
            } else {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    received = self.socket.recv_from(&mut buf) => Some(received),
                    () = self.clock.sleep_until(wake) => None,
                }
            };
            let now = self.clock.now();
            if let Some(Ok((n, source))) = input
                && let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, &buf[..n])
            {
                let _ = self
                    .rtc
                    .handle_input(Input::Receive(now.into_std(), receive));
            }
            let _ = self.rtc.handle_input(Input::Timeout(now.into_std()));
            if stop.is_cancelled() {
                return;
            }
            while self.clock.now() >= origin + Duration::from_millis(20 * next_frame) {
                let (connected, talking) = {
                    let state = self.state.lock().unwrap();
                    (state.connected, state.talking)
                };
                if connected && talking {
                    let frame: Vec<i16> = (0..FRAME_SAMPLES)
                        .map(|n| {
                            let phase =
                                2.0 * std::f32::consts::PI * 1_000.0 * (tone_phase + n) as f32
                                    / 48_000.0;
                            (phase.sin() * 8_000.0) as i16
                        })
                        .collect();
                    tone_phase += FRAME_SAMPLES;
                    let len = encoder.encode(&frame, &mut packet).unwrap();
                    let _ = self.rtc.writer(self.mid).map(|writer| {
                        writer.write(
                            pt,
                            self.clock.now().into_std(),
                            MediaTime::new(next_frame * 960, Frequency::FORTY_EIGHT_KHZ),
                            packet[..len].to_vec(),
                        )
                    });
                }
                next_frame += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The server rig and the engine side

fn relay_config(
    mock: &MockTurn,
    scheme: &str,
    host: &str,
    only: bool,
    setup_timeout: Duration,
) -> WebRtcConfig {
    WebRtcConfig {
        http_addr: ([127, 0, 0, 1], 0).into(),
        udp_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 8,
        connect_timeout: Duration::from_secs(5),
        disconnect_grace: Duration::from_millis(500),
        send_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        turn: Some(TurnConfig::new(TurnProvider::Coturn {
            urls: vec![mock.url(scheme, host)],
            secret: Secret::new(SECRET.to_owned()),
        })),
        turn_relay: Some(RelayConfig {
            only,
            setup_timeout,
            release_timeout: Duration::from_millis(500),
            // The mock TURN server and the "browser" are on the loopback interface.
            allow_local_peers: true,
            ..RelayConfig::default()
        }),
        ..WebRtcConfig::default()
    }
}

fn plain_config(mock: &MockTurn, only: bool) -> WebRtcConfig {
    relay_config(mock, "turn", "127.0.0.1", only, Duration::from_secs(3))
}

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: WebRtcServer,
}

impl Rig {
    async fn start(config: WebRtcConfig, clock: SharedClock) -> Self {
        let parent = CancellationToken::new();
        let server = WebRtcServer::bind(config, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    fn http_addr(&self) -> SocketAddr {
        self.server.http_addr()
    }

    async fn accept(&mut self) -> TransportSession {
        let link = tokio::time::timeout(WAIT, self.server.accept(&self.parent))
            .await
            .expect("no call arrived")
            .unwrap()
            .expect("the server stopped");
        TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap()
    }

    /// A browser that connects through the relay, with the engine's session started.
    async fn call(&mut self) -> (Browser, TransportSession) {
        let browser = make_offer(&Cands::Real)
            .post(self.http_addr(), true)
            .await
            .unwrap_or_else(|r| panic!("{} {}", r.status, r.body));
        let mut session = self.accept().await;
        match next_event(&mut session).await {
            TransportEvent::Started(_) => {}
            other => panic!("expected Started, got {other:?}"),
        }
        browser.wait_connected().await;
        (browser, session)
    }

    async fn active(&self, count: u64) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.server.stats().sessions_active != count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "sessions_active is {}",
                self.server.stats().sessions_active
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn next_event(session: &mut TransportSession) -> TransportEvent {
    tokio::time::timeout(WAIT, session.recv())
        .await
        .expect("no event")
        .expect("session ended")
}

/// Collects the caller audio the engine receives until `samples` have arrived.
async fn caller_audio(session: &mut TransportSession, samples: usize) -> Vec<i16> {
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut audio = Vec::new();
    while audio.len() < samples {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, session.recv())
            .await
            .unwrap_or_else(|_| panic!("only {} caller samples", audio.len()))
            .expect("session ended");
        if let TransportEvent::Audio(frame) = &event {
            audio.extend(
                frame
                    .data()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        }
    }
    audio
}

fn pitch_hz(pcm: &[i16], rate: u32) -> f32 {
    let crossings = pcm.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    crossings as f32 * rate as f32 / pcm.len() as f32
}

fn tone_frame(clock: &SharedClock, seq: u64) -> AudioFrame {
    let rate = SampleRate::Hz24000;
    let count = (rate.hz() / 50) as usize;
    let start = seq as usize * count;
    let pcm: Vec<u8> = (start..start + count)
        .flat_map(|n| {
            let phase = 2.0 * std::f32::consts::PI * 500.0 * n as f32 / rate.hz() as f32;
            ((phase.sin() * 8_000.0) as i16).to_le_bytes()
        })
        .collect();
    AudioFrame::new(Bytes::from(pcm), rate, clock.now(), seq, TurnEpoch::ZERO).unwrap()
}

async fn send_tone(session: &TransportSession, clock: &SharedClock, frames: u64, first: u64) {
    for seq in first..first + frames {
        session
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::ZERO,
                utterance: UtteranceId::new(1),
                frame: tone_frame(clock, seq),
            })
            .await
            .unwrap();
    }
}

/// Both directions of a call: the browser talks 0.6 s that the engine hears, the engine says
/// 0.6 s that the browser hears.
async fn assert_audio_flows(
    browser: &Browser,
    session: &mut TransportSession,
    clock: &SharedClock,
    first_seq: u64,
) {
    browser.talk(true);
    let caller = caller_audio(session, 16_000 * 6 / 10).await;
    browser.talk(false);
    let hz = pitch_hz(&caller[caller.len() / 4..], 16_000);
    assert!((950.0..1_050.0).contains(&hz), "caller pitch {hz} Hz");
    let before = browser.received_samples();
    send_tone(session, clock, 30, first_seq).await;
    browser
        .until("bot audio", |s| {
            s.received.len() >= before + 48_000 * 4 / 10
        })
        .await;
    let received = browser.with(|s| s.received.clone());
    let steady = &received[before + 4_800..received.len().min(before + 48_000 * 4 / 10)];
    let hz = pitch_hz(steady, 48_000);
    assert!((470.0..530.0).contains(&hz), "bot pitch {hz} Hz");
}

// ---------------------------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn a_server_reachable_only_through_the_relay_completes_calls_and_audio_flows_both_ways() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let mut rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let (browser, mut session) = rig.call().await;

    // The answer holds the relayed address and nothing else: no direct candidate to fall back on.
    let candidates = browser.answer_candidates();
    assert_eq!(candidates.len(), 1, "{}", browser.answer);
    let relayed = candidates[0].1;
    assert_ne!(relayed.port(), rig.server.udp_addr().port());
    assert_ne!(relayed.port(), rig.server.candidate_addr().port());

    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;

    // Every packet went through the TURN allocation: the server's own UDP socket stayed silent.
    let stats = rig.server.stats();
    assert_eq!(stats.datagrams_in, 0, "{stats:?}");
    assert_eq!(stats.datagrams_out, 0, "{stats:?}");
    assert_eq!(stats.relay_allocations, 1, "{stats:?}");
    assert_eq!(stats.relay_active, 1, "{stats:?}");
    assert_eq!(stats.relay_setup_failures, 0, "{stats:?}");
    assert!(stats.relay_packets_in > 25, "{stats:?}");
    assert!(stats.relay_packets_out > 25, "{stats:?}");
    assert_eq!(stats.relay_protocol_errors, 0, "{stats:?}");

    // What the TURN server saw: one allocation, a permission for the browser's IP made before
    // any packet from it, nothing dropped for lack of a permission, a bound channel carrying the
    // server's media.
    let events = mock.events();
    assert_eq!(mock.connections(), 1);
    assert_eq!(mock.count(|e| *e == Ev::Allocate), 1);
    let first_permission = events
        .iter()
        .position(|e| matches!(e, Ev::Permission(ip) if *ip == IpAddr::from([127, 0, 0, 1])))
        .expect("no permission for the browser's IP");
    let first_packet = events
        .iter()
        .position(|e| matches!(e, Ev::PeerPacket { .. }))
        .expect("the browser never reached the relay");
    assert!(
        first_permission < first_packet,
        "the permission came after the first packet: {events:?}"
    );
    assert_eq!(
        mock.count(|e| matches!(
            e,
            Ev::PeerPacket {
                forwarded: false,
                ..
            }
        )),
        0,
        "the TURN server dropped packets of the browser: {events:?}"
    );
    assert!(mock.count(|e| *e == Ev::ChannelBind) >= 1);
    assert!(mock.count(|e| *e == Ev::ChannelDataIn) > 20, "{events:?}");

    // The call ends: the allocation is released with a Refresh of lifetime 0.
    session.send(TransportCommand::Stop).await.unwrap();
    mock.until("the allocation to be released", |events| {
        events.contains(&Ev::Refresh(0)) && events.contains(&Ev::AllocationClosed)
    })
    .await;
    rig.active(0).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while rig.server.stats().relay_active != 0 {
        assert!(tokio::time::Instant::now() < deadline, "relay_active stuck");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    browser.stop().await;
}

/// A `MakeWriter` into one shared buffer.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The TURN client crates log whole STUN messages at `trace`. What ends up in a log when an
/// operator (or an embedder's `RUST_LOG`) turns everything on, and whether the filter of
/// `brisvox_transport::webrtc::turn_client_log_allowed` keeps the crates out of it.
#[tokio::test]
async fn the_turn_client_crates_logs_hold_no_password_and_the_filter_removes_them() {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::filter::{FilterExt as _, LevelFilter, filter_fn};
    use tracing_subscriber::layer::SubscriberExt as _;

    // Two layers see every event in this process at TRACE: one raw (what an embedder with
    // `RUST_LOG=trace` and no filter gets), one behind the filter.
    let raw = LogCapture::default();
    let filtered = LogCapture::default();
    let (raw_sink, filtered_sink) = (raw.clone(), filtered.clone());
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || raw_sink.clone())
                .with_filter(LevelFilter::TRACE),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || filtered_sink.clone())
                .with_filter(LevelFilter::TRACE.and(filter_fn(
                    brisvox_transport::webrtc::turn_client_log_allowed,
                ))),
        );
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let mut rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let (browser, mut session) = rig.call().await;
    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;
    session.send(TransportCommand::Stop).await.unwrap();
    mock.until("the allocation to be released", |events| {
        events.contains(&Ev::Refresh(0))
    })
    .await;
    browser.stop().await;

    let usernames = mock.state.usernames.lock().unwrap().clone();
    assert!(!usernames.is_empty());
    let raw = String::from_utf8_lossy(&raw.0.lock().unwrap()).into_owned();
    let filtered = String::from_utf8_lossy(&filtered.0.lock().unwrap()).into_owned();
    let client_crate = |line: &str| {
        ["turn_client_proto", "stun_proto", "turn_types"]
            .iter()
            .any(|name| line.contains(name))
    };
    // The raw log proves the premise: at TRACE the crates log every received STUN message in
    // full (nonce, realm, the MESSAGE-INTEGRITY HMAC of the response). They never log the
    // password (checked below), and the outgoing USERNAME is not in their log lines either, but
    // the credential material that is there is reason enough to keep them out of a log.
    assert!(
        raw.lines().any(|line| client_crate(line)
            && line.contains("received STUN message")
            && line.contains("NONCE")
            && line.contains("MESSAGE-INTEGRITY")),
        "the TURN client crates no longer log whole STUN messages; re-check the filter's reason"
    );
    for name in &usernames {
        // The password itself (derived from the shared secret and the user name) and the
        // shared secret never appear in any log, filtered or not.
        let password = coturn_password(SECRET, name);
        for (what, log) in [("raw", &raw), ("filtered", &filtered)] {
            assert!(
                !log.contains(&password),
                "{what} log holds the TURN password"
            );
            assert!(!log.contains(SECRET), "{what} log holds the shared secret");
        }
    }
    // Behind the filter the client crates keep only warnings and errors (the test's own mock TURN
    // server is built on the same crates and logs its refusals at ERROR; its own target,
    // `turn_server_proto`, is not filtered).
    for line in filtered.lines() {
        assert!(
            !client_crate(line) || line.contains(" ERROR ") || line.contains(" WARN "),
            "{line}"
        );
    }
    assert!(
        raw.lines()
            .any(|line| client_crate(line) && line.contains(" INFO ")),
        "the raw log has no info-level event of the client crates any more"
    );
}

#[tokio::test]
async fn with_a_direct_address_too_the_relay_is_the_lower_priority_candidate() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let mut rig = Rig::start(plain_config(&mock, false), clock.clone()).await;
    let (browser, _session) = rig.call().await;
    let mut candidates = browser.answer_candidates();
    assert_eq!(candidates.len(), 2, "{}", browser.answer);
    candidates.sort_by_key(|(prio, _)| std::cmp::Reverse(*prio));
    assert_eq!(candidates[0].1, rig.server.candidate_addr(), "direct first");
    assert_ne!(candidates[1].1, rig.server.candidate_addr());
    assert!(candidates[0].0 > candidates[1].0);
    // A working direct path wins; the relay was still set up (and released with the call).
    let stats = rig.server.stats();
    assert_eq!(stats.relay_allocations, 1, "{stats:?}");
    browser.stop().await;
}

#[tokio::test]
async fn allocation_permissions_and_channels_are_refreshed_on_the_clock_and_the_call_goes_on() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let mut config = plain_config(&mock, true);
    // The server's clock jumps; ICE may think the browser went quiet in between. The call must
    // not give up on that (the browser's real-time checks bring it back).
    config.disconnect_grace = Duration::from_secs(10 * 3_600);
    let mut rig = Rig::start(config, clock.clone()).await;
    let (browser, mut session) = rig.call().await;
    let relayed = browser.answer_candidates()[0].1;
    let poker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let poker_addr = poker.local_addr().unwrap();
    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;

    // The allocation lasts 30 minutes (what the client asks for), permissions 5 and channels 10: without the
    // client's refreshes the packets below would be dropped (no allocation, no permission) and
    // the allocation would be gone. Each step moves both clocks (the TURN server's too); the
    // packet from the poker wakes the client's task, which then finds the refreshes due.
    let delivered = |mock: &MockTurn| {
        mock.count(|e| matches!(e, Ev::PeerPacket { from, forwarded: true } if *from == poker_addr))
    };
    for step in 1..=70usize {
        clock.advance(Duration::from_secs(30));
        let _ = poker.send_to(b"\x00\x00poke", relayed).await;
        let deadline = tokio::time::Instant::now() + WAIT;
        while delivered(&mock) < step {
            assert!(
                tokio::time::Instant::now() < deadline,
                "step {step} ({} s): the relay stopped delivering: {:?} {:?}",
                step * 30,
                rig.server.stats(),
                mock.events()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let events = mock.events();
    let count = |pick: fn(&Ev) -> bool| events.iter().filter(|e| pick(e)).count();
    let summary: Vec<&Ev> = events
        .iter()
        .filter(|e| !matches!(e, Ev::PeerPacket { .. } | Ev::ChannelDataIn))
        .collect();
    assert!(
        count(|e| matches!(e, Ev::Refresh(n) if *n > 0)) >= 1,
        "refresh: {summary:?}"
    );
    assert!(
        count(|e| matches!(e, Ev::Permission(_))) >= 4,
        "permission: {summary:?}"
    );
    assert!(
        count(|e| *e == Ev::ChannelBind) >= 2,
        "channel: {summary:?}"
    );
    assert!(
        !events.contains(&Ev::AllocationClosed),
        "closed: {summary:?}"
    );
    assert_eq!(rig.server.stats().relay_lost, 0);

    // 35 virtual minutes later the call still carries audio through the same relay: the caller
    // is heard, and the server still answers through the relay (ICE and RTCP). (Bot audio is not jitter buffer
    // cannot be expected to play an RTP timeline that jumped by 35 minutes.)
    browser.talk(true);
    let caller = caller_audio(&mut session, 16_000 * 6 / 10).await;
    browser.talk(false);
    let hz = pitch_hz(&caller[caller.len() / 4..], 16_000);
    assert!((950.0..1_050.0).contains(&hz), "caller pitch {hz} Hz");
    let out_before = rig.server.stats().relay_packets_out;
    let deadline = tokio::time::Instant::now() + WAIT;
    while rig.server.stats().relay_packets_out < out_before + 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing left the relay"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rig.server.stats().sessions_active, 1);
    browser.stop().await;
}

#[tokio::test]
async fn a_permission_that_lapsed_unrefreshed_is_created_again_and_the_call_goes_on() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let mut config = plain_config(&mock, true);
    config.disconnect_grace = Duration::from_secs(10 * 3_600);
    let mut rig = Rig::start(config, clock.clone()).await;
    let (browser, mut session) = rig.call().await;
    let relayed = browser.answer_candidates()[0].1;
    let poker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let poker_addr = poker.local_addr().unwrap();
    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;
    let permissions = |mock: &MockTurn| mock.count(|e| matches!(e, Ev::Permission(_)));
    assert_eq!(permissions(&mock), 1, "{:?}", mock.events());

    // One jump past the permission's five minutes: the refresh never had a chance (a TURN server
    // that did not answer in time, or refused the refresh, leaves the client in the same
    // state). The client must ask for the permission again instead of dropping the peer for the
    // rest of the call.
    clock.advance(Duration::from_secs(400));
    let deadline = tokio::time::Instant::now() + WAIT;
    while permissions(&mock) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the permission was not created again: {:?} {:?}",
            rig.server.stats(),
            mock.events()
        );
        // The relay task's timer runs on real time (this clock only jumps): the retry delay
        // passes in virtual seconds, and a frame from the TURN server (a STUN success response
        // to nothing the client asked, which it ignores) wakes the task to notice.
        clock.advance(Duration::from_secs(3));
        let mut nudge = vec![0x01, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
        nudge.extend_from_slice(&[7; 12]);
        mock.inject(0, nudge);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // And the peer's packets go through again.
    let _ = poker.send_to(b"\x00\x00poke", relayed).await;
    mock.until("the poke to be forwarded", |events| {
        events
            .iter()
            .any(|e| matches!(e, Ev::PeerPacket { from, forwarded: true } if *from == poker_addr))
    })
    .await;
    let stats = rig.server.stats();
    assert_eq!(stats.relay_lost, 0, "{stats:?}");
    assert!(stats.relay_permission_failures >= 1, "{stats:?}");
    browser.stop().await;
}

#[tokio::test]
async fn a_hangup_and_a_server_shutdown_release_the_allocations() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let first = make_offer(&Cands::Real)
        .post(rig.http_addr(), false)
        .await
        .unwrap();
    let second = make_offer(&Cands::Real)
        .post(rig.http_addr(), false)
        .await
        .unwrap();
    assert_eq!(mock.connections(), 2, "one TURN connection per call");
    assert_eq!(rig.server.stats().relay_active, 2);

    // DELETE: the first call's allocation is released; the second one is untouched.
    assert_eq!(first.delete().await.status, 200);
    mock.until("the first allocation to be released", |events| {
        events
            .iter()
            .filter(|e| **e == Ev::AllocationClosed)
            .count()
            == 1
    })
    .await;
    assert_eq!(rig.server.stats().relay_active, 1);

    // Shutdown releases the rest, within the release timeout.
    rig.server.shutdown(WAIT).await.unwrap();
    mock.until("the second allocation to be released", |events| {
        events
            .iter()
            .filter(|e| **e == Ev::AllocationClosed)
            .count()
            == 2
    })
    .await;
    assert_eq!(rig.server.stats().relay_active, 0);
    assert_eq!(
        mock.count(|e| *e == Ev::Refresh(0)),
        2,
        "{:?}",
        mock.events()
    );
    first.stop().await;
    second.stop().await;
}

#[tokio::test]
async fn a_turn_server_that_never_answers_costs_the_setup_timeout_and_the_call_answers_normally() {
    let clock = StepClock::new();
    let mock = MockTurn::start(
        clock.clone(),
        MockOptions {
            silent: true,
            ..MockOptions::default()
        },
    )
    .await;
    let setup = Duration::from_millis(400);
    let mut rig = Rig::start(
        relay_config(&mock, "turn", "127.0.0.1", false, setup),
        clock.clone(),
    )
    .await;
    let started = tokio::time::Instant::now();
    let (browser, mut session) = rig.call().await;
    let took = started.elapsed();
    assert!(
        took >= setup && took < Duration::from_secs(3),
        "the call took {took:?}"
    );
    // The answer has the direct candidate only, and the call works over it.
    let candidates = browser.answer_candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].1, rig.server.candidate_addr());
    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;
    let stats = rig.server.stats();
    assert_eq!(stats.relay_setup_failures, 1, "{stats:?}");
    assert_eq!(stats.relay_allocations, 0, "{stats:?}");
    assert_eq!(stats.relay_active, 0, "{stats:?}");
    assert!(stats.datagrams_in > 0);
    browser.stop().await;
}

#[tokio::test]
async fn in_relay_only_mode_a_failed_allocation_refuses_the_offer_with_503() {
    let clock = StepClock::new();
    let mock = MockTurn::start(
        clock.clone(),
        MockOptions {
            silent: true,
            ..MockOptions::default()
        },
    )
    .await;
    let rig = Rig::start(
        relay_config(&mock, "turn", "127.0.0.1", true, Duration::from_millis(300)),
        clock.clone(),
    )
    .await;
    let refused = make_offer(&Cands::Real)
        .post(rig.http_addr(), true)
        .await
        .err()
        .expect("the offer must be refused");
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(refused.headers.contains_key("retry-after"));
    let stats = rig.server.stats();
    assert_eq!(stats.relay_setup_failures, 1, "{stats:?}");
    assert_eq!(stats.offers_refused, 1, "{stats:?}");
    assert_eq!(stats.sessions_active, 0, "{stats:?}");
}

#[tokio::test]
async fn a_relay_only_503_gives_the_single_use_session_token_back() {
    const SESSION_SECRET: &str = "a-session-secret-of-at-least-32-bytes!";
    let clock = StepClock::new();
    let mock = MockTurn::start(
        clock.clone(),
        MockOptions {
            silent: true,
            ..MockOptions::default()
        },
    )
    .await;
    let rig = Rig::start(
        WebRtcConfig {
            sessions: Some(
                SessionSecrets::new(Secret::new(SESSION_SECRET.to_owned()), None).unwrap(),
            ),
            ..relay_config(&mock, "turn", "127.0.0.1", true, Duration::from_millis(300))
        },
        clock.clone(),
    )
    .await;
    let now = SystemClock::shared()
        .wall()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = mint_token(
        &Secret::new(SESSION_SECRET.to_owned()),
        &TokenSpec {
            audience: Audience::Webrtc,
            sid: "visitor-1".to_owned(),
            not_before: now.saturating_sub(5),
            expires: now + 120,
            agent: None,
            max_s: None,
            meta: BTreeMap::new(),
        },
    )
    .unwrap();
    let bearer = format!("Bearer {token}");
    let sdp = make_offer(&Cands::Real).sdp;
    // The relay cannot be set up (the TURN server never answers): 503, twice. The second post
    // is not a replay (401): the refused call did not use the token up.
    for attempt in 1..=2 {
        let refused = http_with(
            rig.http_addr(),
            "POST",
            "/whip",
            &[("Authorization", &bearer)],
            sdp.as_bytes(),
        )
        .await;
        assert_eq!(refused.status, 503, "attempt {attempt}: {}", refused.body);
    }
    let stats = rig.server.stats();
    assert_eq!(stats.relay_setup_failures, 2, "{stats:?}");
    assert_eq!(stats.offers_unauthorized, 0, "{stats:?}");
}

#[tokio::test]
async fn wrong_credentials_fail_fast_and_the_call_answers_with_its_normal_candidates() {
    let clock = StepClock::new();
    let mock = MockTurn::start(
        clock.clone(),
        MockOptions {
            secret: "not-the-servers-secret".to_owned(),
            ..MockOptions::default()
        },
    )
    .await;
    let mut rig = Rig::start(plain_config(&mock, false), clock.clone()).await;
    let started = tokio::time::Instant::now();
    let (browser, _session) = rig.call().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(browser.answer_candidates().len(), 1);
    let stats = rig.server.stats();
    assert_eq!(stats.relay_setup_failures, 1, "{stats:?}");
    assert_eq!(stats.relay_active, 0, "{stats:?}");
    assert_eq!(mock.count(|e| *e == Ev::Allocate), 0, "{:?}", mock.events());
    browser.stop().await;
}

#[tokio::test]
async fn a_permission_is_needed_for_the_browsers_address_or_its_packets_are_dropped_by_the_turn_server()
 {
    // The offer names only 203.0.113.9; the browser really sends from 127.0.0.1. The TURN
    // server (which enforces permissions) drops its connectivity checks, so ICE cannot complete:
    // this is why the permissions must be right and exist before the answer.
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let browser = make_offer(&Cands::Ips(vec![IpAddr::from([203, 0, 113, 9])]))
        .post(rig.http_addr(), true)
        .await
        .unwrap();
    mock.until("the browser's checks to reach the relay", |events| {
        events.iter().any(|e| {
            matches!(
                e,
                Ev::PeerPacket {
                    forwarded: false,
                    ..
                }
            )
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!browser.connected());
    let events = mock.events();
    assert!(events.contains(&Ev::Permission(IpAddr::from([203, 0, 113, 9]))));
    assert!(!events.contains(&Ev::Permission(IpAddr::from([127, 0, 0, 1]))));
    assert_eq!(
        mock.count(|e| matches!(
            e,
            Ev::PeerPacket {
                forwarded: true,
                ..
            }
        )),
        0
    );
    browser.stop().await;
}

#[tokio::test]
async fn an_offer_without_an_ip_candidate_gets_no_allocation() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    // Direct candidates available: answered normally, no TURN connection made.
    let mut rig = Rig::start(plain_config(&mock, false), clock.clone()).await;
    let browser = make_offer(&Cands::Nothing)
        .post(rig.http_addr(), true)
        .await
        .unwrap();
    let mut session = rig.accept().await;
    assert!(matches!(
        next_event(&mut session).await,
        TransportEvent::Started(_)
    ));
    assert_eq!(browser.answer_candidates().len(), 1);
    assert_eq!(mock.connections(), 0);
    assert_eq!(rig.server.stats().relay_setup_failures, 1);
    browser.stop().await;

    // Relay only: nothing could work, so the offer is refused.
    let rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let refused = make_offer(&Cands::Nothing)
        .post(rig.http_addr(), true)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status, 503);
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn a_malformed_offer_is_refused_before_any_allocation_is_made() {
    let clock = StepClock::new();
    let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
    let rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
    let response = http(rig.http_addr(), "POST", "/whip", b"v=0\r\ns=-\r\n").await;
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(mock.connections(), 0);
    assert_eq!(rig.server.stats().relay_setup_failures, 0);
}

/// Bytes a hostile or broken TURN server could send instead of a frame.
fn bad_frames() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        // Neither STUN nor ChannelData.
        ("garbage", vec![0xFF; 32]),
        // ChannelData that claims 60000 bytes.
        ("oversize", vec![0x40, 0x00, 0xEA, 0x60, 1, 2, 3, 4]),
        // A STUN header with a length that is not a multiple of 4.
        (
            "bad length",
            [
                &[0x01, 0x01, 0x00, 0x03, 0x21, 0x12, 0xA4, 0x42][..],
                &[0u8; 12][..],
            ]
            .concat(),
        ),
    ]
}

#[tokio::test]
async fn garbage_or_oversize_frames_close_that_allocation_and_leave_other_calls_alone() {
    for (name, bad) in bad_frames() {
        let clock = StepClock::new();
        let mock = MockTurn::start(clock.clone(), MockOptions::default()).await;
        let mut rig = Rig::start(plain_config(&mock, true), clock.clone()).await;
        let (victim, victim_session) = rig.call().await;
        let (neighbour, mut neighbour_session) = rig.call().await;
        assert_eq!(rig.server.stats().relay_active, 2);

        // The victim's TURN connection (the first one) delivers a bad frame.
        mock.inject(0, bad);
        let deadline = tokio::time::Instant::now() + WAIT;
        while rig.server.stats().relay_protocol_errors == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name}: not noticed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let stats = rig.server.stats();
        assert_eq!(stats.relay_protocol_errors, 1, "{name}: {stats:?}");
        assert_eq!(stats.relay_lost, 1, "{name}: {stats:?}");
        assert_eq!(stats.relay_active, 1, "{name}: {stats:?}");
        assert_eq!(stats.call_panics, 0, "{name}: {stats:?}");

        // The neighbour's call is untouched: audio still flows both ways through its relay.
        assert_audio_flows(&neighbour, &mut neighbour_session, &rig.clock, 0).await;
        assert_eq!(rig.server.stats().relay_active, 1);

        // The victim lost its only path but is still a call (ICE gives up on it later); the
        // client hanging up ends it, and the server and the neighbour carry on.
        assert_eq!(rig.server.stats().sessions_active, 2);
        assert_eq!(victim.delete().await.status, 200);
        rig.active(1).await;
        victim.stop().await;
        neighbour.stop().await;
        drop((victim_session, neighbour_session));
    }
}

#[tokio::test]
async fn turns_over_tls_works_and_an_untrusted_certificate_is_refused() {
    let clock = StepClock::new();
    let (tls, cert) = tls_material();
    let mock = MockTurn::start(
        clock.clone(),
        MockOptions {
            tls: Some(tls),
            ..MockOptions::default()
        },
    )
    .await;

    // Trusted (the test certificate is an extra root): the call runs through the TLS relay.
    let mut config = relay_config(&mock, "turns", "localhost", true, Duration::from_secs(3));
    config.turn_relay.as_mut().unwrap().extra_roots = vec![cert];
    let mut rig = Rig::start(config, clock.clone()).await;
    let (browser, mut session) = rig.call().await;
    assert_audio_flows(&browser, &mut session, &rig.clock, 0).await;
    let stats = rig.server.stats();
    assert_eq!(stats.relay_allocations, 1, "{stats:?}");
    assert_eq!(stats.datagrams_in, 0, "{stats:?}");
    browser.stop().await;
    drop(session);

    // Not trusted (webpki roots only): the handshake fails, relay-only mode refuses the offer.
    let rig = Rig::start(
        relay_config(&mock, "turns", "localhost", true, Duration::from_secs(3)),
        clock.clone(),
    )
    .await;
    let refused = make_offer(&Cands::Real)
        .post(rig.http_addr(), true)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.status, 503);
    assert_eq!(rig.server.stats().relay_setup_failures, 1);
    assert_eq!(rig.server.stats().relay_allocations, 0);
}

#[test]
fn relay_needs_turn_credentials_and_ice_lite() {
    let relay = Some(RelayConfig::default());
    let no_turn = WebRtcConfig {
        turn_relay: relay.clone(),
        ..WebRtcConfig::default()
    };
    assert!(no_turn.validate().unwrap_err().to_string().contains("turn"));
    let with_turn = |ice_lite: bool| WebRtcConfig {
        turn_relay: relay.clone(),
        turn: Some(TurnConfig::new(TurnProvider::Coturn {
            urls: vec!["turns:turn.example.com:443?transport=tcp".to_owned()],
            secret: Secret::new("s".to_owned()),
        })),
        ice_lite,
        ..WebRtcConfig::default()
    };
    with_turn(true).validate().unwrap();
    assert!(
        with_turn(false)
            .validate()
            .unwrap_err()
            .to_string()
            .contains("ice_lite")
    );
}
