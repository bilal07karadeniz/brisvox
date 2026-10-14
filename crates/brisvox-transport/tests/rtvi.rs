//! RTVI transport tests: real sockets on 127.0.0.1, the real clock, short deadlines.
//!
//! Each test starts an `RtviServer`, plays the engine with a `TransportSession` on the accepted
//! call and plays the Pipecat JS client with a scripted `Browser`. The browser speaks the wire
//! format of `docs/providers/rtvi.md` with its own small protobuf codec (it does not use the
//! crate's `proto` module), so the tests check the bytes, not just a round trip: every message is
//! one binary websocket frame holding one `pipecat.Frame`, RTVI JSON travels as a string inside
//! `MessageFrame.data` (field 4 / field 1), audio as `AudioRawFrame` (field 2: audio 3, sample_rate
//! 4, num_channels 5), and the server must never send a text frame or an `InterruptionFrame`.
#![cfg(feature = "rtvi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)] // test helpers (see clippy.toml)

use std::time::Duration;

use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::rtvi::{
    BotOutput, ErrorData, RtviCall, RtviConfig, RtviEvent, RtviRequest, RtviSender, RtviServer,
    SpokenStatus, TextData,
};
use brisvox_transport::websocket::ServerConfig;
use brisvox_transport::{
    CallRates, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(5);

const CLOSE_NORMAL: u16 = 1000;
const CLOSE_AWAY: u16 = 1001;
const CLOSE_UNSUPPORTED: u16 = 1003;
const CLOSE_POLICY: u16 = 1008;

const PATH: &str = "/rtvi";

/// The fill byte of the audio in epoch 0 (what `Engine::say` sends) and of the audio in epoch 1:
/// the two are told apart on the wire.
const OLD: u8 = 7;
const NEW: u8 = 9;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

// --- a minimal protobuf codec, independent of the crate under test ---------------------------

fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A length-delimited field.
fn pb_bytes(tag: u32, payload: &[u8], out: &mut Vec<u8>) {
    varint(u64::from(tag << 3 | 2), out);
    varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// A varint field.
fn pb_varint(tag: u32, value: u64, out: &mut Vec<u8>) {
    varint(u64::from(tag << 3), out);
    varint(value, out);
}

/// `Frame{ message: MessageFrame{ data: json } }`, as the JS client sends it.
fn pb_message(json: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    pb_bytes(1, json.as_bytes(), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(4, &inner, &mut frame);
    frame
}

/// `Frame{ audio: AudioRawFrame{ name: "audio", audio, sample_rate, num_channels: 1 } }`, as the
/// JS client sends it (`id` 0 is not encoded).
fn pb_audio(pcm: &[u8], sample_rate: u32, channels: u32) -> Vec<u8> {
    let mut inner = Vec::new();
    pb_bytes(2, b"audio", &mut inner);
    pb_bytes(3, pcm, &mut inner);
    pb_varint(4, u64::from(sample_rate), &mut inner);
    pb_varint(5, u64::from(channels), &mut inner);
    let mut frame = Vec::new();
    pb_bytes(2, &inner, &mut frame);
    frame
}

#[derive(Debug, PartialEq)]
enum Field {
    Varint(u64),
    Bytes(Vec<u8>),
}

fn read_varint(buf: &[u8], at: &mut usize) -> u64 {
    let mut value = 0_u64;
    let mut shift = 0;
    loop {
        let byte = buf[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn pb_fields(buf: &[u8]) -> Vec<(u32, Field)> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let key = read_varint(buf, &mut at);
        let tag = (key >> 3) as u32;
        match key & 7 {
            0 => fields.push((tag, Field::Varint(read_varint(buf, &mut at)))),
            2 => {
                let len = read_varint(buf, &mut at) as usize;
                fields.push((tag, Field::Bytes(buf[at..at + len].to_vec())));
                at += len;
            }
            other => panic!("unexpected wire type {other}"),
        }
    }
    fields
}

fn field_bytes(fields: &[(u32, Field)], tag: u32) -> Option<&[u8]> {
    fields.iter().find_map(|(t, f)| match f {
        Field::Bytes(bytes) if *t == tag => Some(bytes.as_slice()),
        _ => None,
    })
}

fn field_varint(fields: &[(u32, Field)], tag: u32) -> Option<u64> {
    fields.iter().find_map(|(t, f)| match f {
        Field::Varint(v) if *t == tag => Some(*v),
        _ => None,
    })
}

// --- the scripted browser --------------------------------------------------------------------

/// What the server sent.
#[derive(Debug)]
enum Incoming {
    /// An RTVI message (the JSON of a `MessageFrame`).
    Rtvi(Value),
    /// An `AudioRawFrame`.
    Audio {
        pcm: Vec<u8>,
        sample_rate: u32,
        channels: u32,
    },
    /// A close frame with its status code.
    Close(Option<u16>),
}

struct Browser {
    ws: Client,
    next_id: u32,
}

impl Browser {
    fn new(ws: Client) -> Self {
        Self { ws, next_id: 0 }
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("{:08x}", 0xa1b2_c3d4_u32 + self.next_id)
    }

    async fn send_binary(&mut self, bytes: Vec<u8>) {
        tokio::time::timeout(WAIT, self.ws.send(Message::Binary(Bytes::from(bytes))))
            .await
            .expect("send timed out")
            .expect("send failed");
    }

    /// An RTVI message the way `PipecatClient` sends it: the envelope with a fresh `id`.
    async fn rtvi(&mut self, kind: &str, data: Value) -> String {
        let id = self.id();
        self.rtvi_with_id(kind, &id, data).await;
        id
    }

    async fn rtvi_with_id(&mut self, kind: &str, id: &str, data: Value) {
        let message = json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data});
        self.send_binary(pb_message(&message.to_string())).await;
    }

    async fn audio(&mut self, pcm: &[u8], sample_rate: u32) {
        self.send_binary(pb_audio(pcm, sample_rate, 1)).await;
    }

    /// `client-ready` exactly as `RTVIMessage.clientReady()` of client-js 1.13.1 builds it.
    async fn client_ready(&mut self) -> String {
        self.rtvi(
            "client-ready",
            json!({"version": "2.1.0",
                   "about": {"library": "@pipecat-ai/client-js", "library_version": "1.13.1"}}),
        )
        .await
    }

    /// The next frame from the server (pings skipped). Panics on a text frame or an
    /// undecodable frame: the server must send neither.
    async fn recv(&mut self) -> Option<Incoming> {
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
            .expect("no frame from the server within the wait");
        match next {
            None | Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => None,
            Some(Err(WsError::Protocol(_))) => None,
            Some(Err(error)) => panic!("websocket error: {error}"),
            Some(Ok(Message::Close(frame))) => {
                Some(Incoming::Close(frame.map(|f| u16::from(f.code))))
            }
            Some(Ok(Message::Text(text))) => {
                panic!("the server sent a text frame, the JS client throws on it: {text}")
            }
            Some(Ok(Message::Binary(bytes))) => Some(decode(&bytes)),
            Some(Ok(other)) => panic!("unexpected message {other:?}"),
        }
    }

    /// The next RTVI message; audio in between fails the test.
    async fn next_rtvi(&mut self) -> Value {
        match self.recv().await {
            Some(Incoming::Rtvi(value)) => value,
            other => panic!("expected an RTVI message, got {other:?}"),
        }
    }

    /// RTVI messages until one of `kind`, skipping audio and other RTVI messages.
    async fn until(&mut self, kind: &str) -> Value {
        loop {
            match self.recv().await {
                Some(Incoming::Rtvi(value)) if value["type"] == kind => return value,
                Some(_) => {}
                None => panic!("connection ended while waiting for {kind}"),
            }
        }
    }

    /// Everything until the connection ends; the status code of the close frame, if any.
    async fn closed(&mut self) -> Option<u16> {
        loop {
            match self.recv().await {
                Some(Incoming::Close(code)) => return code,
                Some(_) => {}
                None => return None,
            }
        }
    }
}

/// Decodes one binary websocket message as the JS client's `ProtobufFrameSerializer` does:
/// only `audio` (field 2) and `message` (field 4) exist for it.
fn decode(bytes: &[u8]) -> Incoming {
    let frame = pb_fields(bytes);
    assert_eq!(frame.len(), 1, "a Frame has exactly one variant: {frame:?}");
    let (tag, field) = &frame[0];
    let Field::Bytes(inner) = field else {
        panic!("a Frame variant is a message");
    };
    let inner = pb_fields(inner);
    match tag {
        4 => {
            let json = field_bytes(&inner, 1).expect("MessageFrame.data");
            let json = std::str::from_utf8(json).expect("utf-8");
            Incoming::Rtvi(serde_json::from_str(json).expect("RTVI JSON"))
        }
        2 => Incoming::Audio {
            pcm: field_bytes(&inner, 3).unwrap_or_default().to_vec(),
            sample_rate: field_varint(&inner, 4).unwrap_or(0) as u32,
            channels: field_varint(&inner, 5).unwrap_or(0) as u32,
        },
        other => panic!("the server sent Frame field {other}, which the JS client cannot decode"),
    }
}

// --- the rig ---------------------------------------------------------------------------------

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

fn fast_rtvi() -> RtviConfig {
    RtviConfig {
        lead_interval: Duration::from_millis(20),
        ..RtviConfig::default()
    }
}

struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: RtviServer,
}

impl Rig {
    async fn start(server: ServerConfig, rtvi: RtviConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = RtviServer::bind(server, rtvi, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    async fn default() -> Self {
        Self::start(fast_server(), fast_rtvi()).await
    }

    fn url(&self, path_and_query: &str) -> String {
        format!("ws://{}{path_and_query}", self.server.local_addr())
    }

    async fn try_connect(
        &self,
        path_and_query: &str,
        origin: Option<&str>,
    ) -> Result<Client, WsError> {
        let mut request = self.url(path_and_query).into_client_request().unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("Origin", HeaderValue::from_str(origin).unwrap());
        }
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request))
            .await
            .expect("connect timed out")?;
        Ok(ws)
    }

    async fn browser(&self) -> Browser {
        Browser::new(self.try_connect(PATH, None).await.expect("connect failed"))
    }

    /// The next call, with its RTVI channels and an engine-side session already opened.
    async fn accept(&mut self) -> Engine {
        let cancel = CancellationToken::new();
        let RtviCall {
            link,
            sender,
            inbox,
        } = tokio::time::timeout(WAIT, self.server.accept_call(&cancel))
            .await
            .expect("no call within the wait")
            .unwrap()
            .expect("server closed");
        let session = TransportSession::open(
            self.clock.clone(),
            link,
            SessionConfig::default(),
            &self.parent,
        )
        .await
        .unwrap();
        Engine {
            session,
            sender,
            inbox,
            clock: self.clock.clone(),
            seq: 0,
        }
    }

    /// Connects, completes the handshake and accepts the call. The `Started` event and the
    /// `bot-ready` answer have been read.
    async fn pair(&mut self) -> (Browser, Engine) {
        let mut browser = self.browser().await;
        let id = browser.client_ready().await;
        let mut engine = self.accept().await;
        let started = next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Started(_))
        })
        .await;
        assert!(matches!(started, TransportEvent::Started(_)));
        let ready = browser.next_rtvi().await;
        assert_eq!(ready["type"], "bot-ready");
        assert_eq!(ready["id"], id);
        (browser, engine)
    }
}

struct Engine {
    session: TransportSession,
    sender: RtviSender,
    inbox: brisvox_transport::rtvi::RtviInbox,
    clock: SharedClock,
    seq: u64,
}

impl Engine {
    /// A 20 ms frame of bot audio at 24 kHz, filled with `fill`.
    fn bot_frame(&mut self, rate: SampleRate, epoch: TurnEpoch, fill: u8) -> AudioFrame {
        let samples = (rate.hz() / 50) as usize;
        self.seq += 1;
        AudioFrame::new(
            Bytes::from(vec![fill; samples * 2]),
            rate,
            self.clock.now(),
            self.seq,
            epoch,
        )
        .unwrap()
    }

    async fn say(&mut self, frames: usize, epoch: TurnEpoch, utterance: u64) {
        self.say_fill(frames, epoch, utterance, OLD).await;
    }

    /// Like `say`, with the byte every sample is filled with (so a test can tell the epochs
    /// apart on the wire).
    async fn say_fill(&mut self, frames: usize, epoch: TurnEpoch, utterance: u64, fill: u8) {
        for _ in 0..frames {
            let frame = self.bot_frame(SampleRate::Hz24000, epoch, fill);
            self.session
                .send(TransportCommand::Audio {
                    epoch,
                    utterance: UtteranceId::new(utterance),
                    frame,
                })
                .await
                .unwrap();
        }
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

async fn stopped(session: &mut TransportSession) -> StopReason {
    match next_event(session, |e| matches!(e, TransportEvent::Stopped { .. })).await {
        TransportEvent::Stopped { reason } => reason,
        _ => unreachable!(),
    }
}

/// Makes a pause on the real clock.
async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

// --- handshake -------------------------------------------------------------------------------

#[tokio::test]
async fn the_handshake_answers_bot_ready_with_the_request_id_and_the_version() {
    let mut rig = Rig::default().await;
    let mut browser = rig.browser().await;
    let id = browser.client_ready().await;
    let mut engine = rig.accept().await;
    let ready = browser.next_rtvi().await;
    assert_eq!(ready["label"], "rtvi-ai");
    assert_eq!(ready["type"], "bot-ready");
    assert_eq!(ready["id"], id, "the request id is echoed");
    assert_eq!(ready["data"]["version"], "2.1.0");
    assert_eq!(ready["data"]["about"]["library"], "brisvox");

    let TransportEvent::Started(info) = engine.session.recv().await.unwrap() else {
        panic!("Started is the first event");
    };
    assert!(info.call_id.starts_with("rtvi-"));
    assert_eq!(info.metadata["transport"], "rtvi");
    assert_eq!(info.metadata["rtvi_client_version"], "2.1.0");
    assert_eq!(info.metadata["client.library"], "@pipecat-ai/client-js");
    assert_eq!(info.metadata["client.library_version"], "1.13.1");
    // 16 kHz in, 24 kHz out: what the JS client records and plays at by default.
    let rates = info.sample_rate().unwrap().unwrap();
    assert_eq!(
        rates,
        CallRates {
            input: SampleRate::Hz16000,
            output: SampleRate::Hz24000
        }
    );
    assert_eq!(rig.server.stats().calls, 1);
}

#[tokio::test]
async fn a_client_that_is_not_compatible_gets_a_warning_and_the_call_goes_on() {
    let mut rig = Rig::default().await;
    let mut browser = rig.browser().await;
    let id = browser
        .rtvi("client-ready", json!({"version": "3.0.0", "about": {}}))
        .await;
    let _engine = rig.accept().await;
    let warning = browser.next_rtvi().await;
    assert_eq!(warning["type"], "error-response");
    assert_eq!(warning["id"], id);
    assert_eq!(
        warning["data"]["error"],
        "RTVI version 3.0.0 is not compatible with server protocol 2.1.0. \
         Compatibility issues may occur."
    );
    let ready = browser.next_rtvi().await;
    assert_eq!(ready["type"], "bot-ready");
    assert_eq!(ready["id"], id);
}

#[tokio::test]
async fn a_legacy_client_gets_its_own_version_back_and_a_missing_one_a_warning() {
    let mut rig = Rig::default().await;
    let mut legacy = rig.browser().await;
    legacy
        .rtvi("client-ready", json!({"version": "1.2.0"}))
        .await;
    let _engine = rig.accept().await;
    let ready = legacy.next_rtvi().await;
    assert_eq!(ready["type"], "bot-ready");
    assert_eq!(ready["data"]["version"], "1.2.0");

    let mut unknown = rig.browser().await;
    unknown.rtvi("client-ready", json!({})).await;
    let _engine2 = rig.accept().await;
    let warning = unknown.next_rtvi().await;
    assert_eq!(warning["type"], "error-response");
    assert_eq!(
        warning["data"]["error"],
        "Client version unknown. Compatibility issues may occur."
    );
    assert_eq!(unknown.next_rtvi().await["type"], "bot-ready");
}

#[tokio::test]
async fn audio_before_client_ready_is_dropped_and_other_messages_are_refused() {
    let mut rig = Rig::default().await;
    let mut browser = rig.browser().await;
    // The JS client never does this, but a server must tolerate frames it cannot use yet.
    browser.audio(&[1u8; 640], 16_000).await;
    let early = browser.rtvi("send-text", json!({"content": "hi"})).await;
    let refusal = browser.next_rtvi().await;
    assert_eq!(refusal["type"], "error-response");
    assert_eq!(refusal["id"], early);
    // No call yet: nothing was handed to the engine.
    assert_eq!(rig.server.stats().calls, 0);
    browser.client_ready().await;
    let mut engine = rig.accept().await;
    let started = engine.session.recv().await.unwrap();
    assert!(matches!(started, TransportEvent::Started(_)));
    // The early audio did not leak into the call.
    pause(Duration::from_millis(50)).await;
    assert_eq!(rig.server.stats().audio_in, 0);
}

#[tokio::test]
async fn no_client_ready_in_time_closes_with_policy() {
    let server = ServerConfig {
        start_timeout: Duration::from_millis(150),
        ..fast_server()
    };
    let rig = Rig::start(server, fast_rtvi()).await;
    let mut browser = rig.browser().await;
    assert_eq!(browser.closed().await, Some(CLOSE_POLICY));
}

#[tokio::test]
async fn a_text_frame_before_client_ready_is_refused_with_unsupported_data() {
    let rig = Rig::default().await;
    let mut browser = rig.browser().await;
    tokio::time::timeout(
        WAIT,
        browser.ws.send(Message::text(
            json!({"label":"rtvi-ai","type":"client-ready","id":"x","data":{"version":"2.1.0"}})
                .to_string(),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(browser.closed().await, Some(CLOSE_UNSUPPORTED));
}

// --- audio -----------------------------------------------------------------------------------

#[tokio::test]
async fn client_audio_reaches_the_engine_as_pcm16_at_the_frames_rate() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    let pcm: Vec<u8> = (0..640u32).map(|n| (n % 251) as u8).collect();
    browser.audio(&pcm, 16_000).await;
    let TransportEvent::Audio(frame) = next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(frame.sample_rate(), SampleRate::Hz16000);
    assert_eq!(frame.data().as_ref(), pcm.as_slice());

    // The frame's own sample_rate is authoritative: 48 kHz is passed on as 48 kHz.
    browser.audio(&[0u8; 1920], 48_000).await;
    let TransportEvent::Audio(frame) = next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(frame.sample_rate(), SampleRate::Hz48000);
    assert_eq!(rig.server.stats().audio_in, 2);
}

#[tokio::test]
async fn unusable_client_audio_is_a_fault_not_a_hangup() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    browser.audio(&[0u8; 640], 11_025).await; // unsupported rate
    browser.send_binary(pb_audio(&[0u8; 640], 16_000, 2)).await; // stereo
    browser.audio(&[0u8; 641], 16_000).await; // odd byte count
    for _ in 0..3 {
        let event = next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Error { .. })
        })
        .await;
        assert!(matches!(event, TransportEvent::Error { .. }));
    }
    // The call is alive: good audio still arrives.
    browser.audio(&[0u8; 640], 16_000).await;
    next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await;
}

#[tokio::test]
async fn bot_audio_goes_out_as_pcm16_frames_at_the_player_rate() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    engine.say(5, TurnEpoch::ZERO, 1).await;
    let mut pcm = Vec::new();
    while pcm.len() < 5 * 960 {
        let Incoming::Audio {
            pcm: part,
            sample_rate,
            channels,
        } = browser.recv().await.expect("connection ended")
        else {
            continue; // events between the audio
        };
        assert_eq!(sample_rate, 24_000);
        assert_eq!(channels, 1);
        assert_eq!(
            part.len(),
            960,
            "one 20 ms frame per message, neither merged nor split"
        );
        pcm.extend_from_slice(&part);
    }
    assert_eq!(pcm, vec![7u8; 5 * 960]);
    assert_eq!(rig.server.stats().audio_out_bytes, 5 * 960);
    assert_eq!(rig.server.stats().audio_out, 5);
}

#[tokio::test]
async fn a_bot_frame_above_max_audio_bytes_is_split_into_whole_samples() {
    // 512 leaves 448 bytes of audio per message after the protobuf wrapper's room (64).
    let server = ServerConfig {
        max_audio_bytes: 512,
        max_control_bytes: 512,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, engine) = rig.pair().await;
    // A pattern that tells a lost, repeated or reordered piece apart.
    let original: Vec<u8> = (0..960_u32).map(|i| (i % 251) as u8).collect();
    let frame = AudioFrame::new(
        Bytes::from(original.clone()),
        SampleRate::Hz24000,
        engine.clock.now(),
        1,
        TurnEpoch::ZERO,
    )
    .unwrap();
    engine
        .session
        .send(TransportCommand::Audio {
            epoch: TurnEpoch::ZERO,
            utterance: UtteranceId::new(1),
            frame,
        })
        .await
        .unwrap();
    let mut parts = Vec::new();
    let mut total = 0;
    while total < original.len() {
        let Incoming::Audio {
            pcm, sample_rate, ..
        } = browser.recv().await.expect("connection ended")
        else {
            continue;
        };
        assert_eq!(sample_rate, 24_000);
        total += pcm.len();
        parts.push(pcm);
    }
    let sizes: Vec<usize> = parts.iter().map(Vec::len).collect();
    assert_eq!(sizes, [448, 448, 64], "split at the message limit");
    assert!(sizes.iter().all(|len| len % 2 == 0), "whole PCM16 samples");
    assert_eq!(parts.concat(), original);
}

#[tokio::test]
async fn bot_audio_at_another_rate_is_dropped_and_reported_once() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    for _ in 0..3 {
        let frame = engine.bot_frame(SampleRate::Hz16000, TurnEpoch::ZERO, 9);
        engine
            .session
            .send(TransportCommand::Audio {
                epoch: TurnEpoch::ZERO,
                utterance: UtteranceId::new(1),
                frame,
            })
            .await
            .unwrap();
    }
    let event = next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Error { .. })
    })
    .await;
    let TransportEvent::Error { message } = event else {
        unreachable!()
    };
    assert!(message.contains("mismatched audio is dropped"), "{message}");
    // Then good audio: the first thing the browser gets is audio at 24 kHz, no 16 kHz frame.
    engine.say(1, TurnEpoch::ZERO, 2).await;
    loop {
        if let Incoming::Audio {
            sample_rate, pcm, ..
        } = browser.recv().await.unwrap()
        {
            assert_eq!(sample_rate, 24_000);
            assert_eq!(pcm, vec![7u8; 960]);
            break;
        }
    }
    assert_eq!(rig.server.stats().bot_audio_dropped, 3);
    // The writer has handled all three bad frames by now (the good one came after them): the
    // engine was told once, not three times.
    let mut mismatch_errors = 1; // the one read above
    let window = async {
        while let Some(event) = engine.session.recv().await {
            if matches!(&event, TransportEvent::Error { message }
                if message.contains("mismatched audio is dropped"))
            {
                mismatch_errors += 1;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(400), window).await;
    assert_eq!(
        mismatch_errors, 1,
        "the rate mismatch is reported once per call"
    );
}

#[tokio::test]
async fn nothing_but_binary_protobuf_frames_ever_leaves_the_server() {
    // `Browser::recv` panics on a text frame or an unknown Frame variant (field 5 included);
    // this test drives every kind of server output through it: audio, the engine's own events,
    // the derived speaking events, a barge-in and the engine hanging up.
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    let mut audio = 0;
    let mut types: Vec<String> = Vec::new();
    engine.say(5, TurnEpoch::ZERO, 1).await;
    engine.sender.send(RtviEvent::BotLlmStarted).await.unwrap();
    engine
        .session
        .send(TransportCommand::Mark { id: "m".to_owned() })
        .await
        .unwrap();
    // Until the first audio frame has arrived (so the barge-in below has something to cut).
    while audio == 0 {
        match browser.recv().await.expect("connection ended") {
            Incoming::Audio { .. } => audio += 1,
            Incoming::Rtvi(value) => types.push(value["type"].as_str().unwrap().to_owned()),
            Incoming::Close(code) => panic!("closed early: {code:?}"),
        }
    }
    engine
        .session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    engine.say_fill(2, TurnEpoch::new(1), 2, NEW).await;
    let mut after_clear = 0;
    while after_clear < 1 {
        match browser.recv().await.expect("connection ended") {
            Incoming::Audio { pcm, .. } => {
                audio += 1;
                if pcm == vec![NEW; 960] {
                    after_clear += 1;
                }
            }
            Incoming::Rtvi(value) => types.push(value["type"].as_str().unwrap().to_owned()),
            Incoming::Close(code) => panic!("closed early: {code:?}"),
        }
    }
    engine.session.send(TransportCommand::Stop).await.unwrap();
    let close = loop {
        match browser.recv().await {
            Some(Incoming::Audio { .. }) => audio += 1,
            Some(Incoming::Rtvi(value)) => types.push(value["type"].as_str().unwrap().to_owned()),
            Some(Incoming::Close(code)) => break code,
            None => panic!("the connection ended without a close frame"),
        }
    };
    assert_eq!(close, Some(CLOSE_NORMAL));
    assert!(audio >= 2, "bot audio arrived: {audio}");
    for wanted in [
        "bot-llm-started",
        "bot-started-speaking",
        "bot-interrupted",
        "bot-stopped-speaking",
    ] {
        assert!(types.iter().any(|t| t == wanted), "{wanted} in {types:?}");
    }
}

// --- events to the client -------------------------------------------------------------------

#[tokio::test]
async fn typed_events_arrive_as_rtvi_json_in_order() {
    let mut rig = Rig::default().await;
    let (mut browser, engine) = rig.pair().await;
    let sender = &engine.sender;
    sender.send(RtviEvent::UserStartedSpeaking).await.unwrap();
    sender.send(RtviEvent::UserStoppedSpeaking).await.unwrap();
    sender
        .send(RtviEvent::UserTranscription(
            brisvox_transport::rtvi::UserTranscription {
                text: "merhaba".to_owned(),
                user_id: String::new(),
                timestamp: "2026-10-08T10:00:00.000+00:00".to_owned(),
                is_final: true,
            },
        ))
        .await
        .unwrap();
    sender.send(RtviEvent::BotLlmStarted).await.unwrap();
    sender
        .send(RtviEvent::BotLlmText(TextData::new("Merhaba")))
        .await
        .unwrap();
    sender.send(RtviEvent::BotLlmStopped).await.unwrap();
    sender
        .send(RtviEvent::BotTtsText(TextData::new("Merhaba, nasilsiniz?")))
        .await
        .unwrap();
    sender
        .send(RtviEvent::BotOutput(BotOutput {
            text: "Merhaba, nasilsiniz?".to_owned(),
            aggregated_by: "sentence".to_owned(),
            will_be_spoken: Some(true),
            spoken_status: Some(SpokenStatus::Completed),
        }))
        .await
        .unwrap();
    sender
        .send(RtviEvent::Error(ErrorData {
            error: "stt connection lost".to_owned(),
            fatal: false,
        }))
        .await
        .unwrap();
    sender
        .send(RtviEvent::ServerMessage(json!({"any": "json"})))
        .await
        .unwrap();
    sender.user_transcription("selam", false).await.unwrap();

    let expected = [
        json!({"label":"rtvi-ai","type":"user-started-speaking"}),
        json!({"label":"rtvi-ai","type":"user-stopped-speaking"}),
        json!({"label":"rtvi-ai","type":"user-transcription","data":{
            "text":"merhaba","user_id":"","timestamp":"2026-10-08T10:00:00.000+00:00","final":true}}),
        json!({"label":"rtvi-ai","type":"bot-llm-started"}),
        json!({"label":"rtvi-ai","type":"bot-llm-text","data":{"text":"Merhaba"}}),
        json!({"label":"rtvi-ai","type":"bot-llm-stopped"}),
        json!({"label":"rtvi-ai","type":"bot-tts-text","data":{"text":"Merhaba, nasilsiniz?"}}),
        json!({"label":"rtvi-ai","type":"bot-output","data":{
            "text":"Merhaba, nasilsiniz?","aggregated_by":"sentence",
            "spoken_status":"completed","will_be_spoken":true}}),
        json!({"label":"rtvi-ai","type":"error","data":{"error":"stt connection lost","fatal":false}}),
        json!({"label":"rtvi-ai","type":"server-message","data":{"any":"json"}}),
    ];
    for want in expected {
        assert_eq!(browser.next_rtvi().await, want);
    }
    let last = browser.next_rtvi().await;
    assert_eq!(last["type"], "user-transcription");
    assert_eq!(last["data"]["text"], "selam");
    assert_eq!(last["data"]["final"], false);
    let stamp = last["data"]["timestamp"].as_str().unwrap();
    assert!(stamp.ends_with("+00:00") && stamp.contains('T'), "{stamp}");
    assert!(last.get("id").is_none(), "server events carry no id");
}

#[tokio::test]
async fn an_oversized_event_is_refused_by_the_sender() {
    let mut rig = Rig::default().await;
    let (_browser, engine) = rig.pair().await;
    let big = "x".repeat(40_000);
    let error = engine
        .sender
        .send(RtviEvent::ServerMessage(json!(big)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exceeds"), "{error}");
}

#[tokio::test]
async fn the_adapter_derives_speaking_events_and_acknowledges_marks_from_the_playout() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    engine.say(5, TurnEpoch::ZERO, 1).await;
    engine
        .session
        .send(TransportCommand::Mark {
            id: "end-of-utterance".to_owned(),
        })
        .await
        .unwrap();
    // bot-started-speaking comes before the first audio frame ...
    match browser.recv().await.unwrap() {
        Incoming::Rtvi(value) => assert_eq!(value["type"], "bot-started-speaking"),
        other => panic!("expected bot-started-speaking first, got {other:?}"),
    }
    // ... and bot-stopped-speaking after the last one (the playout estimate ran out).
    let stopped_speaking = browser.until("bot-stopped-speaking").await;
    assert!(stopped_speaking.get("data").is_none());
    // The mark is acknowledged by the estimate, not by the client.
    let played = next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        played,
        TransportEvent::MarkPlayed {
            id: "end-of-utterance".to_owned()
        }
    );
}

#[tokio::test]
async fn the_playout_lead_is_reported_to_the_engine() {
    let mut rig = Rig::default().await;
    let (_browser, mut engine) = rig.pair().await;
    engine.say(10, TurnEpoch::ZERO, 1).await; // 200 ms of audio, sent at once
    // Reports until the estimate runs out (a zero lead after a positive one).
    let mut leads = Vec::new();
    let collect = async {
        loop {
            let event = next_event(&mut engine.session, |e| {
                matches!(e, TransportEvent::PlaybackLead { .. })
            })
            .await;
            let TransportEvent::PlaybackLead { lead } = event else {
                unreachable!()
            };
            leads.push(lead);
            if lead.is_zero() && leads.iter().any(|l| !l.is_zero()) {
                return;
            }
        }
    };
    tokio::time::timeout(WAIT, collect)
        .await
        .unwrap_or_else(|_| panic!("the lead never ran down to zero"));
    // Audio was sent, so the client is ahead by at least one frame at first (a report of
    // zero all along is the bug) and never by more than was sent.
    let peak = leads.iter().max().copied().unwrap();
    assert!(peak >= Duration::from_millis(20), "{leads:?}");
    assert!(
        leads.iter().all(|lead| *lead <= Duration::from_millis(200)),
        "{leads:?}"
    );
    assert!(
        leads.first().is_some_and(|lead| !lead.is_zero()),
        "the first report follows the first frame: {leads:?}"
    );
}

#[tokio::test]
async fn a_barge_in_sends_bot_interrupted_and_no_audio_of_the_old_epoch_follows() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    engine.say(50, TurnEpoch::ZERO, 1).await; // one second of audio, filled with OLD
    engine
        .session
        .send(TransportCommand::Mark {
            id: "never-played".to_owned(),
        })
        .await
        .unwrap();
    // Wait for the bot to be speaking (and for some old audio to be out), then interrupt.
    browser.until("bot-started-speaking").await;
    loop {
        if let Incoming::Audio { pcm, .. } = browser.recv().await.unwrap() {
            assert_eq!(pcm, vec![OLD; 960]);
            break;
        }
    }
    engine
        .session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    // Old audio may still arrive until bot-interrupted; nothing of it after.
    browser.until("bot-interrupted").await;
    // The engine learns that the mark was cut and that nothing is left to play.
    let (mut cleared, mut zero_lead) = (false, false);
    while !(cleared && zero_lead) {
        match next_event(&mut engine.session, |e| {
            matches!(
                e,
                TransportEvent::MarkCleared { .. }
                    | TransportEvent::MarkPlayed { .. }
                    | TransportEvent::PlaybackLead { .. }
            )
        })
        .await
        {
            TransportEvent::MarkCleared { id } => {
                assert_eq!(id, "never-played");
                cleared = true;
            }
            TransportEvent::PlaybackLead { lead } => zero_lead |= lead.is_zero(),
            other => panic!("the mark must be cut, not played: {other:?}"),
        }
    }
    // The race: audio of the old epoch sent after the clear (dropped as stale), then new audio.
    engine.say_fill(3, TurnEpoch::ZERO, 1, OLD).await;
    engine.say_fill(3, TurnEpoch::new(1), 2, NEW).await;
    let mut new_frames = 0;
    let mut types = Vec::new();
    while new_frames < 3 {
        match browser.recv().await.expect("connection ended") {
            Incoming::Audio { pcm, .. } => {
                assert_eq!(
                    pcm,
                    vec![NEW; 960],
                    "audio of the old epoch reached the client after bot-interrupted"
                );
                new_frames += 1;
            }
            Incoming::Rtvi(value) => types.push(value["type"].as_str().unwrap().to_owned()),
            Incoming::Close(code) => panic!("closed: {code:?}"),
        }
    }
    // The interrupted speech ended with the clear; the new burst started a new one.
    assert_eq!(
        &types[..2],
        ["bot-stopped-speaking", "bot-started-speaking"],
        "{types:?}"
    );
}

#[tokio::test]
async fn auto_events_can_be_turned_off() {
    let rtvi = RtviConfig {
        auto_events: false,
        ..fast_rtvi()
    };
    only_the_engines_events(rtvi, false).await;
}

#[tokio::test]
async fn auto_events_can_be_turned_off_per_call_by_the_sender() {
    only_the_engines_events(fast_rtvi(), true).await;
}

/// With the adapter's derivation off (by the config, or by the call's sender when `switch`), a
/// burst of audio and a clear produce no RTVI message of the adapter's own.
async fn only_the_engines_events(rtvi: RtviConfig, switch: bool) {
    let mut rig = Rig::start(fast_server(), rtvi).await;
    let (mut browser, mut engine) = rig.pair().await;
    assert_eq!(engine.sender.auto_events(), switch);
    if switch {
        engine.sender.set_auto_events(false);
        assert!(!engine.sender.auto_events());
    }
    engine.say(3, TurnEpoch::ZERO, 1).await;
    engine
        .session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    engine.sender.send(RtviEvent::BotTtsStarted).await.unwrap();
    engine
        .sender
        .send(RtviEvent::ServerMessage(json!("sentinel")))
        .await
        .unwrap();
    // Read on well past the end of the three frames of audio (60 ms): the clear's
    // bot-interrupted and the time-driven bot-stopped-speaking would have come by then. The
    // only RTVI messages are the engine's own.
    let mut types = Vec::new();
    let mut seen_sentinel = false;
    let mut idle = Duration::ZERO;
    while idle < Duration::from_millis(500) {
        match recv_within(&mut browser, Duration::from_millis(100)).await {
            Some(Incoming::Rtvi(value)) => {
                seen_sentinel |= value["data"] == "sentinel";
                types.push(value["type"].as_str().unwrap().to_owned());
                idle = Duration::ZERO;
            }
            Some(Incoming::Audio { .. }) => idle = Duration::ZERO,
            Some(Incoming::Close(code)) => panic!("closed: {code:?}"),
            None => idle += Duration::from_millis(100),
        }
    }
    assert!(seen_sentinel, "{types:?}");
    assert_eq!(types, ["bot-tts-started", "server-message"]);
}

// --- events from the client ------------------------------------------------------------------

#[tokio::test]
async fn a_client_message_reaches_the_inbox_and_the_answer_echoes_its_id() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    let id = browser
        .rtvi(
            "client-message",
            json!({"t": "get-weather", "d": {"city": "Ankara"}}),
        )
        .await;
    let request = tokio::time::timeout(WAIT, engine.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request,
        RtviRequest::ClientMessage {
            id: id.clone(),
            t: "get-weather".to_owned(),
            d: Some(json!({"city": "Ankara"})),
        }
    );
    engine
        .sender
        .respond(&id, "get-weather", Some(json!({"temp": 21})))
        .await
        .unwrap();
    assert_eq!(
        browser.next_rtvi().await,
        json!({"label":"rtvi-ai","type":"server-response","id":id,
               "data":{"t":"get-weather","d":{"temp":21}}})
    );
    // And an error answer.
    let id2 = browser.rtvi("client-message", json!({"t": "nope"})).await;
    let _ = engine.inbox.recv().await.unwrap();
    engine
        .sender
        .respond_error(&id2, "unknown message")
        .await
        .unwrap();
    assert_eq!(
        browser.next_rtvi().await,
        json!({"label":"rtvi-ai","type":"error-response","id":id2,
               "data":{"error":"unknown message"}})
    );
}

#[tokio::test]
async fn send_text_and_function_call_results_reach_the_inbox() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    browser
        .rtvi(
            "send-text",
            json!({"content": "hello", "options": {"run_immediately": true, "audio_response": false}}),
        )
        .await;
    browser
        .rtvi(
            "llm-function-call-result",
            json!({"function_name": "f", "tool_call_id": "c1", "arguments": {"a": 1}, "result": "ok"}),
        )
        .await;
    assert_eq!(
        engine.inbox.recv().await.unwrap(),
        RtviRequest::SendText {
            content: "hello".to_owned(),
            run_immediately: true,
            audio_response: false,
        }
    );
    assert_eq!(
        engine.inbox.recv().await.unwrap(),
        RtviRequest::FunctionCallResult {
            function_name: "f".to_owned(),
            tool_call_id: "c1".to_owned(),
            arguments: json!({"a": 1}),
            result: json!("ok"),
        }
    );
}

#[tokio::test]
async fn without_an_inbox_reader_client_requests_are_answered_with_errors() {
    // `Transport::accept` hands out the bare link: the sender and inbox are dropped.
    let mut rig = Rig::default().await;
    let mut browser = rig.browser().await;
    browser.client_ready().await;
    let cancel = CancellationToken::new();
    let link = tokio::time::timeout(WAIT, rig.server.accept(&cancel))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _session = TransportSession::open(
        rig.clock.clone(),
        link,
        SessionConfig::default(),
        &rig.parent,
    )
    .await
    .unwrap();
    assert_eq!(browser.next_rtvi().await["type"], "bot-ready");
    let id = browser.rtvi("client-message", json!({"t": "x"})).await;
    let answer = browser.next_rtvi().await;
    assert_eq!(answer["type"], "error-response");
    assert_eq!(answer["id"], id);
    assert_eq!(answer["data"]["error"], "no handler for this message");
    assert_eq!(rig.server.stats().requests_unanswered, 1);
}

#[tokio::test]
async fn dtmf_buttons_become_dtmf_events_in_order() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    browser
        .rtvi("dtmf", json!({"buttons": ["1", "2", "#", "*"]}))
        .await;
    let mut digits = Vec::new();
    while digits.len() < 4 {
        if let TransportEvent::Dtmf { digit, .. } = next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Dtmf { .. })
        })
        .await
        {
            digits.push(digit);
        }
    }
    assert_eq!(
        digits,
        vec![
            DtmfDigit::D1,
            DtmfDigit::D2,
            DtmfDigit::Pound,
            DtmfDigit::Star
        ]
    );
}

#[tokio::test]
async fn disconnect_bot_is_a_hangup_and_the_server_closes_the_socket() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    browser.rtvi("disconnect-bot", json!({})).await;
    assert_eq!(stopped(&mut engine.session).await, StopReason::PeerHangup);
    assert_eq!(browser.closed().await, Some(CLOSE_NORMAL));
}

#[tokio::test]
async fn a_closed_socket_is_a_hangup() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    browser.ws.close(None).await.unwrap();
    assert_eq!(stopped(&mut engine.session).await, StopReason::PeerHangup);
    let _ = browser;
}

#[tokio::test]
async fn a_vanished_socket_is_a_disconnect() {
    let mut rig = Rig::default().await;
    let (browser, mut engine) = rig.pair().await;
    drop(browser);
    assert_eq!(
        stopped(&mut engine.session).await,
        StopReason::PeerDisconnected
    );
}

// --- faults ----------------------------------------------------------------------------------

#[tokio::test]
async fn bad_messages_are_answered_the_way_pipecat_does() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;

    // Not JSON, and JSON without an id: `error` "Invalid RTVI transport message", non-fatal.
    browser.send_binary(pb_message("not json")).await;
    let error = browser.next_rtvi().await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["data"]["error"], "Invalid RTVI transport message");
    assert_eq!(error["data"]["fatal"], false);
    browser
        .send_binary(pb_message(
            &json!({"label":"rtvi-ai","type":"send-text","data":{"content":"x"}}).to_string(),
        ))
        .await;
    assert_eq!(
        browser.next_rtvi().await["data"]["error"],
        "Invalid RTVI transport message"
    );

    // Another label: dropped without an answer.
    browser
        .send_binary(pb_message(
            &json!({"label":"other","type":"client-ready","id":"z","data":{}}).to_string(),
        ))
        .await;
    // A type the server has no case for: error-response "Unsupported type ...".
    let id = browser
        .rtvi("append-to-context", json!({"role":"user"}))
        .await;
    let answer = browser.next_rtvi().await;
    assert_eq!(answer["type"], "error-response");
    assert_eq!(answer["id"], id);
    assert_eq!(
        answer["data"]["error"],
        "Unsupported type append-to-context"
    );

    // A known type with bad data: error-response naming the type.
    let id = browser.rtvi("send-text", json!({})).await;
    let answer = browser.next_rtvi().await;
    assert_eq!(answer["id"], id);
    assert!(
        answer["data"]["error"]
            .as_str()
            .unwrap()
            .contains("send-text")
    );

    // The call is alive, and the engine saw the two invalid-message faults.
    for _ in 0..2 {
        next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Error { .. })
        })
        .await;
    }
    browser.audio(&[0u8; 640], 16_000).await;
    next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await;
}

#[tokio::test]
async fn a_text_frame_during_the_call_is_a_fault_and_too_many_close_the_call() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    tokio::time::timeout(WAIT, browser.ws.send(Message::text("{}")))
        .await
        .unwrap()
        .unwrap();
    let event = next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Error { .. })
    })
    .await;
    let TransportEvent::Error { message } = event else {
        unreachable!()
    };
    assert!(message.contains("text frames"), "{message}");
    // Still alive.
    browser.audio(&[0u8; 640], 16_000).await;
    next_event(&mut engine.session, |e| {
        matches!(e, TransportEvent::Audio(_))
    })
    .await;
    // More than 16 faults close it.
    for _ in 0..20 {
        browser.send_binary(vec![0xff, 0xff, 0xff]).await;
    }
    assert_eq!(browser.closed().await, Some(CLOSE_POLICY));
}

#[tokio::test]
async fn an_oversized_frame_closes_the_call() {
    let server = ServerConfig {
        max_audio_bytes: 2048,
        max_control_bytes: 1024,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, mut engine) = rig.pair().await;
    browser.audio(&[0u8; 4000], 16_000).await;
    assert_eq!(browser.closed().await, Some(1009));
    let reason = stopped(&mut engine.session).await;
    assert!(
        matches!(
            reason,
            StopReason::PeerHangup | StopReason::PeerDisconnected | StopReason::Failed(_)
        ),
        "{reason:?}"
    );
}

/// Limits for the control-message tests: a `client-message` of about 4 KiB is under the websocket
/// layer's limit (the larger of the two) and over `max_control_bytes`, so only the application
/// check can refuse it.
fn big_control_server() -> ServerConfig {
    ServerConfig {
        max_audio_bytes: 64 * 1024,
        max_control_bytes: 1024,
        ..fast_server()
    }
}

#[tokio::test]
async fn an_oversized_control_message_closes_the_call() {
    let mut rig = Rig::start(big_control_server(), fast_rtvi()).await;
    let (mut browser, mut engine) = rig.pair().await;
    browser
        .rtvi("client-message", json!({"t": "x", "d": "a".repeat(4096)}))
        .await;
    assert_eq!(browser.closed().await, Some(1009));
    let reason = stopped(&mut engine.session).await;
    assert!(
        matches!(
            reason,
            StopReason::PeerHangup | StopReason::PeerDisconnected | StopReason::Failed(_)
        ),
        "{reason:?}"
    );
    // Nothing reached the app.
    assert!(engine.inbox.recv().await.is_none());
}

#[tokio::test]
async fn an_oversized_control_message_before_client_ready_closes_the_connection() {
    let rig = Rig::start(big_control_server(), fast_rtvi()).await;
    let mut browser = rig.browser().await;
    browser
        .rtvi("client-message", json!({"t": "x", "d": "a".repeat(4096)}))
        .await;
    assert_eq!(browser.closed().await, Some(1009));
}

// --- upgrade checks, lifecycle --------------------------------------------------------------

#[tokio::test]
async fn other_paths_are_404_and_a_trailing_slash_is_fine() {
    let rig = Rig::default().await;
    let error = rig.try_connect("/elsewhere", None).await.unwrap_err();
    assert!(
        matches!(&error, WsError::Http(r) if r.status() == 404),
        "{error:?}"
    );
    rig.try_connect("/rtvi/", None).await.unwrap();
}

#[tokio::test]
async fn the_token_is_checked_against_the_query_parameter() {
    let rtvi = RtviConfig {
        token: Some(Secret::new("s3cr3t token+1".to_owned())),
        ..fast_rtvi()
    };
    let rig = Rig::start(fast_server(), rtvi).await;
    let status = |result: Result<Client, WsError>| match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("{other:?}"),
    };
    assert_eq!(status(rig.try_connect(PATH, None).await), 401);
    assert_eq!(
        status(rig.try_connect("/rtvi?token=wrong", None).await),
        403
    );
    // Near misses: a prefix, the secret plus a character, a longer value that starts with it,
    // the secret plus a NUL, another case, an empty value.
    for wrong in [
        "s3cr3t",
        "s3cr3t%20token%2B",
        "s3cr3t%20token%2B1X",
        "s3cr3t%20token%2B1%00",
        "S3CR3T%20TOKEN%2B1",
        "",
    ] {
        assert_eq!(
            status(rig.try_connect(&format!("/rtvi?token={wrong}"), None).await),
            403,
            "{wrong:?}"
        );
    }
    // A malformed escape cannot be decoded: as good as no token.
    for undecodable in ["%zz", "s3cr3t%2", "s3cr3t%", "s3cr3t%+1"] {
        assert_eq!(
            status(
                rig.try_connect(&format!("/rtvi?token={undecodable}"), None)
                    .await
            ),
            401,
            "{undecodable:?}"
        );
    }
    // As the JS transport appends it: encodeURIComponent("s3cr3t token+1").
    assert_eq!(
        status(
            rig.try_connect("/rtvi?token=s3cr3t%20token%2B1", None)
                .await
        ),
        101
    );
    assert_eq!(
        status(
            rig.try_connect("/rtvi?a=1&token=s3cr3t%20token%2B1", None)
                .await
        ),
        101
    );
}

#[tokio::test]
async fn allowed_origins_are_enforced_when_configured() {
    let rtvi = RtviConfig {
        allowed_origins: vec!["https://app.example.com".to_owned()],
        ..fast_rtvi()
    };
    let rig = Rig::start(fast_server(), rtvi).await;
    let status = |result: Result<Client, WsError>| match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("{other:?}"),
    };
    assert_eq!(status(rig.try_connect(PATH, None).await), 403);
    assert_eq!(
        status(rig.try_connect(PATH, Some("https://evil.example")).await),
        403
    );
    assert_eq!(
        status(rig.try_connect(PATH, Some("https://APP.example.com")).await),
        101
    );
}

#[tokio::test]
async fn calls_beyond_the_limit_get_503() {
    let server = ServerConfig {
        max_calls: 1,
        ..fast_server()
    };
    let rig = Rig::start(server, fast_rtvi()).await;
    let _first = rig.try_connect(PATH, None).await.unwrap();
    let error = rig.try_connect(PATH, None).await.unwrap_err();
    assert!(
        matches!(&error, WsError::Http(r) if r.status() == 503),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_engine_hanging_up_closes_the_socket_normally() {
    let mut rig = Rig::default().await;
    let (mut browser, engine) = rig.pair().await;
    engine.session.send(TransportCommand::Stop).await.unwrap();
    assert_eq!(browser.closed().await, Some(CLOSE_NORMAL));
}

#[tokio::test]
async fn shutdown_closes_calls_with_going_away() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    rig.server.shutdown(WAIT).await.unwrap();
    assert_eq!(browser.closed().await, Some(CLOSE_AWAY));
    let _ = stopped(&mut engine.session).await;
    let cancel = CancellationToken::new();
    assert!(rig.server.accept_call(&cancel).await.unwrap().is_none());
}

#[tokio::test]
async fn the_server_pings_and_a_silent_client_is_dropped() {
    let server = ServerConfig {
        ping_interval: Duration::from_millis(50),
        ping_timeout: Duration::from_millis(400),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (browser, mut engine) = rig.pair().await;
    // A client that reads nothing never answers a ping (tungstenite answers while polled).
    let reason = stopped(&mut engine.session).await;
    assert!(
        matches!(
            reason,
            StopReason::PeerDisconnected | StopReason::PeerHangup | StopReason::Failed(_)
        ),
        "{reason:?}"
    );
    // The idle connection was dropped: its permit is back.
    wait_for_active(&rig, 0).await;
    drop(browser);
}

#[tokio::test]
async fn several_calls_run_side_by_side() {
    let mut rig = Rig::default().await;
    let (mut b1, mut e1) = rig.pair().await;
    let (b2, mut e2) = rig.pair().await;
    e1.say(2, TurnEpoch::ZERO, 1).await;
    e2.sender
        .send(RtviEvent::ServerMessage(json!("two")))
        .await
        .unwrap();
    b1.audio(&[1u8; 640], 16_000).await;
    next_event(&mut e1.session, |e| matches!(e, TransportEvent::Audio(_))).await;
    assert_eq!(rig.server.active_calls(), 2);

    // Call 1's caller audio reached call 1's engine only.
    let stray = tokio::time::timeout(
        Duration::from_millis(200),
        next_event(&mut e2.session, |e| matches!(e, TransportEvent::Audio(_))),
    )
    .await;
    assert!(stray.is_err(), "call 2's engine got call 1's caller audio");

    // Everything each browser received over a window: nothing of the other call's.
    let collect = |mut browser: Browser| async move {
        let mut seen = Vec::new();
        while let Some(incoming) = recv_within(&mut browser, Duration::from_millis(400)).await {
            seen.push(incoming);
        }
        seen
    };
    let (seen1, seen2) = tokio::join!(collect(b1), collect(b2));
    let rtvi_types = |seen: &[Incoming]| -> Vec<String> {
        seen.iter()
            .filter_map(|incoming| match incoming {
                Incoming::Rtvi(value) => Some(value["type"].as_str().unwrap().to_owned()),
                _ => None,
            })
            .collect()
    };
    let audio_bytes = |seen: &[Incoming]| -> usize {
        seen.iter()
            .map(|incoming| match incoming {
                Incoming::Audio { pcm, .. } => pcm.len(),
                _ => 0,
            })
            .sum()
    };
    // Call 1 got its own speech and none of call 2's message.
    assert_eq!(audio_bytes(&seen1), 2 * 960);
    let types1 = rtvi_types(&seen1);
    assert!(
        types1.contains(&"bot-started-speaking".to_owned()),
        "{types1:?}"
    );
    assert!(!types1.contains(&"server-message".to_owned()), "{types1:?}");
    // Call 2 got its own message and neither audio nor speaking events of call 1.
    assert_eq!(audio_bytes(&seen2), 0);
    assert_eq!(rtvi_types(&seen2), ["server-message"]);
}

// --- a whole conversation, and no leaked tasks ----------------------------------------------

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

#[tokio::test]
async fn a_conversation_with_barge_in_over_one_connection() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;

    // The user speaks: audio up, the engine reports what it understood.
    for _ in 0..3 {
        browser.audio(&[3u8; 640], 16_000).await;
    }
    for _ in 0..3 {
        next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Audio(_))
        })
        .await;
    }
    engine
        .sender
        .send(RtviEvent::UserStartedSpeaking)
        .await
        .unwrap();
    engine
        .sender
        .send(RtviEvent::UserStoppedSpeaking)
        .await
        .unwrap();
    engine
        .sender
        .user_transcription("merhaba", true)
        .await
        .unwrap();
    assert_eq!(browser.next_rtvi().await["type"], "user-started-speaking");
    assert_eq!(browser.next_rtvi().await["type"], "user-stopped-speaking");
    assert_eq!(browser.next_rtvi().await["type"], "user-transcription");

    // The bot answers: LLM text, speech, the end-of-utterance mark.
    engine.sender.send(RtviEvent::BotLlmStarted).await.unwrap();
    engine
        .sender
        .send(RtviEvent::BotLlmText(TextData::new("Merhaba!")))
        .await
        .unwrap();
    engine.say(40, TurnEpoch::ZERO, 1).await;
    engine
        .session
        .send(TransportCommand::Mark {
            id: "answer-1".to_owned(),
        })
        .await
        .unwrap();
    browser.until("bot-started-speaking").await;
    let mut heard = 0;
    while heard < 5 {
        if let Incoming::Audio {
            pcm, sample_rate, ..
        } = browser.recv().await.unwrap()
        {
            assert_eq!(sample_rate, 24_000);
            heard += pcm.len() / 960;
        }
    }

    // The user interrupts and asks something through the client-message channel.
    engine
        .session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    browser.until("bot-interrupted").await;
    let id = browser
        .rtvi("client-message", json!({"t": "ping", "d": 1}))
        .await;
    let RtviRequest::ClientMessage { id: seen, t, .. } = engine.inbox.recv().await.unwrap() else {
        panic!("a client message");
    };
    assert_eq!((seen.as_str(), t.as_str()), (id.as_str(), "ping"));
    engine
        .sender
        .respond(&id, "ping", Some(json!(1)))
        .await
        .unwrap();
    assert_eq!(browser.until("server-response").await["id"], id);

    // The second answer plays in the new epoch; the first answer's mark was cut.
    engine.say_fill(2, TurnEpoch::new(1), 2, NEW).await;
    next_event(
        &mut engine.session,
        |e| matches!(e, TransportEvent::MarkCleared { id } if id == "answer-1"),
    )
    .await;
    // Old-epoch audio can only arrive up to bot-interrupted (read above); from here on only the
    // new answer's audio reaches the client.
    for _ in 0..2 {
        loop {
            if let Incoming::Audio { pcm, .. } = browser.recv().await.unwrap() {
                assert_eq!(pcm, vec![NEW; 960], "old-epoch audio after the barge-in");
                break;
            }
        }
    }

    // The user hangs up.
    browser.rtvi("disconnect-bot", json!({})).await;
    assert_eq!(stopped(&mut engine.session).await, StopReason::PeerHangup);
    assert_eq!(browser.closed().await, Some(CLOSE_NORMAL));
}

#[tokio::test]
async fn every_way_an_rtvi_call_can_end_leaks_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let mut rig = Rig::default().await;

        // disconnect-bot.
        let (mut browser, mut engine) = rig.pair().await;
        browser.rtvi("disconnect-bot", json!({})).await;
        assert_eq!(stopped(&mut engine.session).await, StopReason::PeerHangup);
        browser.closed().await;
        engine.session.close(WAIT).await.unwrap();

        // An abrupt drop.
        let (browser, mut engine) = rig.pair().await;
        drop(browser);
        assert_eq!(
            stopped(&mut engine.session).await,
            StopReason::PeerDisconnected
        );
        engine.session.close(WAIT).await.unwrap();
        wait_for_active(&rig, 0).await;

        // The engine hanging up.
        let (mut browser, engine) = rig.pair().await;
        engine.session.send(TransportCommand::Stop).await.unwrap();
        browser.closed().await;
        drop(engine);

        // A fatal fault.
        let (mut browser, mut engine) = rig.pair().await;
        for _ in 0..20 {
            browser.send_binary(vec![0xff, 0xff, 0xff]).await;
        }
        stopped(&mut engine.session).await;
        engine.session.close(WAIT).await.unwrap();

        // Refused upgrades, half-open connections and an unready client, then several calls cut
        // off by shutdown.
        let _ = rig.try_connect("/nope", None).await;
        let _half_open = TcpStream::connect(rig.server.local_addr()).await.unwrap();
        let _unready = rig.browser().await;
        let mut open = Vec::new();
        for _ in 0..3 {
            open.push(rig.pair().await);
        }
        drop(open);
        rig.server.shutdown(WAIT).await.unwrap();
    }
    counter
        .wait_for_baseline(&*clock, WAIT)
        .await
        .expect("tasks outlived the server and its calls");
}

// --- review fixes: limits, timeouts and bounded queues ---------------------------------------

/// Queues big server messages until the sender gives up; returns why. The call must be blocked
/// behind a client that does not read, or this panics after 200 000 messages.
async fn flood(sender: &RtviSender) -> brisvox_transport::Error {
    for _ in 0..200_000 {
        let big = RtviEvent::ServerMessage(json!("x".repeat(15_000)));
        if let Err(error) = sender.send(big).await {
            return error;
        }
    }
    panic!("the sender never blocked");
}

/// The next message within `limit`, or `None` if the connection stayed quiet.
async fn recv_within(browser: &mut Browser, limit: Duration) -> Option<Incoming> {
    tokio::time::timeout(limit, browser.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn an_audio_frame_above_max_audio_bytes_closes_even_when_control_is_larger() {
    let server = ServerConfig {
        max_audio_bytes: 2048,
        max_control_bytes: 16 * 1024,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, _engine) = rig.pair().await;
    // Fits the websocket message limit (the larger of the two), not the audio limit.
    browser.audio(&[0u8; 8000], 16_000).await;
    assert_eq!(browser.closed().await, Some(1009));
    assert_eq!(
        rig.server.stats().audio_in,
        0,
        "the frame never reached the engine"
    );
}

#[tokio::test]
async fn client_audio_at_another_rate_is_reported_once_and_still_passed_on() {
    let mut rig = Rig::default().await;
    let (mut browser, mut engine) = rig.pair().await;
    for _ in 0..3 {
        browser.audio(&[0u8; 960], 24_000).await;
    }
    browser.audio(&[0u8; 640], 11_025).await; // a sentinel fault after the three frames
    let mut mismatch = 0;
    loop {
        let TransportEvent::Error { message } = next_event(&mut engine.session, |e| {
            matches!(e, TransportEvent::Error { .. })
        })
        .await
        else {
            unreachable!()
        };
        if message.contains("unsupported audio sample rate") {
            break;
        }
        assert!(message.contains("recorderSampleRate"), "{message}");
        mismatch += 1;
    }
    assert_eq!(mismatch, 1, "reported once per call");
    assert_eq!(rig.server.stats().audio_in, 3, "the frames were passed on");
}

#[tokio::test]
async fn shutdown_before_client_ready_closes_with_going_away() {
    let rig = Rig::default().await;
    let mut browser = rig.browser().await;
    pause(Duration::from_millis(100)).await; // let the server start waiting for client-ready
    rig.server.shutdown(WAIT).await.unwrap();
    assert_eq!(browser.closed().await, Some(CLOSE_AWAY));
}

#[tokio::test]
async fn a_flood_before_client_ready_closes_with_policy() {
    let rig = Rig::default().await;
    let mut browser = rig.browser().await;
    for _ in 0..20 {
        browser.send_binary(pb_message("not json")).await;
    }
    assert_eq!(browser.closed().await, Some(CLOSE_POLICY));
    assert_eq!(rig.server.stats().calls, 0);
}

#[tokio::test]
async fn the_server_sends_pings() {
    let server = ServerConfig {
        ping_interval: Duration::from_millis(50),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, _engine) = rig.pair().await;
    let ping = async {
        loop {
            match browser.ws.next().await {
                Some(Ok(Message::Ping(_))) => return,
                Some(Ok(_)) => {}
                other => panic!("connection ended without a ping: {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), ping)
        .await
        .expect("no ping within 2 s at a 50 ms interval");
}

#[tokio::test]
async fn a_quiet_client_that_answers_pings_stays_connected() {
    let server = ServerConfig {
        ping_interval: Duration::from_millis(50),
        ping_timeout: Duration::from_millis(400),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, mut engine) = rig.pair().await;
    // Three times the idle timeout without a single audio frame or message (a muted microphone):
    // the client only reads, which makes the websocket layer answer the pings.
    let quiet = async { while let Some(Ok(_)) = browser.ws.next().await {} };
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), quiet)
            .await
            .is_err(),
        "the server closed a client that answers pings"
    );
    browser.audio(&[0u8; 640], 16_000).await;
    loop {
        match next_event(&mut engine.session, |_| true).await {
            TransportEvent::Audio(_) => break,
            TransportEvent::Stopped { reason } => panic!("the call ended: {reason:?}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_client_that_stops_reading_ends_the_call_after_the_write_timeout() {
    let server = ServerConfig {
        write_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (browser, mut engine) = rig.pair().await; // `browser` never reads from here on
    let error = flood(&engine.sender).await;
    assert!(
        matches!(
            error,
            brisvox_transport::Error::Core(brisvox_core::Error::ChannelClosed(_))
        ),
        "{error:?}"
    );
    let _ = stopped(&mut engine.session).await;
    // The connection task is gone and its permit released while the client is still connected.
    wait_for_active(&rig, 0).await;
    drop(browser);
}

#[tokio::test]
async fn a_full_event_queue_makes_the_sender_time_out() {
    let server = ServerConfig {
        write_timeout: Duration::from_secs(3),
        send_timeout: Duration::from_millis(150),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (browser, engine) = rig.pair().await;
    let error = flood(&engine.sender).await;
    assert!(
        matches!(
            error,
            brisvox_transport::Error::Core(brisvox_core::Error::Timeout { .. })
        ),
        "{error:?}"
    );
    drop(browser);
}

#[tokio::test]
async fn a_full_inbox_answers_client_messages_with_server_busy() {
    let mut rig = Rig::default().await;
    let (mut browser, engine) = rig.pair().await; // the engine never reads its inbox
    let mut last = String::new();
    for n in 0..33 {
        last = browser
            .rtvi("client-message", json!({"t": "x", "d": n}))
            .await;
    }
    let answer = browser.next_rtvi().await;
    assert_eq!(answer["type"], "error-response");
    assert_eq!(
        answer["id"], last,
        "the 33rd request found the 32-slot inbox full"
    );
    assert_eq!(answer["data"]["error"], "server busy");
    assert_eq!(rig.server.stats().requests_unanswered, 1);
    drop(engine);
}

#[tokio::test]
async fn send_text_and_function_results_are_not_answered_when_the_inbox_is_unread() {
    let mut rig = Rig::default().await;
    let mut browser = rig.browser().await;
    browser.client_ready().await;
    let cancel = CancellationToken::new();
    let link = rig.server.accept(&cancel).await.unwrap().unwrap();
    let _session = TransportSession::open(
        rig.clock.clone(),
        link,
        SessionConfig::default(),
        &rig.parent,
    )
    .await
    .unwrap();
    assert_eq!(browser.next_rtvi().await["type"], "bot-ready");
    browser.rtvi("send-text", json!({"content": "hi"})).await;
    browser
        .rtvi(
            "llm-function-call-result",
            json!({"function_name": "f", "tool_call_id": "c", "arguments": {}, "result": 1}),
        )
        .await;
    let id = browser.rtvi("client-message", json!({"t": "x"})).await;
    // The first answer is the client-message's: the other two have no reply in the protocol.
    let answer = browser.next_rtvi().await;
    assert_eq!(answer["id"], id);
    assert_eq!(answer["data"]["error"], "no handler for this message");
    assert_eq!(
        rig.server.stats().requests_unanswered,
        3,
        "all three were counted"
    );
}

#[tokio::test]
async fn replies_beyond_the_reply_queue_are_dropped_while_the_writer_is_blocked() {
    let server = ServerConfig {
        write_timeout: Duration::from_secs(3),
        send_timeout: Duration::from_millis(150),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let (mut browser, engine) = rig.pair().await;
    // Block the writer behind a client that is not reading yet.
    flood(&engine.sender).await;
    for _ in 0..100 {
        browser.rtvi("send-text", json!({})).await; // bad data: one error-response each
    }
    pause(Duration::from_millis(200)).await;
    let mut replies = 0;
    while let Some(incoming) = recv_within(&mut browser, Duration::from_millis(500)).await {
        if let Incoming::Rtvi(value) = incoming
            && value["type"] == "error-response"
        {
            replies += 1;
        }
    }
    // The writer is blocked, so the queue takes exactly its 16 slots and drops the other 84.
    assert_eq!(
        replies, 16,
        "the 16-slot reply queue keeps 16 of the 100 replies"
    );
}

#[tokio::test]
async fn an_engine_that_takes_no_calls_is_told_so_with_1013() {
    let server = ServerConfig {
        accept_queue: 1,
        send_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    // The first call waits in the accept queue (nobody calls accept_call yet).
    let mut first = rig.browser().await;
    first.client_ready().await;
    assert_eq!(first.next_rtvi().await["type"], "bot-ready");
    let mut second = rig.browser().await;
    second.client_ready().await;
    let error = second.next_rtvi().await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["data"]["error"], "engine is not taking calls");
    assert_eq!(error["data"]["fatal"], true);
    assert_eq!(second.closed().await, Some(1013));
    // The queued call is still good.
    let _engine = rig.accept().await;
}

#[tokio::test]
async fn an_engine_that_takes_no_input_is_told_so_with_1013() {
    let server = ServerConfig {
        send_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_rtvi()).await;
    let mut browser = rig.browser().await;
    browser.client_ready().await;
    let cancel = CancellationToken::new();
    // The call is accepted but its link is never opened as a session: nobody drains its events.
    let _call = rig.server.accept_call(&cancel).await.unwrap().unwrap();
    assert_eq!(browser.next_rtvi().await["type"], "bot-ready");
    let buttons: Vec<&str> = vec!["1"; 100];
    browser.rtvi("dtmf", json!({ "buttons": buttons })).await;
    assert_eq!(browser.closed().await, Some(1013));
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
async fn a_gate_admits_the_browser_and_its_ticket_reaches_the_link() {
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gate = std::sync::Arc::new(Gate(std::sync::atomic::AtomicBool::new(false)));
    let shared: brisvox_transport::admission::SharedGate = gate.clone();
    let server = RtviServer::serve_gated(
        listener,
        fast_server(),
        fast_rtvi(),
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
    let mut browser = rig.browser().await;
    let _ready = browser.client_ready().await;
    let cancel = CancellationToken::new();
    let mut call = tokio::time::timeout(WAIT, rig.server.accept_call(&cancel))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ticket = call
        .link
        .take_ticket()
        .expect("the link carries the gate's ticket");
    assert_eq!(ticket.downcast::<u32>().unwrap(), 7);

    // Refused by the gate: HTTP 503 before the upgrade.
    gate.0.store(true, std::sync::atomic::Ordering::SeqCst);
    match rig.try_connect(PATH, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status().as_u16(), 503),
        Err(e) => panic!("expected a 503, got {e}"),
        Ok(_) => panic!("expected a 503, the browser was upgraded"),
    }
    drop(call);
    rig.server.shutdown(WAIT).await.unwrap();
}

// --- credentials in headers, session tokens ------------------------------------------

mod credentials {
    use std::collections::BTreeMap;
    use std::time::UNIX_EPOCH;

    use brisvox_transport::auth::{Audience, SessionSecrets, TokenSpec, mint_token};

    use super::*;

    const SECRET: &str = "an-rtvi-session-secret-of-32-bytes-or-more";

    fn status(result: &Result<Client, WsError>) -> u16 {
        match result {
            Err(WsError::Http(response)) => response.status().as_u16(),
            Ok(_) => 101,
            Err(other) => panic!("{other:?}"),
        }
    }

    fn unix_now() -> u64 {
        SystemClock::shared()
            .wall()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn spec(sid: &str, audience: Audience) -> TokenSpec {
        TokenSpec {
            audience,
            sid: sid.to_owned(),
            not_before: unix_now() - 5,
            expires: unix_now() + 120,
            agent: Some("support".to_owned()),
            max_s: None,
            meta: BTreeMap::from([("plan".to_owned(), "pro".to_owned())]),
        }
    }

    fn mint(spec: &TokenSpec) -> String {
        mint_token(&Secret::new(SECRET.to_owned()), spec).unwrap()
    }

    fn sessions() -> RtviConfig {
        RtviConfig {
            sessions: Some(SessionSecrets::new(Secret::new(SECRET.to_owned()), None).unwrap()),
            ..fast_rtvi()
        }
    }

    impl Rig {
        /// Connects with extra request headers.
        async fn connect_with(
            &self,
            path_and_query: &str,
            headers: &[(&'static str, &str)],
        ) -> Result<(Client, Option<String>), WsError> {
            let mut request = self.url(path_and_query).into_client_request().unwrap();
            for (name, value) in headers {
                request
                    .headers_mut()
                    .insert(*name, HeaderValue::from_str(value).unwrap());
            }
            let (ws, response) =
                tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(request))
                    .await
                    .expect("connect timed out")?;
            let selected = response
                .headers()
                .get("sec-websocket-protocol")
                .map(|v| v.to_str().unwrap().to_owned());
            Ok((ws, selected))
        }
    }

    fn as_status(result: Result<(Client, Option<String>), WsError>) -> u16 {
        status(&result.map(|(ws, _)| ws))
    }

    #[tokio::test]
    async fn the_static_token_is_accepted_as_a_bearer_header_a_subprotocol_or_the_query() {
        let rtvi = RtviConfig {
            token: Some(Secret::new("dev-token-1".to_owned())),
            ..fast_rtvi()
        };
        let rig = Rig::start(fast_server(), rtvi).await;
        // Query: as before.
        assert_eq!(
            status(&rig.try_connect("/rtvi?token=dev-token-1", None).await),
            101
        );
        // Authorization header.
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Authorization", "Bearer dev-token-1")])
                    .await
            ),
            101
        );
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Authorization", "Bearer nope")])
                    .await
            ),
            403
        );
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Authorization", "Basic dev-token-1")])
                    .await
            ),
            401
        );
        // Subprotocol: the plain name is selected when it is offered.
        let (_ws, selected) = rig
            .connect_with(
                PATH,
                &[(
                    "Sec-WebSocket-Protocol",
                    "brisvox-token.dev-token-1, brisvox",
                )],
            )
            .await
            .unwrap();
        assert_eq!(selected.as_deref(), Some("brisvox"));
        // Only the token entry offered: nothing is selected, the credential is never echoed (a
        // client that insists on a selected subprotocol, like this one, refuses the answer).
        let only_token = rig
            .connect_with(
                PATH,
                &[("Sec-WebSocket-Protocol", "brisvox-token.dev-token-1")],
            )
            .await;
        assert!(
            matches!(only_token, Err(WsError::Protocol(_))),
            "{:?}",
            only_token.map(|_| ())
        );
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Sec-WebSocket-Protocol", "brisvox-token.nope")])
                    .await
            ),
            403
        );
    }

    #[tokio::test]
    async fn a_refusal_is_never_cached() {
        let rtvi = RtviConfig {
            token: Some(Secret::new("dev-token-1".to_owned())),
            ..fast_rtvi()
        };
        let rig = Rig::start(fast_server(), rtvi).await;
        let Err(WsError::Http(response)) = rig.try_connect(PATH, None).await else {
            panic!("expected a refusal");
        };
        assert_eq!(response.headers()["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn a_session_token_opens_one_call_and_reaches_its_metadata() {
        let mut rig = Rig::start(fast_server(), sessions()).await;
        let token = mint(&spec("visitor-1", Audience::Rtvi));
        let (ws, _) = rig
            .connect_with(PATH, &[("Authorization", &format!("Bearer {token}"))])
            .await
            .expect("a valid token connects");
        let mut browser = Browser::new(ws);
        browser.client_ready().await;
        let mut engine = rig.accept().await;
        let TransportEvent::Started(info) = engine.session.recv().await.unwrap() else {
            panic!("Started is the first event");
        };
        assert_eq!(info.metadata["session_id"], "visitor-1");
        assert_eq!(info.metadata["session.agent"], "support");
        assert_eq!(info.metadata["session.meta.plan"], "pro");

        // Single use, on any of the three paths.
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Authorization", &format!("Bearer {token}"))])
                    .await
            ),
            403
        );
        assert_eq!(
            status(&rig.try_connect(&format!("/rtvi?token={token}"), None).await),
            403
        );
    }

    #[tokio::test]
    async fn session_tokens_travel_in_the_query_and_the_subprotocol_too() {
        let rig = Rig::start(fast_server(), sessions()).await;
        let by_query = mint(&spec("by-query", Audience::Rtvi));
        assert_eq!(
            status(
                &rig.try_connect(&format!("/rtvi?token={by_query}"), None)
                    .await
            ),
            101
        );
        let by_protocol = mint(&spec("by-protocol", Audience::Rtvi));
        let (_ws, selected) = rig
            .connect_with(
                PATH,
                &[(
                    "Sec-WebSocket-Protocol",
                    &format!("brisvox, brisvox-token.{by_protocol}"),
                )],
            )
            .await
            .unwrap();
        assert_eq!(selected.as_deref(), Some("brisvox"));
    }

    #[tokio::test]
    async fn bad_session_tokens_are_refused() {
        let rig = Rig::start(fast_server(), sessions()).await;
        let bearer = |token: &str| format!("Bearer {token}");
        // Missing: 401. Wrong audience, expired, signed elsewhere, tampered, junk: 403.
        assert_eq!(status(&rig.try_connect(PATH, None).await), 401);
        let wrong_aud = mint(&spec("a", Audience::Webrtc));
        let mut expired_spec = spec("b", Audience::Rtvi);
        expired_spec.not_before = unix_now() - 500;
        expired_spec.expires = unix_now() - 300;
        let expired = mint(&expired_spec);
        let foreign = mint_token(
            &Secret::new("some-other-secret-of-at-least-32-bytes".to_owned()),
            &spec("c", Audience::Rtvi),
        )
        .unwrap();
        let mut tampered = mint(&spec("d", Audience::Rtvi));
        tampered.push('A');
        for token in [wrong_aud, expired, foreign, tampered, "junk".to_owned()] {
            assert_eq!(
                as_status(
                    rig.connect_with(PATH, &[("Authorization", &bearer(&token))])
                        .await
                ),
                403,
                "{token}"
            );
        }
        // Oversize: 413 before anything is parsed.
        let huge = "a".repeat(5_000);
        assert_eq!(
            as_status(
                rig.connect_with(PATH, &[("Authorization", &bearer(&huge))])
                    .await
            ),
            413
        );
    }
}
