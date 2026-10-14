//! Telnyx media streaming transport tests: real sockets on 127.0.0.1, the real clock,
//! short deadlines.
//!
//! Each test starts a `TelnyxServer`, plays the engine with a `TransportSession` on every accepted
//! call and plays Telnyx with a scripted client (`Phone`) that sends the message sequence of
//! `docs/providers/telnyx.md` and records what the server sends back. Numbers that the docs print
//! as strings (`sequence_number`, `chunk`, `timestamp`) are sent as strings.
#![cfg(feature = "telnyx")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)] // test helpers (see clippy.toml)

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use brisvox_audio::Codec;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{Secret, SharedClock, SystemClock, TurnEpoch};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::telnyx::{
    HangupHook, HangupRequest, TelnyxCodec, TelnyxConfig, TelnyxServer,
};
use brisvox_transport::websocket::ServerConfig;
use brisvox_transport::{
    CallRates, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
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
const CLOSE_POLICY: u16 = 1008;
const CLOSE_SIZE: u16 = 1009;

const PATH: &str = "/telnyx/stream";
const TOKEN: &str = "test-stream-auth-token-0123456789";
const USER_ID: &str = "3E6F995F-85F7-4705-9741-53B116D28237";
const SESSION_ID: &str = "ff55a038-6f5d-11ef-9692-02420aeffb1f";

/// Waits `duration` on the real clock.
async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A fresh id for every phone: the server refuses a second stream for one call.
fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

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

fn fast_telnyx() -> TelnyxConfig {
    TelnyxConfig {
        mark_interval: Duration::ZERO,
        ..TelnyxConfig::default()
    }
}

fn authed_telnyx() -> TelnyxConfig {
    TelnyxConfig {
        auth_token: Some(Secret::new(TOKEN.to_owned())),
        ..fast_telnyx()
    }
}

/// How long the recording hook takes. The adapter promises to finish the hook before it sends
/// the close frame; a hook that returned at once would let a reordered close slip through
/// (everything runs on one thread), so this one yields for a while first and the tests that read
/// the log after the close frame arrived pin the order.
const HOOK_DELAY: Duration = Duration::from_millis(100);

/// A hang-up hook that records its requests (after [`HOOK_DELAY`]).
fn recording_hook(calls: &Arc<Mutex<Vec<HangupRequest>>>, called: &Arc<AtomicBool>) -> HangupHook {
    let calls = Arc::clone(calls);
    let called = Arc::clone(called);
    HangupHook::new(move |request| {
        let calls = Arc::clone(&calls);
        let called = Arc::clone(&called);
        async move {
            pause(HOOK_DELAY).await;
            calls.lock().unwrap().push(request);
            called.store(true, Ordering::SeqCst);
            Ok(())
        }
    })
}

/// One test's server, its clock and the token every session hangs off.
struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: TelnyxServer,
}

impl Rig {
    async fn start(server: ServerConfig, telnyx: TelnyxConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = TelnyxServer::bind(server, telnyx, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    async fn default() -> Self {
        Self::start(fast_server(), fast_telnyx()).await
    }

    fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.server.local_addr())
    }

    /// Opens the websocket the way Telnyx does, optionally with the stream auth token header.
    async fn try_connect(&self, path: &str, token: Option<&str>) -> Result<Client, WsError> {
        let mut request = self.url(path).into_client_request().unwrap();
        if let Some(token) = token {
            request.headers_mut().insert(
                "x-telnyx-streaming-auth-token",
                HeaderValue::from_str(token).unwrap(),
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
        phone.connected(None).await;
        phone.start(json!({})).await;
        let mut session = self.accept().await;
        let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
        assert!(matches!(started, TransportEvent::Started(_)));
        (phone, session)
    }
}

/// What the server sent, classified. Telnyx's schemas for what a server sends forbid extra
/// properties, so every message is checked to have exactly the documented keys (in particular no
/// `stream_id`).
#[derive(Debug, PartialEq)]
enum Out {
    /// A `media` message: the decoded G.711 payload.
    Media(Vec<u8>),
    Mark(String),
    Clear,
    /// Any other JSON text (the test says whether it is expected).
    Other(Value),
    /// A close frame with its status code.
    Close(Option<u16>),
}

/// Telnyx, scripted.
struct Phone {
    ws: Client,
    seq: u64,
    chunk: u64,
    stream_id: String,
    call_control_id: String,
    /// Marks the server sent that this phone has not echoed yet.
    unechoed: Vec<String>,
}

impl Phone {
    fn new(ws: Client) -> Self {
        Self {
            ws,
            seq: 0,
            chunk: 0,
            stream_id: unique("32DE0DEA-53CB-4B21-89A4-9E1819C043BC"),
            call_control_id: unique("v2:T02llQxIyaRkhfRKxgAP8nY511EhFLizdvdUKJiSw8d6A9Bbor"),
            unechoed: Vec::new(),
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

    /// `connected`, with the stream auth token when `token` is given.
    async fn connected(&mut self, token: Option<&str>) {
        let message = match token {
            Some(token) => json!({
                "event": "connected",
                "connected": {"x-telnyx-streaming-auth-token": token},
                "version": "1.0.0",
            }),
            None => json!({"event": "connected", "version": "1.0.0"}),
        };
        self.send_value(&message).await;
    }

    /// `start` as in the docs, plus `extra` merged into the `start` object.
    async fn start(&mut self, extra: Value) {
        let mut start = json!({
            "user_id": USER_ID,
            "call_control_id": self.call_control_id,
            "call_session_id": SESSION_ID,
            "from": "+13122010094",
            "to": "+13122123456",
            "tags": ["TAG1", "TAG2"],
            "client_state": "aGF2ZSBhIG5pY2UgZGF5ID1d",
            "custom_parameters": {"agent": "support-tr", "customer_id": "customer_123"},
            "media_format": {"encoding": "PCMU", "sample_rate": 8000, "channels": 1},
        });
        if let (Some(base), Some(extra)) = (start.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "start",
            "sequence_number": sequence,
            "stream_id": self.stream_id,
            "start": start,
        }))
        .await;
    }

    /// An inbound `media` message with chunk number `chunk`.
    async fn media_chunk(&mut self, chunk: u64, g711: &[u8]) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "media",
            "sequence_number": sequence,
            "stream_id": self.stream_id,
            "media": {
                "track": "inbound",
                "chunk": chunk.to_string(),
                "timestamp": (chunk * 160).to_string(),
                "payload": base64::engine::general_purpose::STANDARD.encode(g711),
            },
        }))
        .await;
    }

    /// The next inbound `media` message in sequence.
    async fn media(&mut self, g711: &[u8]) {
        self.chunk += 1;
        let chunk = self.chunk;
        self.media_chunk(chunk, g711).await;
    }

    async fn dtmf(&mut self, digit: &str) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "dtmf",
            "stream_id": self.stream_id,
            "occurred_at": "2025-06-05T08:54:19.698408Z",
            "sequence_number": sequence,
            "dtmf": {"digit": digit},
        }))
        .await;
    }

    /// A `dtmf` message with an explicit `sequence_number`.
    async fn dtmf_with(&mut self, digit: &str, sequence: u64) {
        self.send_value(&json!({
            "event": "dtmf",
            "stream_id": self.stream_id,
            "occurred_at": "2025-06-05T08:54:19.698408Z",
            "sequence_number": sequence.to_string(),
            "dtmf": {"digit": digit},
        }))
        .await;
    }

    /// An inbound `media` message with chunk number `chunk` and the payload text as given.
    async fn media_text(&mut self, chunk: u64, payload: &str) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "media",
            "sequence_number": sequence,
            "stream_id": self.stream_id,
            "media": {
                "track": "inbound",
                "chunk": chunk.to_string(),
                "timestamp": (chunk * 160).to_string(),
                "payload": payload,
            },
        }))
        .await;
    }

    async fn echo_mark(&mut self, name: &str) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "mark",
            "sequence_number": sequence,
            "stream_id": self.stream_id,
            "mark": {"name": name},
        }))
        .await;
        self.unechoed.retain(|m| m != name);
    }

    async fn stop(&mut self) {
        let sequence = self.next_seq();
        self.send_value(&json!({
            "event": "stop",
            "sequence_number": sequence,
            "stream_id": self.stream_id,
            "stop": {"user_id": USER_ID, "call_control_id": self.call_control_id},
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
        let keys: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        match value["event"].as_str() {
            Some("media") => {
                assert_eq!(keys.len(), 2, "media has event and media only: {value}");
                let inner = value["media"].as_object().expect("media object");
                assert_eq!(inner.len(), 1, "media has payload only: {value}");
                let payload = value["media"]["payload"].as_str().expect("payload");
                Out::Media(
                    base64::engine::general_purpose::STANDARD
                        .decode(payload)
                        .unwrap(),
                )
            }
            Some("mark") => {
                assert_eq!(keys.len(), 2, "mark has event and mark only: {value}");
                let name = value["mark"]["name"]
                    .as_str()
                    .expect("mark name")
                    .to_owned();
                self.unechoed.push(name.clone());
                Out::Mark(name)
            }
            Some("clear") => {
                assert_eq!(keys, ["event"], "clear carries nothing else: {value}");
                Out::Clear
            }
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

/// Asserts that no event satisfying `unwanted` shows up within `within` (events that do not
/// match are skipped).
async fn no_event(
    session: &mut TransportSession,
    within: Duration,
    unwanted: impl Fn(&TransportEvent) -> bool,
) {
    let watch = async {
        while let Some(event) = session.recv().await {
            assert!(!unwanted(&event), "unexpected event {event:?}");
        }
    };
    // Ending the session (`recv` gives `None`) would also be a failure of the tests that use this.
    assert!(
        tokio::time::timeout(within, watch).await.is_err(),
        "the session ended"
    );
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

/// The G.711 bytes of [`pcm20`].
fn g711_20(codec: Codec, sample: i16) -> Vec<u8> {
    let pcm = pcm20(sample);
    let mut out = vec![0u8; 160];
    codec.encode(&pcm, &mut out).unwrap();
    out
}

fn mulaw20(sample: i16) -> Vec<u8> {
    g711_20(Codec::Ulaw, sample)
}

/// The PCM16 a G.711 payload decodes to.
fn decoded(codec: Codec, g711: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; g711.len() * 2];
    codec.decode(g711, &mut out).unwrap();
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
async fn connected_and_start_become_call_info_with_8k_g711_metadata() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.connected(None).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, phone.call_control_id);
    let meta = &info.metadata;
    let get = |key: &str| meta.get(key).map(String::as_str);
    assert_eq!(get("sample_rate"), Some("8000"));
    assert_eq!(get("encoding"), Some("mulaw"));
    assert_eq!(get("outbound_encoding"), Some("mulaw"));
    assert_eq!(get("transport"), Some("telnyx"));
    assert_eq!(get("stream_id"), Some(phone.stream_id.as_str()));
    assert_eq!(get("call_control_id"), Some(phone.call_control_id.as_str()));
    assert_eq!(get("call_session_id"), Some(SESSION_ID));
    assert_eq!(get("user_id"), Some(USER_ID));
    assert_eq!(get("from"), Some("+13122010094"));
    assert_eq!(get("to"), Some("+13122123456"));
    assert_eq!(get("tags"), Some("TAG1,TAG2"));
    assert_eq!(get("client_state"), Some("aGF2ZSBhIG5pY2UgZGF5ID1d"));
    assert_eq!(get("telnyx_version"), Some("1.0.0"));
    assert_eq!(get("custom.agent"), Some("support-tr"));
    assert_eq!(get("custom.customer_id"), Some("customer_123"));
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
async fn a_start_without_connected_is_accepted_and_falls_back_to_the_stream_id() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.start(json!({"call_control_id": null})).await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.call_id, phone.stream_id);
    assert!(!info.metadata.contains_key("telnyx_version"));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn inbound_pcmu_media_is_decoded_to_pcm16_at_8k_in_order() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let frames: Vec<Vec<u8>> = [0x10u8, 0x55, 0xC0].iter().map(|b| vec![*b; 160]).collect();
    for frame in &frames {
        phone.media(frame).await;
    }
    for frame in &frames {
        let audio = next_audio(&mut session).await;
        assert_eq!(audio.sample_rate(), SampleRate::Hz8000);
        assert_eq!(audio.data().len(), 320, "160 G.711 bytes are 160 samples");
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, frame).as_slice()
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_in, 3);
    assert_eq!(stats.inbound_chunks_lost, 0);
    assert_eq!(stats.media_reordered, 0, "in-order frames are never held");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_inbound_pcma_stream_is_decoded_with_a_law() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone.connected(None).await;
    phone
        .start(json!({"media_format": {"encoding": "PCMA", "sample_rate": 8000, "channels": 1}}))
        .await;
    let mut session = rig.accept().await;
    let TransportEvent::Started(info) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.metadata["encoding"], "alaw");
    assert_eq!(
        info.metadata["outbound_encoding"], "mulaw",
        "the config says so"
    );
    phone.media(&[0x2A; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Alaw, &[0x2A; 160]).as_slice()
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn media_on_other_tracks_and_other_streams_is_ignored() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let stream_id = phone.stream_id.clone();
    phone
        .send_value(&json!({
            "event": "media", "sequence_number": "9", "stream_id": stream_id,
            "media": {"track": "outbound", "chunk": "1", "timestamp": "0", "payload": "/////w=="},
        }))
        .await;
    phone
        .send_value(&json!({
            "event": "media", "sequence_number": "10",
            "stream_id": "00000000-0000-0000-0000-000000000000",
            "media": {"track": "inbound", "chunk": "1", "timestamp": "0", "payload": "/////w=="},
        }))
        .await;
    phone.media(&[0x20; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[0x20; 160]).as_slice()
    );
    let stats = rig.server.stats();
    assert_eq!(stats.media_in, 1);
    assert_eq!(stats.foreign_stream_messages, 1);
    assert_eq!(stats.faults, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn stop_mark_and_dtmf_of_another_stream_are_ignored_too() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    // The engine has a mark in flight (`e0`), so a foreign echo of it could be mistaken for ours.
    session.send(audio_command(&rig, 0, 0, 800)).await.unwrap();
    session
        .send(TransportCommand::Mark {
            id: "u1".to_owned(),
        })
        .await
        .unwrap();
    let seen = phone.recv_until(|o| matches!(o, Out::Mark(_))).await;
    assert_eq!(seen.last(), Some(&Out::Mark("e0".to_owned())));
    let other = "00000000-0000-0000-0000-000000000000";
    phone
        .send_value(&json!({
            "event": "stop", "sequence_number": "20", "stream_id": other,
            "stop": {"user_id": USER_ID, "call_control_id": phone.call_control_id},
        }))
        .await;
    phone
        .send_value(&json!({
            "event": "mark", "sequence_number": "21", "stream_id": other,
            "mark": {"name": "e0"},
        }))
        .await;
    phone
        .send_value(&json!({
            "event": "dtmf", "sequence_number": "22", "stream_id": other,
            "dtmf": {"digit": "7"},
        }))
        .await;
    // Nothing of it reaches the engine: no hang-up, no echo, no digit.
    no_event(&mut session, Duration::from_millis(300), |e| {
        matches!(
            e,
            TransportEvent::Stopped { .. }
                | TransportEvent::MarkPlayed { .. }
                | TransportEvent::Dtmf { .. }
        )
    })
    .await;
    let stats = rig.server.stats();
    assert_eq!(stats.foreign_stream_messages, 3);
    assert_eq!(stats.marks_echoed, 0);
    assert_eq!(stats.hangups_requested, 0);
    // The call is alive: its own echo still resolves the mark.
    phone.echo_mark("e0").await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::MarkPlayed {
            id: "u1".to_owned()
        }
    );
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert_eq!(requested(&log).len(), 1, "our own close hangs it up once");
}

#[tokio::test]
async fn numbers_may_arrive_as_json_numbers_and_odd_fields_cost_nothing() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let stream_id = phone.stream_id.clone();
    for (sequence, chunk, timestamp) in [
        (json!(4), json!(1), json!(160)),
        (json!("1.0"), json!(2.5), json!("")),
        (json!(null), json!([1]), json!(null)),
    ] {
        phone
            .send_value(&json!({
                "event": "media", "sequence_number": sequence, "stream_id": stream_id,
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

#[tokio::test]
async fn media_that_overtakes_start_is_dropped_and_the_call_still_starts() {
    let mut rig = Rig::default().await;
    let mut phone = rig.phone().await;
    // Telnyx does not order its frames: a media and a dtmf frame may arrive before `start`.
    phone.media(&[0x11; 160]).await;
    phone.dtmf("1").await;
    phone.connected(None).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert_eq!(rig.server.stats().media_before_start, 2);
    phone.media_chunk(2, &[0x22; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[0x22; 160]).as_slice()
    );
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Out-of-order frames

#[tokio::test]
async fn out_of_order_media_is_reordered_by_chunk() {
    // The hold timer must not be what puts the frames right.
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    let frame = |tag: u8| vec![tag; 160];
    // Arrival order 1 3 2 5 4 6: each swapped pair is put right.
    for chunk in [1u64, 3, 2, 5, 4, 6] {
        phone
            .media_chunk(chunk, &frame(u8::try_from(chunk).unwrap() * 0x10))
            .await;
    }
    for chunk in 1..=6u8 {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &frame(chunk * 0x10)).as_slice(),
            "chunk {chunk} out of place"
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_in, 6);
    assert_eq!(
        stats.media_reordered, 4,
        "3, 2, 5 and 4 took the buffer's path"
    );
    assert_eq!(stats.inbound_chunks_lost, 0);
    assert_eq!(stats.media_late, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_lost_chunk_delays_the_audio_behind_it_by_the_hold_timeout_only() {
    let hold = Duration::from_millis(300);
    let telnyx = TelnyxConfig {
        reorder_timeout: hold,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    next_audio(&mut session).await;
    // Chunk 2 never arrives.
    let sent = rig.clock.now();
    phone.media_chunk(3, &[0x30; 160]).await;
    phone.media_chunk(4, &[0x40; 160]).await;
    for tag in [0x30u8, 0x40] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    let waited = rig.clock.now() - sent;
    assert!(
        waited >= hold - Duration::from_millis(20),
        "released after {waited:?}, before the hold timeout {hold:?}"
    );
    assert!(
        waited < hold * 5,
        "released after {waited:?}: the hold must not stretch past {hold:?}"
    );
    let stats = rig.server.stats();
    assert_eq!(stats.inbound_chunks_lost, 1);
    assert_eq!(stats.media_reordered, 2);
    // The stream goes on in order afterwards, without waiting.
    let sent = rig.clock.now();
    phone.media_chunk(5, &[0x50; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[0x50; 160]).as_slice()
    );
    assert!(
        rig.clock.now() - sent < hold,
        "an in-order frame waits for nothing"
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_gap_behind_a_partial_release_keeps_its_own_full_wait() {
    // Chunk 3 is held at once (gap 2), chunk 5 500 ms later (gap 4), chunk 2 arrives at 1000 ms
    // and releases 2 and 3. Chunk 5 is still held behind gap 4: its wait runs from its own
    // arrival (500 ms) to 2000 ms; the deadline armed for chunk 3 (1500 ms) must not apply. Chunk
    // 4 comes at 1750 ms: 250 ms past the stale deadline and 250 ms before the right one, so a
    // scheduler stall of up to 250 ms (the real clock is used: the sockets are real) changes
    // nothing.
    let hold = Duration::from_millis(1500);
    let telnyx = TelnyxConfig {
        reorder_timeout: hold,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    next_audio(&mut session).await;
    phone.media_chunk(3, &[0x30; 160]).await;
    pause(Duration::from_millis(500)).await;
    phone.media_chunk(5, &[0x50; 160]).await;
    pause(Duration::from_millis(500)).await;
    phone.media_chunk(2, &[0x20; 160]).await;
    for tag in [0x20u8, 0x30] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    // 1750 ms after chunk 3: past its deadline, before chunk 5's.
    pause(Duration::from_millis(750)).await;
    phone.media_chunk(4, &[0x40; 160]).await;
    for tag in [0x40u8, 0x50] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.inbound_chunks_lost, 0, "4 was reordered, not lost");
    assert_eq!(stats.media_late, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_gap_that_never_closes_behind_a_partial_release_is_given_up_after_its_own_wait() {
    // The same shape, but chunk 4 never comes: chunk 5 is released `hold` after its own arrival,
    // neither at the stale deadline of chunk 3 (earlier) nor at a fresh one counted from the
    // partial release (later).
    let hold = Duration::from_millis(1500);
    let telnyx = TelnyxConfig {
        reorder_timeout: hold,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    next_audio(&mut session).await;
    phone.media_chunk(3, &[0x30; 160]).await;
    pause(Duration::from_millis(500)).await;
    let fifth = rig.clock.now();
    phone.media_chunk(5, &[0x50; 160]).await;
    pause(Duration::from_millis(500)).await;
    phone.media_chunk(2, &[0x20; 160]).await;
    for tag in [0x20u8, 0x30, 0x50] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    let waited = rig.clock.now() - fifth;
    assert!(
        waited >= hold - Duration::from_millis(100),
        "chunk 5 was released after {waited:?}, before its own wait of {hold:?}"
    );
    assert!(
        waited < hold + Duration::from_millis(350),
        "chunk 5 was released after {waited:?}: later than its own wait of {hold:?}"
    );
    assert_eq!(rig.server.stats().inbound_chunks_lost, 1, "chunk 4");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_chunk_without_usable_audio_still_fills_its_place() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    // Chunk 2 has an empty payload, chunk 4 is not base64, chunk 6 is a valid one; each is
    // followed at once by the next chunk, which must not wait for the 30 s hold timer.
    phone.media_text(2, "").await;
    phone.media_chunk(3, &[0x30; 160]).await;
    phone.media_text(4, "@@@not base64@@@").await;
    phone.media_chunk(5, &[0x50; 160]).await;
    for tag in [0x10u8, 0x30, 0x50] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.inbound_chunks_lost, 0, "{stats:?}");
    assert_eq!(stats.media_late, 0);
    // The same when the empty chunk arrives out of turn: 8 and 9 are empty and the later 7
    // is behind them.
    phone.media_text(9, "").await;
    phone.media_text(8, "").await;
    phone.media_chunk(7, &[0x70; 160]).await;
    phone.media_chunk(6, &[0x60; 160]).await;
    for tag in [0x60u8, 0x70] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    phone.media_chunk(10, &[0xA0; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[0xA0; 160]).as_slice()
    );
    assert_eq!(rig.server.stats().inbound_chunks_lost, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_wild_chunk_number_is_dropped_and_does_not_deafen_the_call() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    for chunk in 1..=3u64 {
        phone
            .media_chunk(chunk, &[u8::try_from(chunk).unwrap(); 160])
            .await;
    }
    phone.media_chunk(4_000_000_000, &[0xEE; 160]).await;
    for chunk in 4..=6u64 {
        phone
            .media_chunk(chunk, &[u8::try_from(chunk).unwrap(); 160])
            .await;
    }
    for chunk in (1..=3u8).chain(4..=6u8) {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[chunk; 160]).as_slice(),
            "chunk {chunk}"
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_late, 1, "the wild chunk");
    assert_eq!(stats.chunk_resyncs, 0);
    assert_eq!(stats.inbound_chunks_lost, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_numbering_that_restarts_is_followed_and_reported() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    for chunk in 1..=20u64 {
        phone
            .media_chunk(chunk, &[u8::try_from(chunk).unwrap(); 160])
            .await;
    }
    for _ in 1..=20 {
        next_audio(&mut session).await;
    }
    // The counter restarts at 1: 8 frames (the window) are taken for late ones, the 9th makes
    // the buffer follow the new numbering.
    for chunk in 1..=12u64 {
        phone
            .media_chunk(chunk, &[100 + u8::try_from(chunk).unwrap(); 160])
            .await;
    }
    // The frame that made the buffer follow is delivered first, then the fault is reported.
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[109; 160]).as_slice()
    );
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("numbering"), "{message}");
    for chunk in 10..=12u8 {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[100 + chunk; 160]).as_slice(),
            "chunk {chunk}"
        );
    }
    let stats = rig.server.stats();
    assert_eq!(stats.chunk_resyncs, 1);
    assert_eq!(stats.media_late, 8);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_full_window_gives_up_on_a_gap_without_waiting_for_the_timer() {
    let telnyx = TelnyxConfig {
        reorder_window: 2,
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    // Chunk 2 is missing; the third frame behind it overflows the window of two.
    for chunk in [3u64, 4, 5] {
        phone
            .media_chunk(chunk, &[u8::try_from(chunk).unwrap() * 0x10; 160])
            .await;
    }
    for tag in [0x10u8, 0x30, 0x40, 0x50] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    assert_eq!(rig.server.stats().inbound_chunks_lost, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn late_and_duplicate_chunks_are_dropped_not_played_twice() {
    let telnyx = TelnyxConfig {
        reorder_window: 2,
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    for chunk in [3u64, 4, 5] {
        phone
            .media_chunk(chunk, &[u8::try_from(chunk).unwrap() * 0x10; 160])
            .await;
    }
    for _ in 0..4 {
        next_audio(&mut session).await;
    }
    // Chunk 2 shows up after its gap was given up on, and chunk 5 is repeated; then a fresh one.
    phone.media_chunk(2, &[0x20; 160]).await;
    phone.media_chunk(5, &[0x50; 160]).await;
    phone.media_chunk(6, &[0x60; 160]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(
        audio.data().as_ref(),
        decoded(Codec::Ulaw, &[0x60; 160]).as_slice(),
        "the late and the repeated frame must not reach the engine"
    );
    let stats = rig.server.stats();
    assert_eq!(stats.media_late, 2);
    assert_eq!(stats.media_in, 5);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn with_the_reorder_window_off_frames_are_delivered_as_they_come() {
    let telnyx = TelnyxConfig {
        reorder_window: 0,
        reorder_timeout: Duration::ZERO,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    phone.media_chunk(4, &[0x40; 160]).await;
    phone.media_chunk(3, &[0x30; 160]).await;
    for tag in [0x10u8, 0x40, 0x30] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    // The skip from 1 to 4 is counted.
    assert_eq!(rig.server.stats().inbound_chunks_lost, 2);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Outbound audio

#[tokio::test]
async fn outbound_audio_is_base64_mulaw_in_20ms_media_messages_without_a_stream_id() {
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
        // `recv` checks that a media message has exactly `event` and `media.payload`.
        match phone.recv().await.unwrap() {
            Out::Media(bytes) => payloads.push(bytes),
            other => panic!("unexpected {other:?}"),
        }
    }
    for (payload, sample) in payloads.iter().zip(samples) {
        assert_eq!(payload.len(), 160, "20 ms of G.711");
        assert_eq!(*payload, mulaw20(sample));
    }
    let stats = rig.server.stats();
    assert_eq!(stats.media_out, 5);
    assert_eq!(stats.media_out_bytes, 800);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_outbound_codec_follows_the_configuration() {
    let telnyx = TelnyxConfig {
        codec: TelnyxCodec::Pcma,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, session) = rig.pair().await;
    session.send(audio_command(&rig, 0, 0, 1234)).await.unwrap();
    assert_eq!(
        phone.recv().await.unwrap(),
        Out::Media(g711_20(Codec::Alaw, 1234))
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_larger_engine_frame_is_cut_into_20ms_messages() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    // 100 ms in one frame.
    session
        .send(audio_command_at(
            &rig.clock,
            SampleRate::Hz8000,
            0,
            0,
            pcm20(700).repeat(5),
        ))
        .await
        .unwrap();
    for _ in 0..5 {
        assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(700)));
    }
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_tail_under_20ms_waits_for_company_and_is_padded_at_the_utterance_end() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    // 12.5 ms frames: 100 samples each. Telnyx takes no message under 20 ms.
    let short = |seq: u64| {
        audio_command_at(
            &rig.clock,
            SampleRate::Hz8000,
            0,
            seq,
            900i16.to_le_bytes().repeat(100),
        )
    };
    let byte = mulaw20(900)[0];
    session.send(short(0)).await.unwrap();
    session.send(short(1)).await.unwrap();
    // 200 samples: one full message goes out, 40 samples wait.
    assert_eq!(phone.recv().await.unwrap(), Out::Media(vec![byte; 160]));
    session
        .send(TransportCommand::Mark {
            id: "end".to_owned(),
        })
        .await
        .unwrap();
    // The 40-sample tail is padded with silence to 20 ms before the mark.
    let mut silence = [0u8; 1];
    Codec::Ulaw.encode(&[0, 0], &mut silence).unwrap();
    let mut expected = vec![byte; 40];
    expected.resize(160, silence[0]);
    assert_eq!(phone.recv().await.unwrap(), Out::Media(expected));
    assert_eq!(phone.recv().await.unwrap(), Out::Mark("e0".to_owned()));
    // Nothing shorter than 20 ms ever went out.
    assert_eq!(rig.server.stats().media_out, 2);
    phone.echo_mark("e0").await;
    next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
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
    session.send(audio_command(&rig, 0, 3, 500)).await.unwrap();
    assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(500)));
    // One notice for the three frames: the other two must not each become a fault event (they
    // would count toward the limit of 16 that ends the call).
    no_event(&mut session, Duration::from_millis(300), |e| {
        matches!(e, TransportEvent::Error { .. })
    })
    .await;
    assert_eq!(rig.server.stats().faults, 1);
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Marks, clear and playback tracking

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
async fn a_mark_with_no_audio_queued_is_echoed_at_once_and_resolves() {
    // Telnyx: "Submitting marks when no audio is played or queued will result in them being
    // immediately sent back."
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    session
        .send(TransportCommand::Mark {
            id: "nothing-queued".to_owned(),
        })
        .await
        .unwrap();
    let Out::Mark(name) = phone.recv().await.unwrap() else {
        panic!("not a mark");
    };
    phone.echo_mark(&name).await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::MarkPlayed {
            id: "nothing-queued".to_owned()
        }
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn mark_echoes_may_arrive_in_any_order_and_are_matched_by_name() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for (seq, id) in ["first", "second"].into_iter().enumerate() {
        session
            .send(audio_command(&rig, 0, seq as u64, 100))
            .await
            .unwrap();
        session
            .send(TransportCommand::Mark { id: id.to_owned() })
            .await
            .unwrap();
    }
    let mut names = Vec::new();
    while names.len() < 2 {
        if let Out::Mark(name) = phone.recv().await.unwrap() {
            names.push(name);
        }
    }
    // Telnyx does not order its frames: the second mark comes back first.
    phone.echo_mark(&names[1]).await;
    phone.echo_mark(&names[0]).await;
    let mut played = Vec::new();
    while played.len() < 2 {
        if let TransportEvent::MarkPlayed { id } = next_event(&mut session, |e| {
            matches!(e, TransportEvent::MarkPlayed { .. })
        })
        .await
        {
            played.push(id);
        }
    }
    assert_eq!(played, ["second", "first"]);
    session.close(WAIT).await.unwrap();
}

/// The latest `PlaybackLead` the engine can read right now.
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
    let telnyx = TelnyxConfig {
        mark_interval: Duration::from_millis(100),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
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
    assert_eq!(marks[0], "p0");
    // Everything is sent. Telnyx has played the first 100 ms: 100 ms are still ahead of the
    // caller. The progress echo reaches the engine as a lead, not as a mark event.
    phone.echo_mark(&marks[0]).await;
    assert_eq!(lead_event(&mut session).await, Duration::from_millis(100));
    phone.echo_mark(&marks[1]).await;
    assert_eq!(lead_event(&mut session).await, Duration::ZERO);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn clear_sends_the_clear_message_in_order_and_drops_queued_audio() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
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
    for _ in 0..3 {
        assert_eq!(phone.recv().await.unwrap(), Out::Media(mulaw20(100)));
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
    // The phone sees `{"event":"clear"}` in order; nothing of the old epoch follows it, and the
    // mark that was cut before it was released is never sent.
    let mut cleared = false;
    let mut fresh = 0;
    while fresh < 5 {
        match phone.recv().await.unwrap() {
            Out::Media(bytes) if bytes == mulaw20(100) => {
                assert!(!cleared, "old audio after the clear");
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
    assert_eq!(rig.server.stats().clears_sent, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn marks_telnyx_returns_on_a_clear_are_dropped_and_resolve_as_mark_cleared() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
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

    // Barge-in. Per the docs, Telnyx answers the clear by sending the queued mark back; on the
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
async fn a_clear_with_nothing_buffered_is_still_sent_and_drops_a_waiting_tail() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    // A 40-sample tail waits for company ...
    session
        .send(audio_command_at(
            &rig.clock,
            SampleRate::Hz8000,
            0,
            0,
            300i16.to_le_bytes().repeat(40),
        ))
        .await
        .unwrap();
    // ... and a barge-in discards it.
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    assert_eq!(phone.recv().await.unwrap(), Out::Clear);
    session.send(audio_command(&rig, 1, 1, 600)).await.unwrap();
    assert_eq!(
        phone.recv().await.unwrap(),
        Out::Media(mulaw20(600)),
        "the old tail must not be glued to new audio"
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_goodbye_that_waits_for_its_mark_is_played_before_the_hang_up() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
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
    assert!(!called.load(Ordering::SeqCst), "hung up before the mark");
    session.send(TransportCommand::Stop).await.unwrap();
    let (rest, close) = phone.drain().await;
    assert!(rest.is_empty(), "nothing follows the played mark: {rest:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
    assert!(called.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------------------------
// DTMF, stop, errors and the ways a call ends

#[tokio::test]
async fn dtmf_reaches_the_engine_in_order() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    for key in ["1", "2", "#", "*", "0", "A", "D"] {
        phone.dtmf(key).await;
    }
    let mut digits = Vec::new();
    while digits.len() < 7 {
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
            DtmfDigit::A,
            DtmfDigit::D
        ]
    );
    session.close(WAIT).await.unwrap();
}

async fn dtmf_digits(session: &mut TransportSession, count: usize) -> Vec<DtmfDigit> {
    let mut digits = Vec::new();
    while digits.len() < count {
        if let TransportEvent::Dtmf { digit, .. } =
            next_event(session, |e| matches!(e, TransportEvent::Dtmf { .. })).await
        {
            digits.push(digit);
        }
    }
    digits
}

#[tokio::test]
async fn dtmf_that_arrives_out_of_order_is_delivered_by_sequence_number() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_millis(400),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    // The caller keyed 1 2 3 4; Telnyx delivered 2 1 4 3 (the sequence numbers have gaps: they
    // count every frame of the connection).
    phone.dtmf_with("2", 12).await;
    phone.dtmf_with("1", 9).await;
    phone.dtmf_with("4", 31).await;
    phone.dtmf_with("3", 20).await;
    assert_eq!(
        dtmf_digits(&mut session, 4).await,
        [DtmfDigit::D1, DtmfDigit::D2, DtmfDigit::D3, DtmfDigit::D4]
    );
    assert_eq!(rig.server.stats().dtmf_reordered, 4);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn held_dtmf_is_not_lost_when_stop_arrives() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.dtmf_with("2", 8).await;
    phone.dtmf_with("1", 7).await;
    phone.stop().await;
    assert_eq!(
        dtmf_digits(&mut session, 2).await,
        [DtmfDigit::D1, DtmfDigit::D2]
    );
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn with_the_reorder_window_off_dtmf_is_delivered_as_it_comes() {
    let telnyx = TelnyxConfig {
        reorder_window: 0,
        reorder_timeout: Duration::ZERO,
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.dtmf_with("2", 12).await;
    phone.dtmf_with("1", 9).await;
    assert_eq!(
        dtmf_digits(&mut session, 2).await,
        [DtmfDigit::D2, DtmfDigit::D1]
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
async fn stop_is_a_peer_hang_up_and_needs_no_rest_hang_up() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    let (seen, close) = phone.drain().await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(!called.load(Ordering::SeqCst), "the call is over already");
    assert_eq!(rig.server.stats().hangups_requested, 0);
}

#[tokio::test]
async fn audio_held_by_the_reorder_buffer_is_not_lost_when_stop_arrives() {
    let telnyx = TelnyxConfig {
        reorder_timeout: Duration::from_secs(30),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    phone.media_chunk(1, &[0x10; 160]).await;
    phone.media_chunk(3, &[0x30; 160]).await;
    phone.stop().await;
    for tag in [0x10u8, 0x30] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_bare_close_frame_ends_the_session_but_is_hung_up_like_a_vanished_socket() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    // Telnyx closes the socket without a `stop`: its docs do not say that ends the call (it may
    // recycle the socket), so the REST hang-up is asked for; "already ended" is success there.
    let (mut phone, mut session) = rig.pair().await;
    let id = phone.call_control_id.clone();
    phone.ws.close(None).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(called.load(Ordering::SeqCst), "a close frame is no `stop`");
    assert_eq!(calls.lock().unwrap()[0].call_control_id, id);

    // A socket that vanishes is hung up as well.
    called.store(false, Ordering::SeqCst);
    let (phone, mut session) = rig.pair().await;
    drop(phone);
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(calls.lock().unwrap().len(), 2);
}

/// The call-control hook built from the server's stopper announces the stop, so the
/// close that `streaming_stop` causes is not a caller hang-up and asks the host for nothing.
#[cfg(feature = "call-control")]
#[tokio::test]
async fn the_call_control_stop_hook_marks_the_stream_as_stopped_on_purpose() {
    use brisvox_transport::call_control::CallRef;
    use brisvox_transport::call_control::telnyx::StreamStopHook;

    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (phone, mut session) = rig.pair().await;
    let hook = StreamStopHook::from_stopper(rig.server.stream_stopper());
    hook.invoke(&CallRef::new(phone.call_control_id.clone()).unwrap());
    let mut ws = phone.ws;
    ws.close(None).await.unwrap();
    no_event(&mut session, Duration::from_millis(500), |e| {
        matches!(e, TransportEvent::Stopped { .. })
    })
    .await;
    assert!(requested(&log).is_empty(), "{:?}", requested(&log));
    assert_eq!(rig.server.stats().hangups_requested, 0);
    session.send(TransportCommand::Stop).await.unwrap();
    wait_for_active(&rig, 0).await;
}

/// After the host announced `streaming_stop` and Telnyx ended the stream (`how`), the engine's
/// session goes on, nothing hangs the call up, and the call's key stays claimed.
async fn a_stream_stopped_on_purpose(how: &str) {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    let id = phone.call_control_id.clone();
    let stopper = rig.server.stream_stopper();
    assert!(!stopper.expect_stop("no-such-call"), "unknown calls say so");
    assert!(stopper.expect_stop(&id));
    match how {
        "stop" => phone.stop().await,
        "close" => phone.ws.close(None).await.unwrap(),
        _ => drop(phone.ws),
    }
    // The caller did not hang up and nothing was asked of the host: a transfer is under way.
    no_event(&mut session, Duration::from_millis(500), |e| {
        matches!(e, TransportEvent::Stopped { .. })
    })
    .await;
    assert!(requested(&log).is_empty(), "{:?}", requested(&log));
    assert_eq!(rig.server.stats().hangups_requested, 0);
    assert_eq!(rig.server.active_calls(), 1, "the session is kept");
    // What the engine says meanwhile has nowhere to go, and is not an error.
    for seq in 0..20u64 {
        session
            .send(audio_command(&rig, 0, seq, 300))
            .await
            .unwrap();
    }
    // A reconnect of the same call (TeXML) is still refused.
    let mut again = rig.phone().await;
    again.call_control_id = id;
    again.connected(None).await;
    again.start(json!({})).await;
    assert_eq!(again.drain().await.1, Some(CLOSE_POLICY));
    // The engine ends the call when its tool is done: the session closes, still no hang-up.
    session.send(TransportCommand::Stop).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(requested(&log).is_empty(), "{:?}", requested(&log));
}

#[tokio::test]
async fn a_close_frame_after_the_host_stopped_the_stream_is_no_hang_up() {
    a_stream_stopped_on_purpose("close").await;
}

#[tokio::test]
async fn a_stop_frame_after_the_host_stopped_the_stream_is_no_hang_up() {
    a_stream_stopped_on_purpose("stop").await;
}

#[tokio::test]
async fn a_vanished_socket_after_the_host_stopped_the_stream_is_no_hang_up() {
    a_stream_stopped_on_purpose("vanish").await;
}

#[tokio::test]
async fn the_announcement_is_per_call_and_a_caller_hang_up_elsewhere_is_still_reported() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut announced, mut first) = rig.pair().await;
    let (mut other, mut second) = rig.pair().await;
    assert!(
        rig.server
            .stream_stopper()
            .expect_stop(&announced.call_control_id)
    );
    // The other call's `stop` is the caller hanging up.
    other.stop().await;
    assert_eq!(stopped(&mut second).await, StopReason::PeerHangup);
    second.close(WAIT).await.unwrap();
    // The announced one is kept.
    announced.ws.close(None).await.unwrap();
    no_event(&mut first, Duration::from_millis(300), |e| {
        matches!(e, TransportEvent::Stopped { .. })
    })
    .await;
    first.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(requested(&log).is_empty(), "{:?}", requested(&log));
}

#[tokio::test]
async fn the_engine_hanging_up_calls_the_hook_then_closes_the_socket_normally() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, session) = rig.pair().await;
    session.send(TransportCommand::Stop).await.unwrap();
    let (seen, close) = phone.drain().await;
    // No JSON of any kind: Telnyx accepts only media, mark and clear from the server.
    assert!(seen.iter().all(|o| matches!(o, Out::Media(_))), "{seen:?}");
    assert_eq!(close, Some(CLOSE_NORMAL));
    // The hook (which takes HOOK_DELAY) had finished by the time the close frame arrived.
    assert!(
        called.load(Ordering::SeqCst),
        "the close frame must follow the hook"
    );
    let requests = calls.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].call_control_id, phone.call_control_id);
    assert_eq!(requests[0].stream_id, phone.stream_id);
    assert_eq!(requests[0].call_session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(
        requests[0].client_state.as_deref(),
        Some("aGF2ZSBhIG5pY2UgZGF5ID1d")
    );
    assert_eq!(rig.server.stats().hangups_requested, 1);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_hang_up_hook_that_fails_or_hangs_never_keeps_the_socket_open() {
    let failing = HangupHook::new(|_| async { Err::<(), String>("telnyx said 500".to_owned()) });
    let hanging = HangupHook::new(|_| async {
        futures::future::pending::<()>().await;
        Ok::<(), String>(())
    });
    for hook in [failing, hanging] {
        let telnyx = TelnyxConfig {
            hangup: Some(hook),
            hangup_timeout: Duration::from_millis(150),
            ..fast_telnyx()
        };
        let mut rig = Rig::start(fast_server(), telnyx).await;
        let (mut phone, session) = rig.pair().await;
        session.send(TransportCommand::Stop).await.unwrap();
        let (_, close) = phone.drain().await;
        assert_eq!(close, Some(CLOSE_NORMAL));
        let stats = rig.server.stats();
        assert_eq!((stats.hangups_requested, stats.hangups_failed), (1, 1));
        wait_for_active(&rig, 0).await;
    }
}

#[tokio::test]
async fn without_a_hook_the_adapter_only_closes_the_socket() {
    let mut rig = Rig::default().await;
    let (mut phone, session) = rig.pair().await;
    session.send(TransportCommand::Stop).await.unwrap();
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_NORMAL));
    assert_eq!(rig.server.stats().hangups_requested, 0);
}

#[tokio::test]
async fn shutdown_closes_every_call_going_away_and_hangs_them_up() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut a, _sa) = rig.pair().await;
    let (mut b, _sb) = rig.pair().await;
    rig.server.shutdown(WAIT).await.unwrap();
    assert_eq!(a.drain().await.1, Some(CLOSE_AWAY));
    assert_eq!(b.drain().await.1, Some(CLOSE_AWAY));
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_error_frame_from_telnyx_is_reported_to_the_engine_as_a_fault() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let stream_id = phone.stream_id.clone();
    phone
        .send_value(&json!({
            "event": "error", "stream_id": stream_id,
            "payload": {"code": 100_004, "title": "invalid_media",
                        "detail": "Media provided was not base64 encoded"},
        }))
        .await;
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("100004"), "{message}");
    assert!(message.contains("invalid_media"), "{message}");
    assert_eq!(rig.server.stats().telnyx_errors, 1);
    // The call goes on.
    phone.media(&[0x40; 160]).await;
    next_audio(&mut session).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn telnyx_error_frames_never_count_toward_the_fault_limit() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    let stream_id = phone.stream_id.clone();
    // More than the limit of 16: rate limits can recur on a long call.
    for _ in 0..20 {
        phone
            .send_value(&json!({
                "event": "error", "stream_id": stream_id,
                "payload": {"code": 100_005, "title": "rate_limit_reached",
                            "detail": "Too many requests"},
            }))
            .await;
    }
    for _ in 0..20 {
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await;
    }
    // The call is alive and hears the caller.
    phone.media(&[0x40; 160]).await;
    next_audio(&mut session).await;
    let stats = rig.server.stats();
    assert_eq!(stats.telnyx_errors, 20);
    assert_eq!(stats.faults, 20);
    session.close(WAIT).await.unwrap();
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
        r#"{"event":"start","start":{}}"#,
        r#"{"stream_id":"x"}"#,
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
        .send_text(r#"{"event":"somethingNew","stream_id":"x","extra":[1,2,3]}"#)
        .await;
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
        r#"{{"event":"mark","mark":{{"name":"{}"}}}}"#,
        "A".repeat(40_000)
    );
    // Over max_control_bytes (16 KiB) but under the websocket layer's limit (64 KiB). (A `media`
    // frame of this size is audio: see a_media_frame_above_the_control_limit_is_audio.)
    phone.send_text(&big).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_SIZE));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_media_payload_above_the_audio_limit_is_a_fault_and_the_call_goes_on() {
    let server = ServerConfig {
        max_audio_bytes: 4096,
        max_control_bytes: 8192,
        ..fast_server()
    };
    let mut rig = Rig::start(server, fast_telnyx()).await;
    let (mut phone, mut session) = rig.pair().await;
    // 3000 G.711 bytes decode to 6000 PCM16 bytes: over the 4096-byte audio limit.
    phone.media(&[0x40; 3000]).await;
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("exceeds"), "{message}");
    phone.media(&[0x40; 160]).await;
    next_audio(&mut session).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_binary_frame_is_ignored() {
    let mut rig = Rig::default().await;
    let (mut phone, mut session) = rig.pair().await;
    tokio::time::timeout(
        WAIT,
        phone
            .ws
            .send(Message::Binary(Bytes::from_static(&[1, 2, 3]))),
    )
    .await
    .unwrap()
    .unwrap();
    phone.media(&[0x40; 160]).await;
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
    phone.start(json!({})).await;
    let TransportEvent::Error { message } =
        next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await
    else {
        unreachable!()
    };
    assert!(message.contains("start"), "{message}");
    assert_eq!(rig.server.stats().calls, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn unsupported_media_formats_are_refused_before_the_engine_sees_the_call() {
    let rig = Rig::default().await;
    for format in [
        json!({"encoding": "L16", "sample_rate": 16000, "channels": 1}),
        json!({"encoding": "G722", "sample_rate": 8000, "channels": 1}),
        json!({"encoding": "OPUS", "sample_rate": 16000, "channels": 1}),
        json!({"encoding": "PCMU", "sample_rate": 16000, "channels": 1}),
        json!({"encoding": "PCMU", "sample_rate": 8000, "channels": 2}),
    ] {
        let mut phone = rig.phone().await;
        phone.connected(None).await;
        phone.start(json!({"media_format": format})).await;
        let (_, close) = phone.drain().await;
        assert_eq!(close, Some(CLOSE_POLICY), "{format}");
    }
    assert_eq!(rig.server.stats().calls, 0);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_start_without_a_stream_id_or_after_nothing_is_refused() {
    let rig = Rig::default().await;
    let mut phone = rig.phone().await;
    phone
        .send_text(r#"{"event":"start","start":{"call_control_id":"c"}}"#)
        .await;
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(1002), "an invalid start is a protocol error");

    drop(rig);

    // A flood of frames and no start at all. The start timeout is long, so only the limit of 64
    // frames before `start` can be what closes the connection (the start timeout closes with the
    // same code, 1008), and the counter shows which one did.
    let long_start = ServerConfig {
        start_timeout: Duration::from_secs(30),
        ..fast_server()
    };
    let rig = Rig::start(long_start, fast_telnyx()).await;
    let mut phone = rig.phone().await;
    let began = rig.clock.now();
    for chunk in 0..80u64 {
        // The server gives up after 64; later sends may find the socket closing.
        let message = json!({"event": "media", "stream_id": phone.stream_id,
            "media": {"track": "inbound", "chunk": chunk.to_string(), "payload": "AAAA"}});
        let _ = phone.ws.send(Message::text(message.to_string())).await;
    }
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_POLICY));
    assert!(rig.clock.now() - began < Duration::from_secs(10));
    assert_eq!(
        rig.server.stats().media_before_start,
        65,
        "refused at the 65th frame, not by the start timeout"
    );

    // Exactly 64 are tolerated: a `start` behind them opens the call.
    let mut rig = Rig::start(long_start, fast_telnyx()).await;
    let mut phone = rig.phone().await;
    for _ in 0..64 {
        phone.media(&[0x40; 160]).await;
    }
    phone.connected(None).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert_eq!(rig.server.stats().media_before_start, 64);
    session.close(WAIT).await.unwrap();

    // No frame at all: the start timeout closes the connection.
    let server = ServerConfig {
        start_timeout: Duration::from_millis(150),
        ..fast_server()
    };
    let rig = Rig::start(server, fast_telnyx()).await;
    let mut phone = rig.phone().await;
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_POLICY));
}

#[tokio::test]
async fn another_path_is_answered_with_404_and_a_trailing_slash_is_accepted() {
    let mut rig = Rig::default().await;
    match rig.try_connect("/elsewhere", None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 404),
        other => panic!("expected HTTP 404, got {:?}", other.map(|_| ())),
    }
    let mut phone = Phone::new(rig.try_connect("/telnyx/stream/", None).await.unwrap());
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
    let mut rig = Rig::start(config, fast_telnyx()).await;
    let (_phone, _session) = rig.pair().await;
    match rig.try_connect(PATH, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 503),
        other => panic!("expected HTTP 503, got {:?}", other.map(|_| ())),
    }
}

// ---------------------------------------------------------------------------------------------
// One stream per call

#[tokio::test]
async fn a_second_stream_for_a_live_or_ended_call_is_refused() {
    let mut rig = Rig::default().await;
    let (mut first, mut first_session) = rig.pair().await;
    let call = first.call_control_id.clone();

    // Telnyx reconnects while the call is live: refused, the engine is not told.
    let mut again = rig.phone().await;
    again.call_control_id = call.clone();
    again.connected(None).await;
    again.start(json!({})).await;
    assert_eq!(again.drain().await.1, Some(CLOSE_POLICY));
    assert_eq!(rig.server.stats().duplicate_calls_refused, 1);
    assert_eq!(rig.server.stats().calls, 1);

    // The first call is untouched.
    first.media(&[0x40; 160]).await;
    next_audio(&mut first_session).await;

    // After it ended, the same call is still refused (the engine session is gone).
    first.stop().await;
    assert_eq!(stopped(&mut first_session).await, StopReason::PeerHangup);
    first.drain().await;
    first_session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    let mut late = rig.phone().await;
    late.call_control_id = call;
    late.connected(None).await;
    late.start(json!({})).await;
    assert_eq!(late.drain().await.1, Some(CLOSE_POLICY));
    assert_eq!(rig.server.stats().duplicate_calls_refused, 2);
}

// ---------------------------------------------------------------------------------------------
// The stream auth token

fn expect_status(result: Result<Client, WsError>, status: u16) {
    match result {
        Err(WsError::Http(response)) => assert_eq!(response.status(), status),
        other => panic!("expected HTTP {status}, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn the_right_token_in_the_header_and_the_connected_frame_is_accepted() {
    let mut rig = Rig::start(fast_server(), authed_telnyx()).await;
    // Telnyx sends the token both ways.
    let mut phone = Phone::new(rig.try_connect(PATH, Some(TOKEN)).await.unwrap());
    phone.connected(Some(TOKEN)).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    phone.media(&[0x40; 160]).await;
    next_audio(&mut session).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_header_alone_or_the_connected_frame_alone_is_enough() {
    let mut rig = Rig::start(fast_server(), authed_telnyx()).await;
    // Header, and a `connected` that carries no token.
    let mut phone = Phone::new(rig.try_connect(PATH, Some(TOKEN)).await.unwrap());
    phone.connected(None).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();

    // No header (a proxy stripped it), the token in the frame.
    let mut phone = rig.phone().await;
    phone.connected(Some(TOKEN)).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_wrong_header_is_refused_at_the_upgrade_with_401() {
    let rig = Rig::start(fast_server(), authed_telnyx()).await;
    expect_status(rig.try_connect(PATH, Some("not-the-token")).await, 401);
    expect_status(rig.try_connect(PATH, Some("")).await, 401);
    // A prefix of the right token, and the right token plus a byte, are wrong too.
    expect_status(
        rig.try_connect(PATH, Some(&TOKEN[..TOKEN.len() - 1])).await,
        401,
    );
    expect_status(rig.try_connect(PATH, Some(&format!("{TOKEN}x"))).await, 401);
    assert_eq!(rig.server.stats().calls, 0);
}

#[tokio::test]
async fn without_a_proof_of_the_token_start_is_refused_and_the_engine_never_sees_a_call() {
    let rig = Rig::start(fast_server(), authed_telnyx()).await;
    // No header and a `connected` without a token.
    let mut phone = rig.phone().await;
    phone.connected(None).await;
    phone.start(json!({})).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    // No header and no `connected` at all.
    let mut phone = rig.phone().await;
    phone.start(json!({})).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    // No header and a wrong token in the frame.
    let mut phone = rig.phone().await;
    phone.connected(Some("not-the-token")).await;
    phone.start(json!({})).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    // A right header does not excuse a wrong token in the frame.
    let mut phone = Phone::new(rig.try_connect(PATH, Some(TOKEN)).await.unwrap());
    phone.connected(Some("not-the-token")).await;
    phone.start(json!({})).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    assert_eq!(rig.server.stats().calls, 0);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn without_an_auth_token_the_header_is_not_checked() {
    let mut rig = Rig::default().await;
    let mut phone = Phone::new(rig.try_connect(PATH, Some("anything")).await.unwrap());
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_token_never_reaches_the_call_metadata_or_the_debug_output() {
    let mut rig = Rig::start(fast_server(), authed_telnyx()).await;
    let mut phone = Phone::new(rig.try_connect(PATH, Some(TOKEN)).await.unwrap());
    phone.connected(Some(TOKEN)).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert!(!format!("{started:?}").contains(TOKEN));
    assert!(!format!("{:?}", authed_telnyx()).contains(TOKEN));
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn the_server_refuses_to_start_with_an_inconsistent_configuration() {
    let clock = SystemClock::shared();
    let parent = CancellationToken::new();
    let bad = TelnyxConfig {
        chunk_bytes: 80,
        ..fast_telnyx()
    };
    let error = TelnyxServer::bind(fast_server(), bad, clock.clone(), &parent)
        .await
        .err()
        .expect("must refuse");
    assert!(error.to_string().contains("chunk_bytes"), "{error}");
    let error = TelnyxServer::bind(
        ServerConfig {
            max_calls: 0,
            ..fast_server()
        },
        fast_telnyx(),
        clock,
        &parent,
    )
    .await
    .err()
    .expect("must refuse");
    assert!(error.to_string().contains("max_calls"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Keepalive, slow Telnyx and task hygiene

#[tokio::test]
async fn the_server_pings_and_a_silent_peer_hits_the_idle_timeout() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_server()
    };
    let mut rig = Rig::start(config, fast_telnyx()).await;
    let (mut phone, mut session) = rig.pair().await;
    // The scripted peer does not poll its socket, so it never answers a ping.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
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
    let mut rig = Rig::start(config, fast_telnyx()).await;
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
    let mut rig = Rig::start(config, fast_telnyx()).await;
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
        next_audio(&mut session).await;
    }
    // Past the write timeout the call is closed.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn every_way_a_telnyx_call_can_end_leaks_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(AtomicBool::new(false));
        let telnyx = TelnyxConfig {
            hangup: Some(recording_hook(&calls, &called)),
            ..fast_telnyx()
        };
        let mut rig = Rig::start(fast_server(), telnyx).await;

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

        // Frames held by the reorder buffer when the call ends.
        let (mut phone, session) = rig.pair().await;
        phone.media_chunk(1, &[0; 160]).await;
        phone.media_chunk(3, &[0; 160]).await;
        drop(phone);
        drop(session);

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
// A whole conversation, as the docs describe it

#[tokio::test]
async fn a_conversation_with_barge_in_and_a_goodbye_over_one_stream() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let telnyx = TelnyxConfig {
        hangup: Some(recording_hook(&calls, &called)),
        mark_interval: Duration::from_millis(40),
        // The swapped frames must be put right by the buffer, not by its timer (the DTMF key
        // waits for it, as its sequence number has gaps).
        reorder_timeout: Duration::from_millis(1500),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), authed_telnyx_with(telnyx)).await;
    let mut phone = Phone::new(rig.try_connect(PATH, Some(TOKEN)).await.unwrap());
    phone.connected(Some(TOKEN)).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;

    // The caller speaks (frames swapped on the way), presses a key.
    phone.media_chunk(1, &[0x40; 160]).await;
    phone.media_chunk(3, &[0x60; 160]).await;
    phone.media_chunk(2, &[0x50; 160]).await;
    phone.dtmf("5").await;
    for tag in [0x40u8, 0x50, 0x60] {
        let audio = next_audio(&mut session).await;
        assert_eq!(
            audio.data().as_ref(),
            decoded(Codec::Ulaw, &[tag; 160]).as_slice()
        );
    }
    next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;

    // The bot answers; the caller barges in; the bot answers again and says goodbye.
    for seq in 0..10u64 {
        session
            .send(audio_command(&rig, 0, seq, 300))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "answer-1".to_owned(),
        })
        .await
        .unwrap();
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 20..23u64 {
        session
            .send(audio_command(&rig, 1, seq, 400))
            .await
            .unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "goodbye".to_owned(),
        })
        .await
        .unwrap();
    // Read until the goodbye mark; echo every mark that came after the clear, and the ones
    // before it too (Telnyx returns the flushed ones).
    let mut after_clear = false;
    let goodbye = loop {
        match phone.recv().await.unwrap() {
            Out::Clear => after_clear = true,
            Out::Mark(name) if after_clear && name.starts_with('e') => break name,
            Out::Mark(_) | Out::Media(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    for name in phone.unechoed.clone() {
        phone.echo_mark(&name).await;
    }
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|e| matches!(e, TransportEvent::MarkPlayed { id } if id == "goodbye"))
    {
        events.push(
            next_event(&mut session, |e| {
                matches!(
                    e,
                    TransportEvent::MarkPlayed { .. } | TransportEvent::MarkCleared { .. }
                )
            })
            .await,
        );
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TransportEvent::MarkPlayed { id } if id == "answer-1")),
        "the cut utterance must never count as played: {events:?} (goodbye is {goodbye})"
    );
    session.send(TransportCommand::Stop).await.unwrap();
    assert_eq!(phone.drain().await.1, Some(CLOSE_NORMAL));
    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// `telnyx` with the auth token of [`authed_telnyx`].
fn authed_telnyx_with(telnyx: TelnyxConfig) -> TelnyxConfig {
    TelnyxConfig {
        auth_token: Some(Secret::new(TOKEN.to_owned())),
        ..telnyx
    }
}

// ---------------------------------------------------------------------------------------------
// The hang-up hook on every way the adapter ends a call

const CLOSE_AGAIN: u16 = 1013;

type HangupLog = Arc<Mutex<Vec<HangupRequest>>>;

/// `telnyx` with a recording hang-up hook, and the log it writes.
fn hooked(telnyx: TelnyxConfig) -> (TelnyxConfig, HangupLog) {
    let log: HangupLog = Arc::new(Mutex::new(Vec::new()));
    let called = Arc::new(AtomicBool::new(false));
    let config = TelnyxConfig {
        hangup: Some(recording_hook(&log, &called)),
        ..telnyx
    };
    (config, log)
}

fn requested(log: &HangupLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|r| r.call_control_id.clone())
        .collect()
}

#[tokio::test]
async fn a_flood_of_faults_hangs_the_call_up_through_the_hook() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    for _ in 0..20 {
        phone.send_text("garbage").await;
    }
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert_eq!(requested(&log), [phone.call_control_id.as_str()]);
    assert_eq!(rig.server.stats().hangups_requested, 1);
}

#[tokio::test]
async fn an_oversized_message_hangs_the_call_up_through_the_hook() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut phone, mut session) = rig.pair().await;
    // A control message (not audio) above `max_control_bytes`.
    let big = format!(
        r#"{{"event":"mark","mark":{{"name":"{}"}}}}"#,
        "A".repeat(40_000)
    );
    phone.send_text(&big).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    assert_eq!(phone.drain().await.1, Some(CLOSE_SIZE));
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert_eq!(requested(&log), [phone.call_control_id.as_str()]);
}

#[tokio::test]
async fn a_media_frame_above_the_control_limit_is_audio_not_a_fatal_oversize() {
    // `max_control_bytes` (16 KiB) bounds control messages. A media frame is bounded by
    // `max_audio_bytes` (64 KiB, about 32 KiB of G.711): 15000 bytes of G.711 are a 20 KB frame.
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    // Before `start` as well (a media frame may overtake it).
    let mut phone = rig.phone().await;
    phone.media_chunk(1, &vec![0x40; 15_000]).await;
    phone.connected(None).await;
    phone.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert_eq!(rig.server.stats().media_before_start, 1);

    phone.media_chunk(1, &vec![0x40; 15_000]).await;
    let audio = next_audio(&mut session).await;
    assert_eq!(audio.data().len(), 30_000);
    // An audio payload above `max_audio_bytes` is a fault, and the call goes on.
    phone.media_chunk(2, &vec![0x40; 33_000]).await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await;
    phone.media_chunk(3, &[0x40; 160]).await;
    next_audio(&mut session).await;
    assert!(requested(&log).is_empty(), "the call was never hung up");
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_idle_connection_hangs_the_call_up_through_the_hook() {
    let (telnyx, log) = hooked(fast_telnyx());
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_server()
    };
    let mut rig = Rig::start(config, telnyx).await;
    let (phone, mut session) = rig.pair().await;
    // The scripted peer says nothing and answers no ping.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert_eq!(requested(&log), [phone.call_control_id.as_str()]);
}

#[tokio::test]
async fn refusals_on_authentication_or_for_a_duplicate_call_never_reach_the_hook() {
    // Authentication: the call is not proven to be ours, and nothing was started for it.
    let (telnyx, log) = hooked(authed_telnyx());
    let rig = Rig::start(fast_server(), telnyx).await;
    let mut phone = rig.phone().await;
    phone.connected(None).await;
    phone.start(json!({})).await;
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    expect_status(rig.try_connect(PATH, Some("wrong")).await, 401);
    wait_for_active(&rig, 0).await;
    assert!(requested(&log).is_empty(), "{:?}", requested(&log));

    // A duplicate: the live connection serves the call; hanging it up would kill that one.
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut first, mut session) = rig.pair().await;
    let mut again = rig.phone().await;
    again.call_control_id = first.call_control_id.clone();
    again.connected(None).await;
    again.start(json!({})).await;
    assert_eq!(again.drain().await.1, Some(CLOSE_POLICY));
    assert!(requested(&log).is_empty());
    // And the live call goes on: Telnyx ends it itself.
    first.media(&[0x40; 160]).await;
    next_audio(&mut session).await;
    first.stop().await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    session.close(WAIT).await.unwrap();
    wait_for_active(&rig, 0).await;
    assert!(requested(&log).is_empty());
}

#[tokio::test]
async fn a_stream_refused_after_start_is_hung_up_and_may_come_again() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    // A call leg negotiated as G.722 and a host that left `stream_codec` out.
    let mut phone = rig.phone().await;
    let id = phone.call_control_id.clone();
    phone.connected(None).await;
    phone
        .start(json!({"media_format": {"encoding": "G722", "sample_rate": 8000, "channels": 1}}))
        .await;
    // The close frame follows the hook, so the log is complete when it arrives.
    assert_eq!(phone.drain().await.1, Some(CLOSE_POLICY));
    assert_eq!(requested(&log), [id.as_str()]);
    let hung_up = log.lock().unwrap()[0].clone();
    assert_eq!(hung_up.stream_id, phone.stream_id);
    assert_eq!(hung_up.call_session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(rig.server.stats().calls, 0);
    wait_for_active(&rig, 0).await;

    // The same call comes back with a usable format (the host fixed `stream_codec`): not a
    // duplicate, the engine never had it.
    let mut again = rig.phone().await;
    again.call_control_id = id;
    again.connected(None).await;
    again.start(json!({})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    assert_eq!(rig.server.stats().duplicate_calls_refused, 0);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_engine_that_takes_no_calls_gets_a_retry_and_the_call_is_hung_up() {
    let (telnyx, log) = hooked(fast_telnyx());
    let server = ServerConfig {
        accept_queue: 1,
        send_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, telnyx).await;
    // The first call sits in the accept queue: nobody is accepting yet.
    let mut first = rig.phone().await;
    first.connected(None).await;
    first.start(json!({})).await;
    let waiting = async {
        while rig.server.stats().calls < 1 {
            pause(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, waiting).await.unwrap();

    let mut second = rig.phone().await;
    let id = second.call_control_id.clone();
    second.connected(None).await;
    second.start(json!({})).await;
    assert_eq!(second.drain().await.1, Some(CLOSE_AGAIN));
    // The call was not served: it is hung up (the close does not end a Call Control call).
    assert_eq!(requested(&log), [id.as_str()]);

    // The engine catches up; Telnyx (told to try again) reconnects the same call: accepted.
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    let mut retry = rig.phone().await;
    retry.call_control_id = id;
    retry.connected(None).await;
    retry.start(json!({})).await;
    let mut retried = rig.accept().await;
    next_event(&mut retried, |e| matches!(e, TransportEvent::Started(_))).await;
    assert_eq!(rig.server.stats().duplicate_calls_refused, 0);
    retried.close(WAIT).await.unwrap();
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn an_engine_that_takes_no_input_gets_a_1013_and_the_call_is_hung_up() {
    let (telnyx, log) = hooked(fast_telnyx());
    let server = ServerConfig {
        send_timeout: Duration::from_millis(300),
        ..fast_server()
    };
    let mut rig = Rig::start(server, telnyx).await;
    let mut phone = rig.phone().await;
    let id = phone.call_control_id.clone();
    phone.connected(None).await;
    phone.start(json!({})).await;
    // The engine takes the call and never reads its events.
    let cancel = CancellationToken::new();
    let link = tokio::time::timeout(WAIT, rig.server.accept(&cancel))
        .await
        .expect("no call within the wait")
        .unwrap()
        .expect("server closed");
    // Telnyx's own `error` frames are reported as fault events without counting toward the fault
    // limit: they fill the engine's queue (64 events) and the next one waits `send_timeout`.
    let stream_id = phone.stream_id.clone();
    for _ in 0..100 {
        let message = json!({"event": "error", "stream_id": stream_id,
            "payload": {"code": 100_005, "title": "rate_limit_reached", "detail": "x"}});
        if phone
            .ws
            .send(Message::text(message.to_string()))
            .await
            .is_err()
        {
            break;
        }
    }
    let (_, close) = phone.drain().await;
    assert_eq!(close, Some(CLOSE_AGAIN));
    wait_for_active(&rig, 0).await;
    assert_eq!(requested(&log), [id.as_str()]);
    let stats = rig.server.stats();
    assert_eq!(stats.hangups_requested, 1);
    assert!(stats.faults >= 60, "{} faults counted", stats.faults);
    drop(link);
}

#[tokio::test]
async fn a_start_without_a_call_control_id_cannot_be_hung_up_and_says_so() {
    let (telnyx, log) = hooked(fast_telnyx());
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let mut phone = rig.phone().await;
    phone.connected(None).await;
    phone.start(json!({"call_control_id": ""})).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session.send(TransportCommand::Stop).await.unwrap();
    assert_eq!(phone.drain().await.1, Some(CLOSE_NORMAL));
    wait_for_active(&rig, 0).await;
    assert!(requested(&log).is_empty());
    let stats = rig.server.stats();
    assert_eq!((stats.hangups_requested, stats.hangups_skipped), (0, 1));
}

#[tokio::test]
async fn a_hook_that_hangs_does_not_hold_up_a_shutdown() {
    let hanging = HangupHook::new(|_| async {
        futures::future::pending::<()>().await;
        Ok::<(), String>(())
    });
    let telnyx = TelnyxConfig {
        hangup: Some(hanging),
        hangup_timeout: Duration::from_millis(200),
        ..fast_telnyx()
    };
    let mut rig = Rig::start(fast_server(), telnyx).await;
    let (mut a, _sa) = rig.pair().await;
    let (mut b, _sb) = rig.pair().await;
    let began = rig.clock.now();
    rig.server.shutdown(WAIT).await.unwrap();
    assert!(
        rig.clock.now() - began < Duration::from_secs(3),
        "the hook is bounded by hangup_timeout"
    );
    assert_eq!(a.drain().await.1, Some(CLOSE_AWAY));
    assert_eq!(b.drain().await.1, Some(CLOSE_AWAY));
    let stats = rig.server.stats();
    assert_eq!((stats.hangups_requested, stats.hangups_failed), (2, 2));
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
    let server = TelnyxServer::serve_gated(
        listener,
        fast_server(),
        fast_telnyx(),
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
    phone.connected(None).await;
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
