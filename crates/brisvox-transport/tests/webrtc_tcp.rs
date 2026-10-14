//! ICE-TCP tests: the passive TCP candidate of the WebRTC transport.
//!
//! The browser is a `str0m` peer whose only candidate is a TCP-active one: it connects to the
//! server's TCP listener and speaks RFC 4571 frames over it (no UDP socket at all, so a pass
//! proves that STUN, DTLS and SRTP all went over TCP). Real sockets on 127.0.0.1, real clock.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)] // test helpers (see clippy.toml)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{SharedClock, SystemClock, TurnEpoch};
use brisvox_transport::webrtc::opus::{
    FRAME_SAMPLES, LibOpusFactory, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusFactory, OpusSettings,
};
use brisvox_transport::webrtc::{WebRtcConfig, WebRtcServer};
use brisvox_transport::{
    SessionConfig, Transport, TransportCommand, TransportEvent, TransportSession, UtteranceId,
};
use bytes::Bytes;
use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, TcpType};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------------------------
// HTTP signaling (a minimal client)

async fn post_offer(addr: SocketAddr, sdp: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /whip HTTP/1.1\r\nHost: test\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
        sdp.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(sdp.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut raw))
        .await
        .expect("response timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

fn crypto() -> Arc<str0m::crypto::CryptoProvider> {
    Arc::new(str0m::crypto::from_feature_flags())
}

// ---------------------------------------------------------------------------------------------
// The browser, over TCP

/// How the browser cuts its outgoing byte stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writes {
    /// One write per frame.
    Plain,
    /// Every frame cut into pieces of 5 bytes, written one by one.
    Split,
    /// All frames of one round concatenated into a single write.
    Coalesced,
}

#[derive(Default)]
struct State {
    connected: bool,
    closed: bool,
    talking: bool,
    /// The browser stops reading its socket (a stalled peer).
    paused: bool,
    received: Vec<i16>,
}

struct TcpBrowser {
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    answer: String,
}

impl TcpBrowser {
    async fn connect(http: SocketAddr, tcp: SocketAddr, writes: Writes) -> Self {
        Self::connect_with(http, tcp, writes, false).await
    }

    /// `small_buffer`: a tiny receive buffer, so a browser that stops reading fills the path up
    /// after a few kilobytes instead of megabytes.
    async fn connect_with(
        http: SocketAddr,
        tcp: SocketAddr,
        writes: Writes,
        small_buffer: bool,
    ) -> Self {
        let clock = SystemClock::shared();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        if small_buffer {
            socket.set_recv_buffer_size(2048).unwrap();
        }
        let stream = socket.connect(tcp).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let local = stream.local_addr().unwrap();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(crypto())
            .clear_codecs()
            .enable_opus(true, false)
            .build(clock.now().into_std());
        rtc.add_local_candidate(
            Candidate::builder()
                .tcp()
                .host(local)
                .tcptype(TcpType::Active)
                .build()
                .unwrap(),
        );
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = change.apply().unwrap();
        let (status, answer) = post_offer(http, &offer.to_sdp_string()).await;
        assert_eq!(status, 201, "{answer}");
        rtc.sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap())
            .unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = CancellationToken::new();
        let (read_half, write_half) = stream.into_split();
        let (frames_tx, frames_rx) = mpsc::channel(256);
        tokio::spawn(read_frames(read_half, frames_tx, Arc::clone(&state)));
        let driver = Driver {
            rtc,
            write_half,
            frames: frames_rx,
            local,
            server: tcp,
            mid,
            state: Arc::clone(&state),
            clock,
            writes,
        };
        let task = tokio::spawn(driver.run(stop.clone()));
        Self {
            state,
            stop,
            task,
            answer,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    async fn until(&self, what: &str, mut ready: impl FnMut(&State) -> bool) {
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

    async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

/// Reads RFC 4571 frames of any chunking.
async fn read_frames(
    mut read_half: OwnedReadHalf,
    out: mpsc::Sender<Bytes>,
    state: Arc<Mutex<State>>,
) {
    let mut header = [0u8; 2];
    loop {
        while state.lock().unwrap().paused {
            SystemClock::shared().sleep(Duration::from_millis(10)).await;
        }
        if read_half.read_exact(&mut header).await.is_err() {
            return;
        }
        let mut frame = vec![0u8; usize::from(u16::from_be_bytes(header))];
        if read_half.read_exact(&mut frame).await.is_err() {
            return;
        }
        if out.send(Bytes::from(frame)).await.is_err() {
            return;
        }
    }
}

struct Driver {
    rtc: Rtc,
    write_half: OwnedWriteHalf,
    frames: mpsc::Receiver<Bytes>,
    local: SocketAddr,
    server: SocketAddr,
    mid: Mid,
    state: Arc<Mutex<State>>,
    clock: SharedClock,
    writes: Writes,
}

impl Driver {
    async fn flush(&mut self, pending: &mut Vec<Vec<u8>>) {
        match self.writes {
            Writes::Plain => {
                for frame in pending.drain(..) {
                    let _ = self.write_half.write_all(&frame).await;
                }
            }
            Writes::Split => {
                for frame in pending.drain(..) {
                    for piece in frame.chunks(5) {
                        let _ = self.write_half.write_all(piece).await;
                        let _ = self.write_half.flush().await;
                    }
                }
            }
            Writes::Coalesced => {
                let all: Vec<u8> = pending.drain(..).flatten().collect();
                if !all.is_empty() {
                    let _ = self.write_half.write_all(&all).await;
                }
            }
        }
    }

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
        let mut pcm = vec![0i16; MAX_PACKET_SAMPLES];
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        loop {
            let mut pending: Vec<Vec<u8>> = Vec::new();
            let timeout = loop {
                match self.rtc.poll_output() {
                    Ok(Output::Timeout(at)) => break at,
                    Ok(Output::Transmit(t)) => {
                        assert_eq!(t.proto, Protocol::Tcp, "only the TCP candidate exists");
                        let mut framed = Vec::with_capacity(t.contents.len() + 2);
                        framed.extend_from_slice(
                            &u16::try_from(t.contents.len()).unwrap().to_be_bytes(),
                        );
                        framed.extend_from_slice(&t.contents);
                        pending.push(framed);
                    }
                    Ok(Output::Event(Event::Connected)) => {
                        self.state.lock().unwrap().connected = true;
                    }
                    Ok(Output::Event(Event::Closed)) => self.state.lock().unwrap().closed = true,
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
            self.flush(&mut pending).await;
            if !self.rtc.is_alive() {
                return;
            }
            let frame_due = origin + Duration::from_millis(20 * next_frame);
            let wake = tokio::time::Instant::from_std(timeout).min(frame_due);
            let now = self.clock.now();
            if wake > now {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    frame = self.frames.recv() => match frame {
                        Some(data) => {
                            let now = self.clock.now();
                            if let Ok(receive) =
                                Receive::new(Protocol::Tcp, self.server, self.local, &data)
                            {
                                let _ = self.rtc.handle_input(Input::Receive(now.into_std(), receive));
                            }
                        }
                        None => {
                            self.state.lock().unwrap().closed = true;
                            return;
                        }
                    },
                    () = self.clock.sleep_until(wake) => {}
                }
            }
            let now = self.clock.now();
            let _ = self.rtc.handle_input(Input::Timeout(now.into_std()));
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
                    if let Some(writer) = self.rtc.writer(self.mid) {
                        let _ = writer.write(
                            pt,
                            self.clock.now().into_std(),
                            MediaTime::new(next_frame * 960, Frequency::FORTY_EIGHT_KHZ),
                            packet[..len].to_vec(),
                        );
                    }
                }
                next_frame += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The server rig

fn tcp_config() -> WebRtcConfig {
    WebRtcConfig {
        http_addr: ([127, 0, 0, 1], 0).into(),
        udp_addr: ([127, 0, 0, 1], 0).into(),
        tcp_addr: Some(([127, 0, 0, 1], 0).into()),
        max_calls: 8,
        connect_timeout: Duration::from_secs(5),
        disconnect_grace: Duration::from_millis(500),
        send_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        ..WebRtcConfig::default()
    }
}

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: WebRtcServer,
}

impl Rig {
    async fn start(config: WebRtcConfig) -> Self {
        let clock = SystemClock::shared();
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

    async fn call(&mut self, writes: Writes) -> (TcpBrowser, TransportSession) {
        self.call_with(writes, false).await
    }

    async fn call_with(
        &mut self,
        writes: Writes,
        small_buffer: bool,
    ) -> (TcpBrowser, TransportSession) {
        let browser = TcpBrowser::connect_with(
            self.server.http_addr(),
            self.server.tcp_addr().unwrap(),
            writes,
            small_buffer,
        )
        .await;
        let mut session = self.accept().await;
        match next_event(&mut session).await {
            TransportEvent::Started(_) => {}
            other => panic!("expected Started, got {other:?}"),
        }
        browser
            .until("the browser to connect", |s| s.connected)
            .await;
        (browser, session)
    }
}

async fn next_event(session: &mut TransportSession) -> TransportEvent {
    tokio::time::timeout(WAIT, session.recv())
        .await
        .expect("no event")
        .expect("session ended")
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

/// Talks from the browser and from the bot, and checks both directions carried a clean tone.
async fn exchange_audio(rig: &Rig, browser: &TcpBrowser, session: &mut TransportSession) {
    browser.with(|s| s.talking = true);
    let mut caller: Vec<i16> = Vec::new();
    let deadline = rig.clock.now() + WAIT;
    while caller.len() < 16_000 * 6 / 10 {
        assert!(
            rig.clock.now() < deadline,
            "only {} caller samples",
            caller.len()
        );
        if let TransportEvent::Audio(frame) = next_event(session).await {
            caller.extend(
                frame
                    .data()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        }
    }
    browser.with(|s| s.talking = false);
    let hz = pitch_hz(&caller[caller.len() / 4..], 16_000);
    assert!((950.0..1_050.0).contains(&hz), "caller pitch {hz} Hz");

    for seq in 0..50 {
        session
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::ZERO,
                utterance: UtteranceId::new(1),
                frame: tone_frame(&rig.clock, seq),
            })
            .await
            .unwrap();
    }
    browser
        .until("bot audio", |s| s.received.len() >= 48_000 * 4 / 10)
        .await;
    let received = browser.with(|s| std::mem::take(&mut s.received));
    let steady = &received[received.len() / 4..received.len() * 3 / 4];
    let hz = pitch_hz(steady, 48_000);
    assert!((470.0..530.0).contains(&hz), "bot pitch {hz} Hz");
}

// ---------------------------------------------------------------------------------------------
// Tests

async fn run_call_over_tcp(writes: Writes) {
    let mut rig = Rig::start(tcp_config()).await;
    let (browser, mut session) = rig.call(writes).await;
    exchange_audio(&rig, &browser, &mut session).await;
    let stats = rig.server.stats();
    assert_eq!(
        stats.datagrams_in, 0,
        "no UDP packet was involved: {stats:?}"
    );
    assert!(
        stats.tcp_frames_in > 20 && stats.tcp_frames_out > 20,
        "{stats:?}"
    );
    assert_eq!(stats.tcp_connections, 1);
    assert_eq!(stats.tcp_connections_active, 1);
    assert_eq!(stats.tcp_protocol_errors, 0);
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_str0m_client_over_tcp_completes_ice_and_dtls_and_exchanges_audio() {
    run_call_over_tcp(Writes::Plain).await;
}

#[tokio::test]
async fn frames_cut_into_small_pieces_are_reassembled() {
    run_call_over_tcp(Writes::Split).await;
}

#[tokio::test]
async fn several_frames_in_one_write_are_all_delivered() {
    run_call_over_tcp(Writes::Coalesced).await;
}

#[tokio::test]
async fn the_answer_holds_a_udp_and_a_passive_tcp_candidate_with_udp_preferred() {
    let rig = Rig::start(tcp_config()).await;
    let browser = TcpBrowser::connect(
        rig.server.http_addr(),
        rig.server.tcp_addr().unwrap(),
        Writes::Plain,
    )
    .await;
    let candidates: Vec<&str> = browser
        .answer
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .collect();
    assert_eq!(candidates.len(), 2, "{candidates:?}");
    let udp = candidates
        .iter()
        .find(|l| l.contains(" udp "))
        .expect("a UDP candidate");
    let tcp = candidates
        .iter()
        .find(|l| l.contains(" tcp "))
        .expect("a TCP candidate");
    assert!(tcp.contains("tcptype passive"), "{tcp}");
    assert!(
        tcp.contains(&format!(" {} typ host", rig.server.tcp_addr().unwrap()).replace(':', " ")),
        "{tcp}"
    );
    assert!(
        udp.contains(&format!(
            " 127.0.0.1 {} typ host",
            rig.server.udp_addr().port()
        )),
        "{udp}"
    );
    let priority = |line: &str| -> u64 { line.split(' ').nth(3).unwrap().parse().unwrap() };
    assert!(
        priority(udp) > priority(tcp),
        "UDP must be preferred: {udp} / {tcp}"
    );
    browser.stop().await;
}

#[tokio::test]
async fn the_advertised_tcp_port_can_differ_from_the_listener_port() {
    let rig = Rig::start(WebRtcConfig {
        tcp_candidate_port: Some(15140),
        ..tcp_config()
    })
    .await;
    let browser = TcpBrowser::connect(
        rig.server.http_addr(),
        rig.server.tcp_addr().unwrap(),
        Writes::Plain,
    )
    .await;
    assert!(
        browser.answer.lines().any(|l| l.starts_with("a=candidate:")
            && l.contains(" tcp ")
            && l.contains(" 127.0.0.1 15140 typ host")),
        "{}",
        browser.answer
    );
    browser.stop().await;
}

#[tokio::test]
async fn without_a_tcp_address_the_answer_has_no_tcp_candidate() {
    let rig = Rig::start(WebRtcConfig {
        tcp_addr: None,
        ..tcp_config()
    })
    .await;
    assert!(rig.server.tcp_addr().is_none());
    // A plain UDP-only offer.
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto())
        .clear_codecs()
        .enable_opus(true, false)
        .build(SystemClock::shared().now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    let (status, answer) = post_offer(rig.server.http_addr(), &offer.to_sdp_string()).await;
    assert_eq!(status, 201);
    assert!(!answer.contains(" tcp "), "{answer}");
    assert!(answer.contains(" udp "));
}

/// A connection reading until the server closes it; returns whether it did within `within`.
async fn closed_by_server(stream: &mut TcpStream, within: Duration) -> bool {
    let mut sink = [0u8; 256];
    let clock = SystemClock::shared();
    let deadline = clock.now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(clock.now());
        match tokio::time::timeout(remaining, stream.read(&mut sink)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => {}
            Err(_) => return false,
        }
    }
}

/// A STUN binding request carrying `username`.
fn binding_request(username: &str) -> Vec<u8> {
    let mut attrs = Vec::new();
    attrs.extend_from_slice(&0x0006u16.to_be_bytes());
    attrs.extend_from_slice(&u16::try_from(username.len()).unwrap().to_be_bytes());
    attrs.extend_from_slice(username.as_bytes());
    while attrs.len() % 4 != 0 {
        attrs.push(0);
    }
    let mut message = Vec::new();
    message.extend_from_slice(&0x0001u16.to_be_bytes());
    message.extend_from_slice(&u16::try_from(attrs.len()).unwrap().to_be_bytes());
    message.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    message.extend_from_slice(&[7; 12]);
    message.extend_from_slice(&attrs);
    message
}

fn framed(packet: &[u8]) -> Vec<u8> {
    let mut out = u16::try_from(packet.len()).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(packet);
    out
}

#[tokio::test]
async fn an_oversize_frame_closes_the_connection() {
    let rig = Rig::start(WebRtcConfig {
        tcp_max_frame: 512,
        ..tcp_config()
    })
    .await;
    let mut stream = TcpStream::connect(rig.server.tcp_addr().unwrap())
        .await
        .unwrap();
    // Announces 600 bytes (limit 512): closed before the body is even sent.
    stream.write_all(&600u16.to_be_bytes()).await.unwrap();
    assert!(closed_by_server(&mut stream, WAIT).await, "not closed");
    assert_eq!(rig.server.stats().tcp_protocol_errors, 1);

    // A frame at the limit is accepted (and, being unroutable junk, dropped without closing).
    let mut stream = TcpStream::connect(rig.server.tcp_addr().unwrap())
        .await
        .unwrap();
    stream.write_all(&framed(&vec![0u8; 512])).await.unwrap();
    assert!(!closed_by_server(&mut stream, Duration::from_millis(300)).await);
    // The largest frame RFC 4571 can express is refused too.
    stream.write_all(&65_535u16.to_be_bytes()).await.unwrap();
    assert!(closed_by_server(&mut stream, WAIT).await);
    assert_eq!(rig.server.stats().tcp_protocol_errors, 2);
}

#[tokio::test]
async fn a_connection_naming_no_live_call_is_closed_after_the_idle_timeout_junk_or_not() {
    let rig = Rig::start(WebRtcConfig {
        tcp_idle_timeout: Duration::from_millis(400),
        ..tcp_config()
    })
    .await;
    let clock = SystemClock::shared();
    let started = clock.now();
    let mut stream = TcpStream::connect(rig.server.tcp_addr().unwrap())
        .await
        .unwrap();
    // Junk and a STUN request with an unknown ufrag, repeated: neither extends the deadline.
    let stun = framed(&binding_request("nobody:peer"));
    let mut sink = [0u8; 16];
    let closed_after = loop {
        let _ = stream.write_all(&stun).await;
        let _ = stream.write_all(&framed(b"junk")).await;
        match tokio::time::timeout(Duration::from_millis(50), stream.read(&mut sink)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break clock.now().saturating_duration_since(started),
            _ => {}
        }
        assert!(
            clock.now().saturating_duration_since(started) < WAIT,
            "never closed"
        );
    };
    assert!(
        closed_after >= Duration::from_millis(350) && closed_after < WAIT,
        "{closed_after:?}"
    );
    let stats = rig.server.stats();
    assert_eq!(stats.tcp_unrouted_closed, 1, "{stats:?}");
    assert!(stats.datagrams_unrouted >= 2, "{stats:?}");
}

#[tokio::test]
async fn idle_connections_are_bounded_by_the_connection_limit() {
    let rig = Rig::start(WebRtcConfig {
        max_tcp_connections: 4,
        tcp_idle_timeout: Duration::from_secs(30),
        ..tcp_config()
    })
    .await;
    let addr = rig.server.tcp_addr().unwrap();
    let mut streams = Vec::new();
    for _ in 0..12 {
        streams.push(TcpStream::connect(addr).await.unwrap());
        // The accept loop decides in order; give it the turn.
        SystemClock::shared().sleep(Duration::from_millis(20)).await;
    }
    let clock = SystemClock::shared();
    let deadline = clock.now() + WAIT;
    loop {
        let stats = rig.server.stats();
        if stats.tcp_connections_refused == 8 {
            assert_eq!(stats.tcp_connections, 4);
            assert_eq!(stats.tcp_connections_active, 4);
            break;
        }
        assert!(clock.now() < deadline, "{stats:?}");
        SystemClock::shared().sleep(Duration::from_millis(20)).await;
    }
    // Exactly the first four stay open; the rest were closed by the server.
    let mut open = 0;
    for stream in &mut streams {
        if !closed_by_server(stream, Duration::from_millis(100)).await {
            open += 1;
        }
    }
    assert_eq!(open, 4);
    // Closing an idle one frees its slot.
    drop(streams);
    let deadline = clock.now() + WAIT;
    while rig.server.stats().tcp_connections_active != 0 {
        assert!(clock.now() < deadline, "slots not freed");
        SystemClock::shared().sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn ending_the_call_closes_its_tcp_connection() {
    let mut rig = Rig::start(tcp_config()).await;
    let (browser, session) = rig.call(Writes::Plain).await;
    assert_eq!(rig.server.stats().tcp_connections_active, 1);
    // The engine hangs up: the call ends, and the connection with it.
    session.close(WAIT).await.unwrap();
    let deadline = rig.clock.now() + WAIT;
    while rig.server.stats().tcp_connections_active != 0 {
        assert!(rig.clock.now() < deadline, "connection still open");
        SystemClock::shared().sleep(Duration::from_millis(20)).await;
    }
    browser
        .until("the browser to see the close", |s| s.closed)
        .await;
    browser.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Review fixes (2026-10-13)

/// The server's ICE ufrag, from the answer SDP.
fn server_ufrag(answer: &str) -> String {
    answer
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap()
        .trim()
        .to_owned()
}

/// A STUN binding request with the right ufrag and a `MESSAGE-INTEGRITY` made of nothing: what
/// someone who read the cleartext SDP can send.
fn forged_binding_request(ufrag: &str) -> Vec<u8> {
    let username = format!("{ufrag}:attacker");
    let mut attrs = Vec::new();
    attrs.extend_from_slice(&0x0006u16.to_be_bytes());
    attrs.extend_from_slice(&u16::try_from(username.len()).unwrap().to_be_bytes());
    attrs.extend_from_slice(username.as_bytes());
    while attrs.len() % 4 != 0 {
        attrs.push(0);
    }
    attrs.extend_from_slice(&0x0008u16.to_be_bytes());
    attrs.extend_from_slice(&20u16.to_be_bytes());
    attrs.extend_from_slice(&[0xAA; 20]);
    let mut message = Vec::new();
    message.extend_from_slice(&0x0001u16.to_be_bytes());
    message.extend_from_slice(&u16::try_from(attrs.len()).unwrap().to_be_bytes());
    message.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    message.extend_from_slice(&[9; 12]);
    message.extend_from_slice(&attrs);
    message
}

/// 20 ms of white noise (xorshift), which Opus cannot compress.
fn noise_frame(clock: &SharedClock, seq: u64) -> AudioFrame {
    let rate = SampleRate::Hz24000;
    let count = (rate.hz() / 50) as usize;
    let mut x = seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let pcm: Vec<u8> = (0..count)
        .flat_map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (((x >> 16) as i16) / 2).to_le_bytes()
        })
        .collect();
    AudioFrame::new(Bytes::from(pcm), rate, clock.now(), seq, TurnEpoch::ZERO).unwrap()
}

#[tokio::test]
async fn connections_that_know_only_the_ufrag_cannot_push_the_real_browser_out() {
    let mut rig = Rig::start(tcp_config()).await;
    let (browser, mut session) = rig.call(Writes::Plain).await;
    let forged = framed(&forged_binding_request(&server_ufrag(&browser.answer)));

    // More connections than the call remembers of either class, each hammering the server with
    // requests that name the call but fail the integrity check, for the rest of the test.
    let addr = rig.server.tcp_addr().unwrap();
    let stop = CancellationToken::new();
    let mut attackers = Vec::new();
    for _ in 0..10 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let (forged, stop) = (forged.clone(), stop.clone());
        attackers.push(tokio::spawn(async move {
            while !stop.is_cancelled() {
                if stream.write_all(&forged).await.is_err() {
                    return;
                }
                SystemClock::shared().sleep(Duration::from_millis(4)).await;
            }
        }));
    }
    // Let the flood get going. `str0m` (0.24) already refuses a binding request whose
    // MESSAGE-INTEGRITY fails (`Rtc::accepts` verifies it), so the forgeries are counted as
    // rejected; the peer table adds a second wall (see `tcp::TcpPeers`) that its unit tests cover.
    let frames_before = rig.server.stats().tcp_frames_in;
    rig.clock.sleep(Duration::from_millis(300)).await;
    let flooded = rig.server.stats();
    assert!(flooded.tcp_frames_in > frames_before + 100, "{flooded:?}");
    assert!(flooded.datagrams_rejected > 100, "{flooded:?}");

    // The real call keeps working in both directions while the flood goes on.
    exchange_audio(&rig, &browser, &mut session).await;
    let stats = rig.server.stats();
    assert_eq!(stats.datagrams_in, 0, "{stats:?}");
    stop.cancel();
    for attacker in attackers {
        let _ = attacker.await;
    }
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn one_source_ip_is_limited_in_connections_and_zero_turns_the_limit_off() {
    let rig = Rig::start(WebRtcConfig {
        max_tcp_connections_per_ip: 3,
        tcp_idle_timeout: Duration::from_secs(30),
        ..tcp_config()
    })
    .await;
    let addr = rig.server.tcp_addr().unwrap();
    let mut streams = Vec::new();
    for _ in 0..8 {
        streams.push(TcpStream::connect(addr).await.unwrap());
        SystemClock::shared().sleep(Duration::from_millis(20)).await;
    }
    let clock = SystemClock::shared();
    let deadline = clock.now() + WAIT;
    while rig.server.stats().tcp_connections_refused != 5 {
        assert!(clock.now() < deadline, "{:?}", rig.server.stats());
        clock.sleep(Duration::from_millis(20)).await;
    }
    let stats = rig.server.stats();
    assert_eq!(
        (stats.tcp_connections, stats.tcp_connections_active),
        (3, 3)
    );
    // Closing one gives its slot back to the same address.
    streams.remove(0);
    let deadline = clock.now() + WAIT;
    while rig.server.stats().tcp_connections_active != 2 {
        assert!(clock.now() < deadline, "slot not freed");
        clock.sleep(Duration::from_millis(20)).await;
    }
    let again = TcpStream::connect(addr).await.unwrap();
    clock.sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.server.stats().tcp_connections_active, 3);
    drop((streams, again));

    // 0: no per-address limit (every peer is a TCP proxy's address).
    let open = Rig::start(WebRtcConfig {
        max_tcp_connections_per_ip: 0,
        tcp_idle_timeout: Duration::from_secs(30),
        ..tcp_config()
    })
    .await;
    let mut streams = Vec::new();
    for _ in 0..40 {
        streams.push(
            TcpStream::connect(open.server.tcp_addr().unwrap())
                .await
                .unwrap(),
        );
    }
    let deadline = clock.now() + WAIT;
    while open.server.stats().tcp_connections_active != 40 {
        assert!(clock.now() < deadline, "{:?}", open.server.stats());
        clock.sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(open.server.stats().tcp_connections_refused, 0);
}

#[tokio::test]
async fn a_peer_that_stops_reading_is_cut_off_at_the_write_timeout_and_the_call_task_keeps_running()
{
    // Incompressible audio at the highest Opus bit rate: about 64 KB/s towards the browser, so the
    // kernel buffers fill within seconds even though the bot's audio is paced in real time.
    let mut rig = Rig::start(WebRtcConfig {
        write_timeout: Duration::from_millis(500),
        opus: OpusSettings {
            bitrate_bps: 510_000,
            ..OpusSettings::default()
        },
        ..tcp_config()
    })
    .await;
    let (browser, mut session) = rig.call_with(Writes::Plain, true).await;
    assert_eq!(rig.server.stats().tcp_connections_active, 1);

    // The browser keeps talking (the call task keeps receiving) but stops reading its socket.
    browser.with(|s| {
        s.paused = true;
        s.talking = true;
    });
    // Flood downstream audio until the socket path is full and the writer times out. The bot's
    // side never blocks: every send completes at once (frames queue drop-oldest per connection).
    let deadline = rig.clock.now() + Duration::from_secs(40);
    let mut seq = 0u64;
    let mut caller_frames = 0usize;
    while rig.server.stats().tcp_connections_active != 0 {
        assert!(
            rig.clock.now() < deadline,
            "the stalled connection was never cut off: {:?}",
            rig.server.stats()
        );
        for _ in 0..50 {
            tokio::time::timeout(
                Duration::from_secs(1),
                session.send(TransportCommand::Audio {
                    epoch: TurnEpoch::ZERO,
                    utterance: UtteranceId::new(1),
                    frame: noise_frame(&rig.clock, seq),
                }),
            )
            .await
            .expect("sending bot audio must never wait on the socket")
            .unwrap();
            seq += 1;
        }
        // The call task is still alive and reading: caller audio keeps arriving meanwhile.
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(5), session.recv()).await
        {
            if matches!(event, TransportEvent::Audio(_)) {
                caller_frames += 1;
            }
        }
    }
    let stats = rig.server.stats();
    assert!(seq > 100, "flooded {seq} frames");
    assert!(
        caller_frames > 0,
        "the call task stopped delivering the caller's audio: {stats:?}"
    );
    // Frames the full queue evicted, and packets for the gone connection, are counted.
    assert!(
        stats.tcp_frames_out_dropped + stats.datagrams_out_dropped > 0,
        "{stats:?}"
    );
    assert_eq!(stats.call_panics, 0, "{stats:?}");
    assert_eq!(stats.tcp_protocol_errors, 0, "{stats:?}");
    browser.stop().await;
    let _ = session.close(WAIT).await;
}
