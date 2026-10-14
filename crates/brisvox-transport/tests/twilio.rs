//! Twilio Media Streams transport tests: real sockets on 127.0.0.1, the real clock, short
//! deadlines.
//!
//! Each test starts a `TwilioServer`, plays the engine with a `TransportSession` on every
//! accepted call and plays Twilio with a scripted client (`Phone`) that sends the message
//! sequence of `docs/providers/twilio.md` and records what the server sends back. Numbers that
//! the docs print as strings (`sequenceNumber`, `chunk`, `timestamp`) are sent as strings.
#![cfg(feature = "twilio")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)] // test helpers (see clippy.toml)

use std::time::Duration;

use base64::Engine as _;
use brisvox_audio::Codec;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::twilio::{TwilioConfig, TwilioServer, signature};
use brisvox_transport::websocket::ServerConfig;
use brisvox_transport::{
    CallInfo, CallRates, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(5);

const CLOSE_NORMAL: u16 = 1000;
const CLOSE_AWAY: u16 = 1001;
const CLOSE_PROTOCOL: u16 = 1002;
const CLOSE_POLICY: u16 = 1008;
const CLOSE_AGAIN: u16 = 1013;
const CLOSE_SIZE: u16 = 1009;

const STREAM_SID: &str = "MZ0123456789abcdef0123456789abcdef";
const CALL_SID: &str = "CA0123456789abcdef0123456789abcdef";
const ACCOUNT_SID: &str = concat!("AC", "0123456789abcdef0123456789abcdef");
const PATH: &str = "/twilio/stream";
const PUBLIC_URL: &str = "wss://agent.example.com/twilio/stream";
const TOKEN: &str = "test-auth-token-0123456789";

/// Waits `duration` on the real clock.
async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn fast_server() -> ServerConfig {
    ServerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        max_calls: 8,
        handshake_timeout: Duration::from_secs(2),
        start_timeout: Duration::from_secs(2),
        ping_interval: Duration::from_millis(200),
        ping_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(2),
        send_timeout: Duration::from_secs(2),
        close_grace: Duration::from_millis(500),
        ..ServerConfig::default()
    }
}

fn fast_twilio() -> TwilioConfig {
    TwilioConfig {
        mark_interval: Duration::ZERO,
        ..TwilioConfig::default()
    }
}

fn signed_twilio() -> TwilioConfig {
    TwilioConfig {
        auth_token: Some(Secret::new(TOKEN.to_owned())),
        public_url: Some(PUBLIC_URL.to_owned()),
        // These tests pin the signature of the fixed URL; the per-call URLs have their own.
        allow_static_url: true,
        ..fast_twilio()
    }
}

/// One test's server, its clock and the token every session hangs off.
struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: TwilioServer,
}

impl Rig {
    async fn start(server: ServerConfig, twilio: TwilioConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = TwilioServer::bind(server, twilio, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    async fn default() -> Self {
        Self::start(fast_server(), fast_twilio()).await
    }

    fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.server.local_addr())
    }

    /// Opens the websocket the way Twilio does, optionally with a signature header.
    async fn try_connect(&self, path: &str, signature: Option<&str>) -> Result<Client, WsError> {
        let mut request = self.url(path).into_client_request().unwrap();
        if let Some(signature) = signature {
            request.headers_mut().insert(
                "X-Twilio-Signature",
                HeaderValue::from_str(signature).unwrap(),
            );
        }
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request))
            .await
            .expect("connect timed out")?;
        Ok(ws)
    }

    async fn phone(&self) -> Phone {
        Phone::new(self.try_connect(PATH, None).await.expect("connect failed"))
    }

    async fn small_buffer_phone(&self) -> Phone {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let stream = socket.connect(self.server.local_addr()).await.unwrap();
        let (ws, _) = tokio::time::timeout(
            WAIT,
            client_async(self.url(PATH), MaybeTlsStream::Plain(stream)),
        )
        .await
        .expect("upgrade timed out")
        .expect("upgrade failed");
        Phone::new(ws)
    }

    /// The engine side of the next call.
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

    /// Connects, sends `connected` and `start`, and accepts the call.
    async fn pair(&mut self) -> (Phone, TransportSession) {
        let mut phone = self.phone().await;
        phone.connected().await;
        phone.start(json!({})).await;
        let mut session = self.accept().await;
        let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
        assert!(matches!(started, TransportEvent::Started(_)));
        (phone, session)
    }
}

/// What the server sent, classified; every message must carry the stream's `streamSid`.
#[derive(Debug, PartialEq)]
enum Out {
    /// A `media` message: the decoded mu-law payload.
    Media(Vec<u8>),
    Mark(String),
    Clear,
    /// Any other JSON text (the test says whether it is expected).
    Other(Value),
    /// A close frame with its status code.
    Close(Option<u16>),
}

/// Twilio, scripted.
struct Phone {
    ws: Client,
    seq: u64,
    chunk: u64,
    /// Marks the server sent that this phone has not echoed yet.
    unechoed: Vec<String>,
    /// The stream's sid (a resumed stream has another).
    stream_sid: String,
}

impl Phone {
    fn new(ws: Client) -> Self {
        Self {
            ws,
            seq: 0,
            chunk: 0,
            unechoed: Vec::new(),
            stream_sid: STREAM_SID.to_owned(),
        }
    }

    fn next_seq(&mut self) -> String {
        self.seq += 1;
        self.seq.to_string()
    }

    async fn send_value(&mut self, value: &Value) {
        self.send_text(&value.to_string()).await;
    }

    async fn send_text(&mut self, text: &str) {
        tokio::time::timeout(WAIT, self.ws.send(Message::text(text)))
            .await
            .expect("send timed out")
            .expect("send failed");
    }

    async fn connected(&mut self) {
        self.send_value(&json!({"event": "connected", "protocol": "Call", "version": "1.0.0"}))
            .await;
    }

    /// `start` as in the docs, plus `extra` merged into the `start` object.
    async fn start(&mut self, extra: Value) {
        let mut start = json!({
            "accountSid": ACCOUNT_SID,
            "streamSid": self.stream_sid,
            "callSid": CALL_SID,
            "tracks": ["inbound"],
            "mediaFormat": {"encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1},
            "customParameters": {"agent": "support-tr", "FirstName": "Jane"},
        });
        if let (Some(base), Some(extra)) = (start.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "start",
            "sequenceNumber": sequence,
            "start": start,
            "streamSid": self.stream_sid,
        }))
        .await;
    }

    /// An inbound `media` message with `mulaw` as payload.
    async fn media(&mut self, mulaw: &[u8]) {
        self.chunk += 1;
        let sequence = self.next_seq();
        let timestamp = (self.chunk * 20).to_string();
        let message = json!({
            "event": "media",
            "sequenceNumber": sequence,
            "media": {
                "track": "inbound",
                "chunk": self.chunk.to_string(),
                "timestamp": timestamp,
                "payload": base64::engine::general_purpose::STANDARD.encode(mulaw),
            },
            "streamSid": self.stream_sid,
        });
        self.send_value(&message).await;
    }

    async fn dtmf(&mut self, digit: &str) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "dtmf",
            "streamSid": self.stream_sid,
            "sequenceNumber": sequence,
            "dtmf": {"track": "inbound_track", "digit": digit},
        }))
        .await;
    }

    async fn echo_mark(&mut self, name: &str) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "mark",
            "sequenceNumber": sequence,
            "streamSid": self.stream_sid,
            "mark": {"name": name},
        }))
        .await;
        self.unechoed.retain(|m| m != name);
    }

    async fn stop(&mut self) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "stop",
            "sequenceNumber": sequence,
            "stop": {"accountSid": ACCOUNT_SID, "callSid": CALL_SID},
            "streamSid": self.stream_sid,
        }))
        .await;
    }

    /// The next frame from the server (pings skipped), or `None` when the connection ended.
    async fn recv(&mut self) -> Option<Out> {
        // One deadline for the whole call: the server's pings must not keep a missing frame
        // from failing the test (a per-frame timeout restarts with every ping).
        let next = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => return other,
                }
            }
        };
        let next = tokio::time::timeout(WAIT, next)
            .await
            .expect("no frame within the wait");
        match next {
            Some(Ok(Message::Close(frame))) => Some(Out::Close(frame.map(|f| u16::from(f.code)))),
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                Some(self.classify(value))
            }
            Some(Ok(other)) => panic!("unexpected frame {other:?}"),
            Some(Err(_)) | None => None,
        }
    }

    fn classify(&mut self, value: Value) -> Out {
        assert_eq!(
            value["streamSid"], self.stream_sid,
            "the server must use the stream's sid: {value}"
        );
        match value["event"].as_str() {
            Some("media") => {
                let payload = value["media"]["payload"].as_str().expect("payload");
                Out::Media(
                    base64::engine::general_purpose::STANDARD
                        .decode(payload)
                        .unwrap(),
                )
            }
            Some("mark") => {
                let name = value["mark"]["name"]
                    .as_str()
                    .expect("mark name")
                    .to_owned();
                self.unechoed.push(name.clone());
                Out::Mark(name)
            }
            Some("clear") => Out::Clear,
            _ => Out::Other(value),
        }
    }

    /// Reads until `wanted` matches; returns everything read, the match last.
    async fn recv_until(&mut self, wanted: impl Fn(&Out) -> bool) -> Vec<Out> {
        let mut seen = Vec::new();
        loop {
            let out = self.recv().await.expect("connection ended");
            let done = wanted(&out);
            seen.push(out);
            if done {
                return seen;
            }
        }
    }

    /// Reads until the connection ends; returns what it saw and the close code.
    async fn drain(&mut self) -> (Vec<Out>, Option<u16>) {
        let mut seen = Vec::new();
        while let Some(out) = self.recv().await {
            if let Out::Close(code) = out {
                return (seen, code);
            }
            seen.push(out);
        }
        (seen, None)
    }
}

/// The next event satisfying `wanted`, skipping playback reports and the like.
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

async fn stopped(session: &mut TransportSession) -> StopReason {
    match next_event(session, |e| matches!(e, TransportEvent::Stopped { .. })).await {
        TransportEvent::Stopped { reason } => reason,
        _ => unreachable!(),
    }
}

async fn wait_for_active(rig: &Rig, expected: usize) {
    let wait = async {
        while rig.server.active_calls() != expected {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .unwrap_or_else(|_| panic!("active calls stayed at {}", rig.server.active_calls()));
}

/// 20 ms of 8 kHz PCM16 where every sample is `sample`.
fn pcm20(sample: i16) -> Vec<u8> {
    sample.to_le_bytes().repeat(160)
}

/// The mu-law bytes of [`pcm20`].
fn mulaw20(sample: i16) -> Vec<u8> {
    let pcm = pcm20(sample);
    let mut out = vec![0u8; 160];
    Codec::Ulaw.encode(&pcm, &mut out).unwrap();
    out
}

fn audio_command(rig: &Rig, epoch: u32, seq: u64, sample: i16) -> TransportCommand {
    audio_command_at(&rig.clock, SampleRate::Hz8000, epoch, seq, pcm20(sample))
}

fn audio_command_at(
    clock: &SharedClock,
    rate: SampleRate,
    epoch: u32,
    seq: u64,
    bytes: Vec<u8>,
) -> TransportCommand {
    let epoch = TurnEpoch::new(epoch);
    let frame = AudioFrame::new(Bytes::from(bytes), rate, clock.now(), seq, epoch).unwrap();
    TransportCommand::Audio {
        epoch,
        utterance: UtteranceId::new(1),
        frame,
    }
}

// ---------------------------------------------------------------------------------------------
// The message sequence of the docs: connected, start, media

#[tokio::test]
async fn connected_and_start_become_call_info_with_8k_mulaw_metadata() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.connected().await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, CALL_SID);
    let meta = &info.metadata;
    assert_eq!(meta.get("sample_rate").map(String::as_str), Some("8000"));
    assert_eq!(meta.get("encoding").map(String::as_str), Some("mulaw"));
    assert_eq!(meta.get("transport").map(String::as_str), Some("twilio"));
    assert_eq!(meta.get("stream_sid").map(String::as_str), Some(STREAM_SID));
    assert_eq!(meta.get("call_sid").map(String::as_str), Some(CALL_SID));
    assert_eq!(
        meta.get("account_sid").map(String::as_str),
        Some(ACCOUNT_SID)
    );
    assert_eq!(meta.get("tracks").map(String::as_str), Some("inbound"));
    assert_eq!(
        meta.get("twilio_protocol").map(String::as_str),
        Some("Call")
    );
    assert_eq!(
        meta.get("twilio_version").map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        meta.get("custom.agent").map(String::as_str),
        Some("support-tr")
    );
    assert_eq!(
        meta.get("custom.FirstName").map(String::as_str),
        Some("Jane")
    );
    assert!(meta.contains_key("peer_addr"));
    // The typed accessor the engine uses.
    assert_eq!(
        info.sample_rate().unwrap(),
        Some(CallRates::symmetric(SampleRate::Hz8000))
    );
    assert_eq!(rig.server.stats().calls, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_start_without_connected_is_accepted() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, CALL_SID);
    assert!(!info.metadata.contains_key("twilio_protocol"));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn inbound_media_is_decoded_to_pcm16_at_8k_in_order() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    // Three frames, each a different constant mu-law byte.
    let frames: Vec<Vec<u8>> = [0x10u8, 0x55, 0xC0].iter().map(|b| vec![*b; 160]).collect();
    for frame in &frames {
        phone.media(frame).await;
    }
    for frame in &frames {
        let TransportEvent::Audio(audio) =
            next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
        else {
            unreachable!()
        };
        assert_eq!(audio.sample_rate(), SampleRate::Hz8000);
        assert_eq!(audio.data().len(), 320, "160 mu-law bytes are 160 samples");
        let mut expected = vec![0u8; 320];
        Codec::Ulaw.decode(frame, &mut expected).unwrap();
        assert_eq!(audio.data().as_ref(), expected.as_slice());
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_in, 3);
    assert_eq!(stats.inbound_chunks_lost, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn media_on_other_tracks_and_other_streams_is_ignored() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    // An outbound-track media message, a media message for another stream, then a good one.
    phone
        .send_value(&json!({
            "event": "media", "sequenceNumber": "9",
            "media": {"track": "outbound", "chunk": "1", "timestamp": "0", "payload": "/////w=="},
            "streamSid": STREAM_SID,
        }))
        .await;
    phone
        .send_value(&json!({
            "event": "media", "sequenceNumber": "10",
            "media": {"track": "inbound", "chunk": "2", "timestamp": "20", "payload": "/////w=="},
            "streamSid": "MZffffffffffffffffffffffffffffffff",
        }))
        .await;
    phone.media(&[0x20; 160]).await;
    let TransportEvent::Audio(audio) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
    else {
        unreachable!()
    };
    assert_eq!(audio.data().len(), 320);
    let mut expected = vec![0u8; 320];
    Codec::Ulaw.decode(&[0x20; 160], &mut expected).unwrap();
    assert_eq!(audio.data().as_ref(), expected.as_slice());
    let stats = rig.server.stats();
    assert_eq!(stats.media_in, 1);
    assert_eq!(stats.foreign_stream_messages, 1);
    // Neither was a fault: the call is fine.
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn skipped_chunk_numbers_are_counted_as_lost_audio() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media(&[0x30; 160]).await; // chunk 1
    phone.chunk += 3; // chunks 2..=4 never arrive
    phone.media(&[0x30; 160]).await; // chunk 5
    next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    assert_eq!(rig.server.stats().inbound_chunks_lost, 3);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn numbers_may_arrive_as_json_numbers_too() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone
        .send_value(&json!({
            "event": "media", "sequenceNumber": 4,
            "media": {"track": "inbound", "chunk": 2, "timestamp": 40, "payload": "/////w=="},
            "streamSid": STREAM_SID,
        }))
        .await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn fields_the_adapter_does_not_use_cannot_cost_audio_or_the_call() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    // `connected` with a numeric version and an object for the protocol.
    phone
        .send_value(&json!({"event": "connected", "protocol": {"x": 1}, "version": 1}))
        .await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.metadata["twilio_version"], "1");
    assert!(!info.metadata.contains_key("twilio_protocol"));
    // Media with a float, an empty and a null timestamp, and odd sequence numbers and chunks.
    for (sequence, chunk, timestamp) in [
        (json!("1.0"), json!("1"), json!("5.5")),
        (json!(2), json!(2.5), json!("")),
        (json!(null), json!([1]), json!(null)),
    ] {
        phone
            .send_value(&json!({
                "event": "media", "sequenceNumber": sequence, "streamSid": STREAM_SID,
                "media": {"track": "inbound", "chunk": chunk, "timestamp": timestamp,
                          "payload": "/////w=="},
            }))
            .await;
    }
    for _ in 0..3 {
        let event = next_event(&mut session, |e| {
            matches!(e, TransportEvent::Audio(_) | TransportEvent::Error { .. })
        })
        .await;
        assert!(matches!(event, TransportEvent::Audio(_)), "{event:?}");
    }
    assert_eq!(rig.server.stats().faults, 0);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Outbound audio

#[tokio::test]
async fn outbound_audio_is_base64_mulaw_in_20ms_media_messages() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    let samples = [1000i16, -2000, 3000, -4000, 500];
    for (seq, sample) in samples.iter().enumerate() {
        session
            .send(audio_command(&rig, 0, seq as u64, *sample))
            .await
            .unwrap();
    }
    let mut payloads = Vec::new();
    while payloads.len() < samples.len() {
        match phone.recv().await.unwrap() {
            Out::Media(bytes) => payloads.push(bytes),
            other => panic!("unexpected {other:?}"),
        }
    }
    for (payload, sample) in payloads.iter().zip(samples) {
        assert_eq!(payload.len(), 160, "20 ms of mu-law");
        assert_eq!(*payload, mulaw20(sample));
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_out, 5);
    assert_eq!(stats.media_out_bytes, 800);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_media_message_has_exactly_the_documented_fields() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    session.send(audio_command(&rig, 0, 0, 700)).await.unwrap();
    // Read the raw frame.
    let raw = loop {
        match tokio::time::timeout(WAIT, phone.ws.next()).await.unwrap() {
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    let value: Value = serde_json::from_str(raw.as_str()).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 3, "{value}");
    assert_eq!(value["event"], "media");
    assert_eq!(value["streamSid"], STREAM_SID);
    let media = value["media"].as_object().unwrap();
    assert_eq!(media.len(), 1, "only the payload: {value}");
    let payload = media["payload"].as_str().unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap(),
        mulaw20(700)
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_larger_engine_frame_is_cut_into_chunk_sized_messages() {
    let twilio = TwilioConfig {
        chunk_bytes: 80,
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, session) = rig.pair().await;
    // 60 ms in one engine frame: 480 mu-law bytes, six messages of 80 bytes.
    let mut pcm = Vec::new();
    for sample in [100i16, 200, 300] {
        pcm.extend(pcm20(sample));
    }
    session
        .send(audio_command_at(
            &rig.clock,
            SampleRate::Hz8000,
            0,
            0,
            pcm.clone(),
        ))
        .await
        .unwrap();
    let mut got = Vec::new();
    for _ in 0..6 {
        let Out::Media(bytes) = phone.recv().await.unwrap() else {
            panic!("not a media message");
        };
        assert_eq!(bytes.len(), 80);
        got.extend(bytes);
    }
    let mut expected = vec![0u8; 480];
    Codec::Ulaw.encode(&pcm, &mut expected).unwrap();
    assert_eq!(got, expected);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_engine_frame_that_is_not_a_multiple_of_the_chunk_size_keeps_its_tail() {
    // Default chunk of 160 bytes. A 31.25 ms frame (250 mu-law bytes) leaves a 90-byte tail, and
    // a 100-byte frame is shorter than one chunk: neither may lose a byte.
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    let ramp = |samples: i16, step: i16| -> Vec<u8> {
        (0..samples)
            .flat_map(|i| i.wrapping_mul(step).to_le_bytes())
            .collect()
    };
    let long = ramp(250, 37);
    let short = ramp(100, -53);
    for (seq, pcm) in [&long, &short].into_iter().enumerate() {
        session
            .send(audio_command_at(
                &rig.clock,
                SampleRate::Hz8000,
                0,
                seq as u64,
                pcm.clone(),
            ))
            .await
            .unwrap();
    }
    let mut lengths = Vec::new();
    let mut got = Vec::new();
    while got.len() < 350 {
        let Out::Media(bytes) = phone.recv().await.unwrap() else {
            panic!("not a media message");
        };
        lengths.push(bytes.len());
        got.extend(bytes);
    }
    assert_eq!(lengths, [160, 90, 100], "the remainder is its own message");
    let mut expected = vec![0u8; 350];
    Codec::Ulaw.encode(&long, &mut expected[..250]).unwrap();
    Codec::Ulaw.encode(&short, &mut expected[250..]).unwrap();
    assert_eq!(got, expected);
    let wait = async {
        while rig.server.stats().media_out_bytes < 350 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("bytes not counted");
    let stats = rig.server.stats();
    assert_eq!(stats.media_out, 3);
    assert_eq!(stats.media_out_bytes, 350);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn bot_audio_at_another_rate_is_dropped_with_one_fault() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..3u64 {
        session
            .send(audio_command_at(
                &rig.clock,
                SampleRate::Hz16000,
                0,
                seq,
                vec![9; 640],
            ))
            .await
            .unwrap();
    }
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("8000"), "{message}");
    // A good frame still plays, and it is the first thing the phone gets.
    session.send(audio_command(&rig, 0, 3, 500)).await.unwrap();
    assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(500)));
    // Only one fault was reported.
    pause(Duration::from_millis(100)).await;
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(50), session.recv()).await
    {
        assert!(
            !matches!(event, TransportEvent::Error { .. }),
            "second fault: {event:?}"
        );
    }
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Marks and playback tracking

#[tokio::test]
async fn an_engine_mark_after_an_utterance_is_echoed_as_mark_played() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..3u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "utterance-1-end".to_owned(),
        })
        .await
        .unwrap();
    // Three media messages, then the mark, in that order. The engine's id is not on the wire.
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert!(seen[..3].iter().all(|o| matches!(o, Out::Media(_))));
    let Out::Mark(name) = &seen[3] else {
        unreachable!()
    };
    assert_eq!(name, "e0");
    // Twilio echoes it when the audio has played.
    phone.echo_mark(name).await;
    let event = next_event(&mut session, |e| {
        matches!(
            e,
            TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
        )
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::MarkPlayed {
            id: "utterance-1-end".to_owned()
        }
    );
    let stats = rig.server.stats();
    assert_eq!((stats.marks_sent, stats.marks_echoed), (1, 1));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn any_mark_id_works_because_ids_never_go_on_the_wire() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let long = "x".repeat(1000);
    let ids = [
        "",
        "with \"quotes\" and \\ and \n",
        "ünïcödé-ß",
        long.as_str(),
    ];
    for id in ids {
        session
            .send(TransportCommand::Mark { id: id.to_owned() })
            .await
            .unwrap();
        let Out::Mark(name) = phone.recv().await.unwrap() else {
            panic!("not a mark");
        };
        assert!(name.len() < 10, "{name}");
        phone.echo_mark(&name).await;
        assert_eq!(
            next_event(&mut session, |e| matches!(
                e,
                TransportEvent::MarkPlayed { .. }
            ))
            .await,
            TransportEvent::MarkPlayed { id: id.to_owned() }
        );
    }
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn progress_marks_are_sent_every_interval_and_their_echoes_stay_inside_the_adapter() {
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(100),
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, mut session) = rig.pair().await;
    // 600 ms of audio: a progress mark after each 100 ms (five frames).
    for seq in 0..30u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    let mut marks = Vec::new();
    let mut media_before_mark = 0usize;
    let mut media = 0usize;
    while media < 30 {
        match phone.recv().await.unwrap() {
            Out::Media(_) => {
                media += 1;
                if marks.is_empty() {
                    media_before_mark += 1;
                }
            }
            Out::Mark(name) => marks.push(name),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        media_before_mark, 5,
        "the first mark follows 100 ms of audio"
    );
    assert!(
        (5..=6).contains(&marks.len()),
        "{} progress marks for 600 ms",
        marks.len()
    );
    assert!(marks.iter().all(|m| m.starts_with('p')), "{marks:?}");
    for name in &marks {
        phone.echo_mark(name).await;
    }
    // The echoes tell the adapter how long Twilio took, and produce no engine event.
    let wait = async {
        while rig.server.stats().marks_echoed < marks.len() as u64 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("echoes not counted");
    let stats = rig.server.stats();
    // The mark that follows the 30th frame may still be on its way to the phone.
    let read = marks.len() as u64;
    assert!((read..=read + 1).contains(&stats.marks_sent), "{stats:?}");
    assert!(stats.max_echo_latency > Duration::ZERO);
    assert!(stats.max_unplayed >= Duration::from_millis(100));
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(100), session.recv()).await
    {
        assert!(
            !matches!(
                event,
                TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
            ),
            "progress marks must not reach the engine: {event:?}"
        );
    }
    session.close(WAIT).await.unwrap();
}

/// The latest `PlaybackLead` the engine can read right now, or `None`.
async fn lead_event(session: &mut TransportSession) -> Duration {
    match next_event(session, |e| {
        matches!(e, TransportEvent::PlaybackLead { .. })
    })
    .await
    {
        TransportEvent::PlaybackLead { lead } => lead,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn progress_mark_echoes_tell_the_engine_how_far_the_caller_is_behind() {
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(100),
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, mut session) = rig.pair().await;
    // 200 ms of audio with a progress mark after each 100 ms.
    for seq in 0..10u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    let mut marks = Vec::new();
    let mut media = 0usize;
    while media < 10 || marks.len() < 2 {
        match phone.recv().await.unwrap() {
            Out::Media(_) => media += 1,
            Out::Mark(name) => marks.push(name),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(marks.len(), 2, "{marks:?}");
    // Everything is sent. Twilio has played the first 100 ms: 100 ms are still ahead of the
    // caller. The progress echo reaches the engine as a lead, not as a mark event.
    phone.echo_mark(&marks[0]).await;
    assert_eq!(lead_event(&mut session).await, Duration::from_millis(100));
    // All played: the caller has caught up.
    phone.echo_mark(&marks[1]).await;
    assert_eq!(lead_event(&mut session).await, Duration::ZERO);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn unread_lead_reports_are_coalesced_to_the_latest_and_never_stall_the_call() {
    // A lead report is a level: a new one replaces a queued one. An engine that does not read
    // for a while must find exactly one report (the latest) and a call that is still up.
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(20),
        ..fast_twilio()
    };
    let server = ServerConfig {
        send_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, twilio).await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..12u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    // Read everything the phone is going to get: 12 frames and the progress marks between them.
    let mut marks = Vec::new();
    let mut media = 0usize;
    let mut media_after_last_mark = 0u64;
    while media < 12 {
        match phone.recv().await.unwrap() {
            Out::Media(_) => {
                media += 1;
                media_after_last_mark += 1;
            }
            Out::Mark(name) => {
                marks.push(name);
                media_after_last_mark = 0;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(marks.len() >= 8, "{} progress marks", marks.len());
    // Echo them all in one go while the engine reads nothing.
    for name in &marks {
        phone.echo_mark(name).await;
    }
    let wait = async {
        while rig.server.stats().marks_echoed < marks.len() as u64 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("echoes not counted");
    // Let the last report travel through the wire queue to the engine's queue.
    pause(Duration::from_millis(200)).await;
    let mut leads = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(100), session.recv()).await
    {
        if let TransportEvent::PlaybackLead { lead } = event {
            leads.push(lead);
        }
    }
    // The first echo alone left several frames ahead of the caller; the one report that is
    // left is the final level.
    assert_eq!(
        leads,
        [Duration::from_millis(20 * media_after_last_mark)],
        "{} marks echoed",
        marks.len()
    );
    // The call was never treated as stalled.
    assert_eq!(rig.server.stats().faults, 0);
    assert_eq!(rig.server.active_calls(), 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_engine_mark_echo_is_followed_by_the_lead() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..3u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "end".to_owned(),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    let Some(Out::Mark(name)) = seen.last() else {
        panic!("no mark in {seen:?}");
    };
    // 40 ms more audio goes out after the mark, before it is echoed.
    for seq in 3..5u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    phone.recv_until(|o| matches!(o, Out::Media(_))).await;
    phone.recv_until(|o| matches!(o, Out::Media(_))).await;
    phone.echo_mark(name).await;
    // "Played" first, then the level: the mark covers 60 ms, the 40 ms after it are unplayed.
    let first = next_event(&mut session, |e| {
        matches!(
            e,
            TransportEvent::MarkPlayed { .. } | TransportEvent::PlaybackLead { .. }
        )
    })
    .await;
    assert_eq!(
        first,
        TransportEvent::MarkPlayed {
            id: "end".to_owned()
        }
    );
    assert_eq!(lead_event(&mut session).await, Duration::from_millis(40));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn echoes_of_marks_flushed_by_a_clear_produce_no_lead() {
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(40),
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..6u64 {
        session
            .send(audio_command(&rig, 0, seq, 100))
            .await
            .unwrap();
    }
    phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    phone.recv_until(|o| matches!(o, Out::Clear)).await;
    let pending = phone.unechoed.clone();
    assert!(!pending.is_empty());
    for name in &pending {
        phone.echo_mark(name).await;
    }
    let wait = async {
        while rig.server.stats().marks_unmatched < pending.len() as u64 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("echoes not seen");
    // Flushed audio never played, so there is nothing to report.
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(100), session.recv()).await
    {
        assert!(
            !matches!(event, TransportEvent::PlaybackLead { .. }),
            "a flushed mark must not produce a lead: {event:?}"
        );
    }
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_goodbye_that_waits_for_its_mark_is_played_before_the_hang_up() {
    // The documented rule for the engine (module docs of `twilio`): audio, utterance-end mark,
    // wait for MarkPlayed, then Stop. The socket closes only after that, with all audio sent.
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..5u64 {
        session
            .send(audio_command(&rig, 0, seq, 800))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "bye".to_owned(),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    assert_eq!(
        seen.iter().filter(|o| matches!(o, Out::Media(_))).count(),
        5
    );
    let Some(Out::Mark(name)) = seen.last() else {
        unreachable!()
    };
    phone.echo_mark(name).await;
    next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    session.send(TransportCommand::Stop).await.unwrap();
    let (rest, close) = phone.drain().await;
    assert!(rest.is_empty(), "nothing follows the played mark: {rest:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
}

#[tokio::test]
async fn a_mark_that_twilio_never_echoes_does_not_play() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    session
        .send(TransportCommand::Mark {
            id: "never".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(phone.recv().await.unwrap(), Out::Mark(_)));
    // An echo of a name we never sent, and of a name in our scheme that is not in flight.
    phone.echo_mark("my label").await;
    phone.echo_mark("e99").await;
    // The reader has processed both echoes once the counter says so; only then is "no event"
    // meaningful.
    let wait = async {
        while rig.server.stats().marks_unmatched < 2 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("echoes not seen");
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(50), session.recv()).await
    {
        assert!(
            !matches!(event, TransportEvent::MarkPlayed { .. }),
            "{event:?}"
        );
    }
    assert_eq!(rig.server.stats().marks_unmatched, 2);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Barge-in: clear

#[tokio::test]
async fn clear_sends_the_clear_message_in_order_and_drops_queued_audio() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;

    // Thirty frames (600 ms) of epoch 0, then a mark behind them.
    for seq in 0..30u64 {
        session
            .send(audio_command(&rig, 0, seq, 100))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "behind-audio".to_owned(),
        })
        .await
        .unwrap();
    // Let a few frames out, then interrupt.
    let mut before = 0;
    while before < 3 {
        assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(100)));
        before += 1;
    }
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 100..105u64 {
        session
            .send(audio_command(&rig, 1, seq, -3000))
            .await
            .unwrap();
    }

    // The phone sees the clear in order; nothing of the old epoch follows it, and the mark that
    // was cut before it was released is never sent.
    let mut old_before_clear = before;
    let mut cleared = false;
    let mut fresh = 0;
    while fresh < 5 {
        match phone.recv().await.unwrap() {
            Out::Media(bytes) if bytes == mulaw20(100) => {
                assert!(!cleared, "old audio after the clear");
                old_before_clear += 1;
            }
            Out::Media(bytes) => {
                assert!(cleared, "new audio before the clear message");
                assert_eq!(bytes, mulaw20(-3000));
                fresh += 1;
            }
            Out::Clear => {
                assert!(!cleared, "two clears");
                cleared = true;
            }
            other => panic!("unexpected {other:?} (the cut mark must not be sent)"),
        }
    }
    // The clear is applied when it is sent: it purges the session's queue and the wire queue
    // (8 frames) at once, so at most the frame the writer already holds goes out ahead of it,
    // plus the frames the 20 ms pacer releases while this test thread is scheduled. Allow 8
    // frames (160 ms) of scheduling slack on a loaded machine: a clear that waited behind the
    // queued audio would let 25 or all 30 through. The invariant that matters (no old audio
    // after the clear message) is asserted in the loop above.
    const SLACK_FRAMES: usize = 8;
    assert!(
        old_before_clear <= before + SLACK_FRAMES,
        "{old_before_clear} old frames went out before the clear (3 were read before it was sent)"
    );
    // Nothing else of the old epoch was written either: old frames, the 5 new ones, and at most
    // one more the writer had in hand.
    let written = usize::try_from(rig.server.stats().media_out).unwrap();
    assert!(
        written <= before + SLACK_FRAMES + 5 + 1,
        "{written} media messages were written"
    );
    assert_eq!(rig.server.stats().clears_sent, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_mark_cut_before_it_was_sent_resolves_as_mark_cleared() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for seq in 0..30u64 {
        session
            .send(audio_command(&rig, 0, seq, 100))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "behind-audio".to_owned(),
        })
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(phone.recv().await.unwrap(), Out::Media(_)));
    }
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let event = next_event(&mut session, |e| {
        matches!(
            e,
            TransportEvent::MarkCleared { .. } | TransportEvent::MarkPlayed { .. }
        )
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::MarkCleared {
            id: "behind-audio".to_owned()
        }
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn marks_twilio_returns_on_a_clear_are_dropped_and_resolve_as_mark_cleared() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;

    // Three frames and a mark are on Twilio's side: the mark was sent, not yet echoed.
    for seq in 0..3u64 {
        session
            .send(audio_command(&rig, 0, seq, 100))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "sent-not-played".to_owned(),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    let Some(Out::Mark(old_name)) = seen.last() else {
        unreachable!()
    };
    let old_name = old_name.clone();

    // Barge-in. Per the docs, Twilio answers the clear by returning the pending mark; on the
    // wire that looks exactly like a played mark.
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Clear)).await;
    assert_eq!(seen.last(), Some(&Out::Clear));
    phone.echo_mark(&old_name).await;

    // A new utterance with its own mark, sent after the clear, round-trips normally.
    session
        .send(audio_command(&rig, 1, 10, -500))
        .await
        .unwrap();
    session
        .send(TransportCommand::Mark {
            id: "new-utterance".to_owned(),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    let Some(Out::Mark(new_name)) = seen.last() else {
        unreachable!()
    };
    assert_ne!(*new_name, old_name, "mark names are never reused");
    let new_name = new_name.clone();
    phone.echo_mark(&new_name).await;

    // The engine hears: the old mark cleared, the new one played. Never the old one played.
    let mut resolved = Vec::new();
    while resolved.len() < 2 {
        let event = next_event(&mut session, |e| {
            matches!(
                e,
                TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
            )
        })
        .await;
        resolved.push(event);
    }
    assert_eq!(
        resolved,
        vec![
            TransportEvent::MarkCleared {
                id: "sent-not-played".to_owned()
            },
            TransportEvent::MarkPlayed {
                id: "new-utterance".to_owned()
            },
        ]
    );
    let stats = rig.server.stats();
    assert_eq!(stats.marks_cut, 1);
    assert_eq!(stats.marks_unmatched, 1, "the echo the clear caused");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn progress_marks_flushed_by_a_clear_are_forgotten_too() {
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(40),
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, session) = rig.pair().await;
    for seq in 0..6u64 {
        session
            .send(audio_command(&rig, 0, seq, 100))
            .await
            .unwrap();
    }
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    assert!(matches!(seen.last(), Some(Out::Mark(_))));
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    phone.recv_until(|o| matches!(o, Out::Clear)).await;
    let pending = phone.unechoed.clone();
    assert!(!pending.is_empty());
    for name in &pending {
        phone.echo_mark(name).await;
    }
    let wait = async {
        while rig.server.stats().marks_unmatched < pending.len() as u64 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .expect("echoes not seen");
    let stats = rig.server.stats();
    assert_eq!(stats.marks_echoed, 0, "a flushed mark is not a played mark");
    assert!(stats.marks_cut >= 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_clear_with_nothing_buffered_is_still_sent() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    assert_eq!(phone.recv().await.unwrap(), Out::Clear);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// DTMF, stop and the ways a call ends

#[tokio::test]
async fn dtmf_reaches_the_engine_in_order() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for key in ["1", "2", "#", "*", "0", "A"] {
        phone.dtmf(key).await;
    }
    let mut digits = Vec::new();
    while digits.len() < 6 {
        if let TransportEvent::Dtmf { digit, .. } =
            next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await
        {
            digits.push(digit);
        }
    }
    assert_eq!(
        digits,
        [
            DtmfDigit::D1,
            DtmfDigit::D2,
            DtmfDigit::Pound,
            DtmfDigit::Star,
            DtmfDigit::D0,
            DtmfDigit::A
        ]
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_invalid_dtmf_digit_is_a_fault_and_the_call_goes_on() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone.dtmf("x").await;
    phone.dtmf("12").await;
    phone.dtmf("5").await;
    let mut faults = 0;
    loop {
        match next_event(&mut session, |e| {
            matches!(
                e,
                TransportEvent::Error { .. } | TransportEvent::Dtmf { .. }
            )
        })
        .await
        {
            TransportEvent::Error { message } => {
                assert!(message.contains("dtmf"), "{message}");
                faults += 1;
            }
            TransportEvent::Dtmf { digit, .. } => {
                assert_eq!(digit, DtmfDigit::D5);
                break;
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(faults, 2);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn stop_is_a_hang_up() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    // The server closes the socket (it has nothing to say: Twilio accepts no stop message).
    let (seen, close) = phone.drain().await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_close_frame_is_a_hang_up_and_a_vanished_socket_is_a_disconnect() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone.ws.close(None).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();

    let (phone, mut session) = rig.pair().await;
    drop(phone);
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn the_engine_hanging_up_closes_the_socket_with_a_normal_close() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    session.send(TransportCommand::Stop).await.unwrap();
    let (seen, close) = phone.drain().await;
    // No JSON of any kind: Twilio accepts only media, mark and clear.
    assert!(seen.iter().all(|o| matches!(o, Out::Media(_))), "{seen:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn shutdown_closes_every_call_going_away() {
    let mut rig = Rig::default().await;
    let (mut a, _sa) = rig.pair().await;
    let (mut b, _sb) = rig.pair().await;
    rig.server.shutdown(WAIT).await.unwrap();
    assert_eq!(a.drain().await.1, Some(CLOSE_AWAY));
    assert_eq!(b.drain().await.1, Some(CLOSE_AWAY));
}

// ---------------------------------------------------------------------------------------------
// Malformed input

#[tokio::test]
async fn malformed_messages_are_faults_and_the_call_survives() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for text in [
        "this is not json",
        "[]",
        r#"{"event":"media"}"#,
        r#"{"event":"media","media":{"payload":"@@@not base64@@@"}}"#,
        r#"{"event":"mark"}"#,
        r#"{"event":"start","start":{"streamSid":"MZ1"}}"#,
        r#"{"streamSid":"MZ1"}"#,
    ] {
        phone.send_text(text).await;
        let TransportEvent::Error { message } =
            next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
        else {
            unreachable!()
        };
        assert!(!message.is_empty(), "{text}");
    }
    // Unknown events and fields are not faults.
    phone
        .send_text(r#"{"event":"somethingNew","streamSid":"MZ1","extra":[1,2,3]}"#)
        .await;
    // The call still works.
    phone.media(&[0x40; 160]).await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::Audio(_) | TransportEvent::Error { .. })
    })
    .await;
    assert!(matches!(event, TransportEvent::Audio(_)), "{event:?}");
    assert_eq!(rig.server.stats().faults, 7);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_flood_of_faults_closes_the_call() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for _ in 0..20 {
        phone.send_text("garbage").await;
    }
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_POLICY));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_oversized_message_closes_the_call_with_a_size_code() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let big = format!(
        r#"{{"event":"media","media":{{"payload":"{}"}}}}"#,
        "A".repeat(40_000)
    );
    // Over max_control_bytes (16 KiB) but under the websocket layer's limit (64 KiB).
    phone.send_text(&big).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_SIZE));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_frame_above_the_websocket_limit_closes_the_call_with_a_size_code() {
    // The websocket layer's limit is max(max_audio_bytes, max_control_bytes) = 4096 here: a frame
    // just above it is refused by tungstenite itself (a capacity error), before any JSON is
    // looked at. (A far larger frame would lose the close frame to a TCP reset, since most of it
    // is still unread; the generic websocket tests do the same.)
    let server = ServerConfig {
        max_audio_bytes: 4096,
        max_control_bytes: 1024,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_twilio()).await;
    let (mut phone, mut session) = rig.pair().await;
    let huge = format!(
        r#"{{"event":"media","media":{{"payload":"{}"}}}}"#,
        "A".repeat(4100)
    );
    phone.send_text(&huge).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_SIZE));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_media_payload_above_the_audio_limit_is_a_fault_and_the_call_goes_on() {
    // max_audio_bytes is 1 KiB here (512 samples of mu-law), max_control_bytes still 16 KiB, so a
    // 4 KB payload passes the control check and hits the audio cap in the media handler.
    let server = ServerConfig {
        max_audio_bytes: 1024,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_twilio()).await;
    let (mut phone, mut session) = rig.pair().await;
    phone
        .send_value(&json!({
            "event": "media", "streamSid": STREAM_SID,
            "media": {"track": "inbound", "payload": "A".repeat(4000)},
        }))
        .await;
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("exceeds"), "{message}");
    // A payload within the limit still works.
    phone.media(&[0x40; 160]).await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::Audio(_) | TransportEvent::Error { .. })
    })
    .await;
    assert!(matches!(event, TransportEvent::Audio(_)), "{event:?}");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_binary_frame_is_ignored() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone
        .ws
        .send(Message::Binary(Bytes::from_static(&[1, 2, 3])))
        .await
        .unwrap();
    phone.media(&[0x50; 160]).await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::Audio(_) | TransportEvent::Error { .. })
    })
    .await;
    assert!(matches!(event, TransportEvent::Audio(_)), "{event:?}");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_second_start_is_a_fault_not_a_new_call() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    phone.start(json!({"callSid": "CAother"})).await;
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("start"), "{message}");
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// The opening: what a start may and may not be

#[tokio::test]
async fn a_start_with_another_media_format_is_refused() {
    for format in [
        json!({"encoding": "audio/x-alaw", "sampleRate": 8000, "channels": 1}),
        json!({"encoding": "audio/x-mulaw", "sampleRate": 16000, "channels": 1}),
        json!({"encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 2}),
    ] {
        let rig = Rig::default().await;
        let mut phone = rig.phone().await;
        phone.connected().await;
        phone.start(json!({"mediaFormat": format})).await;
        let (_, close) = phone.drain().await;
        assert_eq!(close, Some(CLOSE_POLICY), "{format}");
    }
}

#[tokio::test]
async fn media_before_start_and_a_missing_start_are_refused() {
    let rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.connected().await;
    phone.media(&[0; 160]).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));

    let mut phone = rig.phone().await;
    phone.send_text("nope").await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_PROTOCOL));

    // Nothing at all within the start timeout.
    let config = ServerConfig {
        start_timeout: Duration::from_millis(200),
        ..fast_server()
    };
    let rig = Rig::start(config, fast_twilio()).await;
    let mut phone = rig.phone().await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_start_without_a_stream_sid_is_refused() {
    let rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone
        .send_text(r#"{"event":"start","start":{"callSid":"CA1"}}"#)
        .await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_PROTOCOL));
}

#[tokio::test]
async fn another_path_is_answered_with_404_and_a_trailing_slash_is_accepted() {
    let mut rig = Rig::default().await;
    match rig.try_connect("/elsewhere", None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 404),
        other => panic!("expected HTTP 404, got {:?}", other.map(|_| ())),
    }
    let mut phone = Phone::new(rig.try_connect("/twilio/stream/", None).await.unwrap());
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_call_limit_answers_503() {
    let config = ServerConfig {
        max_calls: 1,
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let (_phone, _session) = rig.pair().await;
    match rig.try_connect(PATH, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 503),
        other => panic!("expected HTTP 503, got {:?}", other.map(|_| ())),
    }
}

// ---------------------------------------------------------------------------------------------
// X-Twilio-Signature

fn expect_status(result: Result<Client, WsError>, status: u16) {
    match result {
        Err(WsError::Http(response)) => assert_eq!(response.status(), status),
        other => panic!("expected HTTP {status}, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn a_valid_signature_is_accepted() {
    let mut rig = Rig::start(fast_server(), signed_twilio()).await;
    let header = signature::sign(TOKEN, PUBLIC_URL);
    let mut phone = Phone::new(rig.try_connect(PATH, Some(&header)).await.unwrap());
    phone.connected().await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn every_url_form_twilio_might_sign_is_accepted() {
    let rig = Rig::start(fast_server(), signed_twilio()).await;
    for form in [
        "wss://agent.example.com/twilio/stream",
        "https://agent.example.com/twilio/stream",
        "wss://agent.example.com:443/twilio/stream",
        "https://agent.example.com:443/twilio/stream",
        "wss://agent.example.com/twilio/stream/",
        "https://agent.example.com/twilio/stream/",
    ] {
        let header = signature::sign(TOKEN, form);
        let ws = rig.try_connect(PATH, Some(&header)).await;
        assert!(ws.is_ok(), "{form} was refused");
    }
}

#[tokio::test]
async fn a_missing_or_wrong_signature_is_refused_with_403() {
    let rig = Rig::start(fast_server(), signed_twilio()).await;
    expect_status(rig.try_connect(PATH, None).await, 403);
    expect_status(rig.try_connect(PATH, Some("")).await, 403);
    expect_status(rig.try_connect(PATH, Some("!!!not-base64!!!")).await, 403);
    expect_status(
        rig.try_connect(PATH, Some(&signature::sign("wrong-token", PUBLIC_URL)))
            .await,
        403,
    );
    expect_status(
        rig.try_connect(
            PATH,
            Some(&signature::sign(TOKEN, "wss://agent.example.com/other")),
        )
        .await,
        403,
    );
    expect_status(
        rig.try_connect(
            PATH,
            Some(&signature::sign(
                TOKEN,
                "wss://evil.example.com/twilio/stream",
            )),
        )
        .await,
        403,
    );
    // A signature the server would accept, but on the wrong path: the path is checked first.
    expect_status(
        rig.try_connect("/other", Some(&signature::sign(TOKEN, PUBLIC_URL)))
            .await,
        404,
    );
    // Refused upgrades take no call slot.
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_gateway_path_that_differs_from_the_listener_path_is_signed_over_the_public_url() {
    // A gateway serves wss://agent.example.com/voice/twilio and forwards to /twilio/stream. The
    // signature is over the URL as written in the TwiML, so the request path (which the gateway
    // rewrote) and `config.path` must play no part in it.
    const GATEWAY_URL: &str = "wss://agent.example.com/voice/twilio";
    let twilio = TwilioConfig {
        public_url: Some(GATEWAY_URL.to_owned()),
        ..signed_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let header = signature::sign(TOKEN, GATEWAY_URL);
    let mut phone = Phone::new(rig.try_connect(PATH, Some(&header)).await.unwrap());
    phone.connected().await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();
    // A signature over the host plus the listener path is not what Twilio signed: refused.
    expect_status(
        rig.try_connect(PATH, Some(&signature::sign(TOKEN, PUBLIC_URL)))
            .await,
        403,
    );
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn without_an_auth_token_the_signature_is_not_checked() {
    let rig = Rig::default().await;
    assert!(rig.try_connect(PATH, None).await.is_ok());
    assert!(rig.try_connect(PATH, Some("garbage")).await.is_ok());
}

#[tokio::test]
async fn the_server_refuses_to_start_with_an_inconsistent_configuration() {
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let no_url = TwilioConfig {
        auth_token: Some(Secret::new(TOKEN.to_owned())),
        allow_static_url: true,
        ..fast_twilio()
    };
    let error = TwilioServer::bind(fast_server(), no_url, clock.clone(), &parent)
        .await
        .err()
        .expect("must refuse");
    assert!(error.to_string().contains("public_url"), "{error}");
    let error = TwilioServer::bind(
        ServerConfig {
            max_calls: 0,
            ..fast_server()
        },
        fast_twilio(),
        clock,
        &parent,
    )
    .await
    .err()
    .expect("must refuse");
    assert!(error.to_string().contains("max_calls"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Keepalive, slow Twilio and task hygiene

#[tokio::test]
async fn the_server_pings_and_a_silent_peer_hits_the_idle_timeout() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let (mut phone, mut session) = rig.pair().await;
    // Twilio does not poll its socket, so it never answers a ping.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    // Now it reads: pings, then the close.
    let mut pings = 0;
    let close = loop {
        match tokio::time::timeout(WAIT, phone.ws.next()).await.unwrap() {
            Some(Ok(Message::Ping(_))) => pings += 1,
            Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            other => panic!("connection ended: {other:?}"),
        }
    };
    assert!(pings >= 2, "{pings} pings");
    assert_eq!(close, Some(CLOSE_POLICY));
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn pings_keep_a_quiet_call_alive() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let (mut phone, session) = rig.pair().await;
    let mut pings = 0;
    let idle = async {
        loop {
            match phone.ws.next().await {
                Some(Ok(Message::Ping(_))) => pings += 1,
                Some(Ok(_)) => {}
                other => panic!("connection ended: {other:?}"),
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(750), idle).await;
    assert!(pings >= 5, "only {pings} pings in 750 ms");
    session.send(audio_command(&rig, 0, 0, 800)).await.unwrap();
    assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(800)));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_peer_that_takes_no_output_is_dropped_without_stalling_input() {
    let config = ServerConfig {
        write_timeout: Duration::from_millis(600),
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let mut phone = rig.small_buffer_phone().await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;

    // Six minutes of audio in one frame: more than the sockets buffer (the receive buffer is
    // tiny), and the phone reads none of it.
    session
        .send(audio_command_at(
            &rig.clock,
            SampleRate::Hz8000,
            0,
            0,
            vec![1; 6 << 20],
        ))
        .await
        .unwrap();
    pause(Duration::from_millis(150)).await;
    // While the writer is stuck, the engine still hears the phone.
    for _ in 0..3 {
        phone.media(&[0x33; 160]).await;
    }
    for _ in 0..3 {
        next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    }
    // Past the write timeout the call is closed.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn every_way_a_twilio_call_can_end_leaks_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let mut rig = Rig::default().await;

        // Stop.
        let (mut phone, mut session) = rig.pair().await;
        phone.stop().await;
        assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
        phone.drain().await;
        session.close(WAIT).await.unwrap();

        // An abrupt drop.
        let (phone, mut session) = rig.pair().await;
        drop(phone);
        assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
        session.close(WAIT).await.unwrap();
        wait_for_active(&rig, 0).await;

        // The engine hanging up.
        let (mut phone, session) = rig.pair().await;
        session.send(TransportCommand::Stop).await.unwrap();
        phone.drain().await;
        drop(session);

        // A fatal fault.
        let (mut phone, mut session) = rig.pair().await;
        for _ in 0..20 {
            phone.send_text("garbage").await;
        }
        stopped(&mut session).await;
        session.close(WAIT).await.unwrap();

        // Refused upgrades and half-open connections, then several calls cut off by shutdown.
        let _ = rig.try_connect("/nope", None).await;
        let _half_open = TcpStream::connect(rig.server.local_addr()).await.unwrap();
        let mut open = Vec::new();
        for _ in 0..3 {
            open.push(rig.pair().await);
        }
        let _unstarted = rig.phone().await;
        drop(open);
        rig.server.shutdown(WAIT).await.unwrap();
    }
    counter
        .wait_for_baseline(&*clock, WAIT)
        .await
        .expect("tasks outlived the server and its calls");
}

// ---------------------------------------------------------------------------------------------
// A whole conversation, as the doc describes it

#[tokio::test]
async fn a_conversation_with_barge_in_over_one_stream() {
    let twilio = TwilioConfig {
        mark_interval: Duration::from_millis(100),
        ..fast_twilio()
    };
    let mut rig = Rig::start(fast_server(), twilio).await;
    let (mut phone, mut session) = rig.pair().await;

    // The caller speaks and presses a key.
    phone.media(&[0x20; 160]).await;
    phone.dtmf("5").await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;

    // The bot answers with a long utterance and marks its end.
    for seq in 0..40u64 {
        session
            .send(audio_command(&rig, 0, seq, 200))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "answer-1".to_owned(),
        })
        .await
        .unwrap();
    for _ in 0..4 {
        assert!(matches!(phone.recv().await.unwrap(), Out::Media(_)));
    }

    // The caller interrupts: the engine clears and starts the next answer.
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 100..110u64 {
        session
            .send(audio_command(&rig, 1, seq, -200))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "answer-2".to_owned(),
        })
        .await
        .unwrap();

    // Twilio: after the clear only new audio; it echoes every mark it gets, as the real one
    // does for the flushed ones.
    let mut cleared = false;
    let mut fresh = 0;
    let mut played = None;
    while played.is_none() {
        match phone.recv().await.unwrap() {
            Out::Media(bytes) if bytes == mulaw20(200) => {
                assert!(!cleared, "old audio after clear")
            }
            Out::Media(bytes) => {
                assert!(cleared);
                assert_eq!(bytes, mulaw20(-200));
                fresh += 1;
            }
            Out::Clear => cleared = true,
            Out::Mark(name) => {
                phone.echo_mark(&name).await;
                if name.starts_with('e') && cleared {
                    played = Some(name);
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(fresh, 10);
    // The engine's view: answer-1 cleared, answer-2 played.
    let mut resolved = Vec::new();
    while resolved.len() < 2 {
        resolved.push(
            next_event(&mut session, |e| {
                matches!(
                    e,
                    TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
                )
            })
            .await,
        );
    }
    assert_eq!(
        resolved,
        vec![
            TransportEvent::MarkCleared {
                id: "answer-1".to_owned()
            },
            TransportEvent::MarkPlayed {
                id: "answer-2".to_owned()
            },
        ]
    );

    // The caller hangs up.
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Live (needs a Twilio account, a number and a public wss URL; see docs/providers/twilio.md)

/// A short real call. Skipped unless `BRISVOX_LIVE_TESTS=1`. Reads `TWILIO_AUTH_TOKEN` and
/// `TWILIO_LIVE_PUBLIC_URL` (the `wss://` URL in the TwiML, which a tunnel or proxy forwards to
/// `TWILIO_LIVE_BIND`, default `127.0.0.1:8081`), waits up to `TWILIO_LIVE_WAIT_SECS` (default
/// 120) for a call, listens for one second of caller audio, plays a second of tone, expects the
/// mark echo and hangs up. Never prints the token.
#[tokio::test]
async fn live_call_with_a_real_twilio_account() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set BRISVOX_LIVE_TESTS=1 (and TWILIO_AUTH_TOKEN, TWILIO_LIVE_PUBLIC_URL)"
        );
        return;
    }
    let (Ok(token), Ok(public_url)) = (
        std::env::var("TWILIO_AUTH_TOKEN"),
        std::env::var("TWILIO_LIVE_PUBLIC_URL"),
    ) else {
        eprintln!("skipped: TWILIO_AUTH_TOKEN and TWILIO_LIVE_PUBLIC_URL are required");
        return;
    };
    let bind = std::env::var("TWILIO_LIVE_BIND").unwrap_or_else(|_| "127.0.0.1:8081".to_owned());
    let wait_secs: u64 = std::env::var("TWILIO_LIVE_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let path = public_url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|at| rest.split_at(at).1.to_owned()))
        .unwrap_or_else(|| "/".to_owned());
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let mut server = TwilioServer::bind(
        ServerConfig {
            bind_addr: bind.parse().expect("TWILIO_LIVE_BIND"),
            ..ServerConfig::default()
        },
        TwilioConfig {
            path,
            auth_token: Some(Secret::new(token)),
            public_url: Some(public_url),
            // The live test is pointed at a fixed TwiML URL.
            allow_static_url: true,
            ..TwilioConfig::default()
        },
        clock.clone(),
        &parent,
    )
    .await
    .unwrap();
    eprintln!(
        "waiting {wait_secs} s for a call on {}",
        server.local_addr()
    );
    let cancel = CancellationToken::new();
    let link = tokio::time::timeout(Duration::from_secs(wait_secs), server.accept(&cancel))
        .await
        .expect("no call within TWILIO_LIVE_WAIT_SECS")
        .unwrap()
        .expect("server closed");
    let mut session =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &parent)
            .await
            .unwrap();
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    let info: CallInfo = info;
    assert_eq!(
        info.metadata.get("encoding").map(String::as_str),
        Some("mulaw")
    );
    let mut heard = 0;
    while heard < 50 {
        if let TransportEvent::Audio(_) =
            next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
        {
            heard += 1;
        }
    }
    for seq in 0..50u64 {
        let pcm: Vec<u8> = (0..160)
            .flat_map(|i| {
                let phase = (seq * 160 + i) as f32 * 440.0 * std::f32::consts::TAU / 8000.0;
                ((phase.sin() * 6000.0) as i16).to_le_bytes()
            })
            .collect();
        session
            .send(audio_command_at(&clock, SampleRate::Hz8000, 0, seq, pcm))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "live-end".to_owned(),
        })
        .await
        .unwrap();
    let played = next_event(&mut session, |e| {
        matches!(
            e,
            TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
        )
    })
    .await;
    assert_eq!(
        played,
        TransportEvent::MarkPlayed {
            id: "live-end".to_owned()
        }
    );
    eprintln!("live stats: {:?}", server.stats());
    session.send(TransportCommand::Stop).await.unwrap();
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Admission gate

/// Admits with a ticket holding 7, or refuses as shutting down.
struct Gate(std::sync::atomic::AtomicBool);

impl brisvox_transport::admission::AdmissionGate for Gate {
    fn try_admit(
        &self,
    ) -> Result<brisvox_transport::admission::AdmissionTicket, brisvox_transport::admission::Refusal>
    {
        if self.0.load(std::sync::atomic::Ordering::SeqCst) {
            Err(brisvox_transport::admission::Refusal::ShuttingDown)
        } else {
            Ok(brisvox_transport::admission::AdmissionTicket::new(7_u32))
        }
    }
}

#[tokio::test]
async fn a_gate_admits_the_stream_and_its_ticket_reaches_the_link() {
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gate = std::sync::Arc::new(Gate(std::sync::atomic::AtomicBool::new(false)));
    let shared: brisvox_transport::admission::SharedGate = gate.clone();
    let server = TwilioServer::serve_gated(
        listener,
        fast_server(),
        fast_twilio(),
        clock.clone(),
        &parent,
        Some(shared),
    )
    .unwrap();
    let mut rig = Rig {
        clock,
        parent,
        server,
    };
    let mut phone = rig.phone().await;
    phone.connected().await;
    phone.start(json!({})).await;
    let cancel = CancellationToken::new();
    let mut link = tokio::time::timeout(
        WAIT,
        brisvox_transport::Transport::accept(&mut rig.server, &cancel),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let ticket = link
        .take_ticket()
        .expect("the link carries the gate's ticket");
    assert_eq!(ticket.downcast::<u32>().unwrap(), 7);

    // Refused by the gate: HTTP 503 before the upgrade.
    gate.0.store(true, std::sync::atomic::Ordering::SeqCst);
    match rig.try_connect(PATH, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), 503),
        Err(e) => panic!("expected a 503, got {e}"),
        Ok(_) => panic!("expected a 503, the stream was upgraded"),
    }
    drop(link);
    rig.server.shutdown(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Streams that end on purpose and come back

const SECOND_STREAM_SID: &str = "MZ2222222222222222222222222222222a";

/// The call comes back on a new stream: a phone with another stream sid whose `start` carries the
/// `resume` parameter.
async fn resumed_phone(rig: &Rig) -> Phone {
    let mut phone = rig.phone().await;
    phone.stream_sid = SECOND_STREAM_SID.to_owned();
    phone.connected().await;
    phone
        .start(json!({"customParameters": {"resume": "dtmf"}}))
        .await;
    phone
}

/// Nothing reaches the engine for a while (it would be a hang-up).
async fn assert_no_stop(session: &mut TransportSession) {
    let quiet = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            match session.recv().await {
                Some(TransportEvent::Stopped { reason }) => return Some(reason),
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await;
    assert!(
        quiet.is_err(),
        "the engine was told the call ended: {quiet:?}"
    );
}

#[tokio::test]
async fn an_announced_stop_keeps_the_session_and_a_resume_stream_continues_it() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let resumer = rig.server.stream_resumer();
    assert!(resumer.expect_resume(CALL_SID, Duration::from_secs(10)));
    phone.stop().await;
    // The adapter closes the old stream and tells the engine nothing.
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_NORMAL));
    assert_no_stop(&mut session).await;
    // What the engine says while no stream exists is held for the new one ...
    session.send(audio_command(&rig, 0, 0, 500)).await.unwrap();

    let mut second = resumed_phone(&rig).await;
    // ... and the engine's session goes on with the new socket: output reaches it, in order.
    session.send(audio_command(&rig, 0, 1, 700)).await.unwrap();
    let mut media = Vec::new();
    while media.len() < 2 {
        if let Some(Out::Media(m)) = second.recv().await {
            media.push(m);
        }
    }
    assert_ne!(
        media[0], media[1],
        "the held audio comes first, then the new"
    );
    // ... and its input reaches the engine.
    second.media(&[0xFF; 160]).await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await;
    let stats = rig.server.stats();
    assert_eq!(stats.sessions_parked, 1);
    assert_eq!(stats.streams_resumed, 1);
    assert_eq!(stats.calls, 1, "one call, two streams");
    // The new stream is an ordinary one: its stop is a hang-up (not announced).
    second.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_resume_stream_that_arrives_before_the_old_stream_ended_is_held() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    assert!(
        rig.server
            .stream_resumer()
            .expect_resume(CALL_SID, Duration::from_secs(10))
    );
    // Twilio starts the new stream before it closes the old one.
    let mut second = resumed_phone(&rig).await;
    pause(Duration::from_millis(100)).await;
    assert_no_stop(&mut session).await;
    phone.stop().await;
    // The old stream's stop must be handled before the audio goes out: a pacer that releases its
    // first frame at once would otherwise race the stop and write the single frame to the
    // stream that is going away. The call stays up on the new stream.
    assert_no_stop(&mut session).await;
    session.send(audio_command(&rig, 0, 0, 700)).await.unwrap();
    let seen = second.recv_until(|o| matches!(o, Out::Media(_))).await;
    assert!(matches!(seen.last(), Some(Out::Media(_))));
    assert_eq!(rig.server.stats().streams_resumed, 1);
    drop(second);
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_vanished_socket_and_a_close_frame_are_expected_too() {
    let mut rig = Rig::default().await;
    for vanish in [true, false] {
        let (mut phone, mut session) = rig.pair().await;
        let resumer = rig.server.stream_resumer();
        assert!(resumer.expect_resume(CALL_SID, Duration::from_secs(10)));
        if vanish {
            drop(phone);
        } else {
            phone.ws.close(None).await.unwrap();
        }
        assert_no_stop(&mut session).await;
        let mut second = resumed_phone(&rig).await;
        session.send(audio_command(&rig, 0, 0, 700)).await.unwrap();
        second.recv_until(|o| matches!(o, Out::Media(_))).await;
        session.send(TransportCommand::Stop).await.unwrap();
        let (_, close) = second.drain().await;
        assert_eq!(close, Some(CLOSE_NORMAL));
        wait_for_active(&rig, 0).await;
    }
}

#[tokio::test]
async fn a_resume_stream_nobody_expects_is_refused_and_the_call_goes_on() {
    let mut rig = Rig::default().await;
    // No call at all.
    let mut orphan = resumed_phone(&rig).await;
    assert_eq!(orphan.drain().await.1, Some(CLOSE_POLICY));
    // A live call whose stream was not announced.
    let (mut phone, mut session) = rig.pair().await;
    let mut orphan = resumed_phone(&rig).await;
    assert_eq!(orphan.drain().await.1, Some(CLOSE_POLICY));
    session.send(audio_command(&rig, 0, 0, 700)).await.unwrap();
    phone.recv_until(|o| matches!(o, Out::Media(_))).await;
    assert_eq!(rig.server.stats().streams_resumed, 0);
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
}

#[tokio::test]
async fn a_withdrawn_announcement_makes_the_stop_a_hang_up_again() {
    // The host's update was refused, the stream never ended on purpose; when the
    // caller then really hangs up, the engine must hear it at once and not after the linger.
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let resumer = rig.server.stream_resumer();
    assert!(resumer.expect_resume(CALL_SID, Duration::from_secs(600)));
    assert!(resumer.cancel_expectation(CALL_SID));
    assert!(
        !resumer.cancel_expectation(CALL_SID),
        "nothing left to withdraw"
    );
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    assert_eq!(rig.server.stats().sessions_parked, 0);
}

#[tokio::test]
async fn a_resume_stream_gets_in_when_the_server_is_full() {
    // The parked session keeps its own permit, so its next stream takes no
    // capacity and must not be turned away by the call limit.
    let config = ServerConfig {
        max_calls: 1,
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let (mut phone, mut session) = rig.pair().await;
    assert!(
        rig.server
            .stream_resumer()
            .expect_resume(CALL_SID, Duration::from_secs(10))
    );
    phone.stop().await;
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_NORMAL));
    assert_no_stop(&mut session).await;

    // A new call is still refused at the limit (it is not a continuation).
    let mut stranger = rig.phone().await;
    stranger.connected().await;
    stranger.start(json!({})).await;
    assert_eq!(stranger.drain().await.1, Some(CLOSE_AGAIN));

    // The continuation of the parked call is served although the limit is reached.
    let mut second = resumed_phone(&rig).await;
    session.send(audio_command(&rig, 0, 0, 500)).await.unwrap();
    second.recv_until(|o| matches!(o, Out::Media(_))).await;
    assert_eq!(rig.server.stats().streams_resumed, 1);
    second.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_unexpected_resume_stream_at_the_limit_is_still_a_503() {
    let config = ServerConfig {
        max_calls: 1,
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_twilio()).await;
    let (_phone, _session) = rig.pair().await;
    // Nobody waits for a continuation: the over-limit connection is not even upgraded.
    match rig.try_connect(PATH, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 503),
        other => panic!("expected HTTP 503, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn an_announcement_for_another_call_changes_nothing() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    assert!(!rig.server.stream_resumer().expect_resume(
        "CAffffffffffffffffffffffffffffffff",
        Duration::from_secs(10)
    ));
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
}

#[tokio::test]
async fn a_parked_session_nobody_resumed_ends_as_a_disconnect_after_its_wait() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    assert!(
        rig.server
            .stream_resumer()
            .expect_resume(CALL_SID, Duration::from_millis(300))
    );
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    // The call is forgotten: a late resume is refused.
    let mut late = resumed_phone(&rig).await;
    assert_eq!(late.drain().await.1, Some(CLOSE_POLICY));
}

#[tokio::test]
async fn the_engine_hanging_up_a_parked_call_ends_it() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    assert!(
        rig.server
            .stream_resumer()
            .expect_resume(CALL_SID, Duration::from_secs(30))
    );
    phone.stop().await;
    pause(Duration::from_millis(100)).await;
    session.send(TransportCommand::Stop).await.unwrap();
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_barge_in_while_the_stream_is_away_drops_what_was_held() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    assert!(
        rig.server
            .stream_resumer()
            .expect_resume(CALL_SID, Duration::from_secs(10))
    );
    phone.stop().await;
    session.send(audio_command(&rig, 0, 0, 500)).await.unwrap();
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    pause(Duration::from_millis(200)).await;
    let mut second = resumed_phone(&rig).await;
    // Only audio sent after the clear (of the newer epoch) is played.
    session.send(audio_command(&rig, 1, 1, 900)).await.unwrap();
    let first = second.recv_until(|o| matches!(o, Out::Media(_))).await;
    assert_eq!(first.last(), Some(&Out::Media(mulaw20(900))), "{first:?}");
    assert!(
        !first.contains(&Out::Clear),
        "the clear was applied to the held audio, not replayed"
    );
}

// ---- Per-call stream URLs ----

/// Real time plus a shift of the wall clock, so a test can move past an expiry without waiting.
#[derive(Debug, Default)]
struct ShiftedClock {
    shift_secs: std::sync::atomic::AtomicU64,
}

impl brisvox_core::Clock for ShiftedClock {
    fn now(&self) -> tokio::time::Instant {
        SystemClock.now()
    }

    fn wall(&self) -> std::time::SystemTime {
        SystemClock.wall()
            + Duration::from_secs(self.shift_secs.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> tokio::time::Sleep {
        SystemClock.sleep_until(deadline)
    }
}

const STREAM_SECRET: &str = "per-call-stream-secret-0123456789abcdef";

fn per_call_twilio(legacy: bool) -> TwilioConfig {
    TwilioConfig {
        auth_token: Some(Secret::new(TOKEN.to_owned())),
        public_url: Some(PUBLIC_URL.to_owned()),
        stream_secret: Some(Secret::new(STREAM_SECRET.to_owned())),
        allow_static_url: legacy,
        ..fast_twilio()
    }
}

/// A per-call rig: the server on a shiftable clock and the signer an operator's webhook (or the
/// server's own TwiML) would use.
struct PerCall {
    rig: Rig,
    clock: std::sync::Arc<ShiftedClock>,
    signer: brisvox_transport::twilio::stream_token::StreamUrlSigner,
}

impl PerCall {
    async fn start(twilio: TwilioConfig) -> Self {
        let clock = std::sync::Arc::new(ShiftedClock::default());
        let shared: SharedClock = clock.clone();
        let parent = CancellationToken::new();
        let server = TwilioServer::bind(fast_server(), twilio, shared.clone(), &parent)
            .await
            .unwrap();
        let signer = brisvox_transport::twilio::stream_token::StreamUrlSigner::new(
            &Secret::new(STREAM_SECRET.to_owned()),
            shared.clone(),
            Duration::from_secs(120),
        )
        .unwrap();
        Self {
            rig: Rig {
                clock: shared,
                parent,
                server,
            },
            clock,
            signer,
        }
    }

    fn shift(&self, secs: u64) {
        self.clock
            .shift_secs
            .fetch_add(secs, std::sync::atomic::Ordering::Relaxed);
    }

    /// `(path, signature)` of the per-call URL with `segment`, signed as Twilio would sign it.
    fn signed(segment: &str) -> (String, String) {
        let signature = signature::sign(TOKEN, &format!("{PUBLIC_URL}/{segment}"));
        (format!("{PATH}/{segment}"), signature)
    }

    async fn connect(&self, segment: &str) -> Result<Client, WsError> {
        let (path, signature) = Self::signed(segment);
        self.rig.try_connect(&path, Some(&signature)).await
    }
}

#[tokio::test]
async fn a_valid_per_call_url_opens_the_call_for_its_own_sid_only() {
    let mut per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    let ws = per.connect(&segment).await.expect("valid URL accepted");
    let mut phone = Phone::new(ws);
    phone.connected().await;
    phone.start(json!({})).await;
    let mut session = per.rig.accept().await;
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert!(matches!(started, TransportEvent::Started(_)));
}

#[tokio::test]
async fn a_per_call_url_cannot_open_another_call() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per
        .signer
        .segment("CAother0000000000000000000000000000")
        .unwrap();
    let ws = per
        .connect(&segment)
        .await
        .expect("the upgrade itself is valid");
    let mut phone = Phone::new(ws);
    phone.connected().await;
    // The start names CALL_SID, not the call the URL was issued for.
    phone.start(json!({})).await;
    assert_eq!(phone.recv().await, Some(Out::Close(Some(CLOSE_POLICY))));
}

#[tokio::test]
async fn a_replayed_per_call_url_is_refused_with_403() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    let first = per.connect(&segment).await.expect("first use");
    // Even with the very same signed upgrade (a captured request), the second one is refused.
    expect_status(per.connect(&segment).await, 403);
    drop(first);
    expect_status(per.connect(&segment).await, 403);
}

#[tokio::test]
async fn an_expired_per_call_url_is_refused_with_403() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    per.shift(121);
    expect_status(per.connect(&segment).await, 403);
    // A fresh one still works.
    let fresh = per.signer.segment(CALL_SID).unwrap();
    assert!(per.connect(&fresh).await.is_ok());
}

#[tokio::test]
async fn a_tampered_per_call_url_is_refused_with_403() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    let (sid, rest) = segment.split_once('.').unwrap();
    let (exp, mac) = rest.split_once('.').unwrap();
    let longer = exp.parse::<u64>().unwrap() + 3000;
    let flipped = format!(
        "{}{}",
        &mac[..mac.len() - 1],
        if mac.ends_with('A') { 'B' } else { 'A' }
    );
    for bad in [
        format!("{sid}.{longer}.{mac}"),
        format!("CAevil.{exp}.{mac}"),
        format!("{sid}.{exp}.{flipped}"),
        format!("{sid}.{exp}"),
        "garbage".to_owned(),
    ] {
        expect_status(per.connect(&bad).await, 403);
    }
    // None of that used the real one up.
    assert!(per.connect(&segment).await.is_ok());
}

#[tokio::test]
async fn a_signature_over_another_url_is_refused_and_does_not_burn_the_token() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    let path = format!("{PATH}/{segment}");
    // The signature of the fixed URL (what a captured legacy upgrade carries).
    let fixed = signature::sign(TOKEN, PUBLIC_URL);
    expect_status(per.rig.try_connect(&path, Some(&fixed)).await, 403);
    // The signature of another call's URL.
    let other = per
        .signer
        .segment("CAother0000000000000000000000000000")
        .unwrap();
    let (_, other_signature) = PerCall::signed(&other);
    expect_status(
        per.rig.try_connect(&path, Some(&other_signature)).await,
        403,
    );
    // No signature at all.
    expect_status(per.rig.try_connect(&path, None).await, 403);
    // A wrong auth token (an attacker without Twilio's token but with the URL).
    let forged = signature::sign("not-the-token", &format!("{PUBLIC_URL}/{segment}"));
    expect_status(per.rig.try_connect(&path, Some(&forged)).await, 403);
    // The legitimate upgrade still works: nothing above used the token up.
    assert!(per.connect(&segment).await.is_ok());
}

#[tokio::test]
async fn the_fixed_url_is_refused_when_per_call_urls_are_on_unless_legacy_is_allowed() {
    let strict = PerCall::start(per_call_twilio(false)).await;
    let fixed = signature::sign(TOKEN, PUBLIC_URL);
    expect_status(strict.rig.try_connect(PATH, Some(&fixed)).await, 403);
    let legacy = PerCall::start(per_call_twilio(true)).await;
    assert!(legacy.rig.try_connect(PATH, Some(&fixed)).await.is_ok());
    // Both forms work side by side in the legacy mode.
    let segment = legacy.signer.segment(CALL_SID).unwrap();
    assert!(legacy.connect(&segment).await.is_ok());
}

#[tokio::test]
async fn deeper_paths_and_servers_without_a_secret_answer_404() {
    let per = PerCall::start(per_call_twilio(false)).await;
    let segment = per.signer.segment(CALL_SID).unwrap();
    expect_status(
        per.rig
            .try_connect(&format!("{PATH}/{segment}/more"), Some("x"))
            .await,
        404,
    );
    // A server with only the fixed URL has no per-call routes.
    let fixed_only = Rig::start(fast_server(), signed_twilio()).await;
    expect_status(
        fixed_only
            .try_connect(&format!("{PATH}/{segment}"), Some("x"))
            .await,
        404,
    );
}
