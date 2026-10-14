//! SIP over TLS and SDES-SRTP: a real TLS listener on 127.0.0.1, a scripted carrier that
//! speaks raw SIP over `tokio-rustls` and raw (S)RTP over UDP, the engine as a `TransportSession`.
//!
//! The certificates are made with `rcgen` per test run. The SRTP side of the scripted carrier is
//! the crate's own `SrtpTx`/`SrtpRx`, which `src/sip/srtp.rs` pins to the RFC 3711 test vectors and
//! to the independent `webrtc-srtp` implementation, so a wire-format error on either side would
//! show up there first; what these tests add is the negotiation (SDP, policy, transports) and
//! that the bytes on the wire really are encrypted.
#![cfg(feature = "sip")]
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

use brisvox_audio::Codec;
use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_transport::sip::srtp::{MasterKey, SrtpRx, SrtpSuite, SrtpTx};
use brisvox_transport::sip::{
    Cidr, InboundAuth, SipConfig, SipHandle, SipServer, SipStats, SipTlsConfig, SrtpPolicy,
};
use brisvox_transport::{
    SessionConfig, StopReason, Transport, TransportCommand, TransportEvent, TransportSession,
    UtteranceId,
};
use bytes::Bytes;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);

async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

// ---------------------------------------------------------------------------------------------
// Certificates

struct Pki {
    cert_pem: String,
    key_pem: String,
    der: CertificateDer<'static>,
}

/// A self-signed server certificate for 127.0.0.1 and localhost, valid or long expired. The
/// scripted carrier trusts it directly as a trust anchor.
fn pki(expired: bool) -> Pki {
    pki_for(&["localhost", "127.0.0.1"], expired)
}

/// A self-signed server certificate for exactly these names.
fn pki_for(names: &[&str], expired: bool) -> Pki {
    let mut params =
        rcgen::CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
            .unwrap();
    if expired {
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2021, 1, 1);
    }
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    Pki {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        der: cert.der().clone(),
    }
}

fn client_config(trusted: &[CertificateDer<'static>]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in trusted {
        roots.add(cert.clone()).unwrap();
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

fn tls_listener(pki: &Pki) -> SipTlsConfig {
    SipTlsConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        handshake_timeout: Duration::from_secs(2),
        ..SipTlsConfig::new(pki.cert_pem.clone(), pki.key_pem.clone())
    }
}

fn config(pki: &Pki, srtp: SrtpPolicy) -> SipConfig {
    SipConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        send_timeout: Duration::from_secs(2),
        ack_timeout: Duration::from_secs(3),
        hangup_timeout: Duration::from_secs(2),
        tls: Some(tls_listener(pki)),
        srtp,
        ..SipConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// SIP text

#[derive(Debug, Clone)]
struct Msg {
    start: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Msg {
    fn parse(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.split("\r\n");
        let start = lines.next().unwrap_or("").to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| {
                let name = match name.trim().to_ascii_lowercase().as_str() {
                    "v" => "via".to_owned(),
                    "f" => "from".to_owned(),
                    "t" => "to".to_owned(),
                    "i" => "call-id".to_owned(),
                    "m" => "contact".to_owned(),
                    "c" => "content-type".to_owned(),
                    "l" => "content-length".to_owned(),
                    other => other.to_owned(),
                };
                (name, value.trim().to_owned())
            })
            .collect();
        Self {
            start,
            headers,
            body: body.as_bytes().to_vec(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn status(&self) -> Option<u16> {
        self.start
            .strip_prefix("SIP/2.0 ")?
            .split(' ')
            .next()?
            .parse()
            .ok()
    }

    fn method(&self) -> Option<&str> {
        if self.start.starts_with("SIP/2.0") {
            return None;
        }
        self.start.split(' ').next()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn cseq_method(&self) -> String {
        self.header("cseq")
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1
            .to_owned()
    }

    fn to_tag(&self) -> Option<String> {
        self.header("to")?
            .split(';')
            .find_map(|p| p.trim().strip_prefix("tag="))
            .map(str::to_owned)
    }
}

fn response(request: &Msg, code: u16, reason: &str) -> String {
    let mut out = format!("SIP/2.0 {code} {reason}\r\n");
    for via in request.all("via") {
        out.push_str(&format!("Via: {via}\r\n"));
    }
    out.push_str(&format!("From: {}\r\n", request.header("from").unwrap()));
    out.push_str(&format!("To: {}\r\n", request.header("to").unwrap()));
    out.push_str(&format!(
        "Call-ID: {}\r\n",
        request.header("call-id").unwrap()
    ));
    out.push_str(&format!("CSeq: {}\r\n", request.header("cseq").unwrap()));
    out.push_str("Content-Length: 0\r\n\r\n");
    out
}

// ---------------------------------------------------------------------------------------------
// SDP

fn plain_sdp(rtp_port: u16) -> String {
    format!(
        "v=0\r\no=peer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {rtp_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=sendrecv\r\n"
    )
}

/// A secure offer with these `a=crypto` lines (already formatted, without the `a=crypto:` prefix).
fn secure_sdp(rtp_port: u16, crypto: &[String]) -> String {
    let mut sdp = format!(
        "v=0\r\no=peer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {rtp_port} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\n"
    );
    for line in crypto {
        sdp.push_str(&format!("a=crypto:{line}\r\n"));
    }
    sdp.push_str("a=sendrecv\r\n");
    sdp
}

fn crypto_line(tag: u32, suite: SrtpSuite, key: &MasterKey) -> String {
    format!("{tag} {} inline:{}", suite.name(), key.to_base64())
}

fn sdp_rtp_addr(sdp: &str) -> SocketAddr {
    let ip = sdp
        .lines()
        .find_map(|l| l.strip_prefix("c=IN IP4 "))
        .unwrap()
        .trim();
    let port = sdp
        .lines()
        .find_map(|l| l.strip_prefix("m=audio "))
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    format!("{ip}:{port}").parse().unwrap()
}

/// The answer's `a=crypto` line: (tag, suite name, key).
fn answer_crypto(sdp: &str) -> Option<(u32, String, MasterKey)> {
    let line = sdp.lines().find_map(|l| l.strip_prefix("a=crypto:"))?;
    let mut parts = line.split_ascii_whitespace();
    let tag = parts.next()?.parse().ok()?;
    let suite = parts.next()?.to_owned();
    let key = MasterKey::from_base64(parts.next()?.strip_prefix("inline:")?).ok()?;
    Some((tag, suite, key))
}

// ---------------------------------------------------------------------------------------------
// RTP

fn rtp_bytes(sequence: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80, 0x00];
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn ulaw(sample: i16, samples: usize) -> Vec<u8> {
    let pcm: Vec<i16> = vec![sample; samples];
    let mut out = vec![0u8; samples];
    Codec::Ulaw.encode_samples(&pcm, &mut out).unwrap();
    out
}

fn bot_frame(clock: &SharedClock, seq: u64, sample: i16) -> TransportCommand {
    let pcm: Vec<u8> = std::iter::repeat_n(sample.to_le_bytes(), 160)
        .flatten()
        .collect();
    let epoch = TurnEpoch::new(0);
    let frame = AudioFrame::new(
        Bytes::from(pcm),
        SampleRate::Hz8000,
        clock.now(),
        seq,
        epoch,
    )
    .unwrap();
    TransportCommand::Audio {
        epoch,
        utterance: UtteranceId::new(1),
        frame,
    }
}

async fn recv_datagram(socket: &UdpSocket, wait: Duration) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        Ok(Err(error)) => panic!("rtp socket error: {error}"),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The engine side

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: SipServer,
}

impl Rig {
    async fn start(config: SipConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = SipServer::bind(config, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    fn tls_addr(&self) -> SocketAddr {
        self.server.handle().tls_addr().expect("a TLS listener")
    }

    fn handle(&self) -> SipHandle {
        self.server.handle()
    }

    async fn accept(&mut self) -> TransportSession {
        let cancel = CancellationToken::new();
        let link = tokio::time::timeout(WAIT, self.server.accept(&cancel))
            .await
            .expect("no call within the wait")
            .unwrap()
            .expect("server closed");
        TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap()
    }

    async fn stats_where(&self, wanted: impl Fn(&SipStats) -> bool) -> SipStats {
        let wait = async {
            loop {
                let stats = self.server.stats();
                if wanted(&stats) {
                    return stats;
                }
                pause(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("counters never matched: {:?}", self.server.stats()))
    }
}

async fn next_event(
    session: &mut TransportSession,
    wanted: impl Fn(&TransportEvent) -> bool,
) -> TransportEvent {
    let wait = async {
        loop {
            let event = session.recv().await.expect("session ended");
            if wanted(&event) {
                return event;
            }
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("no matching event within the wait")
}

async fn next_audio(session: &mut TransportSession) -> AudioFrame {
    match next_event(session, |e| matches!(e, TransportEvent::Audio(_))).await {
        TransportEvent::Audio(frame) => frame,
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted carrier over TLS

struct TlsPeer {
    stream: TlsStream<TcpStream>,
    rtp: UdpSocket,
    server: SocketAddr,
    local: SocketAddr,
    call_id: String,
    cseq: u32,
    branches: u32,
    to_tag: Option<String>,
    buffer: Vec<u8>,
}

impl TlsPeer {
    async fn connect(
        server: SocketAddr,
        config: Arc<rustls::ClientConfig>,
    ) -> std::io::Result<Self> {
        let tcp = TcpStream::connect(server).await?;
        let local = tcp.local_addr()?;
        let stream = TlsConnector::from(config)
            .connect(ServerName::IpAddress(server.ip().into()), tcp)
            .await?;
        Ok(Self {
            stream,
            rtp: UdpSocket::bind("127.0.0.1:0").await?,
            server,
            local,
            call_id: format!("tls-call-{}@peer.test", local.port()),
            cseq: 0,
            branches: 0,
            to_tag: None,
            buffer: Vec::new(),
        })
    }

    fn rtp_port(&self) -> u16 {
        self.rtp.local_addr().unwrap().port()
    }

    fn request(
        &mut self,
        method: &str,
        cseq: u32,
        extra: &[(&str, String)],
        body: Option<&str>,
    ) -> String {
        self.branches += 1;
        let mut to = "<sip:agent@example.com>".to_owned();
        if let Some(tag) = &self.to_tag {
            to.push_str(&format!(";tag={tag}"));
        }
        let mut out = format!(
            "{method} sip:agent@{};transport=tls SIP/2.0\r\nVia: SIP/2.0/TLS {};branch=z9hG4bK-tls-{}-{}\r\n\
             Max-Forwards: 70\r\nFrom: \"Alice\" <sip:alice@example.com>;tag=ft{}\r\nTo: {to}\r\n\
             Call-ID: {}\r\nCSeq: {cseq} {method}\r\nContact: <sip:alice@{};transport=tls>\r\n",
            self.server,
            self.local,
            self.local.port(),
            self.branches,
            self.local.port(),
            self.call_id,
            self.local
        );
        for (name, value) in extra {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        match body {
            Some(body) => out.push_str(&format!(
                "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )),
            None => out.push_str("Content-Length: 0\r\n\r\n"),
        }
        out
    }

    async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    /// The next SIP message on the connection, `None` when the server closed it or nothing came
    /// within `wait`.
    async fn recv_within(&mut self, wait: Duration) -> Option<Msg> {
        #[allow(clippy::disallowed_methods)] // test harness, real time
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buffer[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                if self.buffer.len() >= end + 4 + length {
                    let rest = self.buffer.split_off(end + 4 + length);
                    let message = std::mem::replace(&mut self.buffer, rest);
                    return Some(Msg::parse(&message));
                }
            }
            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await;
            match read {
                Ok(Ok(n)) if n > 0 => self.buffer.extend_from_slice(&chunk[..n]),
                _ => return None,
            }
        }
    }

    async fn recv(&mut self) -> Msg {
        self.recv_within(WAIT).await.expect("no SIP message")
    }

    async fn final_response(&mut self) -> Msg {
        loop {
            let msg = self.recv().await;
            if msg.status().is_some_and(|s| s >= 200) {
                return msg;
            }
        }
    }

    /// INVITE with `sdp`; the final response. On 200 the ACK is sent and the To tag is kept.
    async fn call(&mut self, sdp: &str, extra: &[(&str, String)]) -> Msg {
        self.cseq += 1;
        let text = self.request("INVITE", self.cseq, extra, Some(sdp));
        self.send(&text).await;
        let answer = self.final_response().await;
        let tag = answer.to_tag();
        if answer.status() == Some(200) {
            self.to_tag = tag.clone();
        }
        let saved = self.to_tag.take();
        self.to_tag = tag;
        let ack = self.request("ACK", self.cseq, &[], None);
        self.to_tag = saved;
        self.send(&ack).await;
        answer
    }

    async fn bye(&mut self) -> Msg {
        self.cseq += 1;
        let text = self.request("BYE", self.cseq, &[], None);
        self.send(&text).await;
        loop {
            let msg = self.final_response().await;
            if msg.cseq_method() == "BYE" {
                return msg;
            }
        }
    }
}

async fn good_peer(rig: &Rig, pki: &Pki) -> TlsPeer {
    TlsPeer::connect(
        rig.tls_addr(),
        client_config(std::slice::from_ref(&pki.der)),
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// SIPS

#[tokio::test]
async fn a_call_over_tls_is_served_with_a_tls_contact_and_the_bye_returns_on_the_connection() {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, SrtpPolicy::Off)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let answer = peer.call(&plain_sdp(peer.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    // The Contact names TLS and the TLS port: the peer's in-dialog requests go there.
    let contact = answer.header("contact").unwrap().to_ascii_lowercase();
    assert!(contact.contains("transport=tls"), "{contact}");
    assert!(
        contact.contains(&rig.tls_addr().port().to_string()),
        "{contact}"
    );
    assert!(
        answer.text().contains("m=audio") && answer.text().contains("RTP/AVP"),
        "plain RTP when SRTP is off: {}",
        answer.text()
    );
    assert!(!answer.text().contains("a=crypto"));

    let mut session = rig.accept().await;
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    let TransportEvent::Started(info) = started else {
        unreachable!()
    };
    assert_eq!(
        info.metadata.get("signaling").map(String::as_str),
        Some("tls")
    );
    assert_eq!(info.metadata.get("srtp").map(String::as_str), Some("none"));

    // Audio works as on any transport.
    let dest = sdp_rtp_addr(&answer.text());
    peer.rtp
        .send_to(&rtp_bytes(1, 0, 5, &ulaw(200, 160)), dest)
        .await
        .unwrap();
    assert_eq!(next_audio(&mut session).await.sample_count(), 160);

    // The engine hangs up: the BYE arrives on the same TLS connection.
    session.send(TransportCommand::Stop).await.unwrap();
    let bye = loop {
        let msg = peer.recv().await;
        if msg.method() == Some("BYE") {
            break msg;
        }
    };
    assert!(
        bye.all("via")[0].starts_with("SIP/2.0/TLS"),
        "the BYE names TLS in its Via: {bye:?}"
    );
    peer.send(&response(&bye, 200, "OK")).await;
    let reason = loop {
        if let TransportEvent::Stopped { reason } = next_event(&mut session, |e| {
            matches!(e, TransportEvent::Stopped { .. })
        })
        .await
        {
            break reason;
        }
    };
    assert_eq!(reason, StopReason::Requested);
    let stats = rig.server.stats();
    assert_eq!(stats.tls_connections, 1);
    assert_eq!(stats.srtp_calls, 0);
}

#[tokio::test]
async fn a_peer_bye_over_tls_ends_the_call() {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, SrtpPolicy::Off)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let answer = peer.call(&plain_sdp(peer.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(200));
    let mut session = rig.accept().await;
    let ok = peer.bye().await;
    assert_eq!(ok.status(), Some(200));
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::Stopped { .. })
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::Stopped {
            reason: StopReason::PeerHangup
        }
    );
}

#[tokio::test]
async fn digest_authentication_applies_on_tls_too() {
    let pki = pki(false);
    let cfg = SipConfig {
        inbound_auth: Some(InboundAuth {
            realm: "brisvox.test".to_owned(),
            users: vec![("carrier".to_owned(), Secret::new("trunk-secret".to_owned()))],
            nonce_ttl: Duration::from_secs(10),
        }),
        ..config(&pki, SrtpPolicy::Off)
    };
    let rig = Rig::start(cfg).await;
    let mut peer = good_peer(&rig, &pki).await;
    let answer = peer.call(&plain_sdp(peer.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(401), "{answer:?}");
    assert!(answer.header("www-authenticate").is_some());
}

#[tokio::test]
async fn a_client_that_does_not_trust_the_certificate_fails_and_the_server_goes_on() {
    let pki_server = pki(false);
    let other = pki(false);
    let rig = Rig::start(config(&pki_server, SrtpPolicy::Off)).await;

    // Unknown issuer: the client refuses the handshake.
    let wrong = TlsPeer::connect(
        rig.tls_addr(),
        client_config(std::slice::from_ref(&other.der)),
    )
    .await;
    assert!(
        wrong.is_err(),
        "a certificate nobody vouches for was accepted"
    );
    // The server saw the handshake end badly, and still serves a client that trusts it.
    rig.stats_where(|s| s.tls_handshake_failures >= 1).await;
    let mut peer = good_peer(&rig, &pki_server).await;
    let answer = peer.call(&plain_sdp(peer.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
}

#[tokio::test]
async fn an_expired_server_certificate_is_refused_by_the_client() {
    let expired = pki(true);
    let rig = Rig::start(config(&expired, SrtpPolicy::Off)).await;
    let result = TlsPeer::connect(
        rig.tls_addr(),
        client_config(std::slice::from_ref(&expired.der)),
    )
    .await;
    let error = result.err().expect("an expired certificate was accepted");
    assert!(
        error.to_string().to_ascii_lowercase().contains("expired"),
        "{error}"
    );
    rig.stats_where(|s| s.tls_handshake_failures >= 1).await;
}

#[tokio::test]
async fn plain_tcp_and_garbage_on_the_tls_port_never_reach_the_sip_layer() {
    let pki = pki(false);
    let rig = Rig::start(config(&pki, SrtpPolicy::Off)).await;
    // A cleartext INVITE sent to the SIPS port: the handshake fails, nothing is answered.
    let mut tcp = TcpStream::connect(rig.tls_addr()).await.unwrap();
    tcp.write_all(
        b"INVITE sip:agent@example.com SIP/2.0\r\nVia: SIP/2.0/TCP 127.0.0.1:1;branch=z9hG4bK1\r\n\
          From: <sip:a@x>;tag=1\r\nTo: <sip:b@x>\r\nCall-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    )
    .await
    .unwrap();
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(WAIT, tcp.read(&mut buf))
        .await
        .unwrap();
    // A TLS alert or a close, never a SIP response.
    if let Ok(n) = read {
        assert!(!String::from_utf8_lossy(&buf[..n]).contains("SIP/2.0"));
    }
    rig.stats_where(|s| s.tls_handshake_failures >= 1).await;
    assert_eq!(rig.server.stats().calls_inbound, 0);
}

#[tokio::test]
async fn a_silent_client_is_dropped_at_the_handshake_deadline() {
    let pki = pki(false);
    let cfg = SipConfig {
        tls: Some(SipTlsConfig {
            handshake_timeout: Duration::from_millis(300),
            ..tls_listener(&pki)
        }),
        ..config(&pki, SrtpPolicy::Off)
    };
    let rig = Rig::start(cfg).await;
    let mut tcp = TcpStream::connect(rig.tls_addr()).await.unwrap();
    // Connected, never says anything: the server closes after the deadline.
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), tcp.read(&mut buf))
        .await
        .expect("the server kept a silent connection open");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    rig.stats_where(|s| s.tls_handshake_failures >= 1).await;
}

#[tokio::test]
async fn connection_limits_per_address_hold_and_free_up() {
    let pki = pki(false);
    let cfg = SipConfig {
        tls: Some(SipTlsConfig {
            max_connections_per_ip: 2,
            ..tls_listener(&pki)
        }),
        ..config(&pki, SrtpPolicy::Off)
    };
    let rig = Rig::start(cfg).await;
    let client = client_config(std::slice::from_ref(&pki.der));
    let first = TlsPeer::connect(rig.tls_addr(), client.clone())
        .await
        .unwrap();
    let second = TlsPeer::connect(rig.tls_addr(), client.clone())
        .await
        .unwrap();
    // The third is closed before any handshake.
    let third = TlsPeer::connect(rig.tls_addr(), client.clone()).await;
    assert!(
        third.is_err(),
        "a third connection from one address was served"
    );
    rig.stats_where(|s| s.tls_rejected >= 1).await;
    // Closing one frees the slot.
    drop(first);
    let mut tries = 0;
    let again = loop {
        match TlsPeer::connect(rig.tls_addr(), client.clone()).await {
            Ok(peer) => break peer,
            Err(error) if tries < 50 => {
                tries += 1;
                let _ = error;
                pause(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("the slot never came back: {error}"),
        }
    };
    drop((second, again));
}

#[tokio::test]
async fn the_source_acl_applies_before_the_tls_handshake() {
    let pki = pki(false);
    let cfg = SipConfig {
        allowed_sources: vec!["192.0.2.0/24".parse::<Cidr>().unwrap()],
        ..config(&pki, SrtpPolicy::Off)
    };
    let rig = Rig::start(cfg).await;
    let denied = TlsPeer::connect(
        rig.tls_addr(),
        client_config(std::slice::from_ref(&pki.der)),
    )
    .await;
    assert!(
        denied.is_err(),
        "a source outside the ACL got a TLS session"
    );
    rig.stats_where(|s| s.tls_rejected >= 1).await;
    assert_eq!(
        rig.server.stats().tls_handshake_failures,
        0,
        "no handshake was attempted"
    );
}

// ---------------------------------------------------------------------------------------------
// SRTP

#[tokio::test]
async fn an_sdes_call_over_tls_carries_encrypted_audio_both_ways() {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, SrtpPolicy::Optional)).await;
    let mut peer = good_peer(&rig, &pki).await;

    let peer_key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &peer_key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let text = answer.text();
    assert!(text.contains("RTP/SAVP"), "{text}");
    let (tag, suite, server_key) = answer_crypto(&text).expect("an a=crypto line in the answer");
    assert_eq!((tag, suite.as_str()), (1, "AES_CM_128_HMAC_SHA1_80"));
    assert_ne!(server_key, peer_key, "each direction has its own key");

    let mut session = rig.accept().await;
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    let TransportEvent::Started(info) = started else {
        unreachable!()
    };
    assert_eq!(
        info.metadata.get("srtp").map(String::as_str),
        Some("AES_CM_128_HMAC_SHA1_80")
    );
    let dest = sdp_rtp_addr(&text);
    let mut peer_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &peer_key).unwrap();
    let mut peer_rx = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_80, &server_key).unwrap();

    // Peer -> engine: an SRTP packet is decrypted and played.
    let plain = rtp_bytes(1, 0, 0x77, &ulaw(300, 160));
    let mut wire = plain.clone();
    peer_tx.protect(&mut wire).unwrap();
    assert_ne!(
        wire[12..172],
        plain[12..],
        "the peer's payload is encrypted on the wire"
    );
    peer.rtp.send_to(&wire, dest).await.unwrap();
    let frame = next_audio(&mut session).await;
    let mut expected = vec![0u8; 320];
    Codec::Ulaw.decode(&ulaw(300, 160), &mut expected).unwrap();
    assert_eq!(frame.data().as_ref(), expected.as_slice());

    // The same packet in the clear, and a forged one, are not played.
    peer.rtp
        .send_to(&rtp_bytes(2, 160, 0x77, &ulaw(300, 160)), dest)
        .await
        .unwrap();
    let mut forged = rtp_bytes(3, 320, 0x77, &ulaw(300, 160));
    peer_tx.protect(&mut forged).unwrap();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    peer.rtp.send_to(&forged, dest).await.unwrap();
    rig.stats_where(|s| s.srtp_packets_rejected >= 2).await;

    // Engine -> peer: the packet on the wire is not the plain G.711 payload, and decrypts to it.
    session.send(bot_frame(&rig.clock, 0, 500)).await.unwrap();
    let packet = recv_datagram(&peer.rtp, WAIT)
        .await
        .expect("no RTP from the bot");
    assert_eq!(
        packet.len(),
        12 + 160 + 10,
        "header, payload and the 80-bit tag"
    );
    let plain_payload = ulaw(500, 160);
    assert_ne!(
        packet[12..172],
        plain_payload[..],
        "the bot's payload is encrypted on the wire"
    );
    let mut received = packet.clone();
    let n = peer_rx.unprotect(&mut received).unwrap();
    assert_eq!(&received[12..n], plain_payload.as_slice());
    // A replay of that packet is refused by the peer's receiver.
    let mut again = packet;
    assert!(peer_rx.unprotect(&mut again).is_err());

    assert_eq!(rig.server.stats().srtp_calls, 1);
    peer.bye().await;
}

#[tokio::test]
async fn the_second_crypto_line_is_taken_when_the_first_is_unsupported() {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, SrtpPolicy::Required)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let key = MasterKey::generate().unwrap();
    let key32 = MasterKey::generate().unwrap();
    let lines = vec![
        // An AEAD suite and an MKI line: not supported, ignored.
        format!("1 AEAD_AES_128_GCM inline:{}", key.to_base64()),
        format!(
            "2 AES_CM_128_HMAC_SHA1_80 inline:{}|2^31|1:4",
            key.to_base64()
        ),
        crypto_line(3, SrtpSuite::AesCm128HmacSha1_32, &key32),
    ];
    let answer = peer.call(&secure_sdp(peer.rtp_port(), &lines), &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let (tag, suite, server_key) = answer_crypto(&answer.text()).unwrap();
    assert_eq!((tag, suite.as_str()), (3, "AES_CM_128_HMAC_SHA1_32"));
    // The 32-bit tag is in use: the bot's packet is 4 bytes longer than the payload only.
    let mut session = rig.accept().await;
    let dest = sdp_rtp_addr(&answer.text());
    let mut peer_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_32, &key32).unwrap();
    let mut wire = rtp_bytes(1, 0, 9, &ulaw(100, 160));
    peer_tx.protect(&mut wire).unwrap();
    peer.rtp.send_to(&wire, dest).await.unwrap();
    assert_eq!(next_audio(&mut session).await.sample_count(), 160);
    session.send(bot_frame(&rig.clock, 0, 100)).await.unwrap();
    let packet = recv_datagram(&peer.rtp, WAIT).await.unwrap();
    assert_eq!(packet.len(), 12 + 160 + 4);
    let mut rx = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_32, &server_key).unwrap();
    let mut received = packet;
    let n = rx.unprotect(&mut received).unwrap();
    assert_eq!(&received[12..n], ulaw(100, 160).as_slice());
}

#[tokio::test]
async fn with_both_profiles_on_offer_srtp_is_preferred_over_tls_and_plain_is_the_fallback() {
    let pki = pki(false);
    let rig = Rig::start(config(&pki, SrtpPolicy::Optional)).await;
    // Two audio sections: plain first, secure second. Secure wins.
    let mut peer = good_peer(&rig, &pki).await;
    let key = MasterKey::generate().unwrap();
    let both = format!(
        "{}m=audio {} RTP/SAVP 0\r\na=rtpmap:0 PCMU/8000\r\na=crypto:{}\r\n",
        plain_sdp(peer.rtp_port()),
        peer.rtp_port(),
        crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &key)
    );
    let answer = peer.call(&both, &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let text = answer.text();
    assert!(text.contains("RTP/SAVP"), "{text}");
    assert!(answer_crypto(&text).is_some());
    // The plain section is declined (port 0) in the answer.
    assert!(text.contains("m=audio 0 RTP/AVP"), "{text}");
    peer.bye().await;

    // A plain-only offer is still served under `optional`.
    let mut plain = good_peer(&rig, &pki).await;
    let answer = plain.call(&plain_sdp(plain.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    assert!(!answer.text().contains("a=crypto"));
}

#[tokio::test]
async fn required_srtp_refuses_a_plain_offer_with_488() {
    let pki = pki(false);
    let rig = Rig::start(config(&pki, SrtpPolicy::Required)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let answer = peer.call(&plain_sdp(peer.rtp_port()), &[]).await;
    assert_eq!(answer.status(), Some(488), "{answer:?}");
    // An RTP/SAVP section without a usable crypto line is refused as well.
    let mut second = good_peer(&rig, &pki).await;
    let bare = secure_sdp(second.rtp_port(), &[]);
    let answer = second.call(&bare, &[]).await;
    assert_eq!(answer.status(), Some(488), "{answer:?}");
    let stats = rig.server.stats();
    assert_eq!(stats.rejected_insecure, 2);
    assert_eq!(stats.calls_inbound, 0);
}

#[tokio::test]
async fn srtp_off_refuses_a_secure_only_offer() {
    let pki = pki(false);
    let rig = Rig::start(config(&pki, SrtpPolicy::Off)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(488), "{answer:?}");
}

/// A raw UDP carrier talking to the plain SIP port of a server that also has a TLS listener.
async fn udp_invite(sip_addr: SocketAddr, sdp: &str) -> Msg {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let invite = format!(
        "INVITE sip:agent@{sip_addr} SIP/2.0\r\nVia: SIP/2.0/UDP {local};branch=z9hG4bK-udp-{}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=u{}\r\nTo: <sip:agent@example.com>\r\nCall-ID: udp-{}@peer.test\r\n\
         CSeq: 1 INVITE\r\nContact: <sip:alice@{local}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        local.port(),
        local.port(),
        local.port(),
        sdp.len()
    );
    sock.send_to(invite.as_bytes(), sip_addr).await.unwrap();
    loop {
        let mut buf = vec![0u8; 8192];
        let (n, _) = tokio::time::timeout(WAIT, sock.recv_from(&mut buf))
            .await
            .expect("no answer to the UDP INVITE")
            .unwrap();
        let msg = Msg::parse(&buf[..n]);
        if msg.status().is_some_and(|s| s >= 200) {
            return msg;
        }
    }
}

#[tokio::test]
async fn sdes_keys_offered_over_udp_are_refused_and_never_used() {
    let pki = pki(false);
    for policy in [SrtpPolicy::Optional, SrtpPolicy::Required] {
        let rig = Rig::start(config(&pki, policy)).await;
        let key = MasterKey::generate().unwrap();
        let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = rtp.local_addr().unwrap().port();
        let offer = secure_sdp(
            port,
            &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &key)],
        );
        let answer = udp_invite(rig.server.local_addr(), &offer).await;
        assert_eq!(answer.status(), Some(488), "{policy:?}: {answer:?}");
        assert!(
            !answer.text().contains("a=crypto"),
            "a key was answered on cleartext signaling"
        );
        assert_eq!(rig.server.stats().rejected_insecure, 1);
    }
}

#[tokio::test]
async fn over_udp_a_plain_offer_is_served_under_optional_and_refused_under_required() {
    let pki = pki(false);
    let optional = Rig::start(config(&pki, SrtpPolicy::Optional)).await;
    let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let answer = udp_invite(
        optional.server.local_addr(),
        &plain_sdp(rtp.local_addr().unwrap().port()),
    )
    .await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");

    let required = Rig::start(config(&pki, SrtpPolicy::Required)).await;
    let answer = udp_invite(
        required.server.local_addr(),
        &plain_sdp(rtp.local_addr().unwrap().port()),
    )
    .await;
    assert_eq!(answer.status(), Some(488), "{answer:?}");
}

#[tokio::test]
async fn a_reinvite_cannot_downgrade_an_srtp_call() {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, SrtpPolicy::Optional)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(200));
    let _session = rig.accept().await;

    // A re-INVITE with plain RTP is refused (488) and the call stays up.
    peer.cseq += 1;
    let reinvite = peer.request("INVITE", peer.cseq, &[], Some(&plain_sdp(peer.rtp_port())));
    peer.send(&reinvite).await;
    let refused = peer.final_response().await;
    assert_eq!(refused.status(), Some(488), "{refused:?}");
    assert_eq!(rig.handle().active_calls(), 1);

    // The same offer again (a refresh) keeps our key: nothing was re-keyed.
    peer.cseq += 1;
    let refresh = peer.request(
        "INVITE",
        peer.cseq,
        &[],
        Some(&secure_sdp(
            peer.rtp_port(),
            &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &key)],
        )),
    );
    peer.send(&refresh).await;
    let same = peer.final_response().await;
    assert_eq!(same.status(), Some(200), "{same:?}");
    let ack = peer.request("ACK", peer.cseq, &[], None);
    peer.send(&ack).await;
    let (_, _, key_after_refresh) = answer_crypto(&same.text()).unwrap();
    let (_, _, first_server_key) = answer_crypto(&answer.text()).unwrap();
    assert_eq!(key_after_refresh, first_server_key);

    // A re-INVITE with a fresh key under the same suite is accepted; the answer carries a FRESH
    // key of ours (a leaked answer must not expose the audio before it).
    let new_key = MasterKey::generate().unwrap();
    peer.cseq += 1;
    let reinvite = peer.request(
        "INVITE",
        peer.cseq,
        &[],
        Some(&secure_sdp(
            peer.rtp_port(),
            &[crypto_line(7, SrtpSuite::AesCm128HmacSha1_80, &new_key)],
        )),
    );
    peer.send(&reinvite).await;
    let accepted = peer.final_response().await;
    assert_eq!(accepted.status(), Some(200), "{accepted:?}");
    let ack = peer.request("ACK", peer.cseq, &[], None);
    peer.send(&ack).await;
    let (tag, _, server_key_again) = answer_crypto(&accepted.text()).unwrap();
    assert_eq!(tag, 7);
    assert_ne!(server_key_again, first_server_key);
    // And another re-key brings another key.
    peer.cseq += 1;
    let third = peer.request(
        "INVITE",
        peer.cseq,
        &[],
        Some(&secure_sdp(
            peer.rtp_port(),
            &[crypto_line(
                8,
                SrtpSuite::AesCm128HmacSha1_80,
                &MasterKey::generate().unwrap(),
            )],
        )),
    );
    peer.send(&third).await;
    let third = peer.final_response().await;
    assert_eq!(third.status(), Some(200), "{third:?}");
    let ack = peer.request("ACK", peer.cseq, &[], None);
    peer.send(&ack).await;
    let (_, _, third_key) = answer_crypto(&third.text()).unwrap();
    assert_ne!(third_key, server_key_again);
    assert_ne!(third_key, first_server_key);
}

// ---------------------------------------------------------------------------------------------
// Key material

/// A writer that keeps everything the subscriber prints.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn key_material_never_reaches_brisvoxs_logs_or_debug_output() {
    let capture = Capture::default();
    // The whole process logs at TRACE into `capture`: every target, brisvox's and the stack's.
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_target(true)
            .with_writer(capture.clone())
            .finish(),
    )
    .unwrap();

    let pki = pki(false);
    let cfg = config(&pki, SrtpPolicy::Optional);
    // Debug output of the configuration never holds the private key.
    let debug = format!("{cfg:?}");
    let key_body: String = pki
        .key_pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    assert!(!debug.contains(&key_body));
    assert!(!debug.contains("PRIVATE KEY"));

    let mut rig = Rig::start(cfg).await;
    let mut peer = good_peer(&rig, &pki).await;
    let peer_key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &peer_key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(200));
    let (_, _, server_key) = answer_crypto(&answer.text()).unwrap();
    let mut session = rig.accept().await;
    let dest = sdp_rtp_addr(&answer.text());
    let mut tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &peer_key).unwrap();
    for seq in 1..5u16 {
        let mut wire = rtp_bytes(seq, u32::from(seq) * 160, 3, &ulaw(10, 160));
        tx.protect(&mut wire).unwrap();
        peer.rtp.send_to(&wire, dest).await.unwrap();
        next_audio(&mut session).await;
    }
    peer.bye().await;
    pause(Duration::from_millis(200)).await;

    let logs = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    assert!(!logs.is_empty(), "the capture saw nothing");
    // The stack (`rsipstack`) logs whole SIP messages at info: that is why the server binary
    // holds its target at warn whatever RUST_LOG says (`telemetry::sip_stack_log_allowed`). What
    // brisvox itself writes must never hold a key, whatever the level.
    // A record starts with its timestamp; the stack's messages continue over several lines.
    let mut records: Vec<String> = Vec::new();
    for line in logs.lines() {
        if line.starts_with(|c: char| c.is_ascii_digit()) || records.is_empty() {
            records.push(String::new());
        }
        if let Some(record) = records.last_mut() {
            record.push_str(line);
            record.push('\n');
        }
    }
    let own: String = records
        .iter()
        .filter(|record| !record.contains("rsipstack"))
        .map(String::as_str)
        .collect();
    for (what, secret) in [
        ("the peer's SRTP key", peer_key.to_base64()),
        ("our SRTP key", server_key.to_base64()),
        ("the TLS private key", key_body.clone()),
    ] {
        if let Some(line) = own.lines().find(|l| l.contains(&secret)) {
            let shown: String = line.chars().take(300).collect();
            panic!("{what} reached brisvox's logs: {shown}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Outbound calls over TLS

/// A callee: a TLS SIP server on a free port.
struct TlsCallee {
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    rtp: UdpSocket,
}

/// One accepted connection of a [`TlsCallee`].
struct CalleeConn {
    stream: tokio_rustls::server::TlsStream<TcpStream>,
    buffer: Vec<u8>,
}

impl TlsCallee {
    async fn new(pki: &Pki) -> Self {
        use rustls::pki_types::PrivateKeyDer;
        use rustls::pki_types::pem::PemObject;
        let key = PrivateKeyDer::from_pem_slice(pki.key_pem.as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pki.der.clone()], key)
        .unwrap();
        Self {
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            rtp: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    async fn accept(&self) -> CalleeConn {
        let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("the dialer never connected")
            .unwrap();
        let stream = tokio::time::timeout(WAIT, self.acceptor.accept(tcp))
            .await
            .expect("no TLS handshake")
            .unwrap();
        CalleeConn {
            stream,
            buffer: Vec::new(),
        }
    }
}

impl CalleeConn {
    async fn recv(&mut self) -> Msg {
        loop {
            if let Some(end) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buffer[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                if self.buffer.len() >= end + 4 + length {
                    let rest = self.buffer.split_off(end + 4 + length);
                    let message = std::mem::replace(&mut self.buffer, rest);
                    return Msg::parse(&message);
                }
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(WAIT, self.stream.read(&mut chunk))
                .await
                .expect("no data from the dialer")
                .unwrap();
            assert!(n > 0, "the dialer closed the connection");
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }

    async fn request_of(&mut self, method: &str) -> Msg {
        loop {
            let msg = self.recv().await;
            if msg.method() == Some(method) {
                return msg;
            }
        }
    }

    /// `200 OK` to `invite` with `sdp`, Contact on `contact`.
    async fn ok(&mut self, invite: &Msg, contact: SocketAddr, sdp: &str) {
        let mut out = String::from("SIP/2.0 200 OK\r\n");
        for via in invite.all("via") {
            out.push_str(&format!("Via: {via}\r\n"));
        }
        out.push_str(&format!("From: {}\r\n", invite.header("from").unwrap()));
        out.push_str(&format!(
            "To: {};tag=callee-tag\r\n",
            invite.header("to").unwrap()
        ));
        out.push_str(&format!(
            "Call-ID: {}\r\n",
            invite.header("call-id").unwrap()
        ));
        out.push_str(&format!("CSeq: {}\r\n", invite.header("cseq").unwrap()));
        out.push_str(&format!(
            "Contact: <sip:callee@{contact};transport=tls>\r\n"
        ));
        out.push_str(&format!(
            "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
            sdp.len()
        ));
        self.stream.write_all(out.as_bytes()).await.unwrap();
    }
}

fn dial_config(pki: &Pki, srtp: SrtpPolicy) -> SipConfig {
    SipConfig {
        tls: Some(SipTlsConfig {
            client_ca_pem: Some(pki.cert_pem.clone()),
            ..tls_listener(pki)
        }),
        ..config(pki, srtp)
    }
}

type DialTask =
    tokio::task::JoinHandle<Result<brisvox_transport::CallLink, brisvox_transport::sip::DialError>>;

fn dial(rig: &Rig, uri: String) -> DialTask {
    let handle = rig.handle();
    tokio::spawn(async move {
        let mut request =
            brisvox_transport::sip::DialRequest::new(uri, "sip:+15557654321@brisvox.test");
        request.ring_timeout = Duration::from_secs(3);
        handle.dial(request, &CancellationToken::new()).await
    })
}

#[tokio::test]
async fn an_outbound_call_over_tls_offers_srtp_and_carries_encrypted_audio() {
    let pki = pki(false);
    let rig = Rig::start(dial_config(&pki, SrtpPolicy::Optional)).await;
    let callee = TlsCallee::new(&pki).await;
    let task = dial(&rig, format!("sips:+15551234567@{}", callee.addr()));

    let mut conn = callee.accept().await;
    let invite = conn.request_of("INVITE").await;
    assert!(
        invite
            .all("via")
            .iter()
            .any(|v| v.starts_with("SIP/2.0/TLS")),
        "{invite:?}"
    );
    let offer = invite.text();
    assert!(offer.contains("RTP/SAVP"), "{offer}");
    let (tag, suite, dialer_key) = answer_crypto(&offer).expect("a crypto line in the offer");
    assert_eq!((tag, suite.as_str()), (1, "AES_CM_128_HMAC_SHA1_80"));
    let dialer_rtp = sdp_rtp_addr(&offer);

    let callee_key = MasterKey::generate().unwrap();
    let answer = format!(
        "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\na=sendrecv\r\n",
        callee.rtp.local_addr().unwrap().port(),
        callee_key.to_base64()
    );
    conn.ok(&invite, callee.addr(), &answer).await;

    let link = tokio::time::timeout(WAIT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    conn.request_of("ACK").await;
    let mut session = TransportSession::open(
        rig.clock.clone(),
        link,
        SessionConfig::default(),
        &rig.parent,
    )
    .await
    .unwrap();
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    let TransportEvent::Started(info) = started else {
        unreachable!()
    };
    assert_eq!(info.metadata["signaling"], "tls");
    assert_eq!(info.metadata["srtp"], "AES_CM_128_HMAC_SHA1_80");

    let mut callee_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &callee_key).unwrap();
    let mut callee_rx = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_80, &dialer_key).unwrap();
    let mut wire = rtp_bytes(1, 0, 0x55, &ulaw(250, 160));
    callee_tx.protect(&mut wire).unwrap();
    callee.rtp.send_to(&wire, dialer_rtp).await.unwrap();
    assert_eq!(next_audio(&mut session).await.sample_count(), 160);
    session.send(bot_frame(&rig.clock, 0, 400)).await.unwrap();
    let packet = recv_datagram(&callee.rtp, WAIT)
        .await
        .expect("no RTP to the callee");
    assert_ne!(packet[12..172], ulaw(400, 160)[..], "encrypted on the wire");
    let mut received = packet;
    let n = callee_rx.unprotect(&mut received).unwrap();
    assert_eq!(&received[12..n], ulaw(400, 160).as_slice());
    assert_eq!(rig.server.stats().srtp_calls, 1);

    // The BYE goes back over the connection the call was set up on.
    session.send(TransportCommand::Stop).await.unwrap();
    let bye = conn.request_of("BYE").await;
    assert_eq!(bye.header("call-id"), invite.header("call-id"));
}

#[tokio::test]
async fn an_answer_without_srtp_to_a_secure_offer_ends_the_outbound_call() {
    let pki = pki(false);
    let rig = Rig::start(dial_config(&pki, SrtpPolicy::Required)).await;
    let callee = TlsCallee::new(&pki).await;
    let task = dial(&rig, format!("sips:+15551234567@{}", callee.addr()));
    let mut conn = callee.accept().await;
    let invite = conn.request_of("INVITE").await;
    // A plain RTP/AVP answer: no crypto line.
    let plain = format!(
        "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
        callee.rtp.local_addr().unwrap().port()
    );
    conn.ok(&invite, callee.addr(), &plain).await;
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(
        matches!(result, Err(brisvox_transport::sip::DialError::BadAnswer(_))),
        "{:?}",
        result.err()
    );
    // The unusable session is ended with a BYE.
    conn.request_of("BYE").await;
}

#[tokio::test]
async fn required_srtp_refuses_to_dial_a_target_that_is_not_tls_and_a_dead_one_fails() {
    let pki = pki(false);
    let rig = Rig::start(dial_config(&pki, SrtpPolicy::Required)).await;
    let mut request = brisvox_transport::sip::DialRequest::new(
        "sip:+15551234567@127.0.0.1:5090;transport=udp",
        "sip:+15557654321@brisvox.test",
    );
    request.ring_timeout = Duration::from_secs(2);
    let result = rig.handle().dial(request, &CancellationToken::new()).await;
    assert!(
        matches!(
            result,
            Err(brisvox_transport::sip::DialError::InvalidRequest(_))
        ),
        "{:?}",
        result.err()
    );

    // Nothing listens there: the call fails (and does not hang).
    let task = dial(&rig, "sips:+15551234567@127.0.0.1:1".to_owned());
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(result.is_err(), "a call to nowhere succeeded");
}

#[tokio::test]
async fn a_callee_with_an_untrusted_certificate_is_not_called() {
    let ours = pki(false);
    let stranger = pki(false);
    let rig = Rig::start(dial_config(&ours, SrtpPolicy::Optional)).await;
    let callee = TlsCallee::new(&stranger).await;
    let task = dial(&rig, format!("sips:+15551234567@{}", callee.addr()));
    // The callee sees the dialer abort the handshake; no INVITE is ever sent.
    let accepted = tokio::time::timeout(WAIT, callee.listener.accept()).await;
    if let Ok(Ok((tcp, _))) = accepted {
        assert!(callee.acceptor.accept(tcp).await.is_err());
    }
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(result.is_err(), "a call to an untrusted callee succeeded");
}

// ---------------------------------------------------------------------------------------------
// Adversarial-review regressions

/// An in-ACL attacker who knows the Call-ID and both tags of a TLS dialog and sends from its own
/// UDP sockets.
struct Attacker {
    sip: UdpSocket,
    rtp: UdpSocket,
    key: MasterKey,
    server: SocketAddr,
}

impl Attacker {
    async fn new(server: SocketAddr) -> Self {
        Self {
            sip: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            rtp: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            key: MasterKey::generate().unwrap(),
            server,
        }
    }

    /// The attacker's SDP: RTP/SAVP, its own `a=crypto` and its own `c=` and port.
    fn sdp(&self) -> String {
        secure_sdp(
            self.rtp.local_addr().unwrap().port(),
            &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &self.key)],
        )
    }

    /// An in-dialog request for `peer`'s dialog over cleartext UDP, with `via_proto` claimed in
    /// the Via (the lie of the review's HIGH finding).
    async fn send(
        &self,
        peer: &TlsPeer,
        method: &str,
        cseq: u32,
        via_proto: &str,
        body: Option<&str>,
    ) {
        let local = self.sip.local_addr().unwrap();
        let to_tag = peer.to_tag.clone().unwrap();
        let mut text = format!(
            "{method} sip:agent@{} SIP/2.0\r\nVia: SIP/2.0/{via_proto} {local};branch=z9hG4bK-evil-{cseq}\r\n\
             Max-Forwards: 70\r\nFrom: \"Alice\" <sip:alice@example.com>;tag=ft{}\r\n\
             To: <sip:agent@example.com>;tag={to_tag}\r\nCall-ID: {}\r\nCSeq: {cseq} {method}\r\n\
             Contact: <sip:evil@{local}>\r\n",
            self.server,
            peer.local.port(),
            peer.call_id
        );
        match body {
            Some(body) => text.push_str(&format!(
                "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )),
            None => text.push_str("Content-Length: 0\r\n\r\n"),
        }
        self.sip
            .send_to(text.as_bytes(), self.server)
            .await
            .unwrap();
    }

    /// Everything that arrives on the attacker's signaling socket within `wait`.
    async fn heard(&self, wait: Duration) -> Vec<Msg> {
        let mut out = Vec::new();
        while let Some(datagram) = recv_datagram(&self.sip, wait).await {
            out.push(Msg::parse(&datagram));
        }
        out
    }
}

/// A TLS SRTP call that is up, with the server's key and where its media goes.
struct SecureCall {
    rig: Rig,
    peer: TlsPeer,
    session: TransportSession,
    server_key: MasterKey,
    peer_key: MasterKey,
    dest: SocketAddr,
}

async fn secure_call(srtp: SrtpPolicy) -> SecureCall {
    let pki = pki(false);
    let mut rig = Rig::start(config(&pki, srtp)).await;
    let mut peer = good_peer(&rig, &pki).await;
    let peer_key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &peer_key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let (_, _, server_key) = answer_crypto(&answer.text()).unwrap();
    let session = rig.accept().await;
    let dest = sdp_rtp_addr(&answer.text());
    SecureCall {
        rig,
        peer,
        session,
        server_key,
        peer_key,
        dest,
    }
}

/// The bot says something; it must reach `peer` (decrypts with `server_key`), and nothing may
/// reach `elsewhere`.
async fn media_still_goes_to(
    call: &mut SecureCall,
    server_key: &MasterKey,
    seq: u64,
    elsewhere: &UdpSocket,
) {
    call.session
        .send(bot_frame(&call.rig.clock, seq, 500))
        .await
        .unwrap();
    let packet = recv_datagram(&call.peer.rtp, WAIT)
        .await
        .expect("the bot's audio no longer reaches the real peer");
    let mut received = packet;
    let mut rx = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_80, server_key).unwrap();
    let n = rx.unprotect(&mut received).unwrap();
    assert_eq!(&received[12..n], ulaw(500, 160).as_slice());
    assert!(
        recv_datagram(elsewhere, Duration::from_millis(300))
            .await
            .is_none(),
        "audio went to the attacker's address"
    );
}

#[tokio::test]
async fn a_cleartext_reinvite_claiming_tls_gets_no_key_and_moves_no_media() {
    let mut call = secure_call(SrtpPolicy::Required).await;
    let evil = Attacker::new(call.rig.server.local_addr()).await;
    let key_text = call.server_key.to_base64();

    // The review's proof: UDP, `Via: SIP/2.0/TLS`, RTP/SAVP, own a=crypto, own c=. (And an
    // honest UDP Via, so that the answer can reach the attacker.)
    evil.send(&call.peer, "INVITE", 50, "TLS", Some(&evil.sdp()))
        .await;
    evil.send(&call.peer, "INVITE", 51, "UDP", Some(&evil.sdp()))
        .await;
    evil.send(&call.peer, "UPDATE", 52, "UDP", Some(&evil.sdp()))
        .await;
    evil.send(&call.peer, "BYE", 53, "UDP", None).await;
    let heard = evil.heard(Duration::from_millis(800)).await;
    assert!(
        heard
            .iter()
            .any(|m| m.status() == Some(488) && m.cseq_method() == "INVITE"),
        "no 488 for the re-INVITE: {:?}",
        heard
            .iter()
            .map(|m| (&m.start, m.header("cseq")))
            .collect::<Vec<_>>()
    );
    assert!(
        heard.iter().any(|m| m.status() == Some(403)),
        "no 403 for the BYE: {:?}",
        heard
            .iter()
            .map(|m| (&m.start, m.header("cseq")))
            .collect::<Vec<_>>()
    );
    for message in &heard {
        let text = String::from_utf8_lossy(&message.body).into_owned();
        assert!(
            !text.contains("a=crypto") && !text.contains(&key_text),
            "a key was answered in the clear to {:?}",
            message.header("cseq")
        );
    }
    call.rig
        .stats_where(|s| s.insecure_dialog_requests >= 4)
        .await;

    // The call is untouched: still up, the media goes to the real peer, the attacker's key is
    // worth nothing, and the dialog's CSeq did not move (a legitimate re-INVITE, CSeq 2, works).
    assert_eq!(call.rig.handle().active_calls(), 1);
    let evil_rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_key = call.server_key.clone();
    media_still_goes_to(&mut call, &server_key, 0, &evil.rtp).await;
    let mut forged = rtp_bytes(1, 0, 0x99, &ulaw(300, 160));
    SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &evil.key)
        .unwrap()
        .protect(&mut forged)
        .unwrap();
    evil.rtp.send_to(&forged, call.dest).await.unwrap();
    call.rig.stats_where(|s| s.srtp_packets_rejected >= 1).await;
    drop(evil_rtp);
    call.peer.cseq += 1;
    let legit = call.peer.request(
        "INVITE",
        call.peer.cseq,
        &[],
        Some(&secure_sdp(
            call.peer.rtp_port(),
            &[crypto_line(
                1,
                SrtpSuite::AesCm128HmacSha1_80,
                &call.peer_key,
            )],
        )),
    );
    call.peer.send(&legit).await;
    let ok = call.peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
}

#[tokio::test]
async fn an_ack_answer_that_did_not_come_over_tls_is_dropped_and_one_over_tls_is_applied() {
    let mut call = secure_call(SrtpPolicy::Required).await;
    let evil = Attacker::new(call.rig.server.local_addr()).await;

    // The real peer re-INVITEs without an offer: the server's offer is in the 200, the peer's
    // answer is due in the ACK. The attacker's ACK gets there first, with its own SDP.
    call.peer.cseq += 1;
    let cseq = call.peer.cseq;
    let reinvite = call.peer.request("INVITE", cseq, &[], None);
    call.peer.send(&reinvite).await;
    let ok = call.peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    let (_, _, offered_key) = answer_crypto(&ok.text()).expect("the server's offer");
    assert_eq!(offered_key, call.server_key, "the call keeps its key");
    evil.send(&call.peer, "ACK", cseq, "TLS", Some(&evil.sdp()))
        .await;
    call.rig
        .stats_where(|s| s.insecure_dialog_requests >= 1)
        .await;
    let server_key = call.server_key.clone();
    media_still_goes_to(&mut call, &server_key, 0, &evil.rtp).await;

    // A second round: the same over TLS carries a real answer with a NEW peer key and is applied;
    // the peer's audio under the new key is played, under the old key it is not.
    call.peer.cseq += 1;
    let cseq = call.peer.cseq;
    let reinvite = call.peer.request("INVITE", cseq, &[], None);
    call.peer.send(&reinvite).await;
    let ok = call.peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    let new_key = MasterKey::generate().unwrap();
    let ack = call.peer.request(
        "ACK",
        cseq,
        &[],
        Some(&secure_sdp(
            call.peer.rtp_port(),
            &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &new_key)],
        )),
    );
    call.peer.send(&ack).await;
    pause(Duration::from_millis(200)).await;
    let mut under_old = rtp_bytes(1, 0, 0x77, &ulaw(300, 160));
    SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &call.peer_key)
        .unwrap()
        .protect(&mut under_old)
        .unwrap();
    call.peer.rtp.send_to(&under_old, call.dest).await.unwrap();
    call.rig.stats_where(|s| s.srtp_packets_rejected >= 1).await;
    let mut under_new = rtp_bytes(2, 160, 0x77, &ulaw(300, 160));
    SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &new_key)
        .unwrap()
        .protect(&mut under_new)
        .unwrap();
    call.peer.rtp.send_to(&under_new, call.dest).await.unwrap();
    assert_eq!(next_audio(&mut call.session).await.sample_count(), 160);
}

#[tokio::test]
async fn a_suite_switch_without_a_new_key_is_refused_and_old_packets_stay_replays() {
    let mut call = secure_call(SrtpPolicy::Required).await;
    // A genuine packet, played once.
    let mut tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &call.peer_key).unwrap();
    let mut wire = rtp_bytes(100, 0, 0x1234, &ulaw(300, 160));
    tx.protect(&mut wire).unwrap();
    call.peer.rtp.send_to(&wire, call.dest).await.unwrap();
    assert_eq!(next_audio(&mut call.session).await.sample_count(), 160);

    // The re-INVITE switches to the _32 suite with the SAME key: refused.
    call.peer.cseq += 1;
    let switch = call.peer.request(
        "INVITE",
        call.peer.cseq,
        &[],
        Some(&secure_sdp(
            call.peer.rtp_port(),
            &[crypto_line(
                2,
                SrtpSuite::AesCm128HmacSha1_32,
                &call.peer_key,
            )],
        )),
    );
    call.peer.send(&switch).await;
    let refused = call.peer.final_response().await;
    assert_eq!(refused.status(), Some(488), "{refused:?}");

    // The earlier packet, cut to a _32 packet, is rejected, and so is the original replay.
    let before = call.rig.server.stats().srtp_packets_rejected;
    let mut cut = wire.clone();
    cut.truncate(wire.len() - 10 + 4);
    call.peer.rtp.send_to(&cut, call.dest).await.unwrap();
    call.peer.rtp.send_to(&wire, call.dest).await.unwrap();
    call.rig
        .stats_where(|s| s.srtp_packets_rejected >= before + 2)
        .await;
    let heard = tokio::time::timeout(Duration::from_millis(300), call.session.recv()).await;
    assert!(
        !matches!(heard, Ok(Some(TransportEvent::Audio(_)))),
        "a replay was played"
    );
}

#[tokio::test]
async fn too_many_srtp_authentication_failures_in_a_second_end_the_call() {
    let pki = pki(false);
    let cfg = SipConfig {
        srtp_auth_failure_limit: 20,
        ..config(&pki, SrtpPolicy::Required)
    };
    let mut rig = Rig::start(cfg).await;
    let mut peer = good_peer(&rig, &pki).await;
    let peer_key = MasterKey::generate().unwrap();
    let offer = secure_sdp(
        peer.rtp_port(),
        &[crypto_line(1, SrtpSuite::AesCm128HmacSha1_80, &peer_key)],
    );
    let answer = peer.call(&offer, &[]).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let _session = rig.accept().await;
    let dest = sdp_rtp_addr(&answer.text());
    // A burst of forged packets (random tags).
    for seq in 0..60u16 {
        let mut forged = rtp_bytes(seq, 0, 0x4242, &ulaw(1, 160));
        forged.extend_from_slice(&[seq as u8; 10]);
        peer.rtp.send_to(&forged, dest).await.unwrap();
    }
    rig.stats_where(|s| s.srtp_flood_hangups == 1).await;
    // The server hung up: a BYE comes back on the connection.
    let bye = loop {
        let msg = peer.recv().await;
        if msg.method() == Some("BYE") {
            break msg;
        }
    };
    assert_eq!(bye.header("call-id"), Some(peer.call_id.as_str()));
    rig.stats_where(|s| s.srtp_packets_rejected >= 21).await;
}

// -- idle connections --------------------------------------------------------------------------

fn idle_config(pki: &Pki, idle: Duration) -> SipConfig {
    SipConfig {
        tls: Some(SipTlsConfig {
            idle_timeout: idle,
            ..tls_listener(pki)
        }),
        ..config(pki, SrtpPolicy::Off)
    }
}

/// Whether the server closes `peer`'s connection within `wait` (reads until EOF or an error).
async fn closed_within(peer: &mut TlsPeer, wait: Duration) -> bool {
    #[allow(clippy::disallowed_methods)] // test harness, real time
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = [0u8; 256];
    loop {
        match tokio::time::timeout_at(deadline, peer.stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {} // a keepalive pong
            Err(_) => return false,
        }
    }
}

#[tokio::test]
async fn a_silent_established_tls_connection_is_closed_after_the_idle_timeout() {
    let pki = pki(false);
    let rig = Rig::start(idle_config(&pki, Duration::from_millis(500))).await;
    let mut peer = good_peer(&rig, &pki).await;
    assert!(
        closed_within(&mut peer, Duration::from_secs(5)).await,
        "the server kept a silent connection open"
    );
    let stats = rig.stats_where(|s| s.tls_idle_closed == 1).await;
    assert_eq!(stats.tls_connections, 1);
    // The slot is free again: a new connection works (per-IP limit of 1).
    let again = good_peer(&rig, &pki).await;
    drop(again);
}

#[tokio::test]
async fn crlf_keepalives_keep_a_tls_connection_open_and_silence_after_them_ends_it() {
    let pki = pki(false);
    let rig = Rig::start(idle_config(&pki, Duration::from_millis(600))).await;
    let mut peer = good_peer(&rig, &pki).await;
    // Three idle periods' worth of keepalives (RFC 5626 CRLFCRLF pings), 200 ms apart.
    for _ in 0..9 {
        pause(Duration::from_millis(200)).await;
        peer.send("\r\n\r\n").await;
    }
    assert_eq!(
        rig.server.stats().tls_idle_closed,
        0,
        "closed despite keepalives"
    );
    // A request still works on it.
    let mut drain = [0u8; 64];
    while tokio::time::timeout(Duration::from_millis(50), peer.stream.read(&mut drain))
        .await
        .is_ok_and(|r| r.is_ok_and(|n| n > 0))
    {}
    peer.cseq += 1;
    let options = peer.request("OPTIONS", peer.cseq, &[], None);
    peer.send(&options).await;
    let reply = peer.final_response().await;
    assert_eq!(reply.status(), Some(200), "{reply:?}");
    // Then nothing: closed within a few idle periods.
    assert!(closed_within(&mut peer, Duration::from_secs(5)).await);
    rig.stats_where(|s| s.tls_idle_closed == 1).await;
}

// -- outbound through an explicit proxy address ------------------------------------------------

#[tokio::test]
async fn a_sips_callee_through_an_explicit_proxy_ip_is_verified_for_its_domain() {
    // The callee's certificate names the domain and NOT the proxy's IP address.
    let callee_pki = pki_for(&["callee.example.test"], false);
    let server_pki = pki(false);
    let cfg = SipConfig {
        tls: Some(SipTlsConfig {
            client_ca_pem: Some(callee_pki.cert_pem.clone()),
            ..tls_listener(&server_pki)
        }),
        ..config(&server_pki, SrtpPolicy::Optional)
    };
    let rig = Rig::start(cfg).await;
    let callee = TlsCallee::new(&callee_pki).await;
    let handle = rig.handle();
    let proxy = callee.addr();
    let task = tokio::spawn(async move {
        let mut request = brisvox_transport::sip::DialRequest::new(
            "sips:+15551234567@callee.example.test",
            "sip:+15557654321@brisvox.test",
        );
        request.destination = Some(proxy);
        request.ring_timeout = Duration::from_secs(3);
        handle.dial(request, &CancellationToken::new()).await
    });

    let mut conn = callee.accept().await;
    let invite = conn.request_of("INVITE").await;
    let offer = invite.text();
    let (_, _, dialer_key) = answer_crypto(&offer).expect("a crypto line in the offer");
    assert!(offer.contains("RTP/SAVP"), "{offer}");
    let callee_key = MasterKey::generate().unwrap();
    let answer = format!(
        "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\na=sendrecv\r\n",
        callee.rtp.local_addr().unwrap().port(),
        callee_key.to_base64()
    );
    conn.ok(&invite, proxy, &answer).await;
    let link = tokio::time::timeout(WAIT, task)
        .await
        .unwrap()
        .unwrap()
        .expect("the call through the proxy failed");
    conn.request_of("ACK").await;
    let mut session = TransportSession::open(
        rig.clock.clone(),
        link,
        SessionConfig::default(),
        &rig.parent,
    )
    .await
    .unwrap();
    // Audio flows (the dialer's key is the one in its offer).
    let mut callee_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &callee_key).unwrap();
    let mut wire = rtp_bytes(1, 0, 0x55, &ulaw(250, 160));
    callee_tx.protect(&mut wire).unwrap();
    callee
        .rtp
        .send_to(&wire, sdp_rtp_addr(&offer))
        .await
        .unwrap();
    assert_eq!(next_audio(&mut session).await.sample_count(), 160);
    let _ = dialer_key;
    // The BYE goes back over the same connection.
    session.send(TransportCommand::Stop).await.unwrap();
    let bye = conn.request_of("BYE").await;
    assert_eq!(bye.header("call-id"), invite.header("call-id"));
}

#[tokio::test]
async fn a_proxy_whose_certificate_is_for_another_domain_is_not_called() {
    let wrong = pki_for(&["somebody-else.example.test"], false);
    let server_pki = pki(false);
    let cfg = SipConfig {
        tls: Some(SipTlsConfig {
            client_ca_pem: Some(wrong.cert_pem.clone()),
            ..tls_listener(&server_pki)
        }),
        ..config(&server_pki, SrtpPolicy::Optional)
    };
    let rig = Rig::start(cfg).await;
    let callee = TlsCallee::new(&wrong).await;
    let handle = rig.handle();
    let proxy = callee.addr();
    let task = tokio::spawn(async move {
        let mut request = brisvox_transport::sip::DialRequest::new(
            "sips:+15551234567@callee.example.test",
            "sip:+15557654321@brisvox.test",
        );
        request.destination = Some(proxy);
        request.ring_timeout = Duration::from_secs(3);
        handle.dial(request, &CancellationToken::new()).await
    });
    // The handshake fails on the dialer's side: no INVITE ever arrives.
    let accepted = tokio::time::timeout(Duration::from_secs(2), callee.listener.accept()).await;
    if let Ok(Ok((tcp, _))) = accepted {
        let _ = tokio::time::timeout(Duration::from_secs(1), callee.acceptor.accept(tcp)).await;
    }
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    let error = result.err().expect("the call must fail").to_string();
    assert!(error.contains("callee.example.test"), "{error}");
}
