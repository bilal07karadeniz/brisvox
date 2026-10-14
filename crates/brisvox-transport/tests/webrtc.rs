//! WebRTC transport tests: real sockets on 127.0.0.1, the real clock, short deadlines.
//!
//! The browser is played by a second `str0m` peer in the test (full ICE agent, `sendrecv` Opus,
//! libopus on its side), driven by its own task over its own UDP socket. The engine is played
//! by a `TransportSession` on every accepted call. Signaling goes through a raw HTTP/1.1 client.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)] // test helpers (see clippy.toml)

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_inference::{InferencePool, PoolConfig};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::auth::{Audience, SessionSecrets, TokenSpec, mint_token};
use brisvox_transport::webrtc::opus::{
    FRAME_SAMPLES, LibOpusFactory, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusFactory, OpusSettings,
};
use brisvox_transport::webrtc::{WebRtcConfig, WebRtcOptions, WebRtcServer};
use brisvox_transport::{
    CallInfo, CallRates, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId,
};
use bytes::Bytes;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------------------------
// A raw HTTP/1.1 client

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

async fn http(
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
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes()).await.unwrap();
    // A server may answer (and close) before it has read the whole body.
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

async fn post_offer(addr: SocketAddr, sdp: &str) -> HttpResponse {
    http(
        addr,
        "POST",
        "/whip",
        &[("Content-Type", "application/sdp")],
        sdp.as_bytes(),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The browser: a str0m peer with a full ICE agent

#[derive(Default)]
struct BrowserState {
    connected: bool,
    disconnected: bool,
    closed: bool,
    /// The test asks the browser to close its connection (DTLS close_notify).
    close_requested: bool,
    talking: bool,
    /// Decoded 48 kHz PCM of everything the server sent.
    received: Vec<i16>,
    packets: usize,
    /// When the last audio packet from the server arrived.
    last_packet_at: Option<tokio::time::Instant>,
    /// Audio packets the browser wrote (before any loss it simulates).
    sent_packets: usize,
    /// RTP datagrams the simulated network dropped (`BrowserOptions::drop_every`).
    dropped_on_wire: usize,
    /// RTP time (48 kHz ticks) and talkspurt (marker) bit of every packet received, in order.
    rtp_log: Vec<(u64, bool)>,
    /// When the first packet since `first_packet_at` was last cleared arrived.
    first_packet_at: Option<tokio::time::Instant>,
}

struct BrowserOptions {
    direction: Direction,
    /// Lines to insert into the offer before it is sent (for example an mDNS candidate).
    extra_offer_lines: Vec<String>,
    /// Does not start the driver: ICE can never complete.
    silent: bool,
    /// Which candidates the offer carries.
    candidates: OfferCandidates,
    /// The network drops every n-th RTP media datagram the browser sends (not STUN, DTLS, RTCP).
    drop_every: Option<usize>,
    /// Audio packets written per 20 ms tick while talking (1 is a real browser; more is a flood).
    packets_per_frame: usize,
    /// Silence suppression: after every `n` packets the RTP timestamp jumps by `ticks` while the
    /// sequence numbers stay contiguous.
    ts_jump: Option<(usize, u64)>,
    /// `Authorization: Bearer` value of the offer.
    bearer: Option<String>,
}

/// What a browser puts into its offer. Real browsers hide host addresses behind mDNS names the
/// server cannot resolve (and the demo page has no ICE servers), so the server usually sees no
/// usable remote candidate and must learn the address from the browser's first STUN request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OfferCandidates {
    /// The browser's real `127.0.0.1` host candidate.
    Real,
    /// Only a `<uuid>.local` name (Chrome's default).
    MdnsOnly,
    /// No candidate line at all.
    Nothing,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            direction: Direction::SendRecv,
            extra_offer_lines: Vec::new(),
            silent: false,
            candidates: OfferCandidates::Real,
            drop_every: None,
            packets_per_frame: 1,
            ts_jump: None,
            bearer: None,
        }
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

fn crypto() -> Arc<str0m::crypto::CryptoProvider> {
    Arc::new(str0m::crypto::from_feature_flags())
}

impl Browser {
    /// Offers, posts, applies the answer and starts the driver task.
    async fn connect(http_addr: SocketAddr, options: BrowserOptions) -> Self {
        let clock = SystemClock::shared();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = socket.local_addr().unwrap();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(crypto())
            .clear_codecs()
            .enable_opus(true, false)
            .build(clock.now().into_std());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Audio, options.direction, None, None, None);
        let (offer, pending) = change.apply().unwrap();
        let mut sdp = offer.to_sdp_string();
        if options.candidates != OfferCandidates::Real {
            sdp = sdp
                .split_inclusive('\n')
                .filter(|line| {
                    !line.starts_with("a=candidate:") && !line.starts_with("a=end-of-candidates")
                })
                .collect();
        }
        if options.candidates == OfferCandidates::MdnsOnly {
            sdp = sdp.replacen(
                "a=ice-ufrag",
                "a=candidate:9 1 udp 2113937151 0a1b2c3d-4e5f-6789-abcd-ef0123456789.local 54321 typ host generation 0\r\na=ice-ufrag",
                1,
            );
        }
        for line in &options.extra_offer_lines {
            sdp = sdp.replacen("a=ice-ufrag", &format!("{line}\r\na=ice-ufrag"), 1);
        }
        let response = match &options.bearer {
            None => post_offer(http_addr, &sdp).await,
            Some(token) => {
                let auth = format!("Bearer {token}");
                http(
                    http_addr,
                    "POST",
                    "/whip",
                    &[
                        ("Content-Type", "application/sdp"),
                        ("Authorization", &auth),
                    ],
                    sdp.as_bytes(),
                )
                .await
            }
        };
        assert_eq!(response.status, 201, "{}", response.body);
        let session_url = response.headers["location"].clone();
        let answer = response.body;
        rtc.sdp_api()
            .accept_answer(
                pending_ref(pending),
                SdpAnswer::from_sdp_string(&answer).unwrap(),
            )
            .unwrap();
        let state = Arc::new(Mutex::new(BrowserState::default()));
        let stop = CancellationToken::new();
        let task = if options.silent {
            None
        } else {
            let driver = BrowserDriver {
                rtc,
                socket,
                local,
                mid,
                state: Arc::clone(&state),
                clock,
                drop_every: options.drop_every,
                packets_per_frame: options.packets_per_frame.max(1),
                ts_jump: options.ts_jump,
            };
            Some(tokio::spawn(driver.run(stop.clone())))
        };
        Self {
            state,
            stop,
            task,
            session_url,
            answer,
            http_addr,
        }
    }

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

    fn talk(&self, on: bool) {
        self.with(|s| s.talking = on);
    }

    fn received_samples(&self) -> usize {
        self.with(|s| s.received.len())
    }

    fn take_received(&self) -> Vec<i16> {
        self.with(|s| std::mem::take(&mut s.received))
    }

    async fn delete(&self) -> HttpResponse {
        http(self.http_addr, "DELETE", &self.session_url, &[], &[]).await
    }

    async fn stop(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// `SdpPendingOffer` is moved into `accept_answer`; this keeps the call site readable.
fn pending_ref(pending: SdpPendingOffer) -> SdpPendingOffer {
    pending
}

struct BrowserDriver {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    mid: Mid,
    state: Arc<Mutex<BrowserState>>,
    clock: SharedClock,
    drop_every: Option<usize>,
    packets_per_frame: usize,
    ts_jump: Option<(usize, u64)>,
}

/// Is this datagram RTP media (not STUN, DTLS or RTCP)? RFC 7983: the first byte tells the
/// protocol, the payload type tells RTP from RTCP (RTCP uses 192..=223).
fn is_rtp(datagram: &[u8]) -> bool {
    matches!(datagram, [first, second, ..] if (128..=191).contains(first) && !(72..=76).contains(&(*second & 0x7f)))
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
        let mut rtp_datagrams = 0usize;
        let mut written_packets = 0usize;
        let mut ts_extra = 0u64;
        let mut media_ticks = 0u64;
        loop {
            // Drain the rtc.
            let timeout = loop {
                match self.rtc.poll_output() {
                    Ok(Output::Timeout(at)) => break at,
                    Ok(Output::Transmit(t)) => {
                        if let Some(every) = self.drop_every
                            && is_rtp(&t.contents)
                        {
                            rtp_datagrams += 1;
                            if rtp_datagrams.is_multiple_of(every) {
                                self.state.lock().unwrap().dropped_on_wire += 1;
                                continue;
                            }
                        }
                        let _ = self.socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(Event::Connected)) => {
                        self.state.lock().unwrap().connected = true;
                    }
                    Ok(Output::Event(Event::IceConnectionStateChange(
                        IceConnectionState::Disconnected,
                    ))) => self.state.lock().unwrap().disconnected = true,
                    Ok(Output::Event(Event::Closed)) => {
                        self.state.lock().unwrap().closed = true;
                    }
                    Ok(Output::Event(Event::MediaData(data))) => {
                        if let Ok(n) = decoder.decode(&data.data, &mut pcm) {
                            let mut state = self.state.lock().unwrap();
                            state.received.extend_from_slice(&pcm[..n]);
                            state.packets += 1;
                            state.last_packet_at = Some(self.clock.now());
                            state.first_packet_at.get_or_insert(self.clock.now());
                            let ticks = if data.time.denom() == 48_000 {
                                data.time.numer()
                            } else {
                                data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer()
                            };
                            state.rtp_log.push((ticks, data.audio_start_of_talk_spurt));
                        }
                    }
                    Ok(Output::Event(_)) => {}
                    Err(_) => return,
                }
            };
            if !self.rtc.is_alive() {
                return;
            }
            // Next audio frame is due at origin + 20 ms * next_frame.
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
            // A receive error is Windows reporting the ICMP "port unreachable" of an earlier send.
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
            if std::mem::take(&mut self.state.lock().unwrap().close_requested) {
                let _ = self.rtc.close();
            }
            // Send a 1 kHz tone frame every 20 ms while talking and connected.
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
                    for _ in 0..self.packets_per_frame {
                        // The media clock runs one frame per packet (a flood runs ahead).
                        let ticks = if self.packets_per_frame > 1 {
                            media_ticks += 960;
                            media_ticks
                        } else {
                            next_frame * 960
                        };
                        let written = self.rtc.writer(self.mid).map(|writer| {
                            writer.write(
                                pt,
                                self.clock.now().into_std(),
                                MediaTime::new(ticks + ts_extra, Frequency::FORTY_EIGHT_KHZ),
                                packet[..len].to_vec(),
                            )
                        });
                        if matches!(written, Some(Ok(()))) {
                            self.state.lock().unwrap().sent_packets += 1;
                            written_packets += 1;
                            if let Some((every, ticks)) = self.ts_jump
                                && written_packets.is_multiple_of(every)
                            {
                                ts_extra += ticks;
                            }
                        }
                    }
                }
                next_frame += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The server rig and the engine side

fn fast_config() -> WebRtcConfig {
    WebRtcConfig {
        http_addr: ([127, 0, 0, 1], 0).into(),
        udp_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 8,
        // The offer rate has its own tests; the others make many offers from one address.
        offers_per_minute_per_ip: None,
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
        Self::start_with(config, WebRtcOptions::default()).await
    }

    async fn start_with(config: WebRtcConfig, options: WebRtcOptions) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = WebRtcServer::bind_with(config, options, clock.clone(), &parent)
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

    /// Takes the next call and opens a session on it.
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

    /// A browser, connected and with the engine's session open and started.
    async fn call(&mut self) -> (Browser, TransportSession, CallInfo) {
        self.call_with(BrowserOptions::default()).await
    }

    /// [`call`](Self::call) with a browser that behaves as `options` says.
    async fn call_with(
        &mut self,
        options: BrowserOptions,
    ) -> (Browser, TransportSession, CallInfo) {
        let browser = Browser::connect(self.http_addr(), options).await;
        let mut session = self.accept().await;
        let info = match next_event(&mut session).await {
            TransportEvent::Started(info) => info,
            other => panic!("expected Started, got {other:?}"),
        };
        browser.wait_connected().await;
        (browser, session, info)
    }
}

async fn next_event(session: &mut TransportSession) -> TransportEvent {
    tokio::time::timeout(WAIT, session.recv())
        .await
        .expect("no event")
        .expect("session ended")
}

/// Reads events until `pick` returns a value, collecting caller audio on the way.
async fn until_event<T>(
    session: &mut TransportSession,
    audio: &mut Vec<i16>,
    mut pick: impl FnMut(&TransportEvent) -> Option<T>,
) -> T {
    let clock = SystemClock::shared();
    let deadline = clock.now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(clock.now());
        let event = tokio::time::timeout(remaining, session.recv())
            .await
            .expect("timed out waiting for an event")
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
        if let Some(value) = pick(&event) {
            return value;
        }
    }
}

async fn stopped(session: &mut TransportSession) -> StopReason {
    let mut audio = Vec::new();
    until_event(session, &mut audio, |event| match event {
        TransportEvent::Stopped { reason } => Some(reason.clone()),
        _ => None,
    })
    .await
}

/// Zero-crossing pitch estimate of a clean tone.
fn pitch_hz(pcm: &[i16], rate: u32) -> f32 {
    let crossings = pcm.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    crossings as f32 * rate as f32 / pcm.len() as f32
}

fn tone_frame(
    clock: &SharedClock,
    hz: f32,
    seq: u64,
    epoch: TurnEpoch,
    rate: SampleRate,
) -> AudioFrame {
    let count = (rate.hz() / 50) as usize;
    let start = seq as usize * count;
    let pcm: Vec<u8> = (start..start + count)
        .flat_map(|n| {
            let phase = 2.0 * std::f32::consts::PI * hz * n as f32 / rate.hz() as f32;
            ((phase.sin() * 8_000.0) as i16).to_le_bytes()
        })
        .collect();
    AudioFrame::new(Bytes::from(pcm), rate, clock.now(), seq, epoch).unwrap()
}

async fn send_tone(
    session: &TransportSession,
    clock: &SharedClock,
    frames: u64,
    epoch: TurnEpoch,
    first: u64,
) {
    for seq in first..first + frames {
        session
            .send(TransportCommand::Audio {
                epoch,
                utterance: UtteranceId::new(1),
                frame: tone_frame(clock, 500.0, seq, epoch, SampleRate::Hz24000),
            })
            .await
            .unwrap();
    }
}

async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

/// Collects the caller audio the engine receives for `duration`.
async fn drain_caller_audio(session: &mut TransportSession, duration: Duration) -> Vec<i16> {
    let clock = SystemClock::shared();
    let deadline = clock.now() + duration;
    let mut audio = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(clock.now());
        if remaining.is_zero() {
            return audio;
        }
        match tokio::time::timeout(remaining, session.recv()).await {
            Ok(Some(TransportEvent::Audio(frame))) => audio.extend(
                frame
                    .data()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            ),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return audio,
        }
    }
}

/// Waits until the browser has logged `count` RTP packets in total.
async fn wait_for_rtp(browser: &Browser, count: usize) {
    browser
        .until("RTP packets from the server", |s| s.rtp_log.len() >= count)
        .await;
}

// ---------------------------------------------------------------------------------------------
// Signaling

#[tokio::test]
async fn post_answers_201_with_an_sdp_answer_and_a_session_url_and_delete_ends_it() {
    let rig = Rig::start(fast_config()).await;
    let browser = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            silent: true,
            ..BrowserOptions::default()
        },
    )
    .await;
    assert!(
        browser.session_url.starts_with("/whip/"),
        "{}",
        browser.session_url
    );
    let id = browser.session_url.trim_start_matches("/whip/");
    assert_eq!(id.len(), 32, "128-bit hex session id: {id}");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(browser.answer.contains("m=audio"), "{}", browser.answer);
    assert!(browser.answer.to_lowercase().contains("opus/48000"));
    assert!(browser.answer.contains("a=ice-lite"), "ICE-lite answer");
    assert!(browser.answer.contains("a=sendrecv"), "sendrecv deviation");
    assert!(
        browser.answer.contains("typ host") && browser.answer.contains("127.0.0.1"),
        "host candidate of the UDP socket"
    );
    assert_eq!(rig.server.active_calls(), 1);

    let deleted = browser.delete().await;
    assert_eq!(deleted.status, 200);
    wait_for_active(&rig, 0).await;
    assert_eq!(browser.delete().await.status, 404, "already gone");
    let unknown = http(rig.http_addr(), "DELETE", "/whip/nosuchsession", &[], &[]).await;
    assert_eq!(unknown.status, 404);
}

async fn wait_for_active(rig: &Rig, count: usize) {
    let clock = SystemClock::shared();
    let deadline = clock.now() + WAIT;
    while rig.server.active_calls() != count {
        assert!(
            clock.now() < deadline,
            "active calls stayed at {}",
            rig.server.active_calls()
        );
        clock.sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn options_answers_with_accept_post_and_cors_when_configured() {
    let rig = Rig::start(WebRtcConfig {
        cors_origin: Some("https://app.example".to_owned()),
        ..fast_config()
    })
    .await;
    let response = http(rig.http_addr(), "OPTIONS", "/whip", &[], &[]).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.headers["accept-post"], "application/sdp");
    assert!(response.headers["allow"].contains("POST"));
    assert_eq!(
        response.headers["access-control-allow-origin"],
        "https://app.example"
    );
    assert!(response.headers["access-control-allow-headers"].contains("Authorization"));
    let session = http(rig.http_addr(), "OPTIONS", "/whip/abc", &[], &[]).await;
    assert_eq!(session.status, 200);
    assert!(session.headers["allow"].contains("DELETE"));

    // Without a configured origin no CORS header is sent at all.
    let plain = Rig::start(fast_config()).await;
    let response = http(plain.http_addr(), "OPTIONS", "/whip", &[], &[]).await;
    assert_eq!(response.status, 200);
    assert!(!response.headers.contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn malformed_offers_and_wrong_requests_are_refused() {
    let rig = Rig::start(fast_config()).await;
    let addr = rig.http_addr();
    let sdp_header = [("Content-Type", "application/sdp")];

    let garbage = http(addr, "POST", "/whip", &sdp_header, b"this is not sdp").await;
    assert_eq!(garbage.status, 400, "{}", garbage.body);
    let empty = http(addr, "POST", "/whip", &sdp_header, b"").await;
    assert_eq!(empty.status, 400);
    let not_utf8 = http(addr, "POST", "/whip", &sdp_header, &[0xff, 0xfe, 0xfd]).await;
    assert_eq!(not_utf8.status, 400);
    // A syntactically valid SDP without an audio section.
    let video_only = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
a=group:BUNDLE 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\n\
a=mid:0\r\na=sendrecv\r\na=rtcp-mux\r\na=ice-ufrag:abcd\r\na=ice-pwd:0123456789abcdef01234567\r\n\
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
a=setup:actpass\r\na=rtpmap:96 VP8/90000\r\n";
    let video = http(addr, "POST", "/whip", &sdp_header, video_only.as_bytes()).await;
    assert_eq!(video.status, 400, "{}", video.body);
    // Too many media sections.
    let many = format!(
        "v=0\r\ns=-\r\nt=0 0\r\n{}",
        "m=audio 9 RTP/AVP 0\r\n".repeat(10)
    );
    let many = http(addr, "POST", "/whip", &sdp_header, many.as_bytes()).await;
    assert_eq!(many.status, 400);
    assert!(many.body.contains("media sections"), "{}", many.body);

    let wrong_type = http(
        addr,
        "POST",
        "/whip",
        &[("Content-Type", "text/plain")],
        b"v=0",
    )
    .await;
    assert_eq!(wrong_type.status, 415);
    assert_eq!(wrong_type.headers["accept-post"], "application/sdp");
    let no_type = http(addr, "POST", "/whip", &[], b"v=0").await;
    assert_eq!(no_type.status, 415);
    // A content type with parameters is fine (it then fails on the SDP, not on the type).
    let with_charset = http(
        addr,
        "POST",
        "/whip",
        &[("Content-Type", "Application/SDP; charset=utf-8")],
        b"v=0",
    )
    .await;
    assert_eq!(with_charset.status, 400);

    let huge = vec![b'a'; 70_000];
    let huge = http(addr, "POST", "/whip", &sdp_header, &huge).await;
    assert_eq!(huge.status, 413);

    assert_eq!(http(addr, "GET", "/whip", &[], &[]).await.status, 405);
    assert_eq!(
        http(addr, "PATCH", "/whip/abc", &[], b"x").await.status,
        405
    );
    assert_eq!(http(addr, "GET", "/nothing", &[], &[]).await.status, 404);
    assert_eq!(http(addr, "DELETE", "/whip", &[], &[]).await.status, 405);

    assert_eq!(
        rig.server.active_calls(),
        0,
        "a refused offer holds no slot"
    );
    assert!(rig.server.stats().offers_refused >= 8);
}

#[tokio::test]
async fn a_bearer_token_is_required_on_post_and_delete_when_configured() {
    let rig = Rig::start(WebRtcConfig {
        bearer_token: Some(Secret::new("s3cret-token".to_owned())),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp_header =
        |auth: &'static str| [("Content-Type", "application/sdp"), ("Authorization", auth)];
    let none = http(
        addr,
        "POST",
        "/whip",
        &[("Content-Type", "application/sdp")],
        b"v=0",
    )
    .await;
    assert_eq!(none.status, 401);
    assert_eq!(none.headers["www-authenticate"], "Bearer");
    let wrong = http(addr, "POST", "/whip", &sdp_header("Bearer nope"), b"v=0").await;
    assert_eq!(wrong.status, 401);
    let scheme = http(
        addr,
        "POST",
        "/whip",
        &sdp_header("Basic s3cret-token"),
        b"v=0",
    )
    .await;
    assert_eq!(scheme.status, 401);
    // With the token the request gets as far as the SDP check.
    let right = http(
        addr,
        "POST",
        "/whip",
        &sdp_header("bearer s3cret-token"),
        b"v=0",
    )
    .await;
    assert_eq!(right.status, 400);
    // DELETE needs it too; OPTIONS (CORS preflight) and the demo page do not.
    assert_eq!(
        http(addr, "DELETE", "/whip/abc", &[], &[]).await.status,
        401
    );
    assert_eq!(
        http(
            addr,
            "DELETE",
            "/whip/abc",
            &[("Authorization", "Bearer s3cret-token")],
            &[]
        )
        .await
        .status,
        404
    );
    assert_eq!(http(addr, "OPTIONS", "/whip", &[], &[]).await.status, 200);
    assert_eq!(http(addr, "GET", "/demo", &[], &[]).await.status, 200);
    assert_eq!(
        http(addr, "GET", "/demo/brisvox-call.js", &[], &[])
            .await
            .status,
        200,
        "the demo page's script needs no token either"
    );
}

#[tokio::test]
async fn the_call_limit_answers_503_with_retry_after() {
    let rig = Rig::start(WebRtcConfig {
        max_calls: 1,
        ..fast_config()
    })
    .await;
    let first = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            silent: true,
            ..BrowserOptions::default()
        },
    )
    .await;
    // A second offer needs an offer body; reuse a fresh browser's SDP via a raw POST.
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto())
        .build(rig.clock.now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    let busy = post_offer(rig.http_addr(), &offer.to_sdp_string()).await;
    assert_eq!(busy.status, 503);
    assert_eq!(busy.headers["retry-after"], "1");
    // Freed by the first call's end, the slot serves the next offer.
    assert_eq!(first.delete().await.status, 200);
    wait_for_active(&rig, 0).await;
    let again = post_offer(rig.http_addr(), &offer.to_sdp_string()).await;
    assert_eq!(again.status, 201, "{}", again.body);
}

#[tokio::test]
async fn an_offer_with_an_mdns_candidate_is_accepted() {
    // Browsers hide host addresses behind `<uuid>.local` names the server cannot resolve.
    let rig = Rig::start(fast_config()).await;
    let browser = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            silent: true,
            extra_offer_lines: vec![
                "a=candidate:9 1 udp 2113937151 0a1b2c3d-4e5f-6789-abcd-ef0123456789.local 54321 typ host generation 0"
                    .to_owned(),
            ],
            ..BrowserOptions::default()
        },
    )
    .await;
    assert!(browser.answer.contains("m=audio"));
}

#[tokio::test]
async fn the_demo_page_is_served_with_the_signaling_path() {
    let rig = Rig::start(fast_config()).await;
    let page = http(rig.http_addr(), "GET", "/demo", &[], &[]).await;
    assert_eq!(page.status, 200);
    assert!(page.headers["content-type"].starts_with("text/html"));
    // The page is a thin module around the shared browser client, served next to it: the paths
    // are substituted and nothing is loaded from elsewhere.
    assert!(page.body.contains("type=\"module\""));
    assert!(
        page.body
            .contains("import { BrisvoxCall } from \"/demo/brisvox-call.js\""),
        "module path substituted"
    );
    assert!(
        page.body.contains("const WHIP = \"/whip\""),
        "path substituted"
    );
    assert!(!page.body.contains("__WHIP_PATH__"));
    assert!(
        page.body.contains("const WS = \"/ws\""),
        "ws path substituted"
    );
    assert!(!page.body.contains("__WS_PATH__"));
    assert!(!page.body.contains("__MODULE_PATH__"));
    // Headers: no type sniffing, and a policy that lets exactly the page's own script run: the
    // module from this origin, the inline script by a per-response nonce, the AudioWorklet from a
    // Blob URL, connections to this origin only.
    assert_eq!(page.headers["x-content-type-options"], "nosniff");
    let csp = page.headers["content-security-policy"].clone();
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("script-src 'self' blob: 'nonce-"), "{csp}");
    assert!(csp.contains("worker-src blob:"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    assert!(!csp.contains("unsafe-eval") && !csp.contains("script-src 'unsafe-inline'"));
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("a nonce in the policy")
        .to_owned();
    assert_eq!(nonce.len(), 32, "128 random bits as hex");
    assert!(
        page.body
            .contains(&format!("<script type=\"module\" nonce=\"{nonce}\">")),
        "the inline script carries the nonce of this response"
    );
    assert!(!page.body.contains("__NONCE__"));
    let again = http(rig.http_addr(), "GET", "/demo", &[], &[]).await;
    assert_ne!(
        again.headers["content-security-policy"], csp,
        "a new nonce for every response"
    );
    assert!(!page.body.contains("src=\"http"));
    assert!(
        !page.body.contains("RTCPeerConnection"),
        "no duplicated call code in the page"
    );

    // The module itself: JavaScript, with the WebRTC client, the WebSocket fallback and the
    // AudioWorklet code (loaded from a Blob URL) in it.
    let module = http(rig.http_addr(), "GET", "/demo/brisvox-call.js", &[], &[]).await;
    assert_eq!(module.status, 200);
    assert!(module.headers["content-type"].starts_with("text/javascript"));
    assert_eq!(module.headers["x-content-type-options"], "nosniff");
    assert!(module.headers["content-security-policy"].contains("default-src 'none'"));
    assert!(module.body.contains("export class BrisvoxCall"));
    assert!(module.body.contains("getUserMedia"));
    assert!(module.body.contains("RTCPeerConnection"));
    assert!(module.body.contains("addModule"));
    assert!(module.body.contains("URL.createObjectURL"));
    assert!(
        module.body.contains("method: \"DELETE\""),
        "hangs up with a DELETE"
    );
    assert_eq!(
        http(rig.http_addr(), "POST", "/demo/brisvox-call.js", &[], &[])
            .await
            .status,
        405
    );
    assert_eq!(
        http(rig.http_addr(), "POST", "/demo", &[], &[])
            .await
            .status,
        405
    );

    let off = Rig::start(WebRtcConfig {
        demo_path: None,
        ..fast_config()
    })
    .await;
    assert_eq!(
        http(off.http_addr(), "GET", "/demo", &[], &[]).await.status,
        404
    );
    assert_eq!(
        http(off.http_addr(), "GET", "/demo/brisvox-call.js", &[], &[])
            .await
            .status,
        404,
        "the module goes with the page"
    );

    // With the fallback off, the page knows: an empty path.
    let no_ws = Rig::start(WebRtcConfig {
        ws_path: None,
        ..fast_config()
    })
    .await;
    let page = http(no_ws.http_addr(), "GET", "/demo", &[], &[]).await;
    assert!(page.body.contains("const WS = \"\""));
    let custom = Rig::start(WebRtcConfig {
        ws_path: Some("/voice/socket".to_owned()),
        ..fast_config()
    })
    .await;
    let page = http(custom.http_addr(), "GET", "/demo", &[], &[]).await;
    assert!(page.body.contains("const WS = \"/voice/socket\""));
    let moved = Rig::start(WebRtcConfig {
        demo_path: Some("/try/".to_owned()),
        ..fast_config()
    })
    .await;
    let page = http(moved.http_addr(), "GET", "/try", &[], &[]).await;
    assert!(page.body.contains("from \"/try/brisvox-call.js\""));
    assert_eq!(
        http(moved.http_addr(), "GET", "/try/brisvox-call.js", &[], &[])
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn a_slow_request_is_dropped_and_the_server_keeps_serving() {
    let rig = Rig::start(WebRtcConfig {
        request_timeout: Duration::from_millis(200),
        ..fast_config()
    })
    .await;
    let mut stalled = TcpStream::connect(rig.http_addr()).await.unwrap();
    stalled
        .write_all(b"POST /whip HTTP/1.1\r\nContent-Le")
        .await
        .unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(WAIT, stalled.read_to_end(&mut answer))
        .await
        .expect("the server never gave up on the stalled request")
        .unwrap();
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 408"));
    assert_eq!(
        http(rig.http_addr(), "OPTIONS", "/whip", &[], &[])
            .await
            .status,
        200
    );
}

// ---------------------------------------------------------------------------------------------
// Media

#[tokio::test]
async fn ice_lite_connects_on_loopback_and_hands_the_call_to_the_engine() {
    for ice_lite in [true, false] {
        let mut rig = Rig::start(WebRtcConfig {
            ice_lite,
            ..fast_config()
        })
        .await;
        let (browser, session, info) = rig.call().await;
        assert_eq!(
            browser.answer.contains("a=ice-lite"),
            ice_lite,
            "ice_lite = {ice_lite}"
        );
        assert!(info.call_id.starts_with("webrtc-"));
        assert_eq!(info.metadata["transport"], "webrtc");
        assert_eq!(info.metadata["encoding"], "opus");
        assert_eq!(
            info.sample_rate().unwrap(),
            Some(CallRates {
                input: SampleRate::Hz16000,
                output: SampleRate::Hz24000
            })
        );
        assert!(info.metadata["peer_addr"].starts_with("127.0.0.1:"));
        assert!(
            !info
                .call_id
                .contains(browser.session_url.trim_start_matches("/whip/")),
            "the engine must not learn the secret session id"
        );
        let stats = rig.server.stats();
        assert_eq!(stats.sessions_created, 1);
        assert_eq!(stats.calls_started, 1);
        assert!(stats.datagrams_in > 0 && stats.datagrams_out > 0);
        browser.stop().await;
        session.close(WAIT).await.unwrap();
    }
}

#[tokio::test]
async fn audio_flows_both_ways_through_the_transport_api_with_clear_and_mark() {
    let mut rig = Rig::start(fast_config()).await;
    let clock = rig.clock.clone();
    let (browser, mut session, _info) = rig.call().await;

    // Caller -> engine: the browser talks a 1 kHz tone for 0.8 s.
    browser.talk(true);
    let mut caller = Vec::new();
    let deadline = clock.now() + WAIT;
    while caller.len() < 16_000 * 6 / 10 {
        assert!(
            clock.now() < deadline,
            "only {} caller samples",
            caller.len()
        );
        until_event(&mut session, &mut caller, |e| {
            matches!(e, TransportEvent::Audio(_)).then_some(())
        })
        .await;
    }
    browser.talk(false);
    let steady = &caller[caller.len() / 4..];
    let hz = pitch_hz(steady, 16_000);
    assert!((950.0..1_050.0).contains(&hz), "caller pitch {hz} Hz");
    let peak = steady.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!((4_000..14_000).contains(&peak), "caller level {peak}");

    // Engine -> caller: 0.6 s of a 500 Hz tone at the TTS rate (the core paces it in real time).
    let sent_at = clock.now();
    send_tone(&session, &clock, 30, TurnEpoch::ZERO, 0).await;
    browser
        .until("the first bot audio", |s| !s.received.is_empty())
        .await;
    let first_audio = clock.now().saturating_duration_since(sent_at);
    println!("engine command to first sample at the browser: {first_audio:?}");
    // A hang guard only: the latency claim is checked, on a median, by
    // `first_bot_audio_reaches_the_browser_fast_at_the_median`.
    assert!(first_audio < Duration::from_millis(400), "{first_audio:?}");
    browser
        .until("bot audio", |s| s.received.len() >= 48_000 * 4 / 10)
        .await;
    // Mark behind the audio: acknowledged shortly after the audio was sent.
    session
        .send(TransportCommand::Mark {
            id: "end-of-greeting".to_owned(),
        })
        .await
        .unwrap();
    let mut scratch = Vec::new();
    let played = until_event(&mut session, &mut scratch, |event| match event {
        TransportEvent::MarkPlayed { id } => Some(id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(played, "end-of-greeting");
    pause(Duration::from_millis(200)).await;
    let received = browser.take_received();
    assert!(
        received.len() >= 48_000 * 5 / 10,
        "browser got {} samples",
        received.len()
    );
    let steady = &received[received.len() / 4..received.len() * 3 / 4];
    let hz = pitch_hz(steady, 48_000);
    assert!((470.0..530.0).contains(&hz), "bot pitch {hz} Hz");
    let stats = rig.server.stats();
    assert!(stats.audio_frames_in > 0 && stats.audio_frames_out >= 30);
    assert!(stats.rtp_packets_out >= 28, "{stats:?}");
    // A clean network: nothing to conceal, nothing over the packet rate.
    assert_eq!(stats.frames_concealed, 0, "{stats:?}");
    assert_eq!(stats.audio_packets_rate_limited, 0, "{stats:?}");

    // Barge-in: a long utterance with a mark behind it, cleared half-way.
    send_tone(&session, &clock, 100, TurnEpoch::ZERO, 30).await;
    session
        .send(TransportCommand::Mark {
            id: "never-played".to_owned(),
        })
        .await
        .unwrap();
    browser
        .until("the long utterance", |s| {
            s.received.len() >= 48_000 * 3 / 10
        })
        .await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::ZERO.next(),
        })
        .await
        .unwrap();
    let cleared = until_event(&mut session, &mut scratch, |event| match event {
        TransportEvent::MarkCleared { id } => Some(id.clone()),
        TransportEvent::MarkPlayed { id } => panic!("{id} was played, not cleared"),
        _ => None,
    })
    .await;
    assert_eq!(cleared, "never-played");
    // What the browser still gets after the clear is what was already in flight: well under the
    // 2 s that were queued, and it stops.
    pause(Duration::from_millis(250)).await;
    let before = browser.received_samples();
    pause(Duration::from_millis(400)).await;
    let after = browser.received_samples();
    assert_eq!(before, after, "audio kept arriving after the clear");
    assert!(
        after < 48_000 * 12 / 10,
        "{after} samples: the cleared audio was not dropped"
    );
    // New epoch audio flows again.
    browser.take_received();
    send_tone(&session, &clock, 15, TurnEpoch::ZERO.next(), 130).await;
    browser
        .until("audio after the clear", |s| {
            s.received.len() >= 48_000 * 2 / 10
        })
        .await;

    // Hang-up by the engine closes the browser's connection.
    session.send(TransportCommand::Stop).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::Requested);
    wait_for_active(&rig, 0).await;
    browser
        .until(
            "the engine's hang-up to close the browser's connection",
            |s| s.closed,
        )
        .await;
    browser.stop().await;
}

#[tokio::test]
async fn the_codec_runs_on_the_inference_pool_when_one_is_given() {
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 64,
            name: "test-codec".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let mut rig = Rig::start_with(fast_config(), WebRtcOptions::with_pool(pool.handle())).await;
    let (browser, mut session, _) = rig.call().await;
    browser.talk(true);
    let mut caller = Vec::new();
    let mut frames = 0;
    until_event(&mut session, &mut caller, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 20).then_some(())
    })
    .await;
    send_tone(&session, &rig.clock, 20, TurnEpoch::ZERO, 0).await;
    browser
        .until("bot audio", |s| s.received.len() >= 48_000 / 4)
        .await;
    let metrics = pool.metrics();
    assert!(
        metrics.completed >= 40,
        "codec jobs ran on the pool: {metrics:?}"
    );
    assert_eq!(metrics.panicked, 0);
    assert_eq!(rig.server.stats().codec_dropped, 0);
    browser.stop().await;
    session.close(WAIT).await.unwrap();
    rig.server.shutdown(WAIT).await.unwrap();
    drop(rig);
    pool.shutdown();
}

#[tokio::test]
async fn a_delete_is_a_peer_hangup_for_the_engine() {
    let mut rig = Rig::start(fast_config()).await;
    let (browser, mut session, _) = rig.call().await;
    assert_eq!(browser.delete().await.status, 200);
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
    browser.stop().await;
}

#[tokio::test]
async fn the_browser_closing_its_connection_is_a_peer_hangup() {
    let mut rig = Rig::start(fast_config()).await;
    let (browser, mut session, _) = rig.call().await;
    browser.with(|s| s.close_requested = true);
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
    browser.stop().await;
}

/// A codec whose first decoder panics on its first packet: a bug in one call.
#[derive(Debug, Default)]
struct PanickyFactory {
    decoders: std::sync::atomic::AtomicUsize,
}

struct PanickyDecoder(Box<dyn brisvox_transport::webrtc::opus::OpusDecode>, bool);

impl brisvox_transport::webrtc::opus::OpusDecode for PanickyDecoder {
    fn decode(
        &mut self,
        packet: &[u8],
        pcm: &mut [i16],
    ) -> Result<usize, brisvox_transport::webrtc::opus::CodecError> {
        assert!(!self.1, "decoder bug (expected by the test)");
        self.0.decode(packet, pcm)
    }

    fn conceal(
        &mut self,
        pcm: &mut [i16],
    ) -> Result<usize, brisvox_transport::webrtc::opus::CodecError> {
        self.0.conceal(pcm)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

impl OpusFactory for PanickyFactory {
    fn encoder(
        &self,
        settings: &OpusSettings,
    ) -> Result<
        Box<dyn brisvox_transport::webrtc::opus::OpusEncode>,
        brisvox_transport::webrtc::opus::CodecError,
    > {
        LibOpusFactory.encoder(settings)
    }

    fn decoder(
        &self,
    ) -> Result<
        Box<dyn brisvox_transport::webrtc::opus::OpusDecode>,
        brisvox_transport::webrtc::opus::CodecError,
    > {
        let first = self
            .decoders
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        Ok(Box::new(PanickyDecoder(LibOpusFactory.decoder()?, first)))
    }
}

#[tokio::test]
async fn a_panic_in_one_call_ends_that_call_only() {
    let mut rig = Rig::start_with(
        fast_config(),
        WebRtcOptions {
            opus: Arc::new(PanickyFactory::default()),
            codec_pool: None,
        },
    )
    .await;
    // Call one: its decoder panics on the first packet from the browser.
    let (first, mut first_session, _) = rig.call().await;
    first.talk(true);
    assert_eq!(
        stopped(&mut first_session).await,
        StopReason::PeerDisconnected
    );
    wait_for_active(&rig, 0).await;
    assert_eq!(rig.server.stats().call_panics, 1);
    first.stop().await;
    // Call two is unaffected, and the server still takes calls.
    let (second, mut second_session, _) = rig.call().await;
    second.talk(true);
    let mut audio = Vec::new();
    let mut frames = 0;
    until_event(&mut second_session, &mut audio, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 10).then_some(())
    })
    .await;
    assert_eq!(rig.server.stats().call_panics, 1);
    second.stop().await;
    second_session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_full_accept_queue_ends_the_new_call_at_once() {
    let mut rig = Rig::start(WebRtcConfig {
        accept_queue: 1,
        ..fast_config()
    })
    .await;
    // The engine does not take calls: the first connected call fills the queue.
    let first = Browser::connect(rig.http_addr(), BrowserOptions::default()).await;
    first.wait_connected().await;
    let second = Browser::connect(rig.http_addr(), BrowserOptions::default()).await;
    second
        .until("the refused call to be closed", |s| s.closed)
        .await;
    wait_for_active(&rig, 1).await;
    // The queued call is still there for the engine.
    let mut session = rig.accept().await;
    assert!(matches!(
        next_event(&mut session).await,
        TransportEvent::Started(_)
    ));
    first.stop().await;
    second.stop().await;
}

#[tokio::test]
async fn the_per_address_limit_answers_429() {
    let rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(1),
        ..fast_config()
    })
    .await;
    let first = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            silent: true,
            ..BrowserOptions::default()
        },
    )
    .await;
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto())
        .build(rig.clock.now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    let second = post_offer(rig.http_addr(), &offer.to_sdp_string()).await;
    assert_eq!(second.status, 429);
    assert_eq!(second.headers["retry-after"], "1");
    assert_eq!(first.delete().await.status, 200);
    wait_for_active(&rig, 0).await;
    assert_eq!(
        post_offer(rig.http_addr(), &offer.to_sdp_string())
            .await
            .status,
        201
    );
}

#[tokio::test]
async fn bot_audio_at_the_wrong_rate_is_dropped_with_one_fault() {
    let mut rig = Rig::start(fast_config()).await;
    let clock = rig.clock.clone();
    let (browser, mut session, _) = rig.call().await;
    for seq in 0..5 {
        session
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::ZERO,
                utterance: UtteranceId::new(1),
                frame: tone_frame(&clock, 500.0, seq, TurnEpoch::ZERO, SampleRate::Hz8000),
            })
            .await
            .unwrap();
    }
    let mut scratch = Vec::new();
    let message = until_event(&mut session, &mut scratch, |event| match event {
        TransportEvent::Error { message } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(message.contains("output_rate_mismatch"), "{message}");
    pause(Duration::from_millis(300)).await;
    assert_eq!(browser.received_samples(), 0, "mismatched audio was played");
    assert_eq!(rig.server.stats().audio_out_dropped, 5);
    browser.stop().await;
}

#[tokio::test]
async fn a_sendonly_offer_works_for_the_caller_but_the_bot_cannot_speak() {
    let mut rig = Rig::start(fast_config()).await;
    let browser = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            direction: Direction::SendOnly,
            ..BrowserOptions::default()
        },
    )
    .await;
    assert!(browser.answer.contains("a=recvonly"), "{}", browser.answer);
    let mut session = rig.accept().await;
    assert!(matches!(
        next_event(&mut session).await,
        TransportEvent::Started(_)
    ));
    browser.wait_connected().await;
    browser.talk(true);
    let mut caller = Vec::new();
    let mut frames = 0;
    until_event(&mut session, &mut caller, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 10).then_some(())
    })
    .await;
    send_tone(&session, &rig.clock, 3, TurnEpoch::ZERO, 0).await;
    let message = until_event(&mut session, &mut caller, |event| match event {
        TransportEvent::Error { message } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(message.contains("sendonly"), "{message}");
    browser.stop().await;
}

#[tokio::test]
async fn an_offer_whose_browser_never_connects_is_abandoned_and_frees_its_slot() {
    let mut rig = Rig::start(WebRtcConfig {
        connect_timeout: Duration::from_millis(400),
        ..fast_config()
    })
    .await;
    let silent = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            silent: true,
            ..BrowserOptions::default()
        },
    )
    .await;
    assert_eq!(rig.server.active_calls(), 1);
    wait_for_active(&rig, 0).await;
    // The engine never heard of it.
    let accepted =
        tokio::time::timeout(Duration::from_millis(200), rig.server.accept(&rig.parent)).await;
    assert!(accepted.is_err(), "an abandoned call reached the engine");
    drop(silent);
}

#[tokio::test]
async fn several_calls_run_side_by_side_and_each_ends_on_its_own() {
    let mut rig = Rig::start(fast_config()).await;
    let mut calls = Vec::new();
    for _ in 0..4 {
        let browser = Browser::connect(rig.http_addr(), BrowserOptions::default()).await;
        let mut session = rig.accept().await;
        assert!(matches!(
            next_event(&mut session).await,
            TransportEvent::Started(_)
        ));
        browser.wait_connected().await;
        browser.talk(true);
        calls.push((browser, session));
    }
    assert_eq!(rig.server.active_calls(), 4);
    for (_, session) in &mut calls {
        let mut audio = Vec::new();
        let mut frames = 0;
        until_event(session, &mut audio, |event| {
            if matches!(event, TransportEvent::Audio(_)) {
                frames += 1;
            }
            (frames >= 10).then_some(())
        })
        .await;
    }
    // One hangs up; the others carry on.
    let (first, mut first_session) = calls.remove(0);
    assert_eq!(first.delete().await.status, 200);
    assert_eq!(stopped(&mut first_session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 3).await;
    let (browser, session) = &mut calls[0];
    browser.take_received();
    send_tone(session, &rig.clock, 10, TurnEpoch::ZERO, 0).await;
    browser
        .until("audio on a surviving call", |s| {
            s.received.len() >= 48_000 / 10
        })
        .await;
    for (browser, session) in calls {
        browser.stop().await;
        session.close(WAIT).await.unwrap();
    }
    first.stop().await;
}

#[tokio::test]
async fn shutdown_ends_every_call_and_leaves_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let mut rig = Rig::start(fast_config()).await;
        // A finished call, a call hung up by DELETE, a call in the middle of the handshake and
        // one the engine is still talking to when the server goes away.
        let (browser, session, _) = rig.call().await;
        browser.talk(true);
        pause(Duration::from_millis(100)).await;
        browser.stop().await;
        session.close(WAIT).await.unwrap();
        wait_for_active(&rig, 0).await;

        let (browser, mut session, _) = rig.call().await;
        assert_eq!(browser.delete().await.status, 200);
        assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
        session.close(WAIT).await.unwrap();
        browser.stop().await;

        let half = Browser::connect(
            rig.http_addr(),
            BrowserOptions {
                silent: true,
                ..BrowserOptions::default()
            },
        )
        .await;
        let (live, live_session, _) = rig.call().await;
        live.talk(true);
        rig.server.shutdown(WAIT).await.unwrap();
        assert_eq!(rig.server.active_calls(), 0);
        // The shutdown told the live browser (DTLS close), it did not just stop answering.
        live.until("the shutdown to close the browser's connection", |s| {
            s.closed
        })
        .await;
        live.stop().await;
        drop(live_session);
        drop(half);
    }
    counter
        .wait_for_baseline(&*clock, WAIT)
        .await
        .expect("tasks outlived the server and its calls");
}

// ---------------------------------------------------------------------------------------------
// The real-browser path, mark timing, shutdown, connection limits

/// Brings a call up with `candidates` in the browser's offer and carries audio both ways.
async fn call_with_candidates(candidates: OfferCandidates) {
    let mut rig = Rig::start(fast_config()).await;
    let browser = Browser::connect(
        rig.http_addr(),
        BrowserOptions {
            candidates,
            ..BrowserOptions::default()
        },
    )
    .await;
    let mut session = rig.accept().await;
    let info = match next_event(&mut session).await {
        TransportEvent::Started(info) => info,
        other => panic!("expected Started, got {other:?}"),
    };
    // The server learned the address from the browser's STUN request (peer-reflexive).
    assert!(info.metadata["peer_addr"].starts_with("127.0.0.1:"));
    browser.wait_connected().await;
    browser.talk(true);
    let mut caller = Vec::new();
    let mut frames = 0;
    until_event(&mut session, &mut caller, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 25).then_some(())
    })
    .await;
    let hz = pitch_hz(&caller[caller.len() / 4..], 16_000);
    assert!((950.0..1_050.0).contains(&hz), "caller pitch {hz} Hz");
    send_tone(&session, &rig.clock, 20, TurnEpoch::ZERO, 0).await;
    browser
        .until("bot audio", |s| s.received.len() >= 48_000 / 4)
        .await;
    let stats = rig.server.stats();
    assert_eq!(stats.calls_started, 1);
    assert_eq!(stats.decode_errors, 0, "{stats:?}");
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_offer_with_only_an_mdns_candidate_connects_and_carries_audio() {
    // Chrome's default: the host address is hidden behind a `.local` name, so after the server
    // strips it the offer has no usable candidate.
    call_with_candidates(OfferCandidates::MdnsOnly).await;
}

#[tokio::test]
async fn an_offer_without_any_candidate_connects_and_carries_audio() {
    call_with_candidates(OfferCandidates::Nothing).await;
}

#[tokio::test]
async fn a_mark_waits_for_a_partial_frame_tail_and_is_acknowledged_after_mark_delay() {
    let mark_delay = Duration::from_millis(300);
    let mut rig = Rig::start(WebRtcConfig {
        mark_delay,
        ..fast_config()
    })
    .await;
    let clock = rig.clock.clone();
    let (browser, mut session, _) = rig.call().await;
    // 50 ms of audio in 10 ms frames: not a whole number of 20 ms packets. The mark behind it
    // must make the adapter send the remainder (padded) before the mark is acknowledged.
    for seq in 0..5usize {
        let epoch = TurnEpoch::ZERO;
        let samples = 240;
        let pcm: Vec<u8> = (seq * samples..(seq + 1) * samples)
            .flat_map(|n| {
                let phase = 2.0 * std::f32::consts::PI * 500.0 * n as f32 / 24_000.0;
                ((phase.sin() * 8_000.0) as i16).to_le_bytes()
            })
            .collect();
        let frame = AudioFrame::new(
            Bytes::from(pcm),
            SampleRate::Hz24000,
            clock.now(),
            seq as u64,
            epoch,
        )
        .unwrap();
        session
            .send(TransportCommand::Audio {
                epoch,
                utterance: UtteranceId::new(1),
                frame,
            })
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "tail".to_owned(),
        })
        .await
        .unwrap();
    let mut scratch = Vec::new();
    until_event(&mut session, &mut scratch, |event| {
        matches!(event, TransportEvent::MarkPlayed { id } if id == "tail").then_some(())
    })
    .await;
    let acked_at = clock.now();
    // Three packets (two whole ones and the padded remainder), all there when the mark counts.
    let (samples, last_packet) = browser.with(|s| (s.received.len(), s.last_packet_at));
    assert!(
        samples >= 3 * 960,
        "{samples} samples at the browser when the mark was acknowledged: the tail was not sent first"
    );
    // The acknowledgement trails the last packet by about mark_delay (never by zero).
    let gap = acked_at.saturating_duration_since(last_packet.unwrap());
    assert!(
        gap >= mark_delay - Duration::from_millis(60),
        "mark acknowledged {gap:?} after the last packet, mark_delay is {mark_delay:?}"
    );
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn accept_returns_none_after_shutdown_and_hands_out_calls_queued_before() {
    let mut rig = Rig::start(fast_config()).await;
    // A call is queued (connected, not yet taken) when the server shuts down.
    let browser = Browser::connect(rig.http_addr(), BrowserOptions::default()).await;
    browser.wait_connected().await;
    rig.server.shutdown(WAIT).await.unwrap();
    // The engine's accept loop ends with None; it does not hang until its own token fires.
    let mut taken = 0;
    loop {
        let next = tokio::time::timeout(WAIT, rig.server.accept(&CancellationToken::new()))
            .await
            .expect("accept hung after shutdown")
            .unwrap();
        match next {
            Some(_) => taken += 1,
            None => break,
        }
        assert!(taken <= 1, "more calls than were ever started");
    }
    // And it stays None.
    assert!(
        tokio::time::timeout(WAIT, rig.server.accept(&CancellationToken::new()))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    browser.stop().await;
}

#[tokio::test]
async fn connections_over_the_signaling_limit_are_closed_unanswered() {
    let rig = Rig::start(WebRtcConfig {
        max_http_connections: 1,
        request_timeout: Duration::from_secs(3),
        ..fast_config()
    })
    .await;
    // One stalled connection holds the only slot.
    let mut holder = TcpStream::connect(rig.http_addr()).await.unwrap();
    holder.write_all(b"POST /whip HTTP/1.1\r\n").await.unwrap();
    pause(Duration::from_millis(100)).await;
    let mut refused = TcpStream::connect(rig.http_addr()).await.unwrap();
    let _ = refused
        .write_all(b"OPTIONS /whip HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
        .await;
    let mut raw = Vec::new();
    let read = tokio::time::timeout(WAIT, refused.read_to_end(&mut raw))
        .await
        .expect("the over-limit connection was neither answered nor closed");
    assert!(
        raw.is_empty(),
        "an over-limit connection got an answer: {:?} ({read:?})",
        String::from_utf8_lossy(&raw)
    );
    // Once the slot is free the server answers again.
    drop(holder);
    let clock = SystemClock::shared();
    let deadline = clock.now() + WAIT;
    loop {
        let mut stream = TcpStream::connect(rig.http_addr()).await.unwrap();
        let _ = stream
            .write_all(b"OPTIONS /whip HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await;
        let mut raw = Vec::new();
        let _ = tokio::time::timeout(WAIT, stream.read_to_end(&mut raw)).await;
        if raw.starts_with(b"HTTP/1.1 200") {
            break;
        }
        assert!(clock.now() < deadline, "the freed slot never served again");
        clock.sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn an_offer_must_be_exactly_one_live_audio_section_with_rtcp_mux() {
    // RFC 9725 4.4: no partially successful answers.
    let rig = Rig::start(fast_config()).await;
    let offer_with = |kinds: &[MediaKind]| {
        let mut rtc = Rtc::builder()
            .set_crypto_provider(crypto())
            .build(rig.clock.now().into_std());
        rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
        let mut change = rtc.sdp_api();
        for kind in kinds {
            change.add_media(*kind, Direction::SendRecv, None, None, None);
        }
        let (offer, _pending) = change.apply().unwrap();
        offer.to_sdp_string()
    };
    let audio = offer_with(&[MediaKind::Audio]);
    assert_eq!(
        post_offer(rig.http_addr(), &audio).await.status,
        201,
        "the audio-only offer is the baseline"
    );

    for kinds in [
        vec![MediaKind::Audio, MediaKind::Video],
        vec![MediaKind::Audio, MediaKind::Audio],
    ] {
        let refused = post_offer(rig.http_addr(), &offer_with(&kinds)).await;
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(refused.body.contains("media sections"), "{}", refused.body);
    }

    let no_mux: String = audio
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("a=rtcp-mux"))
        .collect();
    let refused = post_offer(rig.http_addr(), &no_mux).await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(refused.body.contains("rtcp-mux"), "{}", refused.body);
    assert_eq!(rig.server.active_calls(), 1, "only the baseline call lives");
}

/// A browser that vanishes without a DTLS close or DELETE (closed tab, lost network) is noticed
/// through ICE-lite consent expiry plus `disconnect_grace`. The real window is about 15 s, so the
/// test pauses tokio's clock once the call is up: the server's timers (all on the injected
/// clock) then run in simulated time, and the whole path (str0m's `Disconnected` event, the
/// grace, `End::PeerGone`, the freed slot and per-address count) runs in milliseconds.
#[tokio::test]
async fn a_vanished_browser_frees_its_slot_and_ends_the_engines_call() {
    let mut rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(1),
        ..fast_config()
    })
    .await;
    let (browser, mut session, _) = rig.call().await;
    assert_eq!(rig.server.active_calls(), 1);
    browser.stop().await;
    tokio::time::pause();
    let clock = SystemClock::shared();
    let started = clock.now();
    let reason = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(TransportEvent::Stopped { reason }) = session.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("a vanished browser was never noticed");
    let noticed_after = clock.now().saturating_duration_since(started);
    println!("vanished browser noticed after {noticed_after:?} (simulated)");
    assert_eq!(reason, StopReason::PeerDisconnected);
    // Documented: about 15 s of consent expiry, then the 500 ms grace of `fast_config`.
    assert!(
        (Duration::from_secs(5)..Duration::from_secs(30)).contains(&noticed_after),
        "{noticed_after:?}"
    );
    // Real time again for the sockets of the next call (paused time would jump over their waits).
    tokio::time::resume();
    wait_for_active(&rig, 0).await;
    // The per-address count was released too: the same address can call again.
    let (again, _session, _) = rig.call().await;
    again.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Packet loss, silence suppression, packet floods

/// Talks for `talk`, then listens `settle` longer; returns the caller audio at the engine.
async fn caller_talks(
    browser: &Browser,
    session: &mut TransportSession,
    talk: Duration,
    settle: Duration,
) -> Vec<i16> {
    browser.talk(true);
    let mut audio = drain_caller_audio(session, talk).await;
    browser.talk(false);
    audio.extend(drain_caller_audio(session, settle).await);
    audio
}

#[tokio::test]
async fn lost_rtp_packets_are_concealed_so_the_caller_audio_keeps_its_length() {
    let mut rig = Rig::start(WebRtcConfig {
        reorder_timeout: Duration::from_millis(40),
        ..fast_config()
    })
    .await;
    let (browser, mut session, _) = rig
        .call_with(BrowserOptions {
            drop_every: Some(8),
            ..BrowserOptions::default()
        })
        .await;
    let caller = caller_talks(
        &browser,
        &mut session,
        Duration::from_millis(1_600),
        Duration::from_millis(600),
    )
    .await;
    let (sent, dropped) = browser.with(|s| (s.sent_packets, s.dropped_on_wire));
    let stats = rig.server.stats();
    println!(
        "sent {sent} packets, the network dropped {dropped}, concealed {}, caller samples {}",
        stats.frames_concealed,
        caller.len()
    );
    assert!(sent >= 60 && dropped >= 6, "sent {sent}, dropped {dropped}");
    // Every isolated loss that was followed by a packet is concealed with exactly one frame.
    assert!(
        stats.frames_concealed >= dropped as u64 - 1 && stats.frames_concealed <= dropped as u64,
        "{} frames concealed for {dropped} lost packets",
        stats.frames_concealed
    );
    // The caller audio is on the time line: one 20 ms frame (320 samples at 16 kHz) per packet
    // the browser sent, concealed ones included, give or take the resampler's delay and the
    // last packet(s). Without concealment it would be short by `dropped` frames.
    let expected = sent * 320;
    assert!(
        caller.len() + 4 * 320 >= expected && caller.len() <= expected + 2 * 320,
        "{} caller samples for {sent} packets ({dropped} lost): expected about {expected}",
        caller.len()
    );
    assert_eq!(stats.decode_errors, 0, "{stats:?}");
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn silence_suppression_jumps_in_the_rtp_clock_are_not_concealed() {
    let mut rig = Rig::start(fast_config()).await;
    // Every 10 packets the sender skips 5 frames of RTP time (DTX), the sequence numbers go on.
    let (browser, mut session, _) = rig
        .call_with(BrowserOptions {
            ts_jump: Some((10, 5 * 960)),
            ..BrowserOptions::default()
        })
        .await;
    let caller = caller_talks(
        &browser,
        &mut session,
        Duration::from_millis(1_200),
        Duration::from_millis(300),
    )
    .await;
    let sent = browser.with(|s| s.sent_packets);
    let stats = rig.server.stats();
    assert!(sent >= 40, "{sent}");
    assert_eq!(
        stats.frames_concealed, 0,
        "a timestamp jump over contiguous sequence numbers is silence, not loss"
    );
    assert!(
        caller.len() <= (sent + 2) * 320,
        "{} samples for {sent} packets: silence was filled in",
        caller.len()
    );
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_packet_flood_is_cut_to_the_rate_limit_and_timers_keep_running() {
    let mut rig = Rig::start(WebRtcConfig {
        max_audio_packets_per_sec: 100,
        ..fast_config()
    })
    .await;
    let clock = rig.clock.clone();
    // 1000 valid packets a second (20 per 20 ms tick): twenty times what a browser sends.
    let (browser, mut session, _) = rig
        .call_with(BrowserOptions {
            packets_per_frame: 20,
            ..BrowserOptions::default()
        })
        .await;
    let started = clock.now();
    browser.talk(true);
    // A mark acknowledgement is a timer of the call: it must still fire under the flood.
    send_tone(&session, &clock, 5, TurnEpoch::ZERO, 0).await;
    session
        .send(TransportCommand::Mark {
            id: "during-the-flood".to_owned(),
        })
        .await
        .unwrap();
    let mut scratch = Vec::new();
    let played = until_event(&mut session, &mut scratch, |event| match event {
        TransportEvent::MarkPlayed { id } => Some(id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(played, "during-the-flood");
    scratch.extend(drain_caller_audio(&mut session, Duration::from_millis(1_000)).await);
    browser.talk(false);
    let elapsed = clock.now().saturating_duration_since(started);
    let sent = browser.with(|s| s.sent_packets);
    let stats = rig.server.stats();
    println!(
        "flood: {sent} packets sent in {elapsed:?}, {} limited, {} frames to the engine",
        stats.audio_packets_rate_limited, stats.audio_frames_in
    );
    assert!(sent >= 1_000, "{sent}");
    assert!(
        stats.audio_packets_rate_limited > 0,
        "the flood was not limited: {stats:?}"
    );
    // At most the burst (100) plus the rate (100 a second) over the time the flood lasted, with a
    // margin for the packets that were counted but still buffered as frames.
    let ceiling = 100 + (elapsed.as_secs_f64() * 100.0) as u64 + 40;
    assert!(
        stats.audio_frames_in <= ceiling,
        "{} frames reached the engine, at most {ceiling} expected: {stats:?}",
        stats.audio_frames_in
    );
    assert!(
        stats.audio_frames_in + stats.audio_packets_rate_limited + 64 >= sent as u64,
        "packets are accepted or counted as limited: {stats:?}"
    );
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// The outgoing RTP time line and latency

#[tokio::test]
async fn the_outgoing_rtp_time_line_is_contiguous_jumps_after_a_pause_and_never_goes_back() {
    let mut rig = Rig::start(fast_config()).await;
    let clock = rig.clock.clone();
    let (browser, session, _) = rig.call().await;

    // Utterance A: 25 frames, paced by the core in real time.
    send_tone(&session, &clock, 25, TurnEpoch::ZERO, 0).await;
    wait_for_rtp(&browser, 24).await;
    pause(Duration::from_millis(150)).await;
    let a_end = browser.with(|s| s.rtp_log.len());
    // A pause of 500 ms, far over the 60 ms tolerance.
    pause(Duration::from_millis(500)).await;
    // Utterance B.
    send_tone(&session, &clock, 12, TurnEpoch::ZERO, 25).await;
    wait_for_rtp(&browser, a_end + 11).await;
    pause(Duration::from_millis(150)).await;
    let b_end = browser.with(|s| s.rtp_log.len());
    // Utterance C is cut off by a clear; the new epoch speaks right after.
    send_tone(&session, &clock, 40, TurnEpoch::ZERO, 37).await;
    wait_for_rtp(&browser, b_end + 5).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::ZERO.next(),
        })
        .await
        .unwrap();
    pause(Duration::from_millis(400)).await;
    let c_end = browser.with(|s| s.rtp_log.len());
    send_tone(&session, &clock, 12, TurnEpoch::ZERO.next(), 77).await;
    wait_for_rtp(&browser, c_end + 11).await;
    pause(Duration::from_millis(100)).await;

    let log = browser.with(|s| s.rtp_log.clone());
    let a = &log[..a_end];
    let b = &log[a_end..b_end];
    let d = &log[c_end..];
    println!(
        "A {} packets from {}; B {} from {}; D {} from {}",
        a.len(),
        a[0].0,
        b.len(),
        b[0].0,
        d.len(),
        d[0].0
    );

    // Time never goes backwards, over the pause and over the clear.
    assert!(
        log.windows(2).all(|w| w[1].0 > w[0].0),
        "RTP time went backwards or stood still: {log:?}"
    );
    // Within an utterance the time line is contiguous: 960 ticks (20 ms) a packet.
    let steps = |packets: &[(u64, bool)]| -> Vec<u64> {
        packets.windows(2).map(|w| w[1].0 - w[0].0).collect()
    };
    for (name, packets) in [("A", a), ("B", b), ("D", d)] {
        let steps = steps(packets);
        let exact = steps.iter().filter(|&&step| step == 960).count();
        assert!(
            exact * 10 >= steps.len() * 8,
            "utterance {name}: steps {steps:?} are not contiguous"
        );
    }
    // The first packet of an utterance opens a talkspurt (marker bit); the rest do not.
    assert!(a[0].1 && b[0].1 && d[0].1, "utterances start with a marker");
    for (name, packets) in [("A", a), ("B", b), ("D", d)] {
        let markers = packets.iter().filter(|p| p.1).count();
        assert!(markers <= 2, "utterance {name} has {markers} markers");
    }
    // After the 500 ms pause the time jumps by about the pause (24 000 ticks) plus the packet that
    // was due, not by one packet.
    let jump = b[0].0 - a[a.len() - 1].0;
    assert!(
        (19_000..=40_000).contains(&jump),
        "the time line jumped by {jump} ticks over a 500 ms pause"
    );
    // After the clear and a 400 ms silence the new epoch jumps ahead too, with a marker.
    let last_before_d = log[c_end - 1].0;
    let jump = d[0].0 - last_before_d;
    assert!(
        jump >= 15_000,
        "the time line jumped by only {jump} ticks over the clear and 400 ms of silence"
    );
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn first_bot_audio_reaches_the_browser_fast_at_the_median() {
    let mut rig = Rig::start(fast_config()).await;
    let clock = rig.clock.clone();
    let (browser, session, _) = rig.call().await;
    let mut latencies = Vec::new();
    for round in 0..10u64 {
        // Idle between utterances, so each one starts from a fresh talkspurt.
        pause(Duration::from_millis(300)).await;
        browser.with(|s| {
            s.received.clear();
            s.first_packet_at = None;
        });
        let sent_at = clock.now();
        send_tone(&session, &clock, 5, TurnEpoch::ZERO, round * 5).await;
        browser
            .until("the first bot audio", |s| s.first_packet_at.is_some())
            .await;
        let first = browser.with(|s| s.first_packet_at).unwrap();
        latencies.push(first.saturating_duration_since(sent_at));
    }
    latencies.sort();
    let median = latencies[latencies.len() / 2];
    println!(
        "engine command to first decoded packet at the browser: {latencies:?}, median {median:?}"
    );
    // Measured 2-4 ms (debug build, loopback, Windows); the bound is ten times that and more, which
    // leaves room for a loaded runner and the 15.6 ms timer tick, but fails for a regression such
    // as a reordering wait (100 ms by default), a larger pacer delay or a bigger Opus frame. The
    // median shrugs off the odd scheduling hiccup (one round took 33 ms).
    assert!(
        median < Duration::from_millis(50),
        "median {median:?}, all {latencies:?}"
    );
    browser.stop().await;
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// The codec pool's failure paths

/// A decoder that panics once, on its first packet, and works afterwards.
struct PanicOnceDecoder(Box<dyn brisvox_transport::webrtc::opus::OpusDecode>, bool);

impl brisvox_transport::webrtc::opus::OpusDecode for PanicOnceDecoder {
    fn decode(
        &mut self,
        packet: &[u8],
        pcm: &mut [i16],
    ) -> Result<usize, brisvox_transport::webrtc::opus::CodecError> {
        assert!(
            !std::mem::take(&mut self.1),
            "decoder bug (expected by the test)"
        );
        self.0.decode(packet, pcm)
    }

    fn conceal(
        &mut self,
        pcm: &mut [i16],
    ) -> Result<usize, brisvox_transport::webrtc::opus::CodecError> {
        self.0.conceal(pcm)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// The first call's decoder panics once; every other decoder is fine.
#[derive(Debug, Default)]
struct PanicOnceFactory {
    decoders: std::sync::atomic::AtomicUsize,
}

impl OpusFactory for PanicOnceFactory {
    fn encoder(
        &self,
        settings: &OpusSettings,
    ) -> Result<
        Box<dyn brisvox_transport::webrtc::opus::OpusEncode>,
        brisvox_transport::webrtc::opus::CodecError,
    > {
        LibOpusFactory.encoder(settings)
    }

    fn decoder(
        &self,
    ) -> Result<
        Box<dyn brisvox_transport::webrtc::opus::OpusDecode>,
        brisvox_transport::webrtc::opus::CodecError,
    > {
        let first = self
            .decoders
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        Ok(Box::new(PanicOnceDecoder(LibOpusFactory.decoder()?, first)))
    }
}

#[tokio::test]
async fn a_panic_in_a_pool_job_drops_that_frame_and_the_call_and_its_neighbour_go_on() {
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 2,
            queue_capacity: 64,
            name: "test-codec-panic".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let mut rig = Rig::start_with(
        fast_config(),
        WebRtcOptions {
            opus: Arc::new(PanicOnceFactory::default()),
            codec_pool: Some(pool.handle()),
        },
    )
    .await;
    // Call one: its decoder panics on the first packet, inside a pool worker, with the call's
    // codec state locked: the lock is poisoned and must be recovered, not propagated.
    let (first, mut first_session, _) = rig.call().await;
    first.talk(true);
    let mut caller = Vec::new();
    let mut frames = 0;
    until_event(&mut first_session, &mut caller, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 10).then_some(())
    })
    .await;
    let stats = rig.server.stats();
    assert!(
        stats.codec_dropped >= 1,
        "the panicked job's frame is dropped: {stats:?}"
    );
    assert_eq!(stats.call_panics, 0, "a pool panic is not a call panic");
    assert_eq!(rig.server.active_calls(), 1, "the call survived");
    assert!(pool.metrics().panicked >= 1);
    // The encoder side of the same (poisoned) state still works: the bot can speak.
    send_tone(&first_session, &rig.clock, 15, TurnEpoch::ZERO, 0).await;
    first
        .until("bot audio after the panic", |s| {
            s.received.len() >= 48_000 / 5
        })
        .await;
    // Call two, on the same pool, is unaffected.
    let (second, mut second_session, _) = rig.call().await;
    second.talk(true);
    let mut audio = Vec::new();
    let mut frames = 0;
    until_event(&mut second_session, &mut audio, |event| {
        if matches!(event, TransportEvent::Audio(_)) {
            frames += 1;
        }
        (frames >= 10).then_some(())
    })
    .await;
    assert_eq!(rig.server.stats().call_panics, 0);
    assert_eq!(rig.server.active_calls(), 2);
    first.stop().await;
    second.stop().await;
    first_session.close(WAIT).await.unwrap();
    second_session.close(WAIT).await.unwrap();
    rig.server.shutdown(WAIT).await.unwrap();
    drop(rig);
    pool.shutdown();
}

#[tokio::test]
#[allow(clippy::disallowed_methods)] // the blocker occupies a pool worker for real
async fn refused_and_timed_out_pool_jobs_drop_frames_and_the_call_recovers() {
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 1,
            name: "test-codec-busy".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let mut rig = Rig::start_with(
        WebRtcConfig {
            codec_timeout: Duration::from_millis(30),
            ..fast_config()
        },
        WebRtcOptions::with_pool(pool.handle()),
    )
    .await;
    let (browser, mut session, _) = rig.call().await;
    // The only worker is busy for 700 ms: the call's codec jobs wait, time out or find the queue
    // full, and their frames are dropped.
    let blocker = pool.handle();
    let blocked = tokio::spawn(async move {
        blocker
            .run(Duration::from_secs(5), || {
                std::thread::sleep(Duration::from_millis(700));
            })
            .await
    });
    pause(Duration::from_millis(50)).await;
    browser.talk(true);
    let during = drain_caller_audio(&mut session, Duration::from_millis(500)).await;
    let stats = rig.server.stats();
    let metrics = pool.metrics();
    println!("while blocked: {stats:?} {metrics:?}");
    assert!(
        stats.codec_dropped >= 5,
        "frames of timed-out or refused jobs are dropped: {stats:?}"
    );
    assert!(metrics.timed_out + metrics.rejected >= 5, "{metrics:?}");
    assert!(
        during.len() < 16_000 / 4,
        "audio reached the engine while the pool was blocked ({} samples)",
        during.len()
    );
    assert_eq!(rig.server.active_calls(), 1, "the call survived the drops");
    assert_eq!(stats.call_panics, 0);
    // The pool is free again: the same call hears the caller and speaks.
    blocked.await.unwrap().unwrap();
    let after = drain_caller_audio(&mut session, Duration::from_millis(700)).await;
    assert!(
        after.len() >= 16_000 / 4,
        "the call did not recover ({} samples)",
        after.len()
    );
    send_tone(&session, &rig.clock, 15, TurnEpoch::ZERO, 0).await;
    browser
        .until("bot audio after the pool recovered", |s| {
            s.received.len() >= 48_000 / 5
        })
        .await;
    browser.stop().await;
    session.close(WAIT).await.unwrap();
    rig.server.shutdown(WAIT).await.unwrap();
    drop(rig);
    pool.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Session tokens, flood limits

const SESSION_SECRET: &str = "a-session-secret-of-at-least-32-bytes!";

fn session_secrets() -> SessionSecrets {
    SessionSecrets::new(Secret::new(SESSION_SECRET.to_owned()), None).unwrap()
}

fn unix_now() -> u64 {
    SystemClock::shared()
        .wall()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token_spec(sid: &str) -> TokenSpec {
    let now = unix_now();
    TokenSpec {
        audience: Audience::Webrtc,
        sid: sid.to_owned(),
        not_before: now.saturating_sub(5),
        expires: now + 120,
        agent: None,
        max_s: None,
        meta: BTreeMap::new(),
    }
}

fn mint(spec: &TokenSpec) -> String {
    mint_token(&Secret::new(SESSION_SECRET.to_owned()), spec).unwrap()
}

/// A valid offer, for raw posts that never connect.
fn offer_sdp(clock: &SharedClock) -> String {
    let mut rtc = Rtc::builder()
        .set_crypto_provider(crypto())
        .build(clock.now().into_std());
    rtc.add_local_candidate(Candidate::host(([127, 0, 0, 1], 9).into(), "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    offer.to_sdp_string()
}

async fn post_with(addr: SocketAddr, sdp: &str, extra: &[(&str, &str)]) -> HttpResponse {
    let mut headers = vec![("Content-Type", "application/sdp")];
    headers.extend_from_slice(extra);
    http(addr, "POST", "/whip", &headers, sdp.as_bytes()).await
}

#[tokio::test]
async fn session_tokens_gate_the_offer_open_one_call_and_reach_its_metadata() {
    let mut rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);

    // No token, a static-looking string, a token for another listener, an expired one, and one
    // signed with another secret: all 401, and nothing reached the SDP parser.
    let none = post_with(addr, &sdp, &[]).await;
    assert_eq!(none.status, 401);
    assert_eq!(none.headers["www-authenticate"], "Bearer");
    assert_eq!(none.headers["cache-control"], "no-store");
    let junk = post_with(addr, &sdp, &[("Authorization", "Bearer nope")]).await;
    assert_eq!(junk.status, 401);
    let mut other_audience = token_spec("aud");
    other_audience.audience = Audience::Rtvi;
    let wrong_aud = post_with(
        addr,
        &sdp,
        &[(
            "Authorization",
            &format!("Bearer {}", mint(&other_audience)),
        )],
    )
    .await;
    assert_eq!(wrong_aud.status, 401);
    let mut expired = token_spec("exp");
    expired.not_before = unix_now() - 400;
    expired.expires = unix_now() - 200;
    let old = post_with(
        addr,
        &sdp,
        &[("Authorization", &format!("Bearer {}", mint(&expired)))],
    )
    .await;
    assert_eq!(old.status, 401);
    let foreign = mint_token(
        &Secret::new("another-secret-of-at-least-32-bytes!!".to_owned()),
        &token_spec("foreign"),
    )
    .unwrap();
    let signed_elsewhere = post_with(
        addr,
        &sdp,
        &[("Authorization", &format!("Bearer {foreign}"))],
    )
    .await;
    assert_eq!(signed_elsewhere.status, 401);
    // An oversize credential is 413 before anything is parsed.
    let huge = format!("Bearer {}", "a".repeat(5000));
    let big = post_with(addr, &sdp, &[("Authorization", &huge)]).await;
    assert_eq!(big.status, 413);
    assert_eq!(rig.server.active_calls(), 0);
    assert!(rig.server.stats().offers_unauthorized >= 6);

    // A good token opens a call whose metadata carries the session.
    let mut spec = token_spec("visitor-7");
    spec.agent = Some("support".to_owned());
    spec.max_s = Some(600);
    spec.meta.insert("plan".to_owned(), "pro".to_owned());
    let token = mint(&spec);
    let (browser, session, info) = rig
        .call_with(BrowserOptions {
            bearer: Some(token.clone()),
            ..BrowserOptions::default()
        })
        .await;
    assert_eq!(info.metadata["session_id"], "visitor-7");
    assert_eq!(info.metadata["session.agent"], "support");
    assert_eq!(info.metadata["session.max_s"], "600");
    assert_eq!(info.metadata["session.meta.plan"], "pro");
    // The same token again: single use.
    let replay = post_with(addr, &sdp, &[("Authorization", &format!("Bearer {token}"))]).await;
    assert_eq!(replay.status, 401);
    // DELETE needs only the session URL once session tokens are in use.
    assert_eq!(browser.delete().await.status, 200);
    drop(session);
    browser.stop().await;
}

#[tokio::test]
async fn a_refusal_for_capacity_does_not_burn_the_token() {
    let rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        max_calls: 1,
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let first = mint(&token_spec("first"));
    let first_call = post_with(addr, &sdp, &[("Authorization", &format!("Bearer {first}"))]).await;
    assert_eq!(first_call.status, 201);
    let second = mint(&token_spec("second"));
    let header = format!("Bearer {second}");
    let busy = post_with(addr, &sdp, &[("Authorization", &header)]).await;
    assert_eq!(busy.status, 503);
    let id = first_call.headers["location"].clone();
    assert_eq!(http(addr, "DELETE", &id, &[], &[]).await.status, 200);
    wait_for_active(&rig, 0).await;
    let retry = post_with(addr, &sdp, &[("Authorization", &header)]).await;
    assert_eq!(
        retry.status, 201,
        "the token survived the 503: {}",
        retry.body
    );
}

#[tokio::test]
async fn the_static_token_still_works_next_to_session_tokens() {
    let rig = Rig::start(WebRtcConfig {
        bearer_token: Some(Secret::new("dev-token".to_owned())),
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let dev = post_with(addr, &sdp, &[("Authorization", "Bearer dev-token")]).await;
    assert_eq!(dev.status, 201);
    let session = mint(&token_spec("both"));
    let header = format!("Bearer {session}");
    let ok = post_with(addr, &sdp, &[("Authorization", &header)]).await;
    assert_eq!(ok.status, 201);
}

#[tokio::test]
async fn the_token_max_s_caps_the_call() {
    let mut rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let mut spec = token_spec("short-call");
    spec.max_s = Some(1);
    let (browser, mut session, _) = rig
        .call_with(BrowserOptions {
            bearer: Some(mint(&spec)),
            ..BrowserOptions::default()
        })
        .await;
    let reason = tokio::time::timeout(Duration::from_secs(5), stopped(&mut session))
        .await
        .expect("the cap did not end the call");
    assert_eq!(reason, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
    browser.stop().await;
}

/// `POST` with a session token: the status and the `Location`.
async fn offer_status(addr: SocketAddr, sdp: &str, token: &str) -> (u16, String) {
    let header = format!("Bearer {token}");
    let response = post_with(addr, sdp, &[("Authorization", &header)]).await;
    let location = response
        .headers
        .get("location")
        .cloned()
        .unwrap_or_default();
    (response.status, location)
}

#[tokio::test]
async fn a_delete_of_an_unconnected_session_refunds_its_token_once_and_at_once() {
    let rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let token = mint(&token_spec("cycle"));
    let (status, location) = offer_status(addr, &sdp, &token).await;
    assert_eq!(status, 201);
    // While the session is pending the token is spent.
    assert_eq!(offer_status(addr, &sdp, &token).await.0, 401);
    // The DELETE's 200 means the token is usable again: no waiting for the call task.
    assert_eq!(http(addr, "DELETE", &location, &[], &[]).await.status, 200);
    let (status, location) = offer_status(addr, &sdp, &token).await;
    assert_eq!(status, 201, "the refund came with the 200");
    // The second hang-up of the same token is not refunded: one token, at most one retry.
    assert_eq!(http(addr, "DELETE", &location, &[], &[]).await.status, 200);
    wait_for_active(&rig, 0).await;
    assert_eq!(
        offer_status(addr, &sdp, &token).await.0,
        401,
        "a token cannot cycle sessions"
    );
}

#[tokio::test]
async fn the_connect_timeout_does_not_refund_the_token() {
    let rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        connect_timeout: Duration::from_millis(400),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let token = mint(&token_spec("times-out"));
    assert_eq!(offer_status(addr, &sdp, &token).await.0, 201);
    wait_for_active(&rig, 0).await;
    assert_eq!(
        offer_status(addr, &sdp, &token).await.0,
        401,
        "a session that timed out keeps its token spent"
    );
}

#[tokio::test]
async fn a_connected_sessions_delete_close_and_vanishing_never_refund() {
    let mut rig = Rig::start(WebRtcConfig {
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let options_for = |sid: &str| BrowserOptions {
        bearer: Some(mint(&token_spec(sid))),
        ..BrowserOptions::default()
    };

    // DELETE of a connected session.
    let options = options_for("deleted");
    let token = options.bearer.clone().unwrap();
    let (browser, mut session, _) = rig.call_with(options).await;
    assert_eq!(browser.delete().await.status, 200);
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
    assert_eq!(offer_status(addr, &sdp, &token).await.0, 401);
    browser.stop().await;

    // The browser closes the connection (End::PeerClosed).
    let options = options_for("closed");
    let token = options.bearer.clone().unwrap();
    let (browser, mut session, _) = rig.call_with(options).await;
    browser.with(|s| s.close_requested = true);
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
    assert_eq!(offer_status(addr, &sdp, &token).await.0, 401);
    browser.stop().await;

    // The browser vanishes (End::PeerGone), in simulated time as in the test above.
    let options = options_for("vanished");
    let token = options.bearer.clone().unwrap();
    let (browser, mut session, _) = rig.call_with(options).await;
    browser.stop().await;
    tokio::time::pause();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(TransportEvent::Stopped { .. }) = session.recv().await {
                return;
            }
        }
    })
    .await
    .expect("a vanished browser was never noticed");
    tokio::time::resume();
    wait_for_active(&rig, 0).await;
    assert_eq!(offer_status(addr, &sdp, &token).await.0, 401);
}

#[tokio::test]
async fn three_hundred_offers_from_one_address_hit_the_rate_limit_and_the_counters_say_so() {
    let rig = Rig::start(WebRtcConfig {
        offers_per_minute_per_ip: Some(5),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let mut statuses = BTreeMap::<u16, usize>::new();
    let mut retry_after = None;
    for _ in 0..300 {
        // A body that is not SDP: the rate check comes before the (costly) parse.
        let response = post_offer(addr, "v=0").await;
        if response.status == 429 {
            retry_after = response.headers.get("retry-after").cloned();
        }
        *statuses.entry(response.status).or_default() += 1;
    }
    assert_eq!(statuses.get(&400), Some(&5), "{statuses:?}");
    assert_eq!(statuses.get(&429), Some(&295), "{statuses:?}");
    assert_eq!(retry_after.as_deref(), Some("12"));
    let stats = rig.server.stats();
    assert_eq!(stats.offers_rate_limited, 295);
    assert_eq!(stats.offers_refused, 300);
}

#[tokio::test]
async fn three_hundred_offers_from_one_address_hold_at_most_the_per_address_cap() {
    let rig = Rig::start(WebRtcConfig {
        max_calls: 400,
        max_handshaking: Some(400),
        max_calls_per_ip: Some(8),
        offers_per_minute_per_ip: None,
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    let mut created = 0;
    let mut limited = 0;
    for _ in 0..300 {
        match post_offer(addr, &sdp).await.status {
            201 => created += 1,
            429 => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!((created, limited), (8, 292));
    assert_eq!(rig.server.stats().offers_ip_limited, 292);
    assert_eq!(rig.server.active_calls(), 8);
}

#[tokio::test]
async fn forwarding_headers_count_only_from_a_trusted_proxy() {
    let sdp_clock = SystemClock::shared();
    // Untrusted: the header is ignored, so changing it does not change who is counted.
    let rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(2),
        max_handshaking: Some(8),
        offers_per_minute_per_ip: None,
        ..fast_config()
    })
    .await;
    let sdp = offer_sdp(&sdp_clock);
    let mut statuses = Vec::new();
    for n in 0..4 {
        let forwarded = format!("9.9.9.{n}");
        statuses.push(
            post_with(rig.http_addr(), &sdp, &[("X-Forwarded-For", &forwarded)])
                .await
                .status,
        );
    }
    assert_eq!(statuses, [201, 201, 429, 429]);

    // Trusted (the test client is 127.0.0.1): the last untrusted hop is the client.
    let rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(2),
        max_handshaking: Some(8),
        offers_per_minute_per_ip: None,
        trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
        ..fast_config()
    })
    .await;
    let mut statuses = Vec::new();
    for (client, lie) in [
        ("9.9.9.1", "6.6.6.1"),
        ("9.9.9.1", "6.6.6.2"),
        ("9.9.9.1", "6.6.6.3"),
        ("9.9.9.2", "6.6.6.1"),
    ] {
        let forwarded = format!("{lie}, {client}");
        statuses.push(
            post_with(rig.http_addr(), &sdp, &[("X-Forwarded-For", &forwarded)])
                .await
                .status,
        );
    }
    assert_eq!(
        statuses,
        [201, 201, 429, 201],
        "the leftmost (client-written) entries do not choose the identity"
    );
}

#[tokio::test]
async fn only_the_configured_forwarding_header_names_the_client() {
    let sdp_clock = SystemClock::shared();
    let sdp = offer_sdp(&sdp_clock);
    let rig = Rig::start(WebRtcConfig {
        max_calls_per_ip: Some(2),
        max_handshaking: Some(8),
        offers_per_minute_per_ip: None,
        trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
        client_ip_header: brisvox_transport::webrtc::ClientIpHeader::Forwarded,
        ..fast_config()
    })
    .await;
    // The proxy writes only `Forwarded` (the same visitor every time); the visitor plants a
    // different X-Forwarded-For each time. With `Forwarded` configured the plant is ignored.
    let mut statuses = Vec::new();
    for n in 0..4 {
        let planted = format!("6.6.6.{n}");
        statuses.push(
            post_with(
                rig.http_addr(),
                &sdp,
                &[("Forwarded", "for=9.9.9.1"), ("X-Forwarded-For", &planted)],
            )
            .await
            .status,
        );
    }
    assert_eq!(statuses, [201, 201, 429, 429]);
    // Another visitor behind the same proxy has its own budget.
    let other = post_with(rig.http_addr(), &sdp, &[("Forwarded", "for=9.9.9.2")]).await;
    assert_eq!(other.status, 201);
}

#[tokio::test]
async fn one_address_cannot_hold_every_signaling_connection_but_a_trusted_proxy_can() {
    async fn answered_while_holding(rig: &Rig, holders: usize) -> bool {
        let mut held = Vec::new();
        for _ in 0..holders {
            let mut holder = TcpStream::connect(rig.http_addr()).await.unwrap();
            holder.write_all(b"POST /whip HTTP/1.1\r\n").await.unwrap();
            held.push(holder);
        }
        pause(Duration::from_millis(100)).await;
        let mut probe = TcpStream::connect(rig.http_addr()).await.unwrap();
        let _ = probe
            .write_all(b"OPTIONS /whip HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await;
        let mut raw = Vec::new();
        let _ = tokio::time::timeout(WAIT, probe.read_to_end(&mut raw)).await;
        drop(held);
        pause(Duration::from_millis(300)).await;
        raw.starts_with(b"HTTP/1.1 200")
    }
    let config = WebRtcConfig {
        max_http_connections: 16,
        max_http_connections_per_ip: 2,
        request_timeout: Duration::from_secs(3),
        ..fast_config()
    };
    // Two stalled connections from one address use its budget: the third is closed unanswered,
    // although the server has plenty of slots.
    let rig = Rig::start(config.clone()).await;
    assert!(!answered_while_holding(&rig, 2).await);
    assert!(
        answered_while_holding(&rig, 1).await,
        "the budget is per address"
    );
    // A trusted proxy's connections are many visitors': exempt.
    let rig = Rig::start(WebRtcConfig {
        trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
        ..config.clone()
    })
    .await;
    assert!(answered_while_holding(&rig, 2).await);
    // 0 turns the cap off.
    let rig = Rig::start(WebRtcConfig {
        max_http_connections_per_ip: 0,
        ..config
    })
    .await;
    assert!(answered_while_holding(&rig, 4).await);
}

#[tokio::test]
async fn handshaking_calls_have_a_smaller_pool_that_frees_up_and_connected_calls_leave_it() {
    let rig = Rig::start(WebRtcConfig {
        max_calls: 8,
        max_handshaking: Some(2),
        offers_per_minute_per_ip: None,
        max_calls_per_ip: None,
        connect_timeout: Duration::from_millis(800),
        ..fast_config()
    })
    .await;
    let addr = rig.http_addr();
    let sdp = offer_sdp(&rig.clock);
    assert_eq!(post_offer(addr, &sdp).await.status, 201);
    assert_eq!(post_offer(addr, &sdp).await.status, 201);
    let full = post_offer(addr, &sdp).await;
    assert_eq!(full.status, 503, "the handshake pool is full");
    assert_eq!(full.headers["retry-after"], "1");
    assert_eq!(rig.server.stats().offers_handshake_limited, 1);
    // Offers that never connect are abandoned at connect_timeout and free their slots.
    wait_for_active(&rig, 0).await;
    assert_eq!(post_offer(addr, &sdp).await.status, 201);
    wait_for_active(&rig, 0).await;

    // A call that connects leaves the handshake pool: with a pool of one, a connected call does
    // not stop the next offer.
    let mut rig = Rig::start(WebRtcConfig {
        max_calls: 8,
        max_handshaking: Some(1),
        offers_per_minute_per_ip: None,
        ..fast_config()
    })
    .await;
    let (connected, _session, _) = rig.call().await;
    assert_eq!(post_offer(rig.http_addr(), &sdp).await.status, 201);
    connected.stop().await;
}

#[tokio::test]
async fn cors_star_needs_authentication() {
    let open = WebRtcServer::bind(
        WebRtcConfig {
            cors_origin: Some("*".to_owned()),
            ..fast_config()
        },
        SystemClock::shared(),
        &CancellationToken::new(),
    )
    .await;
    assert!(open.is_err());
    let secured = Rig::start(WebRtcConfig {
        cors_origin: Some("*".to_owned()),
        sessions: Some(session_secrets()),
        ..fast_config()
    })
    .await;
    let response = http(secured.http_addr(), "OPTIONS", "/whip", &[], &[]).await;
    assert_eq!(response.headers["access-control-allow-origin"], "*");
}
