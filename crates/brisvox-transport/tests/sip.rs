//! SIP/RTP transport tests: real UDP (and one TCP) sockets on 127.0.0.1, the real clock,
//! short deadlines.
//!
//! The carrier or PBX is a scripted peer (`Peer` for inbound calls, `Callee` for outbound calls)
//! that speaks raw SIP text and raw RTP; the engine is a `TransportSession` on every call. Digest
//! responses are computed and checked with rsipstack's own functions (`compute_digest`,
//! `verify_digest`), so these tests alone cannot tell a wrong hash from a right one. The hash itself
//! is pinned by known answers computed with an independent MD5 implementation in the unit test
//! `the_digest_matches_known_answers_computed_independently` (`src/sip/auth.rs`); what these tests
//! add is the exchange: the `uri` in the response equals the retried request's URI, the `qop`
//! fields are present and the password is never sent.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::Duration;

use brisvox_audio::Codec;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::admission::{AdmissionGate, AdmissionTicket, Refusal, SharedGate};
use brisvox_transport::sip::{
    Cidr, DialCredential, DialError, DialRequest, DtmfError, G711, InboundAuth, SipConfig,
    SipHandle, SipServer, SipStats,
};
use brisvox_transport::{
    CallLink, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId,
};
use bytes::Bytes;
use rsipstack::dialog::authenticate::{compute_digest, verify_digest};
use rsipstack::sip::Method;
use rsipstack::sip::headers::auth::Algorithm;
use rsipstack::sip::typed::Authorization;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(5);

/// Waits `duration` on the real clock.
async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

/// Polls the server's counters until `wanted` holds (at most [`WAIT`]) and returns them. A fixed
/// pause followed by an exact-equality assertion is flaky on a loaded machine: the counter is
/// incremented by another task, so wait for the value instead of for a time.
async fn stats_where(rig: &Rig, wanted: impl Fn(&SipStats) -> bool) -> SipStats {
    let wait = async {
        loop {
            let stats = rig.server.stats();
            if wanted(&stats) {
                return stats;
            }
            pause(Duration::from_millis(10)).await;
        }
    };
    match tokio::time::timeout(WAIT, wait).await {
        Ok(stats) => stats,
        Err(_) => panic!(
            "the counters never reached the expected state: {:?}",
            rig.server.stats()
        ),
    }
}

fn config() -> SipConfig {
    SipConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        send_timeout: Duration::from_secs(2),
        ack_timeout: Duration::from_secs(3),
        hangup_timeout: Duration::from_secs(2),
        ..SipConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// SIP text

/// A parsed SIP message.
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
        let rest = self.start.strip_prefix("SIP/2.0 ")?;
        rest.split(' ').next()?.parse().ok()
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

    fn cseq(&self) -> (u32, String) {
        let value = self.header("cseq").unwrap();
        let (n, method) = value.split_once(' ').unwrap();
        (n.parse().unwrap(), method.to_owned())
    }

    fn call_id(&self) -> String {
        self.header("call-id").unwrap().to_owned()
    }

    fn to_tag(&self) -> Option<String> {
        let to = self.header("to")?;
        to.split(';')
            .find_map(|p| p.trim().strip_prefix("tag="))
            .map(str::to_owned)
    }
}

/// The RTP address an SDP announces.
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

/// A response to `request` as a UAS: Via, From, To (with `to_tag`), Call-ID and CSeq copied.
fn response(
    request: &Msg,
    code: u16,
    reason: &str,
    to_tag: Option<&str>,
    contact: Option<SocketAddr>,
    extra: &[(&str, String)],
    body: Option<&str>,
) -> String {
    let mut out = format!("SIP/2.0 {code} {reason}\r\n");
    for via in request.all("via") {
        out.push_str(&format!("Via: {via}\r\n"));
    }
    out.push_str(&format!("From: {}\r\n", request.header("from").unwrap()));
    let mut to = request.header("to").unwrap().to_owned();
    if let (Some(tag), false) = (to_tag, to.contains("tag=")) {
        to.push_str(&format!(";tag={tag}"));
    }
    out.push_str(&format!("To: {to}\r\n"));
    out.push_str(&format!("Call-ID: {}\r\n", request.call_id()));
    out.push_str(&format!("CSeq: {}\r\n", request.header("cseq").unwrap()));
    if let Some(contact) = contact {
        out.push_str(&format!("Contact: <sip:peer@{contact}>\r\n"));
    }
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

// ---------------------------------------------------------------------------------------------
// RTP

struct Rtp {
    marker: bool,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: Vec<u8>,
    from: SocketAddr,
}

fn rtp_bytes(
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = vec![0x80, payload_type | if marker { 0x80 } else { 0 }];
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn parse_rtp(bytes: &[u8], from: SocketAddr) -> Rtp {
    assert!(bytes.len() >= 12, "short RTP packet");
    assert_eq!(bytes[0], 0x80, "RTP version 2, no padding/extension/CSRC");
    Rtp {
        marker: bytes[1] & 0x80 != 0,
        payload_type: bytes[1] & 0x7f,
        sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
        timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        payload: bytes[12..].to_vec(),
        from,
    }
}

async fn recv_rtp(socket: &UdpSocket) -> Rtp {
    recv_rtp_within(socket, WAIT)
        .await
        .expect("no RTP packet within the wait")
}

async fn recv_rtp_within(socket: &UdpSocket, wait: Duration) -> Option<Rtp> {
    let mut buf = [0u8; 2048];
    match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
        Ok(Ok((n, from))) => Some(parse_rtp(&buf[..n], from)),
        Ok(Err(error)) => panic!("rtp socket error: {error}"),
        Err(_) => None,
    }
}

/// Audio payload for the fill value `v`: 160 mu-law bytes of the PCM sample `v * 100`.
fn ulaw_payload(v: i16) -> Vec<u8> {
    let pcm: Vec<i16> = vec![v * 100; 160];
    let mut out = vec![0u8; 160];
    Codec::Ulaw.encode_samples(&pcm, &mut out).unwrap();
    out
}

fn pcm_of_ulaw(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; payload.len() * 2];
    Codec::Ulaw.decode(payload, &mut out).unwrap();
    out
}

fn bot_frame(
    clock: &SharedClock,
    epoch: u32,
    seq: u64,
    sample: i16,
    samples: usize,
) -> TransportCommand {
    let pcm: Vec<u8> = std::iter::repeat_n(sample.to_le_bytes(), samples)
        .flatten()
        .collect();
    let epoch = TurnEpoch::new(epoch);
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

    async fn default() -> Self {
        Self::start(config()).await
    }

    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    fn handle(&self) -> SipHandle {
        self.server.handle()
    }

    async fn open(&self, link: CallLink) -> TransportSession {
        TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap()
    }

    /// The engine side of the next inbound call.
    async fn accept(&mut self) -> TransportSession {
        let cancel = CancellationToken::new();
        let link = tokio::time::timeout(WAIT, self.server.accept(&cancel))
            .await
            .expect("no call within the wait")
            .unwrap()
            .expect("server closed");
        self.open(link).await
    }

    /// A call from a scripted peer, accepted, with the `Started` event consumed.
    async fn inbound(&mut self) -> Call {
        let mut peer = Peer::new(self.addr()).await;
        let offer = peer.offer(&[0, 101]);
        let answer = peer.call_up(&offer).await;
        assert_eq!(answer.status(), Some(200), "{answer:?}");
        let dest = sdp_rtp_addr(&answer.text());
        let mut session = self.accept().await;
        let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
        let TransportEvent::Started(info) = started else {
            unreachable!()
        };
        Call {
            peer,
            session,
            dest,
            call_id: info.call_id,
        }
    }

    async fn wait_for_active(&self, expected: usize) {
        let wait = async {
            while self.server.active_calls() != expected {
                pause(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("active calls stayed at {}", self.server.active_calls()));
    }
}

struct Call {
    peer: Peer,
    session: TransportSession,
    /// Where the server wants RTP.
    dest: SocketAddr,
    call_id: String,
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

/// Asserts that no event matching `bad` arrives within `wait`.
async fn assert_no_event(
    session: &mut TransportSession,
    wait: Duration,
    bad: impl Fn(&TransportEvent) -> bool,
) {
    let clock = SystemClock::shared();
    let end = clock.now() + wait;
    loop {
        let left = end.saturating_duration_since(clock.now());
        match tokio::time::timeout(left, session.recv()).await {
            Err(_) | Ok(None) => return,
            Ok(Some(event)) => assert!(!bad(&event), "unexpected event {event:?}"),
        }
    }
}

async fn next_audio(session: &mut TransportSession) -> AudioFrame {
    match next_event(session, |e| matches!(e, TransportEvent::Audio(_))).await {
        TransportEvent::Audio(frame) => frame,
        _ => unreachable!(),
    }
}

async fn stopped(session: &mut TransportSession) -> StopReason {
    match next_event(session, |e| matches!(e, TransportEvent::Stopped { .. })).await {
        TransportEvent::Stopped { reason } => reason,
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted carrier (UAC)

struct Peer {
    sip: UdpSocket,
    rtp: UdpSocket,
    server: SocketAddr,
    call_id: String,
    from_tag: String,
    cseq: u32,
    branches: u32,
    to_tag: Option<String>,
    ssrc: u32,
    seq: u16,
    ts: u32,
}

impl Peer {
    async fn new(server: SocketAddr) -> Self {
        let sip = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tag = sip.local_addr().unwrap().port();
        Self {
            call_id: format!("call-{tag}@peer.test"),
            from_tag: format!("ft{tag}"),
            sip,
            rtp,
            server,
            cseq: 0,
            branches: 0,
            to_tag: None,
            ssrc: 0x1234_5678,
            seq: 100,
            ts: 16_000,
        }
    }

    fn sip_addr(&self) -> SocketAddr {
        self.sip.local_addr().unwrap()
    }

    fn rtp_port(&self) -> u16 {
        self.rtp.local_addr().unwrap().port()
    }

    /// An SDP offer from this peer: audio on its RTP socket with these payload types (101 is
    /// telephone-event, 0 PCMU, 8 PCMA).
    fn offer(&self, payload_types: &[u8]) -> String {
        let formats: Vec<String> = payload_types.iter().map(u8::to_string).collect();
        let mut sdp = format!(
            "v=0\r\no=peer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP {}\r\n",
            self.rtp_port(),
            formats.join(" ")
        );
        for pt in payload_types {
            match pt {
                0 => sdp.push_str("a=rtpmap:0 PCMU/8000\r\n"),
                8 => sdp.push_str("a=rtpmap:8 PCMA/8000\r\n"),
                101 => sdp.push_str("a=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\n"),
                _ => {}
            }
        }
        sdp.push_str("a=ptime:20\r\na=sendrecv\r\n");
        sdp
    }

    fn branch(&mut self) -> String {
        self.branches += 1;
        format!("z9hG4bK-{}-{}", self.sip_addr().port(), self.branches)
    }

    fn request_text(
        &mut self,
        method: &str,
        uri: &str,
        cseq: u32,
        extra: &[(&str, String)],
        body: Option<(&str, &str)>,
    ) -> String {
        let branch = self.branch();
        let mut to = "<sip:agent@example.com>".to_owned();
        if let Some(tag) = &self.to_tag {
            to.push_str(&format!(";tag={tag}"));
        }
        let mut out = format!(
            "{method} {uri} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch={branch};rport\r\nMax-Forwards: 70\r\n\
             From: \"Alice\" <sip:alice@example.com>;tag={}\r\nTo: {to}\r\nCall-ID: {}\r\nCSeq: {cseq} {method}\r\n\
             Contact: <sip:alice@{}>\r\n",
            self.sip_addr(),
            self.from_tag,
            self.call_id,
            self.sip_addr()
        );
        for (name, value) in extra {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        match body {
            Some((content_type, body)) => out.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )),
            None => out.push_str("Content-Length: 0\r\n\r\n"),
        }
        out
    }

    async fn send_raw(&self, bytes: &[u8]) {
        self.sip.send_to(bytes, self.server).await.unwrap();
    }

    async fn send_text(&self, text: &str) {
        self.send_raw(text.as_bytes()).await;
    }

    async fn recv_within(&self, wait: Duration) -> Option<Msg> {
        let mut buf = vec![0u8; 8192];
        match tokio::time::timeout(wait, self.sip.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(Msg::parse(&buf[..n])),
            Ok(Err(error)) => panic!("sip socket error: {error}"),
            Err(_) => None,
        }
    }

    async fn recv(&self) -> Msg {
        self.recv_within(WAIT)
            .await
            .expect("no SIP message within the wait")
    }

    /// The next final (>= 200) response, skipping provisional ones.
    async fn final_response(&self) -> Msg {
        loop {
            let msg = self.recv().await;
            match msg.status() {
                Some(code) if code >= 200 => return msg,
                Some(_) => {}
                None => panic!("expected a response, got {msg:?}"),
            }
        }
    }

    /// The next final response to a request of this method (retransmissions of earlier answers
    /// are skipped).
    async fn final_response_to(&self, method: &str) -> Msg {
        loop {
            let msg = self.final_response().await;
            if msg.cseq().1 == method {
                return msg;
            }
        }
    }

    /// The next request with this method, skipping responses.
    async fn request_of(&self, method: &str) -> Msg {
        loop {
            let msg = self.recv().await;
            if msg.method() == Some(method) {
                return msg;
            }
        }
    }

    async fn invite_with(&mut self, extra: &[(&str, String)], content_type: &str, body: &str) {
        self.cseq += 1;
        let uri = format!("sip:agent@{}", self.server);
        let text = self.request_text("INVITE", &uri, self.cseq, extra, Some((content_type, body)));
        self.send_text(&text).await;
    }

    async fn invite(&mut self, sdp: &str) {
        self.invite_with(&[("X-Test", "hello".to_owned())], "application/sdp", sdp)
            .await;
    }

    async fn ack(&mut self, final_response: &Msg) {
        let uri = format!("sip:agent@{}", self.server);
        let cseq = final_response.cseq().0;
        let tag = final_response.to_tag();
        let saved = self.to_tag.take();
        self.to_tag = tag;
        let text = self.request_text("ACK", &uri, cseq, &[], None);
        self.to_tag = saved;
        self.send_text(&text).await;
    }

    /// INVITE, wait for the final response; on 200 send the ACK and remember the To tag.
    async fn call_up(&mut self, sdp: &str) -> Msg {
        self.invite(sdp).await;
        let answer = self.final_response().await;
        if answer.status() == Some(200) {
            self.to_tag = answer.to_tag();
        }
        self.ack(&answer).await;
        answer
    }

    async fn bye(&mut self) -> Msg {
        self.cseq += 1;
        let uri = format!("sip:agent@{}", self.server);
        let text = self.request_text("BYE", &uri, self.cseq, &[], None);
        self.send_text(&text).await;
        self.final_response_to("BYE").await
    }

    async fn send_media(
        &mut self,
        dest: SocketAddr,
        payload_type: u8,
        payload: &[u8],
        marker: bool,
    ) {
        let packet = rtp_bytes(payload_type, marker, self.seq, self.ts, self.ssrc, payload);
        self.rtp.send_to(&packet, dest).await.unwrap();
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(payload.len() as u32);
    }

    async fn send_rtp_raw(&self, dest: SocketAddr, packet: &[u8]) {
        self.rtp.send_to(packet, dest).await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Inbound calls

#[tokio::test]
async fn an_inbound_call_is_answered_with_sdp_and_reported_to_the_engine() {
    let mut rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[18, 8, 0, 101]);
    let answer = peer.call_up(&offer).await;

    assert_eq!(answer.status(), Some(200), "{answer:?}");
    assert!(answer.to_tag().is_some(), "the 200 needs a To tag");
    assert_eq!(answer.header("content-type"), Some("application/sdp"));
    assert!(
        answer.header("contact").is_some(),
        "the 200 needs a Contact"
    );
    let sdp = answer.text();
    // G.729 is not supported; PCMA comes first in the offer, so it is the pick.
    assert!(sdp.contains("m=audio "), "{sdp}");
    assert!(sdp.contains(" RTP/AVP 8 101\r\n"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
    assert!(
        sdp.contains("a=rtpmap:101 telephone-event/8000\r\n"),
        "{sdp}"
    );
    assert!(sdp.contains("a=ptime:20\r\n"), "{sdp}");
    assert_eq!(sdp_rtp_addr(&sdp).ip().to_string(), "127.0.0.1");

    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, peer.call_id);
    assert_eq!(info.metadata["transport"], "sip");
    assert_eq!(info.metadata["sample_rate"], "8000");
    assert_eq!(info.metadata["encoding"], "alaw");
    assert_eq!(info.metadata["codec"], "PCMA");
    assert_eq!(info.metadata["dtmf"], "rfc4733");
    assert_eq!(info.metadata["direction"], "inbound");
    assert!(info.metadata["from"].contains("alice@example.com"));
    assert!(info.metadata["to"].contains("agent@example.com"));
    assert_eq!(info.metadata["header.x-test"], "hello");
    assert_eq!(info.metadata["sip_peer"], peer.sip_addr().to_string());
    let rates = info.sample_rate().unwrap().unwrap();
    assert_eq!(rates.input, SampleRate::Hz8000);
    assert_eq!(rates.output, SampleRate::Hz8000);
    assert_eq!(rig.server.stats().calls_inbound, 1);
    assert_eq!(rig.server.active_calls(), 1);
}

#[tokio::test]
async fn audio_flows_both_ways_as_g711_over_rtp_and_the_peer_ends_the_call() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;

    // Caller -> engine: ten PCMU packets decode to PCM16 at 8 kHz, in order.
    for i in 0..10i16 {
        peer.send_media(dest, 0, &ulaw_payload(i + 1), i == 0).await;
        pause(Duration::from_millis(5)).await;
    }
    for i in 0..10i16 {
        let frame = next_audio(&mut session).await;
        assert_eq!(frame.sample_rate(), SampleRate::Hz8000);
        assert_eq!(frame.sample_count(), 160);
        assert_eq!(
            frame.data().as_ref(),
            pcm_of_ulaw(&ulaw_payload(i + 1)).as_slice()
        );
    }

    // Engine -> caller: PCM16 8 kHz becomes 160-byte PCMU packets, 20 ms apart, symmetric RTP.
    let clock = rig.clock.clone();
    for i in 0..6u64 {
        session
            .send(bot_frame(&clock, 0, i, (i as i16 + 1) * 100, 160))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m1".to_owned(),
        })
        .await
        .unwrap();
    let mut packets = Vec::new();
    for _ in 0..6 {
        packets.push(recv_rtp(&peer.rtp).await);
    }
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.payload_type, 0, "the negotiated codec is PCMU");
        assert_eq!(p.payload.len(), 160);
        let expected: Vec<i16> = vec![(i as i16 + 1) * 100; 160];
        let mut ulaw = vec![0u8; 160];
        Codec::Ulaw.encode_samples(&expected, &mut ulaw).unwrap();
        assert_eq!(p.payload, ulaw);
        assert_eq!(p.from, dest, "RTP is sent from the advertised port");
        assert_eq!(p.ssrc, packets[0].ssrc);
        assert_eq!(p.sequence, packets[0].sequence.wrapping_add(i as u16));
        assert_eq!(
            p.timestamp,
            packets[0].timestamp.wrapping_add(160 * i as u32)
        );
        assert_eq!(
            p.marker,
            i == 0,
            "marker only on the first packet of the talkspurt"
        );
    }
    // The mark resolves once the packets before it went out.
    let played = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        played,
        TransportEvent::MarkPlayed {
            id: "m1".to_owned()
        }
    );

    // BYE from the peer ends the call.
    let ok = peer.bye().await;
    assert_eq!(ok.status(), Some(200));
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    rig.wait_for_active(0).await;
    let stats = rig.server.stats();
    assert_eq!(stats.rtp_packets_in, 10);
    assert!(stats.rtp_packets_out >= 6);
}

#[tokio::test]
async fn the_engine_hanging_up_sends_bye() {
    let mut rig = Rig::default().await;
    let Call {
        peer, mut session, ..
    } = rig.inbound().await;
    session.send(TransportCommand::Stop).await.unwrap();
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    assert_eq!(stopped(&mut session).await, StopReason::Requested);
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn rfc_4733_dtmf_is_reported_once_per_key_even_with_repeated_end_packets() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;

    // Digit 5: start (marker), two continuation packets, the end packet three times.
    let start_ts = peer.ts;
    let mut seq = peer.seq;
    let events: [(u16, bool); 6] = [
        (160, false),
        (320, false),
        (480, false),
        (640, true),
        (640, true),
        (640, true),
    ];
    for (i, (duration, end)) in events.into_iter().enumerate() {
        let payload = [
            5,
            if end { 0x80 | 10 } else { 10 },
            (duration >> 8) as u8,
            duration as u8,
        ];
        let packet = rtp_bytes(101, i == 0, seq, start_ts, peer.ssrc, &payload);
        peer.send_rtp_raw(dest, &packet).await;
        seq = seq.wrapping_add(1);
        pause(Duration::from_millis(5)).await;
    }
    // Then '#' (event 11) at a later timestamp.
    let ts2 = start_ts + 640 + 1600;
    for (i, (duration, end)) in [(160u16, false), (320, true), (320, true), (320, true)]
        .into_iter()
        .enumerate()
    {
        let payload = [
            11,
            if end { 0x80 | 10 } else { 10 },
            (duration >> 8) as u8,
            duration as u8,
        ];
        let packet = rtp_bytes(101, i == 0, seq, ts2, peer.ssrc, &payload);
        peer.send_rtp_raw(dest, &packet).await;
        seq = seq.wrapping_add(1);
    }
    let first = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    let second = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    assert!(
        matches!(
            first,
            TransportEvent::Dtmf {
                digit: DtmfDigit::D5,
                ..
            }
        ),
        "{first:?}"
    );
    assert!(
        matches!(
            second,
            TransportEvent::Dtmf {
                digit: DtmfDigit::Pound,
                ..
            }
        ),
        "{second:?}"
    );
    // Every packet of both keys is in (6 + 4), and the repeats of the end packet did not report
    // again.
    stats_where(&rig, |s| s.rtp_packets_in == 10).await;
    pause(Duration::from_millis(50)).await;
    assert_eq!(
        rig.server.stats().dtmf_in,
        2,
        "repeats must not report again"
    );
    // Telephone events are not audio: no packet of them was decoded into an audio frame.
    assert_eq!(rig.server.stats().rtp_packets_in, 10);
    assert_no_event(&mut session, Duration::from_millis(100), |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await;
}

#[tokio::test]
async fn sip_info_dtmf_is_accepted_as_a_fallback() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        mut session,
        ..
    } = rig.inbound().await;
    peer.cseq += 1;
    let uri = format!("sip:agent@{}", peer.server);
    let info = peer.request_text(
        "INFO",
        &uri,
        peer.cseq,
        &[],
        Some(("application/dtmf-relay", "Signal=7\r\nDuration=160\r\n")),
    );
    peer.send_text(&info).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200));
    let event = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    assert!(
        matches!(
            event,
            TransportEvent::Dtmf {
                digit: DtmfDigit::D7,
                ..
            }
        ),
        "{event:?}"
    );
    // An INFO with another body is refused and the call goes on.
    peer.cseq += 1;
    let other = peer.request_text("INFO", &uri, peer.cseq, &[], Some(("text/plain", "hi")));
    peer.send_text(&other).await;
    assert_eq!(peer.final_response().await.status(), Some(415));
    assert_eq!(rig.server.active_calls(), 1);
}

#[tokio::test]
async fn dtmf_is_sent_as_telephone_event_packets_with_the_negotiated_payload_type() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        session,
        call_id,
        ..
    } = rig.inbound().await;
    let handle = rig.handle();
    assert_eq!(
        handle.send_dtmf("no-such-call", DtmfDigit::D1, Duration::from_millis(100)),
        Err(DtmfError::UnknownCall)
    );
    handle
        .send_dtmf(&call_id, DtmfDigit::D7, Duration::from_millis(100))
        .unwrap();
    // 100 ms = 5 packets: four growing ones, then the end packet three times.
    let mut packets = Vec::new();
    for _ in 0..4 + 3 {
        packets.push(recv_rtp(&peer.rtp).await);
    }
    assert!(packets.iter().all(|p| p.payload_type == 101));
    assert!(packets.iter().all(|p| p.timestamp == packets[0].timestamp));
    assert!(packets[0].marker && packets[1..].iter().all(|p| !p.marker));
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.sequence, packets[0].sequence.wrapping_add(i as u16));
        assert_eq!(p.payload.len(), 4);
        assert_eq!(p.payload[0], 7, "event code of '7'");
        let end = p.payload[1] & 0x80 != 0;
        let duration = u16::from_be_bytes([p.payload[2], p.payload[3]]);
        if i < 4 {
            assert!(!end);
            assert_eq!(duration, 160 * (i as u16 + 1));
        } else {
            assert!(end, "the last three packets carry the end bit");
            assert_eq!(duration, 800);
        }
    }
    assert_eq!(rig.server.stats().dtmf_out, 1);
    drop(session);
}

#[tokio::test]
async fn dtmf_cannot_be_sent_when_the_peer_has_no_telephone_event() {
    let mut rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let answer = peer.call_up(&peer.offer(&[0])).await;
    assert_eq!(answer.status(), Some(200));
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.metadata["dtmf"], "none");
    assert_eq!(
        rig.handle()
            .send_dtmf(&info.call_id, DtmfDigit::D1, Duration::from_millis(100)),
        Err(DtmfError::Unsupported)
    );
    assert!(!answer.text().contains("telephone-event"));
}

#[tokio::test]
async fn packets_are_reordered_and_a_lost_packet_becomes_silence() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    // 0, 1, 3, 2, 4: the swap is invisible to the engine.
    let base = peer.seq;
    let ts = peer.ts;
    for (i, n) in [0u16, 1, 3, 2, 4].into_iter().enumerate() {
        let packet = rtp_bytes(
            0,
            i == 0,
            base + n,
            ts + 160 * u32::from(n),
            peer.ssrc,
            &ulaw_payload(n as i16 + 1),
        );
        peer.send_rtp_raw(dest, &packet).await;
    }
    for n in 0..5i16 {
        let frame = next_audio(&mut session).await;
        assert_eq!(
            frame.data().as_ref(),
            pcm_of_ulaw(&ulaw_payload(n + 1)).as_slice(),
            "packet {n}"
        );
    }
    // 5, then 7 and 8 (6 never arrives): after the reorder wait a silence frame stands in.
    for n in [5u16, 7, 8] {
        let packet = rtp_bytes(
            0,
            false,
            base + n,
            ts + 160 * u32::from(n),
            peer.ssrc,
            &ulaw_payload(n as i16 + 1),
        );
        peer.send_rtp_raw(dest, &packet).await;
    }
    let expect = [
        pcm_of_ulaw(&ulaw_payload(6)),
        vec![0u8; 320],
        pcm_of_ulaw(&ulaw_payload(8)),
        pcm_of_ulaw(&ulaw_payload(9)),
    ];
    for (i, want) in expect.iter().enumerate() {
        let frame = next_audio(&mut session).await;
        assert_eq!(frame.data().as_ref(), want.as_slice(), "frame {i}");
    }
    let stats = rig.server.stats();
    assert_eq!(stats.rtp_lost, 1);
    // A duplicate and a very late packet are dropped.
    let late = rtp_bytes(0, false, base + 2, ts + 320, peer.ssrc, &ulaw_payload(3));
    peer.send_rtp_raw(dest, &late).await;
    stats_where(&rig, |s| s.rtp_late_or_duplicate >= 1).await;
    pause(Duration::from_millis(50)).await;
    assert_eq!(rig.server.stats().rtp_late_or_duplicate, 1);
}

#[tokio::test]
async fn rtp_from_another_address_or_ssrc_or_payload_type_is_not_played() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;

    // A third party sends RTP to the call's port.
    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    intruder
        .send_to(&rtp_bytes(0, false, 500, 0, 9, &ulaw_payload(2)), dest)
        .await
        .unwrap();
    // The peer itself sends another SSRC right away, and a payload type nobody negotiated.
    peer.send_rtp_raw(dest, &rtp_bytes(0, false, 900, 0, 0xdead, &ulaw_payload(3)))
        .await;
    peer.send_rtp_raw(
        dest,
        &rtp_bytes(18, false, peer.seq, peer.ts, peer.ssrc, &[0u8; 20]),
    )
    .await;
    peer.send_rtp_raw(dest, b"not rtp at all").await;
    stats_where(&rig, |s| {
        s.rtp_foreign_source >= 1 && s.rtp_foreign_ssrc >= 1 && s.rtp_ignored >= 2
    })
    .await;
    pause(Duration::from_millis(50)).await;
    let stats = rig.server.stats();
    assert_eq!(stats.rtp_foreign_source, 1);
    assert_eq!(stats.rtp_foreign_ssrc, 1);
    assert_eq!(stats.rtp_ignored, 2);
    assert_eq!(stats.rtp_packets_in, 1);

    // The peer's own stream is unaffected.
    peer.send_media(dest, 0, &ulaw_payload(4), false).await;
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(4)).as_slice()
    );
}

#[tokio::test]
async fn latching_follows_a_peer_whose_media_comes_from_another_port() {
    let mut rig = Rig::start(SipConfig {
        rtp_latch: true,
        ..config()
    })
    .await;
    let Call {
        peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    // The SDP said peer.rtp; the media really comes from another socket (a NAT rewrote the port).
    let nat = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    nat.send_to(&rtp_bytes(0, true, 1, 0, 77, &ulaw_payload(5)), dest)
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(5)).as_slice()
    );
    // Bot audio now goes to where the media came from.
    session
        .send(bot_frame(&rig.clock, 0, 0, 300, 160))
        .await
        .unwrap();
    let packet = recv_rtp(&nat).await;
    assert_eq!(packet.payload_type, 0);
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(100))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_clear_drops_queued_audio_and_the_next_audio_starts_a_new_talkspurt() {
    let mut rig = Rig::default().await;
    let Call { peer, session, .. } = rig.inbound().await;
    let clock = rig.clock.clone();
    // One second of audio in a single frame: the adapter queues 50 packets and sends them paced.
    session
        .send(bot_frame(&clock, 0, 0, 1000, 8000))
        .await
        .unwrap();
    let mut old = Vec::new();
    for _ in 0..4 {
        old.push(recv_rtp(&peer.rtp).await);
    }
    assert!(old[0].marker);
    // Barge-in: epoch 1 starts with a clear, then new audio.
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    session
        .send(bot_frame(&clock, 1, 1, -2000, 160))
        .await
        .unwrap();
    let new_pcm = vec![-2000i16; 160];
    let mut new_ulaw = vec![0u8; 160];
    Codec::Ulaw.encode_samples(&new_pcm, &mut new_ulaw).unwrap();
    let old_payload = &old[0].payload;
    let mut trailing_old = 0;
    let found = loop {
        let p = recv_rtp(&peer.rtp).await;
        if &p.payload == old_payload {
            trailing_old += 1;
            assert!(trailing_old < 10, "the queue was not dropped");
            continue;
        }
        break p;
    };
    assert_eq!(found.payload, new_ulaw);
    assert!(
        found.marker,
        "the first packet after a clear is a new talkspurt"
    );
    // Nothing of the old second follows (50 packets would take another second).
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(300))
            .await
            .is_none()
    );
    assert!(
        rig.server.stats().dropped_on_clear >= 30,
        "{:?}",
        rig.server.stats()
    );
}

#[tokio::test]
async fn bot_audio_at_another_rate_is_dropped_and_not_sent() {
    let mut rig = Rig::default().await;
    let Call { peer, session, .. } = rig.inbound().await;
    let frame = AudioFrame::new(
        Bytes::from(vec![0u8; 640]),
        SampleRate::Hz16000,
        rig.clock.now(),
        0,
        TurnEpoch::ZERO,
    )
    .unwrap();
    session
        .send(TransportCommand::Audio {
            epoch: TurnEpoch::ZERO,
            utterance: UtteranceId::new(1),
            frame,
        })
        .await
        .unwrap();
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(300))
            .await
            .is_none()
    );
    assert_eq!(rig.server.stats().dropped_audio_frames, 1);
}

#[tokio::test]
async fn a_silent_peer_is_hung_up_on_after_the_rtp_timeout() {
    let mut rig = Rig::start(SipConfig {
        rtp_timeout: Duration::from_millis(400),
        ..config()
    })
    .await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    // Some audio, then silence.
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    assert_eq!(rig.server.stats().rtp_timeouts, 1);
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn reinvite_moves_the_media_address_and_hold_stops_the_bot_audio() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        session,
        dest,
        ..
    } = rig.inbound().await;
    // A new RTP socket for the peer.
    let moved = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let new_offer = format!(
        "v=0\r\no=peer 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        moved.local_addr().unwrap().port()
    );
    peer.cseq += 1;
    let uri = format!("sip:agent@{}", peer.server);
    let reinvite = peer.request_text(
        "INVITE",
        &uri,
        peer.cseq,
        &[],
        Some(("application/sdp", &new_offer)),
    );
    peer.send_text(&reinvite).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    assert_eq!(
        sdp_rtp_addr(&ok.text()),
        dest,
        "the server keeps its RTP port"
    );
    assert!(ok.text().contains("a=sendrecv"));
    peer.ack(&ok).await;
    session
        .send(bot_frame(&rig.clock, 0, 0, 500, 160))
        .await
        .unwrap();
    let packet = recv_rtp(&moved).await;
    assert_eq!(packet.payload_type, 0);

    // Hold: sendonly from the peer (c=0.0.0.0, the legacy form): the server answers recvonly and
    // stops sending.
    let hold = new_offer
        .replace("c=IN IP4 127.0.0.1", "c=IN IP4 0.0.0.0")
        .replace("a=sendrecv", "a=sendonly")
        .replace("1 2 IN", "1 3 IN");
    peer.cseq += 1;
    let reinvite = peer.request_text(
        "INVITE",
        &uri,
        peer.cseq,
        &[],
        Some(("application/sdp", &hold)),
    );
    peer.send_text(&reinvite).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200));
    assert!(ok.text().contains("a=recvonly"), "{}", ok.text());
    peer.ack(&ok).await;
    session
        .send(bot_frame(&rig.clock, 0, 1, 500, 160))
        .await
        .unwrap();
    assert!(
        recv_rtp_within(&moved, Duration::from_millis(300))
            .await
            .is_none()
    );

    // A re-INVITE the server cannot serve is refused and the call survives.
    let g729 = new_offer
        .replace("RTP/AVP 0 101", "RTP/AVP 18")
        .replace("a=rtpmap:0 PCMU/8000\r\n", "");
    peer.cseq += 1;
    let reinvite = peer.request_text(
        "INVITE",
        &uri,
        peer.cseq,
        &[],
        Some(("application/sdp", &g729)),
    );
    peer.send_text(&reinvite).await;
    assert_eq!(peer.final_response().await.status(), Some(488));
    assert_eq!(rig.server.active_calls(), 1);
}

// ---------------------------------------------------------------------------------------------
// Refusals, malformed input, limits

#[tokio::test]
async fn malformed_and_unacceptable_requests_are_rejected_and_the_server_survives() {
    let mut rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let good = peer.offer(&[0, 101]);

    // Garbage datagrams get no answer and break nothing.
    for garbage in [
        &b"hello"[..],
        &[0xff, 0xfe, 0x00, 0x01][..],
        &b"INVITE sip:a@b SIP/2.0\r\n\r\n"[..],
        &b"SIP/2.0 200 OK\r\n\r\n"[..],
    ] {
        peer.send_raw(garbage).await;
    }
    let unexpected = peer.recv_within(Duration::from_millis(200)).await;
    assert!(unexpected.is_none(), "garbage was answered: {unexpected:?}");
    // The CRLF keepalive ping (RFC 5626) is the one thing that is answered: with a bare CRLF.
    peer.send_raw(b"\r\n\r\n").await;
    let pong = peer.recv_within(WAIT).await.expect("no keepalive pong");
    assert!(pong.start.is_empty() && pong.body.is_empty());
    // Each of these gets a final 4xx and never becomes a call.
    let cases: Vec<(&str, &str, String, u16)> = vec![
        ("not SDP", "text/plain", "hello".to_owned(), 415),
        (
            "broken SDP",
            "application/sdp",
            "v=1\r\nm=audio x RTP/AVP 0\r\n".to_owned(),
            400,
        ),
        (
            "SRTP only",
            "application/sdp",
            good.replace("RTP/AVP", "RTP/SAVP"),
            488,
        ),
        (
            "no common codec",
            "application/sdp",
            good.replace("RTP/AVP 0 101", "RTP/AVP 18")
                .replace("a=rtpmap:0 PCMU/8000\r\n", ""),
            488,
        ),
        ("no offer", "application/sdp", String::new(), 488),
    ];
    for (name, content_type, body, status) in cases {
        peer.call_id = format!("{name}-{}", peer.sip_addr().port());
        peer.invite_with(&[], content_type, &body).await;
        let answer = peer.final_response().await;
        assert_eq!(answer.status(), Some(status), "{name}: {answer:?}");
        peer.ack(&answer).await;
        assert!(answer.body.is_empty() || answer.header("content-type").is_some());
    }

    // A request the stack cannot make a transaction of (here: no From tag, so no transaction
    // key) is dropped without an answer: RFC 3261 section 18.3 lets a UAS discard a malformed
    // datagram, and rsipstack does. Nothing becomes a call and nothing leaks.
    peer.call_id = "no-from-tag".to_owned();
    let branch = peer.branch();
    let untagged = format!(
        "INVITE sip:agent@{} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch={branch}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>\r\nTo: <sip:agent@example.com>\r\nCall-ID: no-from-tag\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:alice@{}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{good}",
        peer.server,
        peer.sip_addr(),
        peer.sip_addr(),
        good.len()
    );
    peer.send_text(&untagged).await;
    let unexpected = peer.recv_within(Duration::from_millis(300)).await;
    assert!(
        unexpected.is_none(),
        "an INVITE without a From tag was answered: {unexpected:?}"
    );

    // Methods the endpoint does not serve, and requests for unknown dialogs.
    peer.cseq += 1;
    peer.call_id = "register-attempt".to_owned();
    let register = peer.request_text("REGISTER", "sip:example.com", peer.cseq, &[], None);
    peer.send_text(&register).await;
    let answer = peer.final_response_to("REGISTER").await;
    assert_eq!(answer.status(), Some(405), "{answer:?}");
    assert!(answer.header("allow").unwrap().contains("INVITE"));
    peer.to_tag = Some("nonexistent".to_owned());
    peer.call_id = "unknown-dialog".to_owned();
    assert_eq!(peer.bye().await.status(), Some(481));

    // Nothing became a call, and the server still answers.
    assert_eq!(rig.server.active_calls(), 0);
    let stats = rig.server.stats();
    assert_eq!(stats.calls_inbound, 0);
    assert!(stats.rejected_invalid >= 7, "{stats:?}");
    peer.to_tag = None;
    peer.call_id = "still-alive".to_owned();
    let answer = peer.call_up(&good).await;
    assert_eq!(answer.status(), Some(200));
    rig.accept().await;
}

#[tokio::test]
async fn options_are_answered_with_the_supported_methods() {
    let rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    peer.cseq += 1;
    let options = peer.request_text(
        "OPTIONS",
        &format!("sip:{}", rig.addr()),
        peer.cseq,
        &[],
        None,
    );
    peer.send_text(&options).await;
    let answer = peer.final_response().await;
    assert_eq!(answer.status(), Some(200));
    let allow = answer.header("allow").unwrap();
    for method in ["INVITE", "ACK", "CANCEL", "BYE", "OPTIONS"] {
        assert!(allow.contains(method), "{allow}");
    }
    assert!(answer.header("accept").unwrap().contains("application/sdp"));
}

#[tokio::test]
async fn over_the_call_limit_an_invite_gets_503_with_retry_after_and_a_slot_frees_up() {
    let mut rig = Rig::start(SipConfig {
        max_calls: 1,
        ..config()
    })
    .await;
    let Call {
        mut peer, session, ..
    } = rig.inbound().await;
    let mut second = Peer::new(rig.addr()).await;
    let offer = second.offer(&[0]);
    second.invite(&offer).await;
    let busy = second.final_response().await;
    assert_eq!(busy.status(), Some(503), "{busy:?}");
    assert!(busy.header("retry-after").is_some());
    second.ack(&busy).await;
    assert_eq!(rig.server.stats().rejected_busy, 1);

    // The first call ends; the slot is free again.
    assert_eq!(peer.bye().await.status(), Some(200));
    drop(session);
    rig.wait_for_active(0).await;
    second.call_id = "second-try".to_owned();
    let answer = second.call_up(&offer).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
}

#[tokio::test]
async fn a_full_accept_queue_refuses_new_calls_with_503() {
    let mut rig = Rig::start(SipConfig {
        accept_queue: 1,
        ..config()
    })
    .await;
    // The engine does not accept: the first call fills the queue.
    let mut first = Peer::new(rig.addr()).await;
    assert_eq!(first.call_up(&first.offer(&[0])).await.status(), Some(200));
    let mut second = Peer::new(rig.addr()).await;
    second.invite(&second.offer(&[0])).await;
    let busy = second.final_response().await;
    assert_eq!(busy.status(), Some(503));
    second.ack(&busy).await;
    // Accepting one frees the queue.
    let session = rig.accept().await;
    drop(session);
}

/// A host gate for the gated-server tests: refuses as told, and counts the reservations still
/// held (a [`Reservation`] gives its one back when dropped).
#[derive(Default)]
struct TestGate {
    refuse: std::sync::Mutex<Option<Refusal>>,
    held: Arc<AtomicUsize>,
    asked: AtomicUsize,
}

/// The reservation a [`TestGate`] hands out.
struct Reservation(Arc<AtomicUsize>);

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

impl AdmissionGate for TestGate {
    fn try_admit(&self) -> Result<AdmissionTicket, Refusal> {
        self.asked.fetch_add(1, AtomicOrdering::SeqCst);
        let refusal = *self
            .refuse
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        self.held.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(AdmissionTicket::new(Reservation(Arc::clone(&self.held))))
    }
}

impl TestGate {
    fn set(&self, refusal: Option<Refusal>) {
        *self.refuse.lock().unwrap() = refusal;
    }

    fn held(&self) -> usize {
        self.held.load(AtomicOrdering::SeqCst)
    }

    async fn wait_held(&self, expected: usize) {
        let wait = async {
            while self.held() != expected {
                pause(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("reservations held stayed at {}", self.held()));
    }
}

async fn gated_rig(gate: &Arc<TestGate>) -> Rig {
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let shared: SharedGate = Arc::clone(gate) as SharedGate;
    let server = SipServer::bind_gated(config(), clock.clone(), &parent, Some(shared))
        .await
        .unwrap();
    Rig {
        clock,
        parent,
        server,
    }
}

#[tokio::test]
async fn a_gated_server_answers_the_hosts_refusal_with_503_before_any_work() {
    let gate = Arc::new(TestGate::default());
    let rig = gated_rig(&gate).await;

    // Busy host: 503 with Retry-After (try again later).
    gate.set(Some(Refusal::Busy));
    let mut busy = Peer::new(rig.addr()).await;
    busy.invite(&busy.offer(&[0])).await;
    let answer = busy.final_response().await;
    assert_eq!(answer.status(), Some(503), "{answer:?}");
    assert!(answer.header("retry-after").is_some(), "{answer:?}");
    busy.ack(&answer).await;

    // Draining host: 503 without Retry-After (go to another instance).
    gate.set(Some(Refusal::ShuttingDown));
    let mut draining = Peer::new(rig.addr()).await;
    draining.invite(&draining.offer(&[0])).await;
    let answer = draining.final_response().await;
    assert_eq!(answer.status(), Some(503), "{answer:?}");
    assert!(answer.header("retry-after").is_none(), "{answer:?}");
    draining.ack(&answer).await;

    assert_eq!(gate.asked.load(AtomicOrdering::SeqCst), 2);
    let stats = stats_where(&rig, |s| s.rejected_busy == 2).await;
    assert_eq!(stats.calls_inbound, 0, "{stats:?}");
    assert_eq!(rig.server.active_calls(), 0);
}

#[tokio::test]
async fn an_admitted_invite_carries_the_hosts_ticket_and_a_failed_one_gives_it_back() {
    let gate = Arc::new(TestGate::default());
    let mut rig = gated_rig(&gate).await;

    // An INVITE the gate admits but the server refuses later (no common codec: 488) gives the
    // reservation back.
    let mut refused = Peer::new(rig.addr()).await;
    refused.invite(&refused.offer(&[18])).await;
    let answer = refused.final_response().await;
    assert_eq!(answer.status(), Some(488), "{answer:?}");
    refused.ack(&answer).await;
    gate.wait_held(0).await;
    assert_eq!(gate.asked.load(AtomicOrdering::SeqCst), 1);

    // An admitted call: the reservation is held through the answer and travels in the link.
    let mut peer = Peer::new(rig.addr()).await;
    let answer = peer.call_up(&peer.offer(&[0])).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let cancel = CancellationToken::new();
    let mut link = tokio::time::timeout(WAIT, rig.server.accept(&cancel))
        .await
        .expect("no call within the wait")
        .unwrap()
        .expect("server closed");
    assert_eq!(gate.held(), 1);
    let ticket = link
        .take_ticket()
        .expect("the link carries the host's ticket");
    assert!(link.take_ticket().is_none(), "the ticket is taken once");
    let reservation = ticket.downcast::<Reservation>().unwrap();
    assert_eq!(gate.held(), 1);
    drop(reservation);
    assert_eq!(gate.held(), 0);
    drop(link);
}

#[tokio::test]
async fn only_allowed_sources_are_served() {
    let blocked = Rig::start(SipConfig {
        allowed_sources: vec!["10.0.0.0/8".parse::<Cidr>().unwrap()],
        ..config()
    })
    .await;
    let mut peer = Peer::new(blocked.addr()).await;
    peer.cseq += 1;
    let options = peer.request_text("OPTIONS", "sip:x", peer.cseq, &[], None);
    peer.send_text(&options).await;
    assert!(
        peer.recv_within(Duration::from_millis(300)).await.is_none(),
        "answered a blocked source"
    );

    let allowed = Rig::start(SipConfig {
        allowed_sources: vec!["127.0.0.0/8".parse::<Cidr>().unwrap()],
        ..config()
    })
    .await;
    let mut peer = Peer::new(allowed.addr()).await;
    peer.cseq += 1;
    let options = peer.request_text("OPTIONS", "sip:x", peer.cseq, &[], None);
    peer.send_text(&options).await;
    assert_eq!(peer.final_response().await.status(), Some(200));
}

// ---------------------------------------------------------------------------------------------
// Digest authentication (inbound)

fn inbound_auth() -> InboundAuth {
    InboundAuth {
        realm: "brisvox.test".to_owned(),
        users: vec![("carrier".to_owned(), Secret::new("trunk-secret".to_owned()))],
        nonce_ttl: Duration::from_secs(10),
    }
}

fn digest_header(user: &str, password: &str, challenge: &str, uri: &str) -> String {
    let nonce = challenge
        .split("nonce=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let response = compute_digest(
        user,
        password,
        "brisvox.test",
        nonce,
        &Method::Invite,
        uri,
        Algorithm::Md5,
        None,
    );
    format!(
        "Digest username=\"{user}\", realm=\"brisvox.test\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm=MD5"
    )
}

#[tokio::test]
async fn an_inbound_invite_is_challenged_and_served_after_a_correct_digest() {
    let mut rig = Rig::start(SipConfig {
        inbound_auth: Some(inbound_auth()),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    let uri = format!("sip:agent@{}", peer.server);

    peer.invite_with(&[], "application/sdp", &offer).await;
    let challenge = peer.final_response().await;
    assert_eq!(challenge.status(), Some(401), "{challenge:?}");
    let www = challenge.header("www-authenticate").unwrap().to_owned();
    assert!(www.contains("realm=\"brisvox.test\""));
    peer.ack(&challenge).await;

    let auth = digest_header("carrier", "trunk-secret", &www, &uri);
    peer.invite_with(
        &[("Authorization", auth.clone())],
        "application/sdp",
        &offer,
    )
    .await;
    let answer = peer.final_response().await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    peer.to_tag = answer.to_tag();
    peer.ack(&answer).await;
    rig.accept().await;

    // The same Authorization cannot be replayed (the nonce is spent).
    let mut replay = Peer::new(rig.addr()).await;
    replay
        .invite_with(&[("Authorization", auth)], "application/sdp", &offer)
        .await;
    assert_eq!(replay.final_response().await.status(), Some(401));

    // A wrong password is refused for good.
    let mut thief = Peer::new(rig.addr()).await;
    thief.invite_with(&[], "application/sdp", &offer).await;
    let challenge = thief.final_response().await;
    let www = challenge.header("www-authenticate").unwrap().to_owned();
    thief.ack(&challenge).await;
    let bad = digest_header("carrier", "guess", &www, &uri);
    thief
        .invite_with(&[("Authorization", bad)], "application/sdp", &offer)
        .await;
    assert_eq!(thief.final_response().await.status(), Some(403));
    let stats = rig.server.stats();
    assert_eq!(stats.rejected_unauthorized, 4);
    assert_eq!(stats.calls_inbound, 1);
}

// ---------------------------------------------------------------------------------------------
// Outbound calls

/// The scripted callee (UAS).
struct Callee {
    sip: UdpSocket,
    rtp: UdpSocket,
    tag: String,
}

impl Callee {
    async fn new() -> Self {
        let sip = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Self {
            sip,
            rtp,
            tag: "callee-tag".to_owned(),
        }
    }

    fn uri(&self, user: &str) -> String {
        format!(
            "sip:{user}@{};transport=udp",
            self.sip.local_addr().unwrap()
        )
    }

    async fn recv(&self) -> (Msg, SocketAddr) {
        let mut buf = vec![0u8; 8192];
        let (n, from) = tokio::time::timeout(WAIT, self.sip.recv_from(&mut buf))
            .await
            .expect("no SIP message within the wait")
            .unwrap();
        (Msg::parse(&buf[..n]), from)
    }

    async fn request_of(&self, method: &str) -> (Msg, SocketAddr) {
        loop {
            let (msg, from) = self.recv().await;
            if msg.method() == Some(method) {
                return (msg, from);
            }
        }
    }

    async fn send(&self, text: &str, to: SocketAddr) {
        self.sip.send_to(text.as_bytes(), to).await.unwrap();
    }

    fn answer_sdp(&self, payload_type: u8) -> String {
        let name = if payload_type == 0 { "PCMU" } else { "PCMA" };
        format!(
            "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP {payload_type} 101\r\n\
             a=rtpmap:{payload_type} {name}/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=ptime:20\r\na=sendrecv\r\n",
            self.rtp.local_addr().unwrap().port()
        )
    }

    /// Answers an INVITE with 180 and 200 (SDP answer picking `payload_type`).
    async fn answer(&self, invite: &Msg, from: SocketAddr, payload_type: u8) {
        let contact = Some(self.sip.local_addr().unwrap());
        self.send(
            &response(invite, 180, "Ringing", Some(&self.tag), contact, &[], None),
            from,
        )
        .await;
        let sdp = self.answer_sdp(payload_type);
        self.send(
            &response(invite, 200, "OK", Some(&self.tag), contact, &[], Some(&sdp)),
            from,
        )
        .await;
    }
}

fn dial_request(callee: &Callee) -> DialRequest {
    DialRequest::new(callee.uri("+15551234567"), "sip:+15557654321@brisvox.test")
}

fn spawn_dial(
    handle: &SipHandle,
    request: DialRequest,
    cancel: &CancellationToken,
) -> tokio::task::JoinHandle<Result<CallLink, DialError>> {
    let handle = handle.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move { handle.dial(request, &cancel).await })
}

async fn joined(
    task: tokio::task::JoinHandle<Result<CallLink, DialError>>,
) -> Result<CallLink, DialError> {
    tokio::time::timeout(WAIT, task)
        .await
        .expect("dial did not finish")
        .unwrap()
}

#[tokio::test]
async fn an_outbound_call_sends_an_offer_and_carries_audio_both_ways() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let cancel = CancellationToken::new();
    let mut request = dial_request(&callee);
    request.headers = vec![(
        "P-Charge-Info".to_owned(),
        "sip:billing@brisvox.test".to_owned(),
    )];
    let task = spawn_dial(&rig.handle(), request, &cancel);

    let (invite, from) = callee.request_of("INVITE").await;
    assert!(
        invite.start.starts_with("INVITE sip:+15551234567@"),
        "{}",
        invite.start
    );
    assert!(
        invite
            .header("from")
            .unwrap()
            .contains("+15557654321@brisvox.test")
    );
    assert_eq!(invite.header("content-type"), Some("application/sdp"));
    assert_eq!(
        invite.header("p-charge-info"),
        Some("sip:billing@brisvox.test")
    );
    assert!(invite.header("max-forwards").is_some());
    let offer = invite.text();
    assert!(
        offer.contains(" RTP/AVP 0 8 101\r\n"),
        "PCMU first, then PCMA, then DTMF: {offer}"
    );
    assert!(offer.contains("a=rtpmap:101 telephone-event/8000\r\n"));
    assert!(offer.contains("a=ptime:20\r\n"));
    let server_rtp = sdp_rtp_addr(&offer);
    callee.answer(&invite, from, 0).await;

    let link = joined(task).await.unwrap();
    let ack = callee.request_of("ACK").await.0;
    assert_eq!(ack.cseq().0, invite.cseq().0);
    let mut session = rig.open(link).await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, invite.call_id());
    assert_eq!(info.metadata["direction"], "outbound");
    assert_eq!(info.metadata["sample_rate"], "8000");
    assert_eq!(info.metadata["codec"], "PCMU");
    assert_eq!(rig.server.stats().calls_outbound, 1);

    // Callee -> engine, and engine -> callee, over the advertised RTP address.
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    drop(sender);
    callee
        .rtp
        .send_to(
            &rtp_bytes(0, true, 7, 1000, 4242, &ulaw_payload(3)),
            server_rtp,
        )
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(3)).as_slice()
    );
    session
        .send(bot_frame(&rig.clock, 0, 0, 700, 160))
        .await
        .unwrap();
    let packet = recv_rtp(&callee.rtp).await;
    assert_eq!(packet.payload_type, 0);
    assert_eq!(packet.from, server_rtp);
    assert!(packet.marker);

    // The engine hangs up: BYE reaches the callee.
    session.send(TransportCommand::Stop).await.unwrap();
    let (bye, from) = callee.request_of("BYE").await;
    callee
        .send(&response(&bye, 200, "OK", None, None, &[], None), from)
        .await;
    assert_eq!(stopped(&mut session).await, StopReason::Requested);
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn an_outbound_call_ended_by_the_callee_reports_a_hang_up() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    callee.answer(&invite, from, 8).await;
    let link = joined(task).await.unwrap();
    let mut session = rig.open(link).await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.metadata["codec"], "PCMA");
    callee.request_of("ACK").await;

    // The callee sends BYE (in-dialog: To has our tag, From has theirs).
    let (cseq_invite, call_id) = (invite.cseq().0, invite.call_id());
    let our_from = invite.header("from").unwrap().to_owned();
    let bye = format!(
        "BYE sip:brisvox@{from} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bK-callee-bye\r\nMax-Forwards: 70\r\n\
         From: <sip:+15551234567@callee.test>;tag={}\r\nTo: {our_from}\r\nCall-ID: {call_id}\r\nCSeq: {} BYE\r\n\
         Content-Length: 0\r\n\r\n",
        callee.sip.local_addr().unwrap(),
        callee.tag,
        cseq_invite + 1
    );
    callee.send(&bye, from).await;
    let (ok, _) = callee.recv().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn a_challenged_outbound_call_is_retried_with_a_digest_response() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.credential = Some(DialCredential {
        username: "trunk-user".to_owned(),
        password: Secret::new("trunk-pass".to_owned()),
        realm: None,
    });
    let task = spawn_dial(&rig.handle(), request, &CancellationToken::new());

    let (first, from) = callee.request_of("INVITE").await;
    assert!(
        first.header("authorization").is_none(),
        "no credentials before the challenge"
    );
    let challenge =
        "Digest realm=\"trunk.test\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5"
            .to_owned();
    callee
        .send(
            &response(
                &first,
                401,
                "Unauthorized",
                Some(&callee.tag),
                None,
                &[("WWW-Authenticate", challenge)],
                None,
            ),
            from,
        )
        .await;
    callee.request_of("ACK").await;

    let (second, from) = callee.request_of("INVITE").await;
    assert_eq!(second.call_id(), first.call_id(), "same dialog");
    assert!(second.cseq().0 > first.cseq().0);
    let raw = second
        .header("authorization")
        .expect("the retry carries credentials")
        .to_owned();
    let auth = Authorization::parse(&raw).unwrap();
    assert_eq!(auth.username, "trunk-user");
    assert_eq!(auth.realm, "trunk.test");
    assert!(
        verify_digest(&auth, "trunk-pass", &Method::Invite, &raw),
        "digest does not verify: {raw}"
    );
    assert!(!verify_digest(&auth, "wrong", &Method::Invite, &raw));
    assert!(
        !raw.contains("trunk-pass"),
        "the password must never be sent"
    );
    callee.answer(&second, from, 0).await;
    let link = joined(task).await.unwrap();
    drop(rig.open(link).await);
}

#[tokio::test]
async fn an_unanswered_challenge_is_a_rejection_with_the_status() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    // No credentials: the 401 is the final answer.
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    let challenge = "Digest realm=\"trunk.test\", nonce=\"abc\", algorithm=MD5".to_owned();
    callee
        .send(
            &response(
                &invite,
                401,
                "Unauthorized",
                Some(&callee.tag),
                None,
                &[("WWW-Authenticate", challenge)],
                None,
            ),
            from,
        )
        .await;
    match joined(task).await {
        Err(DialError::Rejected { status, .. }) => assert_eq!(status, 401),
        other => panic!("expected a 401 rejection, got {:?}", other.map(|_| ())),
    }
    assert_eq!(rig.server.active_calls(), 0);
}

#[tokio::test]
async fn a_rejected_call_reports_the_sip_status() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    callee
        .send(
            &response(
                &invite,
                486,
                "Busy Here",
                Some(&callee.tag),
                None,
                &[],
                None,
            ),
            from,
        )
        .await;
    match joined(task).await {
        Err(DialError::Rejected { status, reason }) => {
            assert_eq!(status, 486);
            assert!(reason.contains("Busy"), "{reason}");
        }
        other => panic!("expected a 486 rejection, got {:?}", other.map(|_| ())),
    }
    callee.request_of("ACK").await;
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn cancelling_a_ringing_call_sends_cancel_and_the_487_is_acknowledged() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let cancel = CancellationToken::new();
    let task = spawn_dial(&rig.handle(), dial_request(&callee), &cancel);
    let (invite, from) = callee.request_of("INVITE").await;
    callee
        .send(
            &response(&invite, 180, "Ringing", Some(&callee.tag), None, &[], None),
            from,
        )
        .await;
    pause(Duration::from_millis(100)).await;
    cancel.cancel();
    match joined(task).await {
        Err(DialError::Cancelled) => {}
        other => panic!("expected Cancelled, got {:?}", other.map(|_| ())),
    }
    let (cancel_request, from) = callee.request_of("CANCEL").await;
    assert_eq!(cancel_request.call_id(), invite.call_id());
    callee
        .send(
            &response(&cancel_request, 200, "OK", None, None, &[], None),
            from,
        )
        .await;
    callee
        .send(
            &response(
                &invite,
                487,
                "Request Terminated",
                Some(&callee.tag),
                None,
                &[],
                None,
            ),
            from,
        )
        .await;
    let ack = callee.request_of("ACK").await.0;
    assert_eq!(ack.cseq().1, "ACK");
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn an_unanswered_call_is_cancelled_after_the_ring_timeout() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.ring_timeout = Duration::from_millis(400);
    let task = spawn_dial(&rig.handle(), request, &CancellationToken::new());
    let (invite, from) = callee.request_of("INVITE").await;
    callee
        .send(
            &response(&invite, 180, "Ringing", Some(&callee.tag), None, &[], None),
            from,
        )
        .await;
    match joined(task).await {
        Err(DialError::NoAnswer) => {}
        other => panic!("expected NoAnswer, got {:?}", other.map(|_| ())),
    }
    let (cancel_request, _) = callee.request_of("CANCEL").await;
    assert_eq!(cancel_request.call_id(), invite.call_id());
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn a_dial_key_makes_a_retried_request_safe() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.dial_key = Some("order-42".to_owned());
    let task = spawn_dial(&rig.handle(), request.clone(), &CancellationToken::new());
    let (invite, from) = callee.request_of("INVITE").await;
    // The same key again while the first is in flight, and after it: refused, no second INVITE.
    match rig
        .handle()
        .dial(request.clone(), &CancellationToken::new())
        .await
    {
        Err(DialError::DuplicateKey) => {}
        other => panic!("expected DuplicateKey, got {:?}", other.map(|_| ())),
    }
    callee.answer(&invite, from, 0).await;
    let link = joined(task).await.unwrap();
    drop(rig.open(link).await);
    match rig.handle().dial(request, &CancellationToken::new()).await {
        Err(DialError::DuplicateKey) => {}
        other => panic!("expected DuplicateKey, got {:?}", other.map(|_| ())),
    }
    let extra = tokio::time::timeout(Duration::from_millis(300), callee.request_of("INVITE")).await;
    assert!(
        extra.is_err(),
        "a duplicate dial must not send a second INVITE"
    );
}

#[tokio::test]
async fn invalid_dial_requests_and_the_call_limit_are_refused_before_anything_is_sent() {
    let rig = Rig::start(SipConfig {
        max_calls: 1,
        ..config()
    })
    .await;
    let handle = rig.handle();
    let cancel = CancellationToken::new();
    let mut bad = DialRequest::new("not a uri", "sip:a@b.test");
    assert!(matches!(
        handle.dial(bad.clone(), &cancel).await,
        Err(DialError::InvalidRequest(_))
    ));
    bad = DialRequest::new("sip:a@b.test", "sip:c@d.test");
    bad.headers = vec![("X-Evil".to_owned(), "a\r\nVia: x".to_owned())];
    assert!(matches!(
        handle.dial(bad, &cancel).await,
        Err(DialError::InvalidRequest(_))
    ));
    let mut zero = DialRequest::new("sip:a@b.test", "sip:c@d.test");
    zero.ring_timeout = Duration::ZERO;
    assert!(matches!(
        handle.dial(zero, &cancel).await,
        Err(DialError::InvalidRequest(_))
    ));

    // At the limit: the only slot is busy with an inbound call.
    let mut rig = rig;
    let Call { session, .. } = rig.inbound().await;
    let callee = Callee::new().await;
    assert!(matches!(
        handle.dial(dial_request(&callee), &cancel).await,
        Err(DialError::AtCapacity)
    ));
    drop(session);
}

#[tokio::test]
async fn a_bad_sdp_answer_ends_the_call_with_a_bye() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    let contact = Some(callee.sip.local_addr().unwrap());
    // The answer picks G.729, which was never offered.
    let sdp = "v=0\r\no=c 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 18\r\n";
    callee
        .send(
            &response(
                &invite,
                200,
                "OK",
                Some(&callee.tag),
                contact,
                &[],
                Some(sdp),
            ),
            from,
        )
        .await;
    match joined(task).await {
        Err(DialError::BadAnswer(_)) => {}
        other => panic!("expected BadAnswer, got {:?}", other.map(|_| ())),
    }
    let (bye, from) = callee.request_of("BYE").await;
    callee
        .send(&response(&bye, 200, "OK", None, None, &[], None), from)
        .await;
    rig.wait_for_active(0).await;
}

// ---------------------------------------------------------------------------------------------
// Inbound cancel, TCP, shutdown

#[tokio::test]
async fn a_cancel_while_ringing_ends_the_attempt_and_no_call_reaches_the_engine() {
    let mut rig = Rig::start(SipConfig {
        ring_time: Duration::from_secs(3),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    peer.invite(&offer).await;
    let ringing = peer.recv().await;
    assert!(matches!(ringing.status(), Some(100 | 180)), "{ringing:?}");
    // Wait for the 180, then cancel.
    let mut saw_180 = ringing.status() == Some(180);
    while !saw_180 {
        saw_180 = peer.recv().await.status() == Some(180);
    }
    let invite_cseq = peer.cseq;
    let uri = format!("sip:agent@{}", peer.server);
    let mut cancel = peer.request_text("CANCEL", &uri, invite_cseq, &[], None);
    // CANCEL carries the INVITE's branch.
    let invite_branch = format!("z9hG4bK-{}-1", peer.sip_addr().port());
    let cancel_branch = format!("z9hG4bK-{}-2", peer.sip_addr().port());
    cancel = cancel.replace(&cancel_branch, &invite_branch);
    peer.send_text(&cancel).await;
    let mut got_487 = false;
    let mut got_cancel_ok = false;
    while !(got_487 && got_cancel_ok) {
        let msg = peer.recv().await;
        match (msg.status(), msg.cseq().1.as_str()) {
            (Some(487), "INVITE") => {
                got_487 = true;
                peer.ack(&msg).await;
            }
            (Some(200), "CANCEL") => got_cancel_ok = true,
            _ => {}
        }
    }
    let nothing = tokio::time::timeout(
        Duration::from_millis(300),
        rig.server.accept(&CancellationToken::new()),
    )
    .await;
    assert!(nothing.is_err(), "a cancelled call reached the engine");
    rig.wait_for_active(0).await;
    assert_eq!(rig.server.stats().cancelled_before_answer, 1);
    assert_eq!(rig.server.stats().calls_inbound, 0);
}

#[tokio::test]
async fn a_call_that_is_never_acknowledged_is_given_up() {
    let mut rig = Rig::start(SipConfig {
        ack_timeout: Duration::from_millis(500),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    peer.invite(&offer).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200));
    // No ACK. The server stops waiting and ends the dialog with a BYE.
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    assert_eq!(rig.server.stats().ack_timeouts, 1);
    rig.wait_for_active(0).await;
    let nothing = tokio::time::timeout(
        Duration::from_millis(200),
        rig.server.accept(&CancellationToken::new()),
    )
    .await;
    assert!(nothing.is_err());
}

async fn read_tcp_message(stream: &mut TcpStream) -> Msg {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(WAIT, stream.read(&mut chunk))
            .await
            .expect("no TCP data within the wait")
            .unwrap();
        assert!(n > 0, "connection closed");
        data.extend_from_slice(&chunk[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
            if data.len() >= end + 4 + length {
                return Msg::parse(&data);
            }
        }
    }
}

#[tokio::test]
async fn signaling_works_over_tcp_too() {
    let mut rig = Rig::start(SipConfig {
        tcp: true,
        ..config()
    })
    .await;
    let addr = rig.addr();
    // The listener starts with the endpoint task: retry the connect briefly.
    let mut stream = {
        let mut attempts = 0;
        loop {
            let connect = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr));
            match connect
                .await
                .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
            {
                Ok(stream) => break stream,
                Err(_) if attempts < 10 => {
                    attempts += 1;
                    pause(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("tcp connect failed: {error}"),
            }
        }
    };
    let local = stream.local_addr().unwrap();
    let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sdp = format!(
        "v=0\r\no=p 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n",
        rtp.local_addr().unwrap().port()
    );
    let invite = format!(
        "INVITE sip:agent@{addr} SIP/2.0\r\nVia: SIP/2.0/TCP {local};branch=z9hG4bK-tcp-1\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=tcp1\r\nTo: <sip:agent@example.com>\r\nCall-ID: tcp-call\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:alice@{local};transport=tcp>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    stream.write_all(invite.as_bytes()).await.unwrap();
    let ok = loop {
        let msg = read_tcp_message(&mut stream).await;
        if msg.status().is_some_and(|s| s >= 200) {
            break msg;
        }
    };
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    // The peer's in-dialog requests go to the Contact: it has to name TCP.
    let contact = ok.header("contact").unwrap();
    assert!(
        contact.to_ascii_lowercase().contains("transport=tcp"),
        "{contact}"
    );
    let tag = ok.to_tag().unwrap();
    let ack = format!(
        "ACK sip:agent@{addr} SIP/2.0\r\nVia: SIP/2.0/TCP {local};branch=z9hG4bK-tcp-2\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=tcp1\r\nTo: <sip:agent@example.com>;tag={tag}\r\nCall-ID: tcp-call\r\nCSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    );
    stream.write_all(ack.as_bytes()).await.unwrap();
    let mut session = rig.accept().await;
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert!(matches!(started, TransportEvent::Started(_)));
    // Audio works as usual.
    let dest = sdp_rtp_addr(&ok.text());
    rtp.send_to(&rtp_bytes(0, true, 1, 0, 5, &ulaw_payload(2)), dest)
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(frame.sample_count(), 160);
    // An in-dialog request on the same connection: SIP INFO with a DTMF key.
    let info = format!(
        "INFO sip:agent@{addr} SIP/2.0\r\nVia: SIP/2.0/TCP {local};branch=z9hG4bK-tcp-3\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=tcp1\r\nTo: <sip:agent@example.com>;tag={tag}\r\nCall-ID: tcp-call\r\nCSeq: 2 INFO\r\n\
         Content-Type: application/dtmf-relay\r\nContent-Length: 24\r\n\r\nSignal=9\r\nDuration=160\r\n"
    );
    stream.write_all(info.as_bytes()).await.unwrap();
    let info_ok = loop {
        let msg = read_tcp_message(&mut stream).await;
        if msg.status().is_some_and(|s| s >= 200) && msg.cseq().1 == "INFO" {
            break msg;
        }
    };
    assert_eq!(info_ok.status(), Some(200), "{info_ok:?}");
    let dtmf = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    assert!(matches!(
        dtmf,
        TransportEvent::Dtmf {
            digit: DtmfDigit::D9,
            ..
        }
    ));
    // The engine hangs up: a BYE arrives on the same connection.
    session.send(TransportCommand::Stop).await.unwrap();
    let bye = loop {
        let msg = read_tcp_message(&mut stream).await;
        if msg.method() == Some("BYE") {
            break msg;
        }
    };
    let reply = response(&bye, 200, "OK", None, None, &[], None);
    stream.write_all(reply.as_bytes()).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::Requested);
}

#[tokio::test]
async fn shutdown_hangs_up_every_call_and_leaks_no_task() {
    let clock = SystemClock::shared();
    let tasks = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    let mut rig = Rig::default().await;
    let Call {
        peer, mut session, ..
    } = rig.inbound().await;
    let server = rig.server;
    let shutdown = tokio::spawn(async move {
        let result = server.shutdown(Duration::from_secs(5)).await;
        (result, server)
    });
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    let (result, server) = tokio::time::timeout(WAIT, shutdown).await.unwrap().unwrap();
    result.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    // New calls are refused after shutdown (no one listens any more).
    assert_eq!(server.active_calls(), 0);
    drop(server);
    drop(session);
    rig.parent.cancel();
    tasks
        .wait_for_baseline(&*clock, Duration::from_secs(3))
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_configurations_are_refused_with_a_reason() {
    let cases: Vec<(SipConfig, &str)> = vec![
        (SipConfig::default(), "public_ip"),
        (
            SipConfig {
                rtp_ports: Some((0, 10)),
                ..config()
            },
            "rtp_ports",
        ),
        (
            SipConfig {
                rtp_ports: Some((20, 10)),
                ..config()
            },
            "rtp_ports",
        ),
        (
            SipConfig {
                codecs: vec![],
                ..config()
            },
            "codecs",
        ),
        (
            SipConfig {
                codecs: vec![G711::Pcmu, G711::Pcmu],
                ..config()
            },
            "codecs",
        ),
        (
            SipConfig {
                max_calls: 0,
                ..config()
            },
            "max_calls",
        ),
        (
            SipConfig {
                accept_queue: 0,
                ..config()
            },
            "accept_queue",
        ),
        (
            SipConfig {
                rtp_timeout: Duration::ZERO,
                ..config()
            },
            "rtp_timeout",
        ),
        (
            SipConfig {
                reorder_depth: 0,
                ..config()
            },
            "reorder_depth",
        ),
        (
            SipConfig {
                inbound_auth: Some(InboundAuth {
                    users: vec![],
                    ..inbound_auth()
                }),
                ..config()
            },
            "inbound_auth",
        ),
    ];
    for (config, needle) in cases {
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        assert!(
            SipServer::bind(config, SystemClock::shared(), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    config().validate().unwrap();
}

#[tokio::test]
async fn rtp_ports_come_from_the_configured_range() {
    // A range of 400 ports below the OS's ephemeral range, placed by a port the OS just handed
    // out (so parallel runs differ). Ports that are taken or excluded by the OS are skipped.
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let base = probe.local_addr().unwrap().port();
    drop(probe);
    let low = 21_000 + (base % 20_000) / 2 * 2;
    let mut rig = Rig::start(SipConfig {
        rtp_ports: Some((low, low + 399)),
        ..config()
    })
    .await;
    let mut ports = Vec::new();
    let mut calls = Vec::new();
    for _ in 0..2 {
        let call = rig.inbound().await;
        ports.push(call.dest.port());
        calls.push(call);
    }
    for port in &ports {
        assert!(
            (low..=low + 399).contains(port),
            "{port} outside {low}..={}",
            low + 399
        );
        assert_eq!(port % 2, 0, "RTP ports are even");
    }
    assert_ne!(ports[0], ports[1]);
}

#[tokio::test]
async fn secrets_stay_out_of_debug_output() {
    let config = SipConfig {
        inbound_auth: Some(inbound_auth()),
        ..config()
    };
    let text = format!("{config:?}");
    assert!(!text.contains("trunk-secret"), "{text}");
    assert!(text.contains("carrier"));
}

// ---------------------------------------------------------------------------------------------
// Review fixes: latching, hold, re-INVITE, pacing, backlog, limits

/// An SDP for a re-INVITE from the scripted peer: audio at `port` on `address`, session version
/// `version`, with `direction`.
fn media_sdp(port: u16, version: u32, direction: &str, address: &str) -> String {
    format!(
        "v=0\r\no=peer 1 {version} IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 {address}\r\nt=0 0\r\nm=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na={direction}\r\n"
    )
}

/// A re-INVITE with `sdp`: waits for the 200 and sends the ACK.
async fn reinvite(peer: &mut Peer, sdp: &str) -> Msg {
    peer.cseq += 1;
    let uri = format!("sip:agent@{}", peer.server);
    let text = peer.request_text(
        "INVITE",
        &uri,
        peer.cseq,
        &[],
        Some(("application/sdp", sdp)),
    );
    peer.send_text(&text).await;
    let ok = peer.final_response_to("INVITE").await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    peer.ack(&ok).await;
    ok
}

fn latch_config() -> SipConfig {
    SipConfig {
        rtp_latch: true,
        ..config()
    }
}

#[tokio::test]
async fn latching_never_follows_a_packet_that_fails_the_stream_checks() {
    let mut rig = Rig::start(latch_config()).await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    // The real stream, from the SDP address: SSRC locked, latch window closed.
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;

    // A third host sends RTP to the call's port: another SSRC, and the peer's own SSRC.
    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for packet in [
        rtp_bytes(0, false, 500, 0, 9, &ulaw_payload(2)),
        rtp_bytes(0, false, peer.seq, peer.ts, peer.ssrc, &ulaw_payload(2)),
    ] {
        intruder.send_to(&packet, dest).await.unwrap();
    }
    stats_where(&rig, |s| s.rtp_foreign_source >= 2).await;
    pause(Duration::from_millis(50)).await;
    assert_eq!(rig.server.stats().rtp_foreign_source, 2);

    // Bot audio still goes to the real peer, and nothing reaches the intruder.
    session
        .send(bot_frame(&rig.clock, 0, 0, 300, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
    assert!(
        recv_rtp_within(&intruder, Duration::from_millis(100))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_rejected_packet_does_not_move_the_media_destination_while_the_window_is_open() {
    let mut rig = Rig::start(latch_config()).await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;
    // A session refresh (same media) opens the latch window again, the SSRC stays locked.
    let refresh = media_sdp(peer.rtp_port(), 2, "sendrecv", "127.0.0.1");
    reinvite(&mut peer, &refresh).await;

    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    intruder
        .send_to(&rtp_bytes(0, false, 500, 0, 9, &ulaw_payload(2)), dest)
        .await
        .unwrap();
    stats_where(&rig, |s| s.rtp_foreign_ssrc >= 1).await;
    pause(Duration::from_millis(50)).await;
    let stats = rig.server.stats();
    assert_eq!(stats.rtp_foreign_ssrc, 1, "{stats:?}");
    assert_eq!(stats.rtp_foreign_source, 0, "{stats:?}");

    session
        .send(bot_frame(&rig.clock, 0, 0, 300, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
    assert!(
        recv_rtp_within(&intruder, Duration::from_millis(100))
            .await
            .is_none(),
        "the media destination followed a packet that was rejected"
    );
}

#[tokio::test]
async fn latching_is_only_possible_in_the_window_after_the_call_is_up() {
    let mut rig = Rig::start(SipConfig {
        rtp_latch: true,
        rtp_latch_window: Duration::from_millis(300),
        ..config()
    })
    .await;
    let Call {
        peer,
        session,
        dest,
        ..
    } = rig.inbound().await;
    pause(Duration::from_millis(500)).await;
    // Too late: media from an address other than the SDP's is foreign again.
    let nat = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    nat.send_to(&rtp_bytes(0, true, 1, 0, 77, &ulaw_payload(5)), dest)
        .await
        .unwrap();
    stats_where(&rig, |s| s.rtp_foreign_source >= 1).await;
    pause(Duration::from_millis(50)).await;
    assert_eq!(rig.server.stats().rtp_foreign_source, 1);
    session
        .send(bot_frame(&rig.clock, 0, 0, 300, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
    assert!(
        recv_rtp_within(&nat, Duration::from_millis(100))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_call_on_hold_is_not_hung_up_for_silence_and_the_deadline_restarts_on_resume() {
    let mut rig = Rig::start(SipConfig {
        rtp_timeout: Duration::from_millis(400),
        ..config()
    })
    .await;
    let Call {
        mut peer,
        mut session,
        dest,
        call_id,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;

    // The peer puts the call on hold (the legacy form, c=0.0.0.0) and sends nothing.
    let hold = media_sdp(peer.rtp_port(), 2, "sendonly", "0.0.0.0");
    reinvite(&mut peer, &hold).await;
    pause(Duration::from_millis(1100)).await; // nearly three times the RTP timeout
    assert_eq!(rig.server.active_calls(), 1);
    assert_eq!(rig.server.stats().rtp_timeouts, 0);
    assert!(
        peer.recv_within(Duration::from_millis(50)).await.is_none(),
        "the server hung up a call on hold"
    );

    // A DTMF key asked for during the hold is not sent, and that is counted.
    rig.handle()
        .send_dtmf(&call_id, DtmfDigit::D5, Duration::from_millis(100))
        .unwrap();
    stats_where(&rig, |s| s.dtmf_dropped >= 1).await;
    pause(Duration::from_millis(50)).await;
    let stats = rig.server.stats();
    assert_eq!((stats.dtmf_out, stats.dtmf_dropped), (0, 1), "{stats:?}");

    // Resumed: the silence deadline counts from now, not from before the hold.
    let resume = media_sdp(peer.rtp_port(), 3, "sendrecv", "127.0.0.1");
    reinvite(&mut peer, &resume).await;
    let resumed = rig.clock.now();
    let bye = peer.request_of("BYE").await;
    assert!(
        (rig.clock.now() - resumed) >= Duration::from_millis(300),
        "hung up {:?} after the resume",
        (rig.clock.now() - resumed)
    );
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    assert_eq!(rig.server.stats().rtp_timeouts, 1);
}

#[tokio::test]
async fn after_a_reinvite_the_new_media_stream_is_heard_at_once() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;

    // The media moves to a new server: new address, new SSRC, new sequence numbers.
    let moved = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let moved_port = moved.local_addr().unwrap().port();
    reinvite(
        &mut peer,
        &media_sdp(moved_port, 2, "sendrecv", "127.0.0.1"),
    )
    .await;
    moved
        .send_to(
            &rtp_bytes(0, true, 7000, 90_000, 0xBEEF, &ulaw_payload(3)),
            dest,
        )
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(3)).as_slice()
    );

    // Hold, then resume with yet another SSRC.
    reinvite(&mut peer, &media_sdp(moved_port, 3, "sendonly", "0.0.0.0")).await;
    reinvite(
        &mut peer,
        &media_sdp(moved_port, 4, "sendrecv", "127.0.0.1"),
    )
    .await;
    moved
        .send_to(&rtp_bytes(0, true, 1, 5, 0xCAFE, &ulaw_payload(4)), dest)
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(4)).as_slice()
    );
    assert_eq!(rig.server.stats().rtp_foreign_ssrc, 0);
}

#[tokio::test]
async fn an_outbound_answer_listing_two_codecs_may_be_heard_in_either() {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    let server_rtp = sdp_rtp_addr(&invite.text());
    let sdp = format!(
        "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 8 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        callee.rtp.local_addr().unwrap().port()
    );
    let contact = Some(callee.sip.local_addr().unwrap());
    let ok = response(
        &invite,
        200,
        "OK",
        Some(&callee.tag),
        contact,
        &[],
        Some(&sdp),
    );
    callee.send(&ok, from).await;
    let link = joined(task).await.unwrap();
    let mut session = rig.open(link).await;

    // The callee sends PCMA (the second format of its own answer), then PCMU.
    let pcm: Vec<i16> = vec![300; 160];
    let mut alaw = vec![0u8; 160];
    Codec::Alaw.encode_samples(&pcm, &mut alaw).unwrap();
    callee
        .rtp
        .send_to(&rtp_bytes(8, true, 1, 0, 77, &alaw), server_rtp)
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    let mut expected = vec![0u8; 320];
    Codec::Alaw.decode(&alaw, &mut expected).unwrap();
    assert_eq!(frame.data().as_ref(), expected.as_slice());
    callee
        .rtp
        .send_to(
            &rtp_bytes(0, false, 2, 160, 77, &ulaw_payload(2)),
            server_rtp,
        )
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(2)).as_slice()
    );
    assert_eq!(rig.server.stats().rtp_ignored, 0);

    // We send the first format of the answer.
    session
        .send(bot_frame(&rig.clock, 0, 0, 700, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&callee.rtp).await.payload_type, 0);
}

#[tokio::test]
async fn rtp_packets_leave_20_ms_apart_not_in_a_burst() {
    let mut rig = Rig::default().await;
    let Call { peer, session, .. } = rig.inbound().await;
    // One second of audio in one frame; the first 26 packets are timed.
    session
        .send(bot_frame(&rig.clock, 0, 0, 1000, 8000))
        .await
        .unwrap();
    let mut arrivals = Vec::new();
    for _ in 0..26 {
        recv_rtp(&peer.rtp).await;
        arrivals.push(rig.clock.now());
    }
    let span = arrivals[25] - arrivals[0];
    // 25 gaps of 20 ms: 500 ms. A burst would take a few milliseconds. The lower bound is the
    // one that detects a burst; the upper bound only has to survive a loaded CI machine (a
    // scheduling stall on one gap must not fail the test), so the typical gap is judged by its
    // median instead.
    assert!(
        span >= Duration::from_millis(430) && span <= Duration::from_millis(1500),
        "26 packets took {span:?}"
    );
    let mut gaps: Vec<Duration> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort();
    let median = gaps[gaps.len() / 2];
    assert!(
        median >= Duration::from_millis(12) && median <= Duration::from_millis(40),
        "the median gap is {median:?}"
    );
    // No run of six packets arrives in under 50 ms (100 ms at 20 ms; a coarse timer may catch
    // up on one slot).
    for window in arrivals.windows(6) {
        let five = window[5] - window[0];
        assert!(five >= Duration::from_millis(50), "a burst: {five:?}");
    }
}

#[tokio::test]
async fn dtmf_event_packets_are_paced_like_audio() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        session,
        call_id,
        ..
    } = rig.inbound().await;
    rig.handle()
        .send_dtmf(&call_id, DtmfDigit::D3, Duration::from_millis(200))
        .unwrap();
    // 200 ms = 10 packets: nine growing ones, then the end packet three times.
    let mut arrivals = Vec::new();
    for _ in 0..9 + 3 {
        let packet = recv_rtp(&peer.rtp).await;
        assert_eq!(packet.payload_type, 101);
        arrivals.push(rig.clock.now());
    }
    let span = arrivals[11] - arrivals[0];
    assert!(
        span >= Duration::from_millis(190) && span <= Duration::from_millis(1000),
        "12 event packets took {span:?}"
    );
    drop(session);
}

#[tokio::test]
async fn a_clear_is_seen_at_once_even_with_seconds_of_audio_queued() {
    let mut rig = Rig::default().await;
    let Call { peer, session, .. } = rig.inbound().await;
    let clock = rig.clock.clone();
    // Three seconds in one frame: 150 packets queued.
    session
        .send(bot_frame(&clock, 0, 0, 1000, 24_000))
        .await
        .unwrap();
    let old_payload = recv_rtp(&peer.rtp).await.payload;
    for _ in 0..2 {
        recv_rtp(&peer.rtp).await;
    }
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    session
        .send(bot_frame(&clock, 1, 1, -2000, 160))
        .await
        .unwrap();
    // A few packets may be in flight; the ~140 still queued must not follow.
    let mut trailing_old = 0;
    let found = loop {
        let packet = recv_rtp(&peer.rtp).await;
        if packet.payload == old_payload {
            trailing_old += 1;
            assert!(
                trailing_old <= 4,
                "the backlog was not dropped by the clear"
            );
            continue;
        }
        break packet;
    };
    assert!(found.marker);
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(rig.server.stats().dropped_on_clear >= 100);
}

#[tokio::test]
async fn audio_of_a_stale_epoch_is_dropped_and_a_dtmf_tone_survives_a_clear() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        session,
        call_id,
        ..
    } = rig.inbound().await;
    let clock = rig.clock.clone();
    // Epoch 1 starts; late audio of epoch 0 must not go out. (The core already filters it; the
    // adapter's own epoch check is a second line that the public API cannot reach.)
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    session
        .send(bot_frame(&clock, 0, 0, 500, 160))
        .await
        .unwrap();
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(200))
            .await
            .is_none(),
        "a stale-epoch frame was sent"
    );
    session
        .send(bot_frame(&clock, 1, 1, 500, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);

    // A tone queued behind audio is kept when the audio is cleared.
    session
        .send(bot_frame(&clock, 1, 2, 500, 8000))
        .await
        .unwrap();
    pause(Duration::from_millis(30)).await;
    rig.handle()
        .send_dtmf(&call_id, DtmfDigit::D2, Duration::from_millis(100))
        .unwrap();
    pause(Duration::from_millis(30)).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(2),
        })
        .await
        .unwrap();
    let mut events = 0;
    let mut audio_after_first_event = 0;
    while let Some(packet) = recv_rtp_within(&peer.rtp, Duration::from_millis(400)).await {
        if packet.payload_type == 101 {
            events += 1;
        } else if events > 0 {
            audio_after_first_event += 1;
        }
    }
    assert_eq!(events, 4 + 3, "the whole tone is sent");
    assert_eq!(audio_after_first_event, 0, "the cleared audio came back");
}

#[tokio::test]
async fn a_frame_that_does_not_fit_the_send_queue_is_dropped_and_counted() {
    let mut rig = Rig::default().await;
    let clock = rig.clock.clone();
    // 11 s = 550 packets: more than the 500 the queue holds.
    let Call { peer, session, .. } = rig.inbound().await;
    session
        .send(bot_frame(&clock, 0, 0, 500, 88_000))
        .await
        .unwrap();
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(200))
            .await
            .is_none()
    );
    assert_eq!(rig.server.stats().dropped_audio_frames, 1);

    // 9.9 s fit (495 packets); a one-second tone behind them (52 more) does not, and is counted.
    let Call {
        peer,
        session,
        call_id,
        ..
    } = rig.inbound().await;
    session
        .send(bot_frame(&clock, 0, 0, 500, 79_200))
        .await
        .unwrap();
    pause(Duration::from_millis(100)).await;
    rig.handle()
        .send_dtmf(&call_id, DtmfDigit::D1, Duration::from_secs(1))
        .unwrap();
    pause(Duration::from_millis(100)).await;
    let stats = rig.server.stats();
    assert_eq!(stats.dropped_audio_frames, 1, "{stats:?}");
    assert_eq!(stats.dtmf_dropped, 1, "{stats:?}");
    assert_eq!(stats.dtmf_out, 0, "{stats:?}");
    // The audio that fit is being sent, and no tone packet is among it.
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
}

#[tokio::test]
async fn an_engine_that_takes_no_events_gets_the_call_hung_up() {
    let mut rig = Rig::start(SipConfig {
        send_timeout: Duration::from_millis(300),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let answer = peer.call_up(&peer.offer(&[0, 101])).await;
    assert_eq!(answer.status(), Some(200));
    let dest = sdp_rtp_addr(&answer.text());
    // The engine accepts the call but never reads its events. Key presses cannot be dropped
    // like audio: past the queue's 64 places the adapter waits, then gives up.
    let _link = tokio::time::timeout(WAIT, rig.server.accept(&CancellationToken::new()))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for i in 0..100u16 {
        let payload = [1u8, 0x80 | 10, 0, 160];
        let packet = rtp_bytes(
            101,
            i == 0,
            peer.seq.wrapping_add(i),
            peer.ts.wrapping_add(u32::from(i) * 1600),
            peer.ssrc,
            &payload,
        );
        peer.send_rtp_raw(dest, &packet).await;
        pause(Duration::from_millis(2)).await;
    }
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    rig.wait_for_active(0).await;
}

#[tokio::test]
async fn a_flood_of_refused_invites_does_not_starve_other_requests() {
    let rig = Rig::default().await;
    let mut flooder = Peer::new(rig.addr()).await;
    for batch in 0..12 {
        for i in 0..50 {
            flooder.call_id = format!("flood-{batch}-{i}@peer.test");
            // 415: refused at once, never acknowledged by the flooder.
            flooder.invite_with(&[], "text/plain", "x").await;
        }
        pause(Duration::from_millis(5)).await;
    }
    pause(Duration::from_millis(100)).await;
    // Another host is served right away (the slots of the refusals are free).
    let mut other = Peer::new(rig.addr()).await;
    other.cseq += 1;
    let options = other.request_text("OPTIONS", "sip:x", other.cseq, &[], None);
    other.send_text(&options).await;
    assert_eq!(other.final_response_to("OPTIONS").await.status(), Some(200));
    assert_eq!(rig.server.active_calls(), 0);
}

#[tokio::test]
async fn in_dialog_requests_beyond_the_handler_limit_are_dropped_not_queued() {
    let mut rig = Rig::start(SipConfig {
        ack_timeout: Duration::from_secs(2),
        ..config()
    })
    .await;
    let Call { mut peer, .. } = rig.inbound().await;
    // Re-INVITEs the peer never acknowledges: each handler waits for its ACK. Past 512 of them
    // the server drops requests instead of starting tasks.
    let sdp = media_sdp(peer.rtp_port(), 2, "sendrecv", "127.0.0.1");
    let uri = format!("sip:agent@{}", peer.server);
    for _ in 0..14 {
        for _ in 0..50 {
            peer.cseq += 1;
            let text = peer.request_text(
                "INVITE",
                &uri,
                peer.cseq,
                &[],
                Some(("application/sdp", &sdp)),
            );
            peer.send_text(&text).await;
        }
        pause(Duration::from_millis(20)).await;
    }
    pause(Duration::from_millis(300)).await;
    let busy = rig.server.stats().rejected_busy;
    assert!(busy > 0, "no request was turned away: {busy}");
    // The call itself is fine.
    assert_eq!(rig.server.active_calls(), 1);
}

#[tokio::test]
async fn unsupported_required_extensions_get_420_and_a_stray_cancel_gets_481() {
    let rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    peer.invite_with(
        &[("Require", "timer, 100rel".to_owned())],
        "application/sdp",
        &offer,
    )
    .await;
    let answer = peer.final_response().await;
    assert_eq!(answer.status(), Some(420), "{answer:?}");
    let unsupported = answer.header("unsupported").unwrap();
    assert!(
        unsupported.contains("timer") && unsupported.contains("100rel"),
        "{unsupported}"
    );
    peer.ack(&answer).await;
    assert_eq!(rig.server.active_calls(), 0);

    // A CANCEL for an INVITE nobody knows.
    peer.call_id = "stray-cancel".to_owned();
    let uri = format!("sip:agent@{}", peer.server);
    let cancel = peer.request_text("CANCEL", &uri, 7, &[], None);
    peer.send_text(&cancel).await;
    assert_eq!(peer.final_response_to("CANCEL").await.status(), Some(481));
}

#[tokio::test]
async fn allowed_sources_also_decide_who_may_answer_our_own_calls() {
    // The callee is on 127.0.0.1, outside the allowed ranges: its answers are dropped unread.
    let rig = Rig::start(SipConfig {
        allowed_sources: vec!["10.0.0.0/8".parse::<Cidr>().unwrap()],
        ..config()
    })
    .await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.ring_timeout = Duration::from_millis(700);
    let task = spawn_dial(&rig.handle(), request, &CancellationToken::new());
    let (invite, from) = callee.request_of("INVITE").await;
    callee.answer(&invite, from, 0).await;
    assert!(matches!(joined(task).await, Err(DialError::NoAnswer)));

    // With the callee's network in the list, the same call works.
    let rig = Rig::start(SipConfig {
        allowed_sources: vec!["127.0.0.0/8".parse::<Cidr>().unwrap()],
        ..config()
    })
    .await;
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    callee.answer(&invite, from, 0).await;
    joined(task).await.unwrap();
}

#[tokio::test]
async fn an_outbound_call_over_tcp_names_tcp_in_its_contact() {
    let rig = Rig::start(SipConfig {
        tcp: true,
        ..config()
    })
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let mut request = DialRequest::new(
        format!("sip:+15551234567@{target};transport=tcp"),
        "sip:+15557654321@brisvox.test",
    );
    request.ring_timeout = Duration::from_millis(800);
    let task = spawn_dial(&rig.handle(), request, &CancellationToken::new());
    let (mut stream, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let invite = read_tcp_message(&mut stream).await;
    assert_eq!(invite.method(), Some("INVITE"));
    let contact = invite.header("contact").unwrap();
    assert!(
        contact.to_ascii_lowercase().contains("transport=tcp"),
        "{contact}"
    );
    assert!(matches!(joined(task).await, Err(DialError::NoAnswer)));
}

// ---------------------------------------------------------------------------------------------
// Review fixes, second round: coverage of documented behaviour that no test pinned

/// The session version of the `o=` line of an SDP.
fn o_version(sdp: &str) -> u64 {
    let line = sdp.lines().find(|l| l.starts_with("o=")).unwrap();
    line.split(' ').nth(2).unwrap().parse().unwrap()
}

/// The ACK of a 2xx to a re-INVITE, with `body` as the SDP answer (a delayed offer) or without.
async fn ack_with_body(peer: &Peer, ok: &Msg, body: Option<&str>) {
    let uri = format!("sip:agent@{}", peer.server);
    let branch = format!("z9hG4bK-ack-{}", ok.cseq().0);
    let mut to = "<sip:agent@example.com>".to_owned();
    if let Some(tag) = ok.to_tag() {
        to.push_str(&format!(";tag={tag}"));
    }
    let mut text = format!(
        "ACK {uri} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch={branch}\r\nMax-Forwards: 70\r\n\
         From: \"Alice\" <sip:alice@example.com>;tag={}\r\nTo: {to}\r\nCall-ID: {}\r\nCSeq: {} ACK\r\n",
        peer.sip_addr(),
        peer.from_tag,
        peer.call_id,
        ok.cseq().0
    );
    match body {
        Some(body) => text.push_str(&format!(
            "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )),
        None => text.push_str("Content-Length: 0\r\n\r\n"),
    }
    peer.send_text(&text).await;
}

/// A request inside the dialog of `peer`; waits for the final answer.
async fn in_dialog(
    peer: &mut Peer,
    method: &str,
    extra: &[(&str, String)],
    body: Option<(&str, &str)>,
) -> Msg {
    peer.cseq += 1;
    let uri = format!("sip:agent@{}", peer.server);
    let text = peer.request_text(method, &uri, peer.cseq, extra, body);
    peer.send_text(&text).await;
    peer.final_response_to(method).await
}

/// An RFC 4733 telephone-event packet.
#[allow(clippy::too_many_arguments)] // the fields of the packet, spelled out at every call
fn event_packet(
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    code: u8,
    end: bool,
    duration: u16,
) -> Vec<u8> {
    let payload = [
        code,
        if end { 0x80 | 10 } else { 10 },
        (duration >> 8) as u8,
        duration as u8,
    ];
    rtp_bytes(payload_type, marker, sequence, timestamp, ssrc, &payload)
}

/// A low, even port such that `count` even ports from it were free a moment ago.
async fn free_even_range(count: u16) -> u16 {
    static BLOCKS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    assert!(count <= 8);
    for _ in 0..200 {
        let block = BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid_block = (std::process::id() % 400) as u16;
        let low = 42_000 + pid_block * 16 + block * 16;
        let mut held = Vec::new();
        for k in 0..count {
            match UdpSocket::bind(("127.0.0.1", low + 2 * k)).await {
                Ok(socket) => held.push(socket),
                Err(_) => break,
            }
        }
        if held.len() == usize::from(count) {
            return low;
        }
    }
    panic!("no free port range found");
}

// -- RTP silence timeout ----------------------------------------------------------------------

#[tokio::test]
async fn a_live_stream_keeps_the_call_up_for_longer_than_the_rtp_timeout() {
    let mut rig = Rig::start(SipConfig {
        rtp_timeout: Duration::from_millis(400),
        ..config()
    })
    .await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    // Audio every 100 ms for 1.5 s: almost four timeouts. Any packet from the peer is life.
    for i in 0..15i16 {
        peer.send_media(dest, 0, &ulaw_payload(i + 1), i == 0).await;
        pause(Duration::from_millis(100)).await;
    }
    assert_eq!(rig.server.active_calls(), 1);
    assert_eq!(rig.server.stats().rtp_timeouts, 0);
    // Then only telephone-event traffic (a caller holding a key): that is life too.
    for i in 0..15u16 {
        let packet = event_packet(
            101,
            i == 0,
            peer.seq.wrapping_add(i),
            peer.ts,
            peer.ssrc,
            1,
            false,
            160 * (i + 1),
        );
        peer.send_rtp_raw(dest, &packet).await;
        pause(Duration::from_millis(100)).await;
    }
    assert_eq!(rig.server.active_calls(), 1);
    assert_eq!(rig.server.stats().rtp_timeouts, 0);
    assert!(
        peer.recv_within(Duration::from_millis(50)).await.is_none(),
        "the server hung up a call that was receiving RTP"
    );
    // Silence at last: now the timeout fires.
    let bye = peer.request_of("BYE").await;
    peer.send_text(&response(&bye, 200, "OK", None, None, &[], None))
        .await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    assert_eq!(rig.server.stats().rtp_timeouts, 1);
}

#[tokio::test]
async fn a_hang_up_toward_a_peer_that_never_answers_the_bye_still_frees_the_slot() {
    let mut rig = Rig::start(SipConfig {
        rtp_timeout: Duration::from_millis(400),
        hangup_timeout: Duration::from_millis(500),
        max_calls: 1,
        ..config()
    })
    .await;
    // The peer vanishes: the RTP timeout fires and its BYE is never answered. The stack's own
    // transaction would wait 32 s; the slot must be free after the hang-up timeout.
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;
    let _unanswered = peer.request_of("BYE").await;
    rig.wait_for_active(0).await;
    assert_eq!(rig.server.stats().rtp_timeouts, 1);

    // The same when the engine hangs up.
    let Call { peer, session, .. } = rig.inbound().await;
    session.send(TransportCommand::Stop).await.unwrap();
    let _unanswered = peer.request_of("BYE").await;
    rig.wait_for_active(0).await;
}

// -- marks ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_mark_waits_for_the_audio_before_it_and_a_clear_drops_it() {
    let mut rig = Rig::default().await;
    let Call {
        peer, mut session, ..
    } = rig.inbound().await;
    let clock = rig.clock.clone();

    // (a) One second of audio in a single frame (the core passes it on whole, so the adapter
    // queues all 50 packets at once) and the mark right behind it: the mark must not resolve
    // while packets are still queued.
    session
        .send(bot_frame(&clock, 0, 0, 200, 8000))
        .await
        .unwrap();
    session
        .send(TransportCommand::Mark {
            id: "after-1s".to_owned(),
        })
        .await
        .unwrap();
    for _ in 0..3 {
        recv_rtp(&peer.rtp).await;
    }
    assert_no_event(&mut session, Duration::from_millis(100), |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    // ... and it does once the last of them left.
    let played = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        played,
        TransportEvent::MarkPlayed {
            id: "after-1s".to_owned()
        }
    );
    while recv_rtp_within(&peer.rtp, Duration::from_millis(100))
        .await
        .is_some()
    {}

    // (b) A second of audio and a mark behind it; a clear in between: the mark never resolves.
    session
        .send(bot_frame(&clock, 0, 100, 200, 8000))
        .await
        .unwrap();
    session
        .send(TransportCommand::Mark {
            id: "cut".to_owned(),
        })
        .await
        .unwrap();
    for _ in 0..2 {
        recv_rtp(&peer.rtp).await;
    }
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    // The engine is told the mark was cut and never that it was played. (The session also
    // ignores the echo of a cut mark, so this holds whether the adapter drops marks on a clear
    // or echoes them; what the test pins is what the engine sees.)
    let cut = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkCleared { .. })
    })
    .await;
    assert_eq!(
        cut,
        TransportEvent::MarkCleared {
            id: "cut".to_owned()
        }
    );
    assert_no_event(&mut session, Duration::from_millis(500), |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    // A mark queued after the clear resolves as usual.
    session
        .send(bot_frame(&clock, 1, 101, 200, 160))
        .await
        .unwrap();
    session
        .send(TransportCommand::Mark {
            id: "fresh".to_owned(),
        })
        .await
        .unwrap();
    let played = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        played,
        TransportEvent::MarkPlayed {
            id: "fresh".to_owned()
        }
    );
}

// -- the advertised address -------------------------------------------------------------------

#[tokio::test]
async fn the_public_ip_is_what_sdp_contact_and_the_bye_target_use() {
    // Bound to every interface, advertised as 127.0.0.2: nothing in the messages may show the
    // bind address, and the peer's in-dialog request goes to the advertised Contact.
    let mut rig = Rig::start(SipConfig {
        bind_addr: ([0, 0, 0, 0], 0).into(),
        public_ip: Some([127, 0, 0, 2].into()),
        ..config()
    })
    .await;
    let port = rig.addr().port();
    let mut peer = Peer::new(SocketAddr::from(([127, 0, 0, 1], port))).await;
    let offer = peer.offer(&[0, 101]);
    let answer = peer.call_up(&offer).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    let sdp = answer.text();
    assert_eq!(sdp_rtp_addr(&sdp).ip().to_string(), "127.0.0.2", "{sdp}");
    assert!(
        sdp.lines()
            .any(|l| l.starts_with("o=brisvox ") && l.ends_with(" IN IP4 127.0.0.2")),
        "{sdp}"
    );
    assert!(
        !sdp.contains("0.0.0.0") && !sdp.contains("127.0.0.1"),
        "{sdp}"
    );
    let contact = answer.header("contact").unwrap();
    assert!(
        contact.contains(&format!("127.0.0.2:{port}")),
        "the Contact names the advertised address and the bound port: {contact}"
    );
    let dest = sdp_rtp_addr(&sdp);
    let mut session = rig.accept().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;
    // The BYE goes to the Contact address, like a real peer's does.
    peer.server = SocketAddr::from(([127, 0, 0, 2], port));
    assert_eq!(peer.bye().await.status(), Some(200));
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);

    // An outbound call: offer and Contact carry the advertised address too.
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    let offer = invite.text();
    assert_eq!(
        sdp_rtp_addr(&offer).ip().to_string(),
        "127.0.0.2",
        "{offer}"
    );
    assert!(
        offer
            .lines()
            .any(|l| l.starts_with("o=brisvox ") && l.ends_with(" IN IP4 127.0.0.2")),
        "{offer}"
    );
    let contact = invite.header("contact").unwrap();
    assert!(contact.contains(&format!("127.0.0.2:{port}")), "{contact}");
    let via = invite.header("via").unwrap();
    assert!(!via.contains("0.0.0.0"), "{via}");
    callee.answer(&invite, from, 0).await;
    drop(rig.open(joined(task).await.unwrap()).await);
}

// -- telephone-event payload type and DTMF wiring -------------------------------------------------

#[tokio::test]
async fn dtmf_uses_the_payload_type_the_peer_negotiated_and_a_repeated_key_is_not_lost() {
    let mut rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = format!(
        "v=0\r\no=peer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 97\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:97 telephone-event/8000\r\na=fmtp:97 0-16\r\na=sendrecv\r\n",
        peer.rtp_port()
    );
    let answer = peer.call_up(&offer).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
    assert!(
        answer
            .text()
            .contains("a=rtpmap:97 telephone-event/8000\r\n"),
        "the answer uses the offered payload type: {}",
        answer.text()
    );
    let dest = sdp_rtp_addr(&answer.text());
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };

    // Receive: payload type 97 is an event; 101 (not negotiated) is ignored. The key '1' pressed
    // twice (two timestamps) is two keys, repeats of one end packet are one.
    let ssrc = peer.ssrc;
    let mut seq = peer.seq;
    let send = |packet: Vec<u8>| {
        let socket = &peer.rtp;
        async move { socket.send_to(&packet, dest).await.unwrap() }
    };
    for (ts, repeats) in [(16_000u32, 3u16), (24_000, 3)] {
        for i in 0..repeats {
            send(event_packet(97, i == 0, seq, ts, ssrc, 1, i > 0, 800)).await;
            seq = seq.wrapping_add(1);
        }
    }
    send(event_packet(101, false, seq, 40_000, ssrc, 2, true, 800)).await;
    for _ in 0..2 {
        let event = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
        assert!(
            matches!(
                event,
                TransportEvent::Dtmf {
                    digit: DtmfDigit::D1,
                    ..
                }
            ),
            "{event:?}"
        );
    }
    let stats = stats_where(&rig, |s| s.rtp_ignored >= 1 && s.rtp_packets_in >= 6).await;
    assert_eq!(stats.dtmf_in, 2, "{stats:?}");
    assert_no_event(&mut session, Duration::from_millis(150), |e| {
        matches!(e, TransportEvent::Audio(_) | TransportEvent::Dtmf { .. })
    })
    .await;

    // Send: the packets use payload type 97.
    rig.handle()
        .send_dtmf(&info.call_id, DtmfDigit::D4, Duration::from_millis(60))
        .unwrap();
    let packets = [
        recv_rtp(&peer.rtp).await,
        recv_rtp(&peer.rtp).await,
        recv_rtp(&peer.rtp).await,
    ];
    assert!(packets.iter().all(|p| p.payload_type == 97), "{}", {
        packets
            .iter()
            .map(|p| p.payload_type.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    drop(session);

    // Outbound: the callee's answer picks payload type 110.
    let callee = Callee::new().await;
    let task = spawn_dial(
        &rig.handle(),
        dial_request(&callee),
        &CancellationToken::new(),
    );
    let (invite, from) = callee.request_of("INVITE").await;
    let server_rtp = sdp_rtp_addr(&invite.text());
    let sdp = format!(
        "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 110\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:110 telephone-event/8000\r\na=sendrecv\r\n",
        callee.rtp.local_addr().unwrap().port()
    );
    let contact = Some(callee.sip.local_addr().unwrap());
    callee
        .send(
            &response(
                &invite,
                200,
                "OK",
                Some(&callee.tag),
                contact,
                &[],
                Some(&sdp),
            ),
            from,
        )
        .await;
    let mut session = rig.open(joined(task).await.unwrap()).await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    callee
        .rtp
        .send_to(
            &event_packet(110, true, 1, 1000, 55, 9, true, 800),
            server_rtp,
        )
        .await
        .unwrap();
    let event = next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    assert!(
        matches!(
            event,
            TransportEvent::Dtmf {
                digit: DtmfDigit::D9,
                ..
            }
        ),
        "{event:?}"
    );
    rig.handle()
        .send_dtmf(&info.call_id, DtmfDigit::D6, Duration::from_millis(60))
        .unwrap();
    assert_eq!(recv_rtp(&callee.rtp).await.payload_type, 110);
}

// -- SSRC takeover and latching checks -----------------------------------------------------------

#[tokio::test]
async fn a_new_ssrc_is_taken_over_after_a_quiet_period() {
    let mut rig = Rig::default().await;
    let Call {
        peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.rtp
        .send_to(&rtp_bytes(0, true, 100, 0, 0xAAAA, &ulaw_payload(1)), dest)
        .await
        .unwrap();
    next_audio(&mut session).await;
    // A blind transfer or a media-server change: the stream stops for more than half a second,
    // then comes back from the same address with another SSRC and new sequence numbers.
    pause(Duration::from_millis(700)).await;
    peer.rtp
        .send_to(&rtp_bytes(0, true, 9000, 0, 0xBBBB, &ulaw_payload(2)), dest)
        .await
        .unwrap();
    let frame = next_audio(&mut session).await;
    assert_eq!(
        frame.data().as_ref(),
        pcm_of_ulaw(&ulaw_payload(2)).as_slice(),
        "the new SSRC was not taken over"
    );
    assert_eq!(rig.server.stats().rtp_foreign_ssrc, 0);
    // The new SSRC is locked in now: the old one, right away, is foreign.
    peer.rtp
        .send_to(
            &rtp_bytes(0, false, 101, 160, 0xAAAA, &ulaw_payload(3)),
            dest,
        )
        .await
        .unwrap();
    stats_where(&rig, |s| s.rtp_foreign_ssrc >= 1).await;
    assert_eq!(rig.server.stats().rtp_foreign_ssrc, 1);
}

#[tokio::test]
async fn a_packet_that_fails_the_payload_or_sequence_check_never_moves_the_destination() {
    let mut rig = Rig::start(latch_config()).await;
    let Call {
        mut peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    peer.send_media(dest, 0, &ulaw_payload(1), true).await;
    next_audio(&mut session).await;
    let (accepted_seq, accepted_ts) = (peer.seq.wrapping_sub(1), peer.ts.wrapping_sub(160));
    // A session refresh opens the latch window again; the SSRC stays locked and the reorder
    // buffer keeps its position.
    let refresh = media_sdp(peer.rtp_port(), 2, "sendrecv", "127.0.0.1");
    reinvite(&mut peer, &refresh).await;

    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // (a) The right SSRC but a payload type nobody negotiated.
    intruder
        .send_to(
            &rtp_bytes(18, false, peer.seq, peer.ts, peer.ssrc, &[0u8; 20]),
            dest,
        )
        .await
        .unwrap();
    // (b) A duplicate of a packet that was already played.
    intruder
        .send_to(
            &rtp_bytes(
                0,
                false,
                accepted_seq,
                accepted_ts,
                peer.ssrc,
                &ulaw_payload(1),
            ),
            dest,
        )
        .await
        .unwrap();
    let stats = stats_where(&rig, |s| s.rtp_ignored >= 1 && s.rtp_late_or_duplicate >= 1).await;
    assert_eq!(
        stats.rtp_foreign_source, 0,
        "the window was open: {stats:?}"
    );

    session
        .send(bot_frame(&rig.clock, 0, 0, 300, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
    assert!(
        recv_rtp_within(&intruder, Duration::from_millis(150))
            .await
            .is_none(),
        "the media destination followed a packet that failed a stream check"
    );
}

// -- digest on outbound calls ------------------------------------------------------------------

/// Dials with credentials, answers the first INVITE with a 401 carrying `challenge`, and returns
/// the retried INVITE and its `Authorization` header (the call is then answered and dropped).
async fn challenged_retry(challenge: &str) -> (Msg, String) {
    let rig = Rig::default().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.credential = Some(DialCredential {
        username: "Mufasa".to_owned(),
        password: Secret::new("Circle Of Life".to_owned()),
        realm: None,
    });
    let task = spawn_dial(&rig.handle(), request, &CancellationToken::new());
    let (first, from) = callee.request_of("INVITE").await;
    callee
        .send(
            &response(
                &first,
                401,
                "Unauthorized",
                Some(&callee.tag),
                None,
                &[("WWW-Authenticate", challenge.to_owned())],
                None,
            ),
            from,
        )
        .await;
    callee.request_of("ACK").await;
    let (second, from) = callee.request_of("INVITE").await;
    let raw = second.header("authorization").unwrap().to_owned();
    callee.answer(&second, from, 0).await;
    drop(rig.open(joined(task).await.unwrap()).await);
    (second, raw)
}

#[tokio::test]
async fn the_digest_response_names_the_retried_uri_and_follows_the_qop() {
    // The `uri` of the response must be the request URI of the retried INVITE: carriers compute
    // the hash over the request line and answer 403 on a mismatch.
    let (retry, raw) = challenged_retry(
        "Digest realm=\"testrealm@host.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5",
    )
    .await;
    let request_uri = retry.start.split(' ').nth(1).unwrap();
    assert_eq!(
        rsipstack::dialog::authenticate::extract_digest_uri_raw(&raw).as_deref(),
        Some(request_uri),
        "{raw}"
    );
    assert!(!raw.contains("qop="), "no qop was offered: {raw}");

    // A carrier that offers qop="auth" gets qop, nc and cnonce back, and a verifiable hash.
    let (retry, raw) = challenged_retry(
        "Digest realm=\"testrealm@host.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", qop=\"auth\", algorithm=MD5",
    )
    .await;
    let request_uri = retry.start.split(' ').nth(1).unwrap();
    assert_eq!(
        rsipstack::dialog::authenticate::extract_digest_uri_raw(&raw).as_deref(),
        Some(request_uri),
        "{raw}"
    );
    // (rsipstack writes the qop value in quotes; RFC 2617 has it as a bare token. Carriers that
    // reject the quoted form are an interop risk that only a real carrier can show.)
    assert!(raw.replace('"', "").contains("qop=auth"), "{raw}");
    assert!(raw.contains("nc=00000001"), "{raw}");
    assert!(raw.contains("cnonce=\""), "{raw}");
    let auth = Authorization::parse(&raw).unwrap();
    assert!(verify_digest(
        &auth,
        "Circle Of Life",
        &Method::Invite,
        &raw
    ));
    assert!(!raw.contains("Circle Of Life"));
}

// -- RTP ports ------------------------------------------------------------------------------------

#[tokio::test]
async fn rtp_ports_are_reused_after_a_hang_up_and_never_odd_when_the_range_ends_odd() {
    // Three even ports (low, low + 2, low + 4) and an odd high end: seven calls in a row wrap
    // the rotation twice and must never bind the odd port low + 5 (reserved for RTCP).
    let low = free_even_range(3).await;
    let mut rig = Rig::start(SipConfig {
        rtp_ports: Some((low, low + 5)),
        ..config()
    })
    .await;
    for round in 0..7 {
        let Call {
            mut peer,
            session,
            dest,
            ..
        } = rig.inbound().await;
        let port = dest.port();
        assert!(
            [low, low + 2, low + 4].contains(&port),
            "round {round}: port {port} outside the even ports of {low}..={}",
            low + 5
        );
        assert_eq!(peer.bye().await.status(), Some(200));
        drop(session);
        rig.wait_for_active(0).await;
    }
}

#[tokio::test]
async fn an_exhausted_port_range_refuses_the_call_with_503_and_recovers() {
    let low = free_even_range(3).await;
    let mut rig = Rig::start(SipConfig {
        rtp_ports: Some((low, low + 4)),
        ..config()
    })
    .await;
    let mut calls = Vec::new();
    for _ in 0..3 {
        calls.push(rig.inbound().await);
    }
    let mut late = Peer::new(rig.addr()).await;
    let offer = late.offer(&[0]);
    late.invite(&offer).await;
    let refusal = late.final_response().await;
    assert_eq!(refusal.status(), Some(503), "{refusal:?}");
    late.ack(&refusal).await;
    assert_eq!(rig.server.stats().rejected_busy, 1);
    assert_eq!(rig.server.active_calls(), 3);

    // One call ends: its port is free again for the next caller.
    let first = calls.remove(0);
    let Call {
        mut peer, session, ..
    } = first;
    assert_eq!(peer.bye().await.status(), Some(200));
    drop(session);
    rig.wait_for_active(2).await;
    late.call_id = "late-retry".to_owned();
    let answer = late.call_up(&offer).await;
    assert_eq!(answer.status(), Some(200), "{answer:?}");
}

// -- dialing --------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dial_refused_before_the_invite_sends_nothing_and_does_not_burn_its_key() {
    let mut rig = Rig::start(SipConfig {
        max_calls: 1,
        ..config()
    })
    .await;
    let handle = rig.handle();
    let cancel = CancellationToken::new();
    let Call {
        mut peer, session, ..
    } = rig.inbound().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.dial_key = Some("order-7".to_owned());
    assert!(matches!(
        handle.dial(request.clone(), &cancel).await,
        Err(DialError::AtCapacity)
    ));
    let sent = tokio::time::timeout(Duration::from_millis(300), callee.request_of("INVITE")).await;
    assert!(sent.is_err(), "an INVITE went out for a refused dial");

    // The slot frees up; the same key dials now (it was forgotten with the refusal).
    assert_eq!(peer.bye().await.status(), Some(200));
    drop(session);
    rig.wait_for_active(0).await;
    let task = spawn_dial(&handle, request, &cancel);
    let (invite, from) = callee.request_of("INVITE").await;
    callee.answer(&invite, from, 0).await;
    drop(rig.open(joined(task).await.unwrap()).await);
}

#[tokio::test]
async fn a_dial_that_finds_no_rtp_port_forgets_its_key_too() {
    let low = free_even_range(1).await;
    let mut rig = Rig::start(SipConfig {
        rtp_ports: Some((low, low)),
        ..config()
    })
    .await;
    let handle = rig.handle();
    let cancel = CancellationToken::new();
    let Call {
        mut peer, session, ..
    } = rig.inbound().await;
    let callee = Callee::new().await;
    let mut request = dial_request(&callee);
    request.dial_key = Some("order-8".to_owned());
    match handle.dial(request.clone(), &cancel).await {
        Err(DialError::DuplicateKey | DialError::AtCapacity) | Ok(_) => {
            panic!("expected a port error")
        }
        Err(_) => {}
    }
    let sent = tokio::time::timeout(Duration::from_millis(300), callee.request_of("INVITE")).await;
    assert!(sent.is_err(), "an INVITE went out without an RTP port");

    assert_eq!(peer.bye().await.status(), Some(200));
    drop(session);
    rig.wait_for_active(0).await;
    let task = spawn_dial(&handle, request, &cancel);
    let (invite, from) = callee.request_of("INVITE").await;
    callee.answer(&invite, from, 0).await;
    drop(rig.open(joined(task).await.unwrap()).await);
}

// -- requests inside the dialog --------------------------------------------------------------------

#[tokio::test]
async fn update_delayed_offer_reinvite_options_and_unserved_methods_are_answered() {
    let mut rig = Rig::default().await;
    let Call {
        mut peer,
        session,
        dest,
        ..
    } = rig.inbound().await;

    // In-dialog OPTIONS: 200 with Allow.
    let options = in_dialog(&mut peer, "OPTIONS", &[], None).await;
    assert_eq!(options.status(), Some(200), "{options:?}");
    assert!(options.header("allow").unwrap().contains("UPDATE"));

    // Methods a voice endpoint does not serve get an answer at once, not a 32 s silence.
    for (method, extra, body) in [
        (
            "REFER",
            vec![("Refer-To", "<sip:bob@example.com>".to_owned())],
            None,
        ),
        ("NOTIFY", vec![("Event", "refer".to_owned())], None),
        ("MESSAGE", vec![], Some(("text/plain", "hi"))),
    ] {
        let answer = tokio::time::timeout(
            Duration::from_secs(3),
            in_dialog(&mut peer, method, &extra, body),
        )
        .await
        .unwrap_or_else(|_| panic!("{method} was not answered"));
        assert_eq!(answer.status(), Some(501), "{method}: {answer:?}");
    }

    // UPDATE without SDP is a session refresh: 200, no body. With SDP it moves the media.
    let refresh = in_dialog(&mut peer, "UPDATE", &[], None).await;
    assert_eq!(refresh.status(), Some(200), "{refresh:?}");
    assert!(refresh.body.is_empty());
    let moved = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sdp = media_sdp(
        moved.local_addr().unwrap().port(),
        2,
        "sendrecv",
        "127.0.0.1",
    );
    let updated = in_dialog(&mut peer, "UPDATE", &[], Some(("application/sdp", &sdp))).await;
    assert_eq!(updated.status(), Some(200), "{updated:?}");
    assert_eq!(sdp_rtp_addr(&updated.text()), dest);
    let v_update = o_version(&updated.text());
    session
        .send(bot_frame(&rig.clock, 0, 0, 500, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&moved).await.payload_type, 0);

    // A re-INVITE without an offer: our SDP (with a higher version) is in the 200 and the
    // peer's answer is in the ACK. Here the answer moves the media to another socket.
    let moved_again = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let answer_sdp = media_sdp(
        moved_again.local_addr().unwrap().port(),
        3,
        "sendrecv",
        "127.0.0.1",
    );
    let ok = in_dialog(&mut peer, "INVITE", &[], None).await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    assert_eq!(ok.header("content-type"), Some("application/sdp"));
    assert_eq!(sdp_rtp_addr(&ok.text()), dest);
    let v_delayed = o_version(&ok.text());
    assert!(v_delayed > v_update, "{v_delayed} <= {v_update}");
    ack_with_body(&peer, &ok, Some(&answer_sdp)).await;
    // The answer in the ACK is applied: bot audio now goes to the new address.
    let wait_for_move = async {
        loop {
            session
                .send(bot_frame(&rig.clock, 0, 1, 500, 160))
                .await
                .unwrap();
            if recv_rtp_within(&moved_again, Duration::from_millis(100))
                .await
                .is_some()
            {
                return;
            }
        }
    };
    tokio::time::timeout(WAIT, wait_for_move)
        .await
        .expect("the answer in the ACK was ignored");

    // A second re-INVITE without an offer whose ACK carries no answer: the version grows again
    // and the media stays where it was.
    let ok = in_dialog(&mut peer, "INVITE", &[], None).await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    assert!(o_version(&ok.text()) > v_delayed);
    ack_with_body(&peer, &ok, None).await;
    while recv_rtp_within(&moved_again, Duration::from_millis(100))
        .await
        .is_some()
    {}
    session
        .send(bot_frame(&rig.clock, 0, 2, 500, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&moved_again).await.payload_type, 0);
    assert_eq!(rig.server.active_calls(), 1);
}

// -- retransmissions ------------------------------------------------------------------------------

#[tokio::test]
async fn a_retransmitted_invite_never_makes_a_second_call_or_a_different_answer() {
    let mut rig = Rig::start(SipConfig {
        ring_time: Duration::from_millis(400),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0, 101]);
    peer.cseq += 1;
    let uri = format!("sip:agent@{}", peer.server);
    let invite = peer.request_text(
        "INVITE",
        &uri,
        peer.cseq,
        &[],
        Some(("application/sdp", &offer)),
    );
    // The very same request (same branch) while the phone rings, ...
    peer.send_text(&invite).await;
    pause(Duration::from_millis(100)).await;
    peer.send_text(&invite).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    // ... and once more after the 200, before the ACK.
    peer.send_text(&invite).await;
    pause(Duration::from_millis(50)).await;
    peer.to_tag = ok.to_tag();
    peer.ack(&ok).await;

    let _session = rig.accept().await;
    let second = tokio::time::timeout(
        Duration::from_millis(300),
        rig.server.accept(&CancellationToken::new()),
    )
    .await;
    assert!(second.is_err(), "a retransmitted INVITE made a second call");
    while let Some(msg) = peer.recv_within(Duration::from_millis(200)).await {
        if msg.status() == Some(200) {
            assert_eq!(msg.to_tag(), ok.to_tag(), "a different answer: {msg:?}");
        }
        assert_ne!(msg.method(), Some("BYE"), "the call was torn down");
    }
    let stats = rig.server.stats();
    assert_eq!(stats.calls_inbound, 1, "{stats:?}");
    assert_eq!(rig.server.active_calls(), 1);

    // A refusal is repeated, not redone.
    let mut other = Peer::new(rig.addr()).await;
    other.cseq += 1;
    let uri = format!("sip:agent@{}", other.server);
    let bad = other.request_text(
        "INVITE",
        &uri,
        other.cseq,
        &[],
        Some(("text/plain", "not sdp")),
    );
    other.send_text(&bad).await;
    let refusal = other.final_response().await;
    assert_eq!(refusal.status(), Some(415));
    other.send_text(&bad).await;
    let again = other.final_response().await;
    assert_eq!(again.status(), Some(415), "{again:?}");
    other.ack(&again).await;
    assert_eq!(rig.server.stats().rejected_invalid, 1);
}

// -- ringing and the metadata of an INVITE ------------------------------------------------------------

#[tokio::test]
async fn an_inbound_call_rings_for_the_ring_time_and_is_then_answered() {
    let mut rig = Rig::start(SipConfig {
        ring_time: Duration::from_millis(300),
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    let started = rig.clock.now();
    peer.invite(&offer).await;
    let mut statuses = Vec::new();
    let ok = loop {
        let msg = peer.recv().await;
        let status = msg.status().unwrap();
        statuses.push(status);
        if status >= 200 {
            break msg;
        }
    };
    let waited = rig.clock.now() - started;
    assert_eq!(ok.status(), Some(200), "{statuses:?}");
    assert!(
        statuses.contains(&180),
        "no 180 before the 200: {statuses:?}"
    );
    assert!(
        waited >= Duration::from_millis(280),
        "answered after {waited:?}, before the ring time"
    );
    peer.to_tag = ok.to_tag();
    peer.ack(&ok).await;
    rig.accept().await;
    assert_eq!(rig.server.stats().calls_inbound, 1);
}

#[tokio::test]
async fn the_ringing_option_alone_sends_a_180_before_the_200() {
    let rig = Rig::start(SipConfig {
        ringing: true,
        ..config()
    })
    .await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    peer.invite(&offer).await;
    let mut statuses = Vec::new();
    loop {
        let status = peer.recv().await.status().unwrap();
        statuses.push(status);
        if status >= 200 {
            break;
        }
    }
    assert!(statuses.contains(&180), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&200));
    rig.parent.cancel();
}

#[tokio::test]
async fn the_invite_metadata_carries_the_identity_headers_within_its_caps() {
    let mut rig = Rig::default().await;
    let mut peer = Peer::new(rig.addr()).await;
    let offer = peer.offer(&[0]);
    let mut extra: Vec<(&str, String)> = vec![
        (
            "P-Asserted-Identity",
            "<sip:+15551230000@carrier.test>".to_owned(),
        ),
        (
            "Diversion",
            "<sip:+15559990000@carrier.test>;reason=unconditional".to_owned(),
        ),
        (
            "Authorization",
            "Digest username=\"u\", realm=\"r\", nonce=\"n\", uri=\"sip:x\", response=\"00\""
                .to_owned(),
        ),
        ("Subject", "not copied".to_owned()),
        ("X-Long", "a".repeat(3000)),
    ];
    let names: Vec<String> = (0..40).map(|i| format!("X-Hdr-{i:02}")).collect();
    for name in &names {
        extra.push((name.as_str(), "v".to_owned()));
    }
    peer.invite_with(&extra, "application/sdp", &offer).await;
    let ok = peer.final_response().await;
    assert_eq!(ok.status(), Some(200), "{ok:?}");
    peer.to_tag = ok.to_tag();
    peer.ack(&ok).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    let m = &info.metadata;
    assert!(
        m["p_asserted_identity"].contains("+15551230000@carrier.test"),
        "{m:?}"
    );
    assert!(
        m["diversion"].contains("+15559990000@carrier.test"),
        "{m:?}"
    );
    assert!(m["request_uri"].starts_with("sip:agent@"), "{m:?}");
    assert!(
        m.keys()
            .all(|k| !k.contains("authorization") && !k.contains("subject")),
        "{m:?}"
    );
    assert!(
        m.values()
            .all(|v| !v.contains("nonce=") && v != "not copied"),
        "{m:?}"
    );
    // At most 16 X- headers are copied, each cut to 256 characters.
    let copied = m.keys().filter(|k| k.starts_with("header.x-")).count();
    assert_eq!(copied, 16, "{m:?}");
    if let Some(long) = m.get("header.x-long") {
        assert_eq!(long.chars().count(), 256);
    }
    assert!(
        m.values().all(|v| v.chars().count() <= 512),
        "an unbounded value reached the metadata: {m:?}"
    );
}

// -- audio the adapter cannot send, and lost packets ---------------------------------------------------

#[tokio::test]
async fn every_bot_frame_at_another_rate_is_dropped_and_counted() {
    let mut rig = Rig::default().await;
    let Call { peer, session, .. } = rig.inbound().await;
    for seq in 0..2u64 {
        let frame = AudioFrame::new(
            Bytes::from(vec![0u8; 640]),
            SampleRate::Hz16000,
            rig.clock.now(),
            seq,
            TurnEpoch::ZERO,
        )
        .unwrap();
        session
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::ZERO,
                utterance: UtteranceId::new(1),
                frame,
            })
            .await
            .unwrap();
    }
    stats_where(&rig, |s| s.dropped_audio_frames >= 2).await;
    assert!(
        recv_rtp_within(&peer.rtp, Duration::from_millis(200))
            .await
            .is_none()
    );
    // The call itself is fine and 8 kHz audio still goes out.
    session
        .send(bot_frame(&rig.clock, 0, 2, 300, 160))
        .await
        .unwrap();
    assert_eq!(recv_rtp(&peer.rtp).await.payload_type, 0);
    assert_eq!(rig.server.stats().dropped_audio_frames, 2);
}

#[tokio::test]
async fn a_short_loss_is_concealed_with_silence_and_a_long_burst_is_not() {
    let mut rig = Rig::default().await; // max_conceal_packets = 3
    let Call {
        peer,
        mut session,
        dest,
        ..
    } = rig.inbound().await;
    let base = peer.seq;
    let ts = peer.ts;
    let send = |n: u16| {
        let packet = rtp_bytes(
            0,
            n == 0,
            base.wrapping_add(n),
            ts.wrapping_add(160 * u32::from(n)),
            peer.ssrc,
            &ulaw_payload(n as i16 + 1),
        );
        let socket = &peer.rtp;
        async move { socket.send_to(&packet, dest).await.unwrap() }
    };
    // 0, 1, then 2-4 are lost (exactly the limit): three silence frames, then 5 and 6.
    for n in [0u16, 1, 5, 6] {
        send(n).await;
    }
    let expect = [
        pcm_of_ulaw(&ulaw_payload(1)),
        pcm_of_ulaw(&ulaw_payload(2)),
        vec![0u8; 320],
        vec![0u8; 320],
        vec![0u8; 320],
        pcm_of_ulaw(&ulaw_payload(6)),
        pcm_of_ulaw(&ulaw_payload(7)),
    ];
    for (i, want) in expect.iter().enumerate() {
        let frame = next_audio(&mut session).await;
        assert_eq!(frame.data().as_ref(), want.as_slice(), "frame {i}");
    }
    // 7-10 are lost (four, one over the limit): no silence is made up, the timeline has a hole.
    for n in [11u16, 12] {
        send(n).await;
    }
    for n in [11i16, 12] {
        let frame = next_audio(&mut session).await;
        assert_eq!(
            frame.data().as_ref(),
            pcm_of_ulaw(&ulaw_payload(n + 1)).as_slice(),
            "packet {n} must follow at once"
        );
    }
    assert_eq!(rig.server.stats().rtp_lost, 3 + 4);
    assert_no_event(&mut session, Duration::from_millis(150), |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await;
}

// -- the accept queue ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_burst_larger_than_the_accept_queue_is_refused_up_front_and_never_hung_up_later() {
    let mut rig = Rig::start(SipConfig {
        accept_queue: 2,
        // The engine is slow to accept and the phones ring for a while: all five INVITEs arrive
        // before any of the calls is queued.
        ring_time: Duration::from_millis(300),
        ..config()
    })
    .await;
    let mut peers = Vec::new();
    for _ in 0..5 {
        let mut peer = Peer::new(rig.addr()).await;
        let offer = peer.offer(&[0]);
        peer.invite(&offer).await;
        peers.push(peer);
    }
    let mut answered = Vec::new();
    let mut refused = 0;
    for mut peer in peers {
        let reply = peer.final_response().await;
        match reply.status() {
            Some(200) => {
                peer.to_tag = reply.to_tag();
                peer.ack(&reply).await;
                answered.push(peer);
            }
            Some(503) => {
                assert!(reply.header("retry-after").is_some());
                peer.ack(&reply).await;
                refused += 1;
            }
            other => panic!("unexpected answer {other:?}"),
        }
    }
    assert_eq!((answered.len(), refused), (2, 3));
    assert_eq!(rig.server.stats().rejected_busy, 3);
    // The two answered calls reach the engine and nobody is hung up.
    let _first = rig.accept().await;
    let _second = rig.accept().await;
    for peer in &answered {
        while let Some(msg) = peer.recv_within(Duration::from_millis(300)).await {
            assert_ne!(msg.method(), Some("BYE"), "an answered call was hung up");
        }
    }
    assert_eq!(rig.server.active_calls(), 2);
}
