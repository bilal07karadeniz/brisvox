//! Websocket transport tests: real sockets on 127.0.0.1, the real clock, short deadlines.
//!
//! Each test starts a `WebSocketServer`, plays the engine with a `TransportSession` on every
//! accepted call and plays the client with a raw tokio-tungstenite socket (or, for the last
//! group, the reference client of `brisvox_transport::websocket::client`).
#![cfg(feature = "websocket")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable
)] // test helpers (see clippy.toml)

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use brisvox_audio::Codec;
use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{SharedClock, SystemClock, TurnEpoch};
use brisvox_sim::leak::TaskCounter;
use brisvox_transport::admission::{AdmissionGate, AdmissionTicket, Refusal, SharedGate};
use brisvox_transport::websocket::client::{self, ClientConfig};
use brisvox_transport::websocket::protocol::{code, stop};
use brisvox_transport::websocket::{
    ClientMessage, Encoding, ServerConfig, ServerMessage, Start, WebSocketServer,
};
use brisvox_transport::{
    CallInfo, CallRates, SessionConfig, StopReason, Transport, TransportCommand, TransportEvent,
    TransportSession, UtteranceId, wav,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async};
use tokio_util::sync::CancellationToken;

/// Upper bound on any single wait in these tests (real time).
const WAIT: Duration = Duration::from_secs(5);

/// Websocket close codes of docs/WEBSOCKET_PROTOCOL.md, spelled out so that a swapped
/// `CloseCode` in the server fails a test.
const CLOSE_NORMAL: u16 = 1000;
const CLOSE_AWAY: u16 = 1001;
const CLOSE_PROTOCOL: u16 = 1002;
const CLOSE_POLICY: u16 = 1008;
const CLOSE_SIZE: u16 = 1009;
const CLOSE_AGAIN: u16 = 1013;

/// Waits `duration` on the real clock.
async fn pause(duration: Duration) {
    SystemClock::shared().sleep(duration).await;
}

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One test's server, its clock and the token every session hangs off.
struct Rig {
    clock: SharedClock,
    parent: CancellationToken,
    server: WebSocketServer,
}

fn fast_config() -> ServerConfig {
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

impl Rig {
    async fn start(config: ServerConfig) -> Self {
        let clock = SystemClock::shared();
        let parent = CancellationToken::new();
        let server = WebSocketServer::bind(config, clock.clone(), &parent)
            .await
            .unwrap();
        Self {
            clock,
            parent,
            server,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/call", self.server.local_addr())
    }

    async fn connect(&self) -> Client {
        let (ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(self.url()))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        ws
    }

    /// Connects and completes `start` at 16 kHz PCM16; returns the client and its `started`.
    async fn call(&self) -> (Client, ServerMessage) {
        self.call_with(Start::new(16_000, Encoding::Pcm16)).await
    }

    async fn call_with(&self, start: Start) -> (Client, ServerMessage) {
        let mut ws = self.connect().await;
        send_json(&mut ws, &ClientMessage::Start(start)).await;
        let started = next_server_message(&mut ws).await;
        assert!(
            matches!(started, ServerMessage::Started { .. }),
            "{started:?}"
        );
        (ws, started)
    }

    /// Like [`Rig::call_with`], on a socket with a tiny receive buffer: a client that stops
    /// reading fills the connection after a few kilobytes, whatever the kernel's autotuning.
    async fn small_buffer_call(&self, start: Start) -> (Client, ServerMessage) {
        let mut ws = self.connect_small_buffer().await;
        send_json(&mut ws, &ClientMessage::Start(start)).await;
        let started = next_server_message(&mut ws).await;
        assert!(
            matches!(started, ServerMessage::Started { .. }),
            "{started:?}"
        );
        (ws, started)
    }

    async fn connect_small_buffer(&self) -> Client {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let stream = socket.connect(self.server.local_addr()).await.unwrap();
        let (ws, _) = tokio::time::timeout(
            WAIT,
            client_async(self.url(), MaybeTlsStream::Plain(stream)),
        )
        .await
        .expect("upgrade timed out")
        .expect("upgrade failed");
        ws
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

    /// Connects, starts and accepts a call in one go.
    async fn pair(&mut self) -> (Client, TransportSession) {
        let (ws, _) = self.call().await;
        let mut session = self.accept().await;
        let started = next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
        assert!(matches!(started, TransportEvent::Started(_)));
        (ws, session)
    }
}

async fn send_json(ws: &mut Client, message: &ClientMessage) {
    tokio::time::timeout(WAIT, ws.send(Message::text(message.to_json())))
        .await
        .expect("send timed out")
        .expect("send failed");
}

async fn send_text(ws: &mut Client, text: &str) {
    tokio::time::timeout(WAIT, ws.send(Message::text(text)))
        .await
        .expect("send timed out")
        .expect("send failed");
}

async fn send_audio(ws: &mut Client, bytes: Vec<u8>) {
    tokio::time::timeout(WAIT, ws.send(Message::Binary(Bytes::from(bytes))))
        .await
        .expect("send timed out")
        .expect("send failed");
}

/// The next non-keepalive frame, or `None` when the connection ended.
async fn next_frame(ws: &mut Client) -> Option<Message> {
    loop {
        let next = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no frame within the wait");
        match next {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(message)) => return Some(message),
            Some(Err(_)) | None => return None,
        }
    }
}

/// The next text frame as a server message (skipping audio).
async fn next_server_message(ws: &mut Client) -> ServerMessage {
    loop {
        match next_frame(ws).await.expect("connection ended") {
            Message::Text(text) => return ServerMessage::parse(text.as_str()).unwrap(),
            Message::Binary(_) => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

/// Reads until the connection ends (a close frame or EOF); returns the text messages seen.
async fn drain_to_end(ws: &mut Client) -> Vec<ServerMessage> {
    drain_with_close(ws).await.0
}

/// Like [`drain_to_end`], and returns the status code of the close frame (`None` if the
/// connection ended without one).
async fn drain_with_close(ws: &mut Client) -> (Vec<ServerMessage>, Option<u16>) {
    let mut seen = Vec::new();
    let mut close = None;
    while let Some(message) = next_frame(ws).await {
        match message {
            Message::Text(text) => seen.push(ServerMessage::parse(text.as_str()).unwrap()),
            Message::Close(frame) => {
                close = frame.map(|f| u16::from(f.code));
                break;
            }
            _ => {}
        }
    }
    (seen, close)
}

/// Expects a fatal `error` with `expected` code, then the end of the connection.
async fn expect_fatal(ws: &mut Client, expected: &str) {
    expect_fatal_message(ws, expected).await;
}

/// Like [`expect_fatal`], and returns the error's `message`.
async fn expect_fatal_message(ws: &mut Client, expected: &str) -> String {
    expect_fatal_closed(ws, expected, None).await
}

/// Like [`expect_fatal`], and expects the close frame to carry the status code `close` (the
/// codes of docs/WEBSOCKET_PROTOCOL.md that clients branch on).
async fn expect_fatal_with_close(ws: &mut Client, expected: &str, close: u16) {
    expect_fatal_closed(ws, expected, Some(close)).await;
}

async fn expect_fatal_closed(ws: &mut Client, expected: &str, close: Option<u16>) -> String {
    let (seen, close_code) = drain_with_close(ws).await;
    let error = seen
        .iter()
        .find_map(|m| match m {
            ServerMessage::Error {
                code,
                fatal: true,
                message,
            } => Some((code.clone(), message.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no fatal error in {seen:?}"));
    assert_eq!(error.0, expected, "{}", error.1);
    if let Some(close) = close {
        assert_eq!(close_code, Some(close), "close code after {expected}");
    }
    error.1
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

/// 20 ms of 16 kHz PCM16 whose bytes are all `fill`.
fn pcm(fill: u8) -> Vec<u8> {
    vec![fill; 640]
}

fn audio_command(rig: &Rig, epoch: u32, seq: u64, fill: u8) -> TransportCommand {
    audio_command_at(rig, SampleRate::Hz16000, epoch, seq, pcm(fill))
}

fn audio_command_at(
    rig: &Rig,
    rate: SampleRate,
    epoch: u32,
    seq: u64,
    bytes: Vec<u8>,
) -> TransportCommand {
    let epoch = TurnEpoch::new(epoch);
    let frame = AudioFrame::new(Bytes::from(bytes), rate, rig.clock.now(), seq, epoch).unwrap();
    TransportCommand::Audio {
        epoch,
        utterance: UtteranceId::new(1),
        frame,
    }
}

// ---------------------------------------------------------------------------------------------
// Handshake and start

#[tokio::test]
async fn handshake_start_and_started() {
    let mut rig = Rig::start(fast_config()).await;
    let mut start = Start::new(16_000, Encoding::Pcm16);
    start.call_id = Some("client-42".to_owned());
    start
        .metadata
        .insert("tenant".to_owned(), "acme".to_owned());
    start
        .metadata
        .insert("sample_rate".to_owned(), "spoofed".to_owned());
    // The WebRTC adapter's trusted key must not be settable by a websocket client.
    start.metadata.insert(
        CallInfo::OUTPUT_SAMPLE_RATE_KEY.to_owned(),
        "8000".to_owned(),
    );
    start.metadata.insert(
        CallInfo::INPUT_SAMPLE_RATE_KEY.to_owned(),
        "8000".to_owned(),
    );
    let (_ws, started) = rig.call_with(start).await;
    let ServerMessage::Started {
        version,
        call_id,
        sample_rate,
        encoding,
    } = started
    else {
        unreachable!()
    };
    assert_eq!(
        (version, sample_rate, encoding),
        (1, 16_000, Encoding::Pcm16)
    );
    assert!(call_id.starts_with("ws-"), "{call_id}");

    let mut session = rig.accept().await;
    let TransportEvent::Started(info) = session.recv().await.unwrap() else {
        panic!("first event is not Started");
    };
    assert_eq!(info.call_id, call_id);
    assert_eq!(info.metadata["tenant"], "acme");
    assert_eq!(info.metadata["client_call_id"], "client-42");
    assert_eq!(info.metadata["transport"], "websocket");
    assert_eq!(info.metadata["encoding"], "pcm16");
    // The server's keys win over the client's.
    assert_eq!(info.metadata["sample_rate"], "16000");
    assert_eq!(info.metadata[CallInfo::SAMPLE_RATE_KEY], "16000");
    assert!(info.metadata["peer_addr"].starts_with("127.0.0.1:"));
    // The typed accessor reads what the adapter wrote, not the client's spoofed value, and a
    // client-supplied `output_sample_rate` is dropped (the call stays symmetric).
    assert!(!info.metadata.contains_key(CallInfo::OUTPUT_SAMPLE_RATE_KEY));
    assert!(!info.metadata.contains_key(CallInfo::INPUT_SAMPLE_RATE_KEY));
    assert_eq!(
        info.sample_rate().unwrap(),
        Some(CallRates::symmetric(SampleRate::Hz16000))
    );
}

#[tokio::test]
async fn started_info_carries_the_negotiated_call_rate_for_the_typed_accessor() {
    for (hz, rate, encoding) in [
        (8_000, SampleRate::Hz8000, Encoding::Mulaw),
        (48_000, SampleRate::Hz48000, Encoding::Pcm16),
    ] {
        let mut rig = Rig::start(fast_config()).await;
        let (_ws, _started) = rig.call_with(Start::new(hz, encoding)).await;
        let mut session = rig.accept().await;
        let TransportEvent::Started(info) = session.recv().await.unwrap() else {
            panic!("first event is not Started");
        };
        assert_eq!(
            info.sample_rate().unwrap(),
            Some(CallRates::symmetric(rate)),
            "{hz} Hz"
        );
    }
}

#[tokio::test]
async fn invalid_start_is_refused_with_a_reason() {
    let rig = Rig::start(fast_config()).await;
    // (text, error code, close code): 1008 policy violation for a well-formed but unacceptable
    // start, 1002 protocol error for a message that is not valid protocol at all.
    let cases: [(&str, &str, u16); 6] = [
        (
            r#"{"type":"start","version":2,"sample_rate":16000}"#,
            code::UNSUPPORTED_VERSION,
            1008,
        ),
        (
            r#"{"type":"start","version":1,"sample_rate":12345}"#,
            code::UNSUPPORTED_SAMPLE_RATE,
            1008,
        ),
        (
            r#"{"type":"start","version":1,"sample_rate":16000,"encoding":"opus"}"#,
            code::INVALID_MESSAGE,
            CLOSE_PROTOCOL,
        ),
        (
            r#"{"type":"dtmf","digit":"1"}"#,
            code::INVALID_MESSAGE,
            CLOSE_POLICY,
        ),
        ("not json at all", code::INVALID_JSON, CLOSE_PROTOCOL),
        (
            r#"{"type":"rewind"}"#,
            code::UNKNOWN_MESSAGE,
            CLOSE_PROTOCOL,
        ),
    ];
    for (text, expected, close) in cases {
        let mut ws = rig.connect().await;
        send_text(&mut ws, text).await;
        expect_fatal_with_close(&mut ws, expected, close).await;
    }
}

#[tokio::test]
async fn audio_before_start_and_start_timeout() {
    let config = ServerConfig {
        start_timeout: Duration::from_millis(250),
        ..fast_config()
    };
    let rig = Rig::start(config).await;
    let mut ws = rig.connect().await;
    send_audio(&mut ws, pcm(1)).await;
    expect_fatal_with_close(&mut ws, code::AUDIO_BEFORE_START, CLOSE_POLICY).await;

    let mut silent = rig.connect().await;
    expect_fatal_with_close(&mut silent, code::START_TIMEOUT, CLOSE_POLICY).await;
}

#[tokio::test]
async fn handshake_timeout_frees_the_slot() {
    let config = ServerConfig {
        handshake_timeout: Duration::from_millis(200),
        max_calls: 1,
        ..fast_config()
    };
    let rig = Rig::start(config).await;
    // A TCP connection that never speaks websocket.
    let mut tcp = TcpStream::connect(rig.server.local_addr()).await.unwrap();
    wait_for_active(&rig, 1).await;
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(WAIT, tokio::io::AsyncReadExt::read(&mut tcp, &mut buf))
        .await
        .expect("server did not close the idle connection");
    assert!(matches!(read, Ok(0) | Err(_)));
    wait_for_active(&rig, 0).await;
    // The single slot is usable again.
    let (_ws, _) = rig.call().await;
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

// ---------------------------------------------------------------------------------------------
// Audio

#[tokio::test]
async fn audio_flows_both_ways_in_order() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;

    for fill in 1..=10u8 {
        send_audio(&mut ws, pcm(fill)).await;
    }
    for fill in 1..=10u8 {
        let TransportEvent::Audio(frame) =
            next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
        else {
            unreachable!()
        };
        assert_eq!(frame.sample_rate(), SampleRate::Hz16000);
        assert_eq!(frame.data().as_ref(), pcm(fill).as_slice());
        assert_eq!(frame.seq(), u64::from(fill) - 1);
    }

    for fill in 101..=110u8 {
        session
            .send(audio_command(&rig, 0, u64::from(fill), fill))
            .await
            .unwrap();
    }
    for fill in 101..=110u8 {
        match next_frame(&mut ws).await.unwrap() {
            Message::Binary(data) => assert_eq!(data.as_ref(), pcm(fill).as_slice()),
            other => panic!("expected audio, got {other:?}"),
        }
    }
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn mulaw_calls_are_converted_both_ways() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, started) = rig.call_with(Start::new(8_000, Encoding::Mulaw)).await;
    assert!(matches!(
        started,
        ServerMessage::Started {
            sample_rate: 8_000,
            encoding: Encoding::Mulaw,
            ..
        }
    ));
    let mut session = rig.accept().await;

    // Client to engine: mu-law bytes arrive as PCM16.
    let encoded: Vec<u8> = (0..160u8).map(|i| i.wrapping_mul(3)).collect();
    send_audio(&mut ws, encoded.clone()).await;
    let TransportEvent::Audio(frame) =
        next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
    else {
        unreachable!()
    };
    assert_eq!(frame.sample_rate(), SampleRate::Hz8000);
    let mut expected = vec![0u8; 320];
    Codec::Ulaw.decode(&encoded, &mut expected).unwrap();
    assert_eq!(frame.data().as_ref(), expected.as_slice());

    // Engine to client: PCM16 leaves as mu-law.
    let source: Vec<u8> = (0..160i16)
        .flat_map(|i| (i * 150 - 12_000).to_le_bytes())
        .collect();
    session
        .send(audio_command_at(
            &rig,
            SampleRate::Hz8000,
            0,
            0,
            source.clone(),
        ))
        .await
        .unwrap();
    let Message::Binary(wire) = next_frame(&mut ws).await.unwrap() else {
        panic!("expected audio");
    };
    let mut expected = vec![0u8; 160];
    Codec::Ulaw.encode(&source, &mut expected).unwrap();
    assert_eq!(wire.as_ref(), expected.as_slice());
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn output_at_another_rate_is_dropped_with_one_notice() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, session) = rig.pair().await;
    for seq in 0..3 {
        session
            .send(audio_command_at(
                &rig,
                SampleRate::Hz24000,
                0,
                seq,
                vec![9; 960],
            ))
            .await
            .unwrap();
    }
    session.send(audio_command(&rig, 0, 3, 77)).await.unwrap();
    let mut notices = 0;
    loop {
        match next_frame(&mut ws).await.unwrap() {
            Message::Text(text) => match ServerMessage::parse(text.as_str()).unwrap() {
                ServerMessage::Error {
                    code, fatal: false, ..
                } if code == code::OUTPUT_RATE_MISMATCH => notices += 1,
                other => panic!("unexpected {other:?}"),
            },
            Message::Binary(data) => {
                assert_eq!(data.as_ref(), pcm(77).as_slice());
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(notices, 1);
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn clear_drops_queued_audio_and_cuts_marks() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;

    // Thirty frames (600 ms) of epoch 0, then a mark behind them.
    for seq in 0..30u64 {
        session.send(audio_command(&rig, 0, seq, 5)).await.unwrap();
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
        if let Message::Binary(data) = next_frame(&mut ws).await.unwrap() {
            assert_eq!(data[0], 5);
            before += 1;
        }
    }
    session
        .send(TransportCommand::Clear {
            epoch: TurnEpoch::new(1),
        })
        .await
        .unwrap();
    for seq in 100..105u64 {
        session
            .send(audio_command(&rig, 1, seq, 0xE0))
            .await
            .unwrap();
    }

    // The client sees the clear in order; nothing of the old epoch follows it.
    let mut old_before_clear = before;
    let mut cleared = false;
    let mut fresh = 0;
    while fresh < 5 {
        match next_frame(&mut ws).await.unwrap() {
            Message::Binary(data) if data[0] == 5 => {
                assert!(!cleared, "old audio after the clear");
                old_before_clear += 1;
            }
            Message::Binary(data) => {
                assert!(cleared, "new audio before the clear message");
                assert_eq!(data[0], 0xE0);
                fresh += 1;
            }
            Message::Text(text) => match ServerMessage::parse(text.as_str()).unwrap() {
                ServerMessage::Clear { epoch } => {
                    assert_eq!(epoch, 1);
                    cleared = true;
                }
                other => panic!("unexpected {other:?} (the cut mark must not be sent)"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        old_before_clear < 15,
        "{old_before_clear} of 30 old frames went out before the clear"
    );
    // The mark was cut: the engine hears MarkCleared, never MarkPlayed.
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

// ---------------------------------------------------------------------------------------------
// Marks and DTMF

#[tokio::test]
async fn mark_round_trip() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    for seq in 0..3u64 {
        session.send(audio_command(&rig, 0, seq, 3)).await.unwrap();
    }
    session
        .send(TransportCommand::Mark {
            id: "m-1".to_owned(),
        })
        .await
        .unwrap();

    // Three audio frames, then the mark (it is sent only after the audio before it was released).
    let mut frames = 0;
    loop {
        match next_frame(&mut ws).await.unwrap() {
            Message::Binary(_) => frames += 1,
            Message::Text(text) => {
                let ServerMessage::Mark { id } = ServerMessage::parse(text.as_str()).unwrap()
                else {
                    panic!("expected a mark");
                };
                assert_eq!(id, "m-1");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(frames, 3);
    send_json(
        &mut ws,
        &ClientMessage::Mark {
            id: "m-1".to_owned(),
        },
    )
    .await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(
        event,
        TransportEvent::MarkPlayed {
            id: "m-1".to_owned()
        }
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn dtmf_reaches_the_engine_in_order() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    for digit in ['5', '#', 'a'] {
        let digit = DtmfDigit::try_from(digit).unwrap();
        send_json(&mut ws, &ClientMessage::Dtmf { digit }).await;
    }
    let mut seen = Vec::new();
    while seen.len() < 3 {
        if let TransportEvent::Dtmf { digit, .. } =
            next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await
        {
            seen.push(digit);
        }
    }
    assert_eq!(seen, [DtmfDigit::D5, DtmfDigit::Pound, DtmfDigit::A]);

    // A key that is not DTMF is a malformed message: error and close.
    send_text(&mut ws, r#"{"type":"dtmf","digit":"x"}"#).await;
    expect_fatal(&mut ws, code::INVALID_MESSAGE).await;
    assert!(matches!(
        stopped(&mut session).await,
        StopReason::PeerDisconnected
    ));
}

// ---------------------------------------------------------------------------------------------
// Malformed input

#[tokio::test]
async fn malformed_json_closes_with_an_error_and_ends_the_call() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    send_text(&mut ws, "{\"type\":").await;
    expect_fatal_with_close(&mut ws, code::INVALID_JSON, CLOSE_PROTOCOL).await;
    // The engine learns of it as a protocol fault, then the call ends.
    let fault = next_event(&mut session, |e| matches!(e, TransportEvent::Error { .. })).await;
    let TransportEvent::Error { message } = fault else {
        unreachable!()
    };
    assert!(message.starts_with(code::INVALID_JSON), "{message}");
    assert!(matches!(
        stopped(&mut session).await,
        StopReason::PeerDisconnected
    ));
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn oversized_and_malformed_frames_are_refused() {
    let config = ServerConfig {
        max_audio_bytes: 4096,
        max_control_bytes: 1024,
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;

    // Audio above the websocket layer's limit, which is the larger of the two limits (here the
    // audio limit, 4096): refused by the websocket layer, and reported all the same.
    let (mut ws, mut session) = rig.pair().await;
    send_audio(&mut ws, vec![0; 4098]).await;
    let message = expect_fatal_closed(&mut ws, code::FRAME_TOO_LARGE, Some(CLOSE_SIZE)).await;
    assert!(message.contains("frame limit"), "{message}");
    stopped(&mut session).await;

    // A control message above the control limit.
    let (mut ws, mut session) = rig.pair().await;
    let long = format!(r#"{{"type":"mark","id":"{}"}}"#, "x".repeat(2000));
    send_text(&mut ws, &long).await;
    expect_fatal_with_close(&mut ws, code::FRAME_TOO_LARGE, CLOSE_SIZE).await;
    stopped(&mut session).await;

    // Half a sample of PCM16.
    let (mut ws, mut session) = rig.pair().await;
    send_audio(&mut ws, vec![0; 641]).await;
    expect_fatal_with_close(&mut ws, code::BAD_AUDIO, CLOSE_POLICY).await;
    stopped(&mut session).await;

    // A second start.
    let (mut ws, mut session) = rig.pair().await;
    send_json(
        &mut ws,
        &ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)),
    )
    .await;
    expect_fatal_with_close(&mut ws, code::UNEXPECTED_START, CLOSE_POLICY).await;
    stopped(&mut session).await;

    // A far larger frame: the connection ends. The error may be lost to a reset here, since most
    // of the frame is still unread (the 4098-byte case above proves the error is sent when the
    // rest of the frame fits the socket buffers).
    let (mut ws, mut session) = rig.pair().await;
    let _ = ws
        .send(Message::Binary(Bytes::from(vec![0u8; 200_000])))
        .await;
    let _ = drain_to_end(&mut ws).await;
    stopped(&mut session).await;
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn unknown_message_types_are_ignored_but_counted() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    send_text(&mut ws, r#"{"type":"rewind","by":5}"#).await;
    let ServerMessage::Error {
        code: reported,
        fatal,
        ..
    } = next_server_message(&mut ws).await
    else {
        panic!("expected an error message");
    };
    assert_eq!(reported, code::UNKNOWN_MESSAGE);
    assert!(!fatal);
    // The call goes on.
    send_json(
        &mut ws,
        &ClientMessage::Dtmf {
            digit: DtmfDigit::D1,
        },
    )
    .await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;

    // A client that keeps doing it is cut off.
    for _ in 0..20 {
        send_text(&mut ws, r#"{"type":"rewind"}"#).await;
    }
    expect_fatal(&mut ws, code::TOO_MANY_ERRORS).await;
    stopped(&mut session).await;
}

#[tokio::test]
async fn a_client_that_floods_audio_is_dropped_oldest_first() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    // The engine reads nothing while the client sends 2000 frames.
    for fill in 0..2000u32 {
        send_audio(&mut ws, pcm((fill % 250) as u8 + 1)).await;
    }
    send_json(
        &mut ws,
        &ClientMessage::Dtmf {
            digit: DtmfDigit::D9,
        },
    )
    .await;
    // The key press is lossless: it is delivered, behind whatever audio survived.
    next_event(&mut session, |e| matches!(e, TransportEvent::Dtmf { .. })).await;
    assert!(
        session.event_stats().dropped_oldest > 0,
        "nothing was dropped: {:?}",
        session.event_stats()
    );
    // And the call is still alive.
    session.send(audio_command(&rig, 0, 0, 42)).await.unwrap();
    match next_frame(&mut ws).await.unwrap() {
        Message::Binary(data) => assert_eq!(data.as_ref(), pcm(42).as_slice()),
        other => panic!("unexpected {other:?}"),
    }
    session.close(WAIT).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Limits, keepalive, slow clients

#[tokio::test]
async fn call_limit_answers_503_and_frees_slots() {
    let config = ServerConfig {
        max_calls: 2,
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (first, mut first_session) = rig.pair().await;
    let (_second, _second_session) = rig.pair().await;
    assert_eq!(rig.server.active_calls(), 2);

    let refused = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(rig.url()))
        .await
        .unwrap();
    match refused {
        Err(WsError::Http(response)) => {
            assert_eq!(response.status().as_u16(), 503);
            assert!(response.headers().contains_key("retry-after"));
        }
        other => panic!("expected an HTTP 503, got {:?}", other.map(|_| ())),
    }

    // Hanging up one call makes room for another.
    drop(first);
    assert!(matches!(
        stopped(&mut first_session).await,
        StopReason::PeerDisconnected
    ));
    wait_for_active(&rig, 1).await;
    let (_third, _) = rig.call().await;
}

#[tokio::test]
async fn silent_client_hits_the_ping_timeout() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (mut ws, mut session) = rig.pair().await;
    // The client does not poll its socket, so it never answers a ping. (Its end of the
    // connection stays open: only the server's timeout can end the call.)
    let reason = stopped(&mut session).await;
    assert_eq!(reason, StopReason::PeerDisconnected);
    // Now the client reads: it finds the error and the close.
    expect_fatal_with_close(&mut ws, code::PING_TIMEOUT, CLOSE_POLICY).await;
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn pings_keep_an_idle_client_alive() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (mut ws, session) = rig.pair().await;
    // Idle for 3x the timeout, but answering pings (reading the socket does that).
    let mut pings = 0;
    let idle = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_))) => pings += 1,
                Some(Ok(_)) => {}
                other => panic!("connection ended: {other:?}"),
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(750), idle).await;
    assert!(pings >= 5, "only {pings} pings in 750 ms");
    // The call is alive: audio still flows.
    session.send(audio_command(&rig, 0, 0, 8)).await.unwrap();
    match next_frame(&mut ws).await.unwrap() {
        Message::Binary(data) => assert_eq!(data[0], 8),
        other => panic!("unexpected {other:?}"),
    }
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_client_that_takes_no_output_is_dropped_without_stalling_input() {
    let config = ServerConfig {
        write_timeout: Duration::from_millis(600),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    // 48 kHz, so one engine frame can be huge.
    let mut start = Start::new(48_000, Encoding::Pcm16);
    start.call_id = Some("slow".to_owned());
    let (mut ws, _) = rig.small_buffer_call(start).await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;

    // 12 MB in one frame: more than the sockets buffer (the client's receive buffer is tiny, so
    // the kernel's autotuning cannot absorb it), and the client reads none of it.
    session
        .send(audio_command_at(
            &rig,
            SampleRate::Hz48000,
            0,
            0,
            vec![1; 12 << 20],
        ))
        .await
        .unwrap();
    // While the writer is stuck on that frame, the engine still hears the client.
    pause(Duration::from_millis(150)).await;
    for _ in 0..3 {
        send_audio(&mut ws, vec![2; 1920]).await;
    }
    for _ in 0..3 {
        let TransportEvent::Audio(frame) =
            next_event(&mut session, |e| matches!(e, TransportEvent::Audio(_))).await
        else {
            unreachable!()
        };
        assert_eq!(frame.data()[0], 2);
    }
    // Past the write timeout the call is closed.
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
    wait_for_active(&rig, 0).await;
}

// ---------------------------------------------------------------------------------------------
// Ending a call

#[tokio::test]
async fn client_stop_and_close_frame_are_hang_ups() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    send_json(&mut ws, &ClientMessage::Stop).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    let seen = drain_to_end(&mut ws).await;
    assert!(
        seen.contains(&ServerMessage::Stop {
            reason: stop::ENDED.to_owned()
        }),
        "{seen:?}"
    );

    let (mut ws, mut session) = rig.pair().await;
    ws.close(None).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    drop(ws);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn engine_stop_tells_the_client() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    session.send(TransportCommand::Stop).await.unwrap();
    assert_eq!(stopped(&mut session).await, StopReason::Requested);
    let (seen, close) = drain_with_close(&mut ws).await;
    assert!(
        seen.contains(&ServerMessage::Stop {
            reason: stop::ENDED.to_owned()
        }),
        "{seen:?}"
    );
    assert_eq!(close, Some(CLOSE_NORMAL));
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn shutdown_says_goodbye_to_every_call() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, _session) = rig.pair().await;
    rig.server.shutdown(WAIT).await.unwrap();
    let (seen, close) = drain_with_close(&mut ws).await;
    assert!(
        seen.contains(&ServerMessage::Stop {
            reason: stop::SHUTDOWN.to_owned()
        }),
        "{seen:?}"
    );
    assert_eq!(close, Some(CLOSE_AWAY));
    assert_eq!(rig.server.active_calls(), 0);
    // Nobody is accepting any more: the connection is refused (or, on a platform that retries
    // SYNs for seconds, does not complete).
    let attempt = tokio::time::timeout(
        Duration::from_millis(300),
        tokio_tungstenite::connect_async(rig.url()),
    )
    .await;
    assert!(!matches!(attempt, Ok(Ok(_))));
}

#[tokio::test]
async fn a_dropped_connection_ends_the_call_and_leaks_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let mut rig = Rig::start(fast_config()).await;
        // Abrupt: the socket is dropped without a close frame.
        let (ws, mut session) = rig.pair().await;
        drop(ws);
        assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
        session.close(WAIT).await.unwrap();
        wait_for_active(&rig, 0).await;

        // Polite: stop, then close.
        let (mut ws, mut session) = rig.pair().await;
        send_json(&mut ws, &ClientMessage::Stop).await;
        assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
        drain_to_end(&mut ws).await;
        session.close(WAIT).await.unwrap();

        // Several calls at once, some cut off in the middle of the handshake or the start.
        let mut open = Vec::new();
        for _ in 0..3 {
            open.push(rig.pair().await);
        }
        let _half_open = rig.connect().await;
        let _raw = TcpStream::connect(rig.server.local_addr()).await.unwrap();
        drop(open);
        rig.server.shutdown(WAIT).await.unwrap();
    }
    counter
        .wait_for_baseline(&*clock, WAIT)
        .await
        .expect("tasks outlived the server and its calls");
}

// ---------------------------------------------------------------------------------------------
// The reference client

/// An engine that loops the caller's audio back, answers a key press with a mark, and ends when
/// the call does. Returns the number of audio frames it echoed and the events it saw.
async fn echo_engine(mut rig: Rig) -> (usize, Vec<TransportEvent>) {
    let mut session = rig.accept().await;
    let mut echoed = 0usize;
    let mut seen = Vec::new();
    while let Some(event) = session.recv().await {
        match &event {
            TransportEvent::Audio(frame) => {
                session
                    .send(TransportCommand::Audio {
                        epoch: frame.epoch(),
                        utterance: UtteranceId::new(1),
                        frame: frame.clone(),
                    })
                    .await
                    .unwrap();
                echoed += 1;
            }
            TransportEvent::Dtmf { digit, .. } => {
                session
                    .send(TransportCommand::Mark {
                        id: format!("key-{}", digit.as_char()),
                    })
                    .await
                    .unwrap();
            }
            _ => {}
        }
        let end = matches!(event, TransportEvent::Stopped { .. });
        if !matches!(
            event,
            TransportEvent::PlaybackPosition { .. } | TransportEvent::Audio(_)
        ) {
            seen.push(event);
        }
        if end {
            break;
        }
    }
    (echoed, seen)
}

/// A ramp the byte-for-byte comparison can tell from silence and from shifted audio.
fn ramp(samples: usize) -> Vec<u8> {
    (0..samples)
        .flat_map(|i| {
            let value = i16::try_from((i * 37) % 20_000).unwrap() - 10_000;
            value.to_le_bytes()
        })
        .collect()
}

fn client_config(rig: &Rig) -> ClientConfig {
    let mut config = ClientConfig::new(rig.url());
    config.linger = Duration::from_millis(400);
    config
}

#[tokio::test]
async fn reference_client_records_the_echoed_audio() {
    let rig = Rig::start(fast_config()).await;
    let mut config = client_config(&rig);
    config.dtmf = "7".to_owned();
    let clock = rig.clock.clone();
    let input = ramp(16_000 / 2); // 0.5 s: 25 frames of 20 ms
    let engine = tokio::spawn(echo_engine(rig));
    // The engine task owns the rig (and with it the server); the URL was taken before.
    let report = client::run(clock, &config, &input, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.frames_sent, 25);
    assert_eq!(report.audio, input, "the recording is not the input");
    assert_eq!(report.audio_frames_received, 25);
    assert_eq!(report.dtmf_sent, 1);
    assert_eq!(report.marks_received, ["key-7"]);
    assert_eq!(report.marks_echoed, ["key-7"]);
    assert_eq!(report.stop_reason.as_deref(), Some(stop::ENDED));
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    // The recording round-trips through the WAV writer and reader.
    let file = wav::encode(16_000, &report.audio);
    let parsed = wav::parse(&file).unwrap();
    assert_eq!(parsed.sample_rate, 16_000);
    assert_eq!(parsed.pcm.as_ref(), input.as_slice());

    let (echoed, seen) = tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
    assert_eq!(echoed, 25);
    assert!(
        seen.contains(&TransportEvent::MarkPlayed {
            id: "key-7".to_owned()
        }),
        "{seen:?}"
    );
    assert!(seen.contains(&TransportEvent::Stopped {
        reason: StopReason::PeerHangup
    }));
}

#[tokio::test]
async fn reference_client_streams_a_repository_fixture() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/en_two_utterances.wav"
    );
    let fixture = wav::parse(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(fixture.sample_rate, 16_000);
    // The first 0.8 s, sent as fast as the server takes it (the echo still plays out in real time).
    let input = fixture.pcm.slice(..16_000 * 2 * 8 / 10);

    let rig = Rig::start(fast_config()).await;
    let mut config = client_config(&rig);
    config.realtime = false;
    let clock = rig.clock.clone();
    let engine = tokio::spawn(echo_engine(rig));
    let report = client::run(clock, &config, &input, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.frames_sent, 40);
    assert_eq!(report.audio.len(), input.len());
    assert_eq!(report.audio, input.as_ref());
    tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
}

#[tokio::test]
async fn reference_client_works_with_mulaw() {
    let rig = Rig::start(fast_config()).await;
    let mut config = client_config(&rig);
    config.sample_rate = 8_000;
    config.encoding = Encoding::Mulaw;
    let clock = rig.clock.clone();
    let input = ramp(8_000 / 4); // 0.25 s
    let engine = tokio::spawn(echo_engine(rig));
    let report = client::run(clock, &config, &input, &CancellationToken::new())
        .await
        .unwrap();
    // G.711 is lossy once, then stable: the echo equals the input after one encode/decode.
    let mut encoded = vec![0u8; input.len() / 2];
    Codec::Ulaw.encode(&input, &mut encoded).unwrap();
    let mut expected = vec![0u8; input.len()];
    Codec::Ulaw.decode(&encoded, &mut expected).unwrap();
    assert_eq!(report.audio, expected);
    tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
}

#[tokio::test]
async fn reference_client_throws_away_cleared_audio() {
    let mut rig = Rig::start(fast_config()).await;
    let mut config = client_config(&rig);
    config.linger = Duration::from_millis(800);
    let clock = rig.clock.clone();
    let url_config = config.clone();
    let engine = tokio::spawn(async move {
        let mut session = rig.accept().await;
        next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
        // 2 s of audio, interrupted after about 200 ms, then 5 frames of the new epoch.
        for seq in 0..100u64 {
            session.send(audio_command(&rig, 0, seq, 1)).await.unwrap();
        }
        pause(Duration::from_millis(200)).await;
        session
            .send(TransportCommand::Clear {
                epoch: TurnEpoch::new(1),
            })
            .await
            .unwrap();
        for seq in 0..5u64 {
            session.send(audio_command(&rig, 1, seq, 2)).await.unwrap();
        }
        stopped(&mut session).await
    });
    let report = client::run(clock, &url_config, &ramp(1600), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.clears, [1]);
    let old = report.audio.iter().filter(|b| **b == 1).count();
    let new = report.audio.iter().filter(|b| **b == 2).count();
    assert_eq!(new, 5 * 640, "the new epoch's audio is complete");
    assert!(old > 0 && old < 40 * 640, "{old} bytes of the old epoch");
    assert_eq!(
        tokio::time::timeout(WAIT, engine).await.unwrap().unwrap(),
        StopReason::PeerHangup
    );
}

#[tokio::test]
async fn reference_client_reports_a_refused_start() {
    let rig = Rig::start(fast_config()).await;
    let mut config = client_config(&rig);
    config.sample_rate = 12_345;
    let error = client::run(
        rig.clock.clone(),
        &config,
        &[0; 64],
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, brisvox_transport::Error::Remote { code, .. } if code == code::UNSUPPORTED_SAMPLE_RATE),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ws_client_binary_streams_a_wav_and_records_the_echo() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/en_two_utterances.wav"
    );
    let fixture = wav::parse(&std::fs::read(path).unwrap()).unwrap();
    let input = fixture.pcm.slice(..16_000 * 2 / 2); // 0.5 s
    let dir = std::env::temp_dir().join(format!("brisvox-ws-client-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input_path = dir.join("in.wav");
    let output_path = dir.join("out.wav");
    std::fs::write(&input_path, wav::encode(16_000, &input)).unwrap();

    let rig = Rig::start(fast_config()).await;
    let url = rig.url();
    let clock = rig.clock.clone();
    let engine = tokio::spawn(echo_engine(rig));
    let began = clock.now();
    let run = {
        let (input_path, output_path) = (input_path.clone(), output_path.clone());
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_ws_client"))
                .arg(url)
                .arg(input_path)
                .arg(output_path)
                .args(["--linger-ms", "400"])
                .output()
                .unwrap()
        })
    };
    let output = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    let took = clock.now() - began;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 0.5 s of input in real time plus the 400 ms linger; the default linger (2 s) would make
    // this 2.5 s, so a binary that ignored --linger-ms fails here.
    assert!(took < Duration::from_millis(1800), "took {took:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sent 25 frames"), "{stdout}");
    let recorded = wav::parse(&std::fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(recorded.sample_rate, 16_000);
    assert_eq!(recorded.pcm, input);
    tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    // Bad usage is reported, not panicked on.
    let usage = std::process::Command::new(env!("CARGO_BIN_EXE_ws_client"))
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
}

// ---------------------------------------------------------------------------------------------
// Review fixes: closing, bounds, configuration, failures of the reference client

/// Reads to the end of the connection and fails the test on a reset: a clean end is the peer's
/// close frame or an orderly end of the stream. Returns the text messages seen.
async fn drain_cleanly(ws: &mut Client) -> Vec<ServerMessage> {
    let mut seen = Vec::new();
    let end = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    seen.push(ServerMessage::parse(text.as_str()).unwrap());
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("the close was not clean: {error:?} (saw {seen:?})"),
                None => break,
            }
        }
    };
    tokio::time::timeout(WAIT, end)
        .await
        .expect("the connection did not end");
    seen
}

#[tokio::test]
async fn a_close_frame_from_the_client_is_answered_with_a_close_frame() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    ws.close(None).await.unwrap();
    // The server answers the close (RFC 6455 5.5.1): no reset.
    drain_cleanly(&mut ws).await;
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn stop_and_close_back_to_back_still_get_the_stop_and_a_clean_close() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    // What a browser does: ws.send(stop); ws.close().
    send_json(&mut ws, &ClientMessage::Stop).await;
    ws.close(None).await.unwrap();
    let seen = drain_cleanly(&mut ws).await;
    assert!(
        seen.contains(&ServerMessage::Stop {
            reason: stop::ENDED.to_owned()
        }),
        "{seen:?}"
    );
    assert_eq!(stopped(&mut session).await, StopReason::PeerHangup);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn a_fatal_error_ends_in_a_clean_close() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    send_text(&mut ws, "not json").await;
    let seen = drain_cleanly(&mut ws).await;
    assert!(
        seen.iter().any(|m| matches!(
            m,
            ServerMessage::Error { code, fatal: true, .. } if code == code::INVALID_JSON
        )),
        "{seen:?}"
    );
    stopped(&mut session).await;
}

#[tokio::test]
async fn invalid_server_configurations_are_refused() {
    let invalid = |change: fn(&mut ServerConfig)| {
        let mut config = fast_config();
        change(&mut config);
        config
    };
    let cases = [
        invalid(|c| c.ping_timeout = c.ping_interval),
        invalid(|c| c.ping_timeout = Duration::from_millis(100)),
        invalid(|c| c.ping_interval = Duration::ZERO),
        invalid(|c| c.write_timeout = Duration::ZERO),
        invalid(|c| c.handshake_timeout = Duration::ZERO),
        invalid(|c| c.start_timeout = Duration::ZERO),
        invalid(|c| c.send_timeout = Duration::ZERO),
        invalid(|c| c.close_grace = Duration::ZERO),
        invalid(|c| c.ping_timeout = Duration::ZERO),
        invalid(|c| c.max_calls = 0),
        invalid(|c| c.accept_queue = 0),
        // Past the cap: refused, not a panic in the semaphore or the channel.
        invalid(|c| c.max_calls = brisvox_transport::websocket::MAX_CALLS + 1),
        invalid(|c| c.max_calls = usize::MAX),
        invalid(|c| c.accept_queue = usize::MAX),
        invalid(|c| c.max_audio_bytes = 0),
        invalid(|c| c.max_audio_bytes = 1),
        invalid(|c| c.max_control_bytes = 0),
        invalid(|c| c.max_control_bytes = 100),
    ];
    for config in cases {
        assert!(config.validate().is_err(), "{config:?}");
        let refused =
            WebSocketServer::bind(config, SystemClock::shared(), &CancellationToken::new())
                .await
                .map(|_| ())
                .unwrap_err();
        assert!(
            matches!(refused, brisvox_transport::Error::Config(_)),
            "{refused:?}"
        );
    }
    fast_config().validate().unwrap();
    ServerConfig::default().validate().unwrap();
    ServerConfig {
        max_calls: brisvox_transport::websocket::MAX_CALLS,
        ..fast_config()
    }
    .validate()
    .unwrap();
}

#[tokio::test]
async fn audio_above_the_audio_limit_is_refused_by_the_connection_not_the_socket_layer() {
    // The websocket layer's limit is the larger of the two (8192): a 2000-byte binary frame passes
    // it and is refused by the audio limit (1024) of the connection itself.
    let config = ServerConfig {
        max_audio_bytes: 1024,
        max_control_bytes: 8192,
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (mut ws, mut session) = rig.pair().await;
    send_audio(&mut ws, vec![0; 2000]).await;
    let message = expect_fatal_closed(&mut ws, code::FRAME_TOO_LARGE, Some(CLOSE_SIZE)).await;
    assert!(message.contains("audio frame exceeds 1024"), "{message}");
    stopped(&mut session).await;
}

#[tokio::test]
async fn an_oversized_start_is_refused_over_the_socket() {
    let mut rig = Rig::start(fast_config()).await;
    let mut start = Start::new(16_000, Encoding::Pcm16);
    start.call_id = Some("x".repeat(200));
    let mut ws = rig.connect().await;
    send_json(&mut ws, &ClientMessage::Start(start)).await;
    expect_fatal(&mut ws, code::INVALID_MESSAGE).await;

    let mut start = Start::new(16_000, Encoding::Pcm16);
    start.metadata.insert("k".to_owned(), "v".repeat(300));
    let mut ws = rig.connect().await;
    send_json(&mut ws, &ClientMessage::Start(start)).await;
    expect_fatal(&mut ws, code::INVALID_MESSAGE).await;
    wait_for_active(&rig, 0).await;
    // The refused starts never reached the engine.
    let cancel = CancellationToken::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rig.server.accept(&cancel))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_mark_id_the_client_could_not_echo_is_not_sent() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, mut session) = rig.pair().await;
    session
        .send(TransportCommand::Mark {
            id: "x".repeat(300),
        })
        .await
        .unwrap();
    let ServerMessage::Error {
        code: reported,
        fatal,
        ..
    } = next_server_message(&mut ws).await
    else {
        panic!("expected an error");
    };
    assert_eq!(reported, code::MARK_ID_TOO_LONG);
    assert!(!fatal);

    // The longest allowed id goes round trip, and the call lives on.
    let longest = "y".repeat(256);
    session
        .send(TransportCommand::Mark {
            id: longest.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut ws).await,
        ServerMessage::Mark {
            id: longest.clone()
        }
    );
    send_json(
        &mut ws,
        &ClientMessage::Mark {
            id: longest.clone(),
        },
    )
    .await;
    let event = next_event(&mut session, |e| {
        matches!(e, TransportEvent::MarkPlayed { .. })
    })
    .await;
    assert_eq!(event, TransportEvent::MarkPlayed { id: longest });
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn output_at_another_rate_stays_dropped_for_seconds_and_the_call_lives_on() {
    let mut rig = Rig::start(fast_config()).await;
    let (mut ws, session) = rig.pair().await;
    // Two seconds of 24 kHz TTS on a 16 kHz call, then audio that matches.
    for seq in 0..100 {
        session
            .send(audio_command_at(
                &rig,
                SampleRate::Hz24000,
                0,
                seq,
                vec![9; 960],
            ))
            .await
            .unwrap();
    }
    session.send(audio_command(&rig, 0, 100, 77)).await.unwrap();
    let mut notices = 0;
    loop {
        match next_frame(&mut ws).await.unwrap() {
            Message::Text(text) => match ServerMessage::parse(text.as_str()).unwrap() {
                ServerMessage::Error {
                    code, fatal: false, ..
                } if code == code::OUTPUT_RATE_MISMATCH => notices += 1,
                other => panic!("unexpected {other:?}"),
            },
            Message::Binary(data) => {
                assert_eq!(data.as_ref(), pcm(77).as_slice(), "mismatched audio leaked");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        notices, 1,
        "the notice is once per call, not once per frame"
    );
    session.close(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_full_accept_queue_refuses_new_calls_with_server_busy() {
    let config = ServerConfig {
        accept_queue: 1,
        send_timeout: Duration::from_millis(300),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    // The first call sits in the accept queue: nobody is accepting.
    let (_first, _) = rig.call().await;
    let mut second = rig.connect().await;
    send_json(
        &mut second,
        &ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)),
    )
    .await;
    expect_fatal_with_close(&mut second, code::SERVER_BUSY, CLOSE_AGAIN).await;
    wait_for_active(&rig, 1).await;
    // The first call is still there for the engine.
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
}

#[tokio::test]
async fn an_engine_that_reads_no_events_gets_the_call_closed_as_overloaded() {
    let config = ServerConfig {
        send_timeout: Duration::from_millis(300),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (mut ws, _) = rig.call().await;
    // The link is taken but never opened, so nothing reads its events (the queue holds 64).
    let cancel = CancellationToken::new();
    let link = rig.server.accept(&cancel).await.unwrap().unwrap();
    for _ in 0..80 {
        send_json(
            &mut ws,
            &ClientMessage::Dtmf {
                digit: DtmfDigit::D1,
            },
        )
        .await;
    }
    expect_fatal_with_close(&mut ws, code::OVERLOADED, CLOSE_AGAIN).await;
    drop(link);
    wait_for_active(&rig, 0).await;
}

#[tokio::test]
async fn over_limit_connections_beyond_the_reject_slots_are_dropped() {
    let config = ServerConfig {
        max_calls: 1,
        handshake_timeout: Duration::from_secs(10),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (_ws, _session) = rig.pair().await;
    let mut raw = Vec::new();
    for _ in 0..40 {
        raw.push(TcpStream::connect(rig.server.local_addr()).await.unwrap());
    }
    pause(Duration::from_millis(300)).await;
    // 32 of them hold a reject slot and wait for an HTTP request; the other 8 were dropped.
    let mut dropped = 0;
    for stream in &mut raw {
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(
            Duration::from_millis(30),
            tokio::io::AsyncReadExt::read(stream, &mut buf),
        )
        .await;
        match read {
            Ok(Ok(0) | Err(_)) => dropped += 1,
            Ok(Ok(_)) => panic!("unexpected data"),
            Err(_) => {}
        }
    }
    assert_eq!(dropped, 8);
}

/// A stuck client: a call at 48 kHz on a socket with a tiny receive buffer, and an engine that
/// has sent one huge frame, so the server's writer is blocked in the socket.
async fn stuck_call(rig: &mut Rig) -> (Client, TransportSession) {
    let (ws, _) = rig
        .small_buffer_call(Start::new(48_000, Encoding::Pcm16))
        .await;
    let mut session = rig.accept().await;
    next_event(&mut session, |e| matches!(e, TransportEvent::Started(_))).await;
    session
        .send(audio_command_at(
            rig,
            SampleRate::Hz48000,
            0,
            0,
            vec![1; 12 << 20],
        ))
        .await
        .unwrap();
    // The writer is now blocked on the frame.
    pause(Duration::from_millis(300)).await;
    (ws, session)
}

#[tokio::test]
async fn an_engine_hangup_does_not_wait_for_a_client_that_stopped_reading() {
    let config = ServerConfig {
        write_timeout: Duration::from_secs(4),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (ws, session) = stuck_call(&mut rig).await;
    let began = rig.clock.now();
    session.send(TransportCommand::Stop).await.unwrap();
    wait_for_active(&rig, 0).await;
    let took = rig.clock.now() - began;
    assert!(
        took < Duration::from_millis(2500),
        "the permit was held for {took:?} (write timeout 4 s)"
    );
    drop(ws);
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_client_that_stopped_reading() {
    let config = ServerConfig {
        write_timeout: Duration::from_secs(4),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (ws, _session) = stuck_call(&mut rig).await;
    rig.server
        .shutdown(Duration::from_millis(2500))
        .await
        .expect("shutdown waited for the stuck client");
    assert_eq!(rig.server.active_calls(), 0);
    drop(ws);
}

#[tokio::test]
async fn every_way_a_connection_can_end_leaks_no_task() {
    let clock = SystemClock::shared();
    let counter = TaskCounter::start_settled(&*clock, Duration::from_millis(50))
        .await
        .unwrap();
    {
        let config = ServerConfig {
            max_calls: 3,
            write_timeout: Duration::from_millis(600),
            ..fast_config()
        };
        let mut rig = Rig::start(config).await;

        // A fatal protocol error.
        let (mut ws, mut session) = rig.pair().await;
        send_text(&mut ws, "nope").await;
        expect_fatal(&mut ws, code::INVALID_JSON).await;
        stopped(&mut session).await;
        session.close(WAIT).await.unwrap();

        // A client that stops reading, until the write timeout drops it.
        let (ws, mut session) = stuck_call(&mut rig).await;
        assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
        session.close(WAIT).await.unwrap();
        drop(ws);

        // The engine hanging up while the client is stuck.
        let (ws, session) = stuck_call(&mut rig).await;
        session.send(TransportCommand::Stop).await.unwrap();
        wait_for_active(&rig, 0).await;
        drop((ws, session));

        // Over-limit connections: answered with 503, and held half-open.
        let mut calls = Vec::new();
        for _ in 0..3 {
            calls.push(rig.pair().await);
        }
        let refused = tokio_tungstenite::connect_async(rig.url()).await;
        assert!(matches!(refused, Err(WsError::Http(_))));
        let _half_open = TcpStream::connect(rig.server.local_addr()).await.unwrap();
        drop(calls);
        rig.server.shutdown(WAIT).await.unwrap();
    }
    counter
        .wait_for_baseline(&*clock, WAIT)
        .await
        .expect("tasks outlived the server and its calls");
}

// ---------------------------------------------------------------------------------------------
// The reference client against a scripted server

type MockWs = WebSocketStream<TcpStream>;

/// A scripted websocket server for one connection: reads `start`, answers `started` (16 kHz
/// PCM16) and runs `script`. Returns its URL and the script's result.
async fn mock_server<T, F, Fut>(script: F) -> (String, tokio::task::JoinHandle<T>)
where
    T: Send + 'static,
    F: FnOnce(MockWs) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/mock", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let first = ws.next().await.unwrap().unwrap();
        assert!(matches!(first, Message::Text(_)), "{first:?}");
        let started = ServerMessage::Started {
            version: 1,
            call_id: "mock-1".to_owned(),
            sample_rate: 16_000,
            encoding: Encoding::Pcm16,
        };
        ws.send(Message::text(started.to_json())).await.unwrap();
        script(ws).await
    });
    (url, task)
}

/// Reads binary frames from the client until `count` arrived.
async fn read_frames(ws: &mut MockWs, count: usize) {
    let mut seen = 0;
    while seen < count {
        if let Some(Ok(Message::Binary(_))) = ws.next().await {
            seen += 1;
        }
    }
}

#[tokio::test]
async fn reference_client_reports_a_connection_that_breaks_mid_stream_as_a_failure() {
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 3).await;
        for _ in 0..2 {
            ws.send(Message::Binary(Bytes::from(vec![7u8; 640])))
                .await
                .unwrap();
        }
        // Let the client take the audio, then vanish without a close frame.
        pause(Duration::from_millis(200)).await;
        drop(ws);
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.linger = Duration::from_millis(400);
    config.io_timeout = Duration::from_secs(1);
    let input = ramp(16_000); // 1 s: 50 frames
    let report = client::run(
        SystemClock::shared(),
        &config,
        &input,
        &CancellationToken::new(),
    )
    .await
    .expect("a break after the call started is a report, not an error");
    assert!(report.server_closed);
    assert!(!report.input_complete);
    assert!(report.transport_error.is_some(), "{report:?}");
    assert_eq!(report.audio, vec![7u8; 1280], "the recording is kept");
    let failure = report.failure().expect("not a success");
    assert!(failure.contains("connection broke"), "{failure}");
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn reference_client_reports_a_fatal_error_that_arrives_mid_stream() {
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 3).await;
        ws.send(Message::Binary(Bytes::from(vec![5u8; 640])))
            .await
            .unwrap();
        let error = ServerMessage::error(code::FRAME_TOO_LARGE, "too big", true);
        ws.send(Message::text(error.to_json())).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
        // Keep reading what the client still sends, until it closes.
        let drain = async { while let Some(Ok(_)) = ws.next().await {} };
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.io_timeout = Duration::from_secs(1);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(16_000),
        &CancellationToken::new(),
    )
    .await
    .expect("a fatal error after the call started is a report, not an error");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.fatal && e.code == code::FRAME_TOO_LARGE),
        "{:?}",
        report.errors
    );
    assert_eq!(report.audio, vec![5u8; 640]);
    let failure = report.failure().expect("not a success");
    assert!(failure.contains(code::FRAME_TOO_LARGE), "{failure}");
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn reference_client_reports_hitting_the_maximum_duration() {
    let (url, server) = mock_server(|mut ws| async move {
        // Silent: reads until the client leaves.
        let drain = async { while let Some(Ok(_)) = ws.next().await {} };
        let _ = tokio::time::timeout(Duration::from_secs(3), drain).await;
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.max_duration = Duration::from_millis(300);
    config.io_timeout = Duration::from_millis(300);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(32_000),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.timed_out);
    assert!(!report.input_complete);
    assert!(report.failure().unwrap().contains("maximum duration"));
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_server_that_hangs_up_with_a_stop_is_not_a_failure() {
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 2).await;
        let stop = ServerMessage::Stop {
            reason: stop::ENDED.to_owned(),
        };
        ws.send(Message::text(stop.to_json())).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
        let drain = async { while let Some(Ok(_)) = ws.next().await {} };
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    })
    .await;
    let config = ClientConfig::new(url);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(16_000),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!report.input_complete, "the bot hung up mid-input");
    assert_eq!(report.stop_reason.as_deref(), Some(stop::ENDED));
    assert_eq!(report.failure(), None);
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

/// A script that bursts one second of audio (ten 100 ms frames) at the client at once, then a
/// mark, and reports how long the client took to echo it. `cut_after` sends a `clear` that long
/// after the mark (the echo must then never come). Ends the call when the client leaves.
async fn burst_and_mark(
    mut ws: MockWs,
    cut_after: Option<Duration>,
) -> (Option<Duration>, Vec<String>) {
    let clock = SystemClock::shared();
    for _ in 0..10 {
        ws.send(Message::Binary(Bytes::from(vec![3u8; 3200])))
            .await
            .unwrap();
    }
    let mark = ServerMessage::Mark {
        id: "m1".to_owned(),
    };
    ws.send(Message::text(mark.to_json())).await.unwrap();
    let marked_at = clock.now();
    if let Some(after) = cut_after {
        pause(after).await;
        let clear = ServerMessage::Clear { epoch: 1 };
        ws.send(Message::text(clear.to_json())).await.unwrap();
    }
    let mut echoed_after = None;
    let mut echoes = Vec::new();
    let read = async {
        while let Some(Ok(message)) = ws.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            match ClientMessage::parse(text.as_str()).unwrap() {
                ClientMessage::Mark { id } => {
                    echoed_after.get_or_insert(clock.now() - marked_at);
                    echoes.push(id);
                    let stop = ServerMessage::Stop {
                        reason: stop::ENDED.to_owned(),
                    };
                    ws.send(Message::text(stop.to_json())).await.unwrap();
                    ws.send(Message::Close(None)).await.unwrap();
                }
                ClientMessage::Stop => {
                    let stop = ServerMessage::Stop {
                        reason: stop::ENDED.to_owned(),
                    };
                    let _ = ws.send(Message::text(stop.to_json())).await;
                    let _ = ws.send(Message::Close(None)).await;
                }
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(4), read).await;
    (echoed_after, echoes)
}

#[tokio::test]
async fn reference_client_echoes_a_mark_only_when_the_audio_before_it_has_played() {
    let (url, server) = mock_server(|ws| burst_and_mark(ws, None)).await;
    let mut config = ClientConfig::new(url);
    // The linger (400 ms) is shorter than the second of audio still playing: the echo must
    // outlast it.
    config.linger = Duration::from_millis(400);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(16_000 / 12), // five frames, sent in 100 ms
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let (echoed_after, echoes) = tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    let echoed_after = echoed_after.expect("the mark was never echoed");
    assert_eq!(echoes, ["m1"]);
    assert_eq!(report.marks_echoed, ["m1"]);
    assert!(
        echoed_after >= Duration::from_millis(900),
        "echoed after {echoed_after:?}, but a second of audio was playing"
    );
    assert!(
        echoed_after < Duration::from_millis(2500),
        "echoed after {echoed_after:?}"
    );
    assert_eq!(report.audio.len(), 32_000);
}

#[tokio::test]
async fn reference_client_never_echoes_a_mark_that_a_clear_cut() {
    let (url, server) =
        mock_server(|ws| burst_and_mark(ws, Some(Duration::from_millis(100)))).await;
    let mut config = ClientConfig::new(url);
    config.linger = Duration::from_millis(400);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(16_000 / 12),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let (echoed_after, echoes) = tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    assert_eq!(echoed_after, None, "{echoes:?}");
    assert!(report.marks_echoed.is_empty());
    assert_eq!(report.clears, [1]);
    assert!(
        report.audio.len() < 16_000,
        "{} bytes were kept; most of the second was cut",
        report.audio.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ws_client_binary_fails_with_exit_code_1_when_the_connection_breaks() {
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 3).await;
        ws.send(Message::Binary(Bytes::from(vec![7u8; 640])))
            .await
            .unwrap();
        pause(Duration::from_millis(200)).await;
        drop(ws);
    })
    .await;
    let dir = std::env::temp_dir().join(format!("brisvox-ws-client-fail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input_path = dir.join("in.wav");
    let output_path = dir.join("out.wav");
    std::fs::write(&input_path, wav::encode(16_000, &ramp(16_000))).unwrap();
    let run = {
        let (input_path, output_path) = (input_path.clone(), output_path.clone());
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_ws_client"))
                .arg(url)
                .arg(input_path)
                .arg(output_path)
                .args(["--linger-ms", "400"])
                .output()
                .unwrap()
        })
    };
    let output = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("the call failed"), "{stderr}");
    // What was heard before the failure is still written.
    let recorded = wav::parse(&std::fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(recorded.pcm.as_ref(), vec![7u8; 640].as_slice());
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Review fixes (round 2): disconnects, liveness, the injected clock, the reference client's
// failure rules and the binary's options

#[tokio::test]
async fn an_abrupt_disconnect_is_a_peer_disconnect_not_a_protocol_fault() {
    let mut rig = Rig::start(fast_config()).await;
    let (ws, mut session) = rig.pair().await;
    // The socket vanishes without a close frame: the reader sees EOF / a reset without a closing
    // handshake, which is not the client's protocol violation.
    drop(ws);
    let mut errors = Vec::new();
    let reason = loop {
        let event = tokio::time::timeout(WAIT, session.recv())
            .await
            .expect("no event within the wait")
            .expect("session ended without Stopped");
        match event {
            TransportEvent::Stopped { reason } => break reason,
            TransportEvent::Error { message } => errors.push(message),
            _ => {}
        }
    };
    assert_eq!(reason, StopReason::PeerDisconnected);
    assert!(errors.is_empty(), "spurious engine errors: {errors:?}");
    wait_for_active(&rig, 0).await;

    // The same before `start`: nothing is refused, nothing reaches the engine.
    let ws = rig.connect().await;
    drop(ws);
    wait_for_active(&rig, 0).await;
    let cancel = CancellationToken::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rig.server.accept(&cancel))
            .await
            .is_err(),
        "a call that never started reached the engine"
    );
}

#[tokio::test]
async fn any_frame_from_the_client_counts_as_life() {
    let config = ServerConfig {
        ping_interval: Duration::from_millis(40),
        ping_timeout: Duration::from_millis(250),
        ..fast_config()
    };
    let mut rig = Rig::start(config).await;
    let (mut ws, mut session) = rig.pair().await;
    // A telephony gateway shape: the client streams audio but never reads its socket, so it
    // answers no ping with a pong. Its audio alone keeps the call alive for 3x the timeout.
    for _ in 0..15 {
        send_audio(&mut ws, pcm(3)).await;
        pause(Duration::from_millis(50)).await;
    }
    assert_eq!(rig.server.active_calls(), 1, "the call was dropped");
    let mut heard = 0;
    while heard < 15 {
        let event = next_event(&mut session, |e| {
            matches!(e, TransportEvent::Audio(_) | TransportEvent::Stopped { .. })
        })
        .await;
        match event {
            TransportEvent::Audio(_) => heard += 1,
            other => panic!("the call ended while the client was talking: {other:?}"),
        }
    }
    // Text frames count too (a key press), then silence ends the call.
    send_json(
        &mut ws,
        &ClientMessage::Dtmf {
            digit: DtmfDigit::D5,
        },
    )
    .await;
    pause(Duration::from_millis(150)).await;
    assert_eq!(rig.server.active_calls(), 1);
    assert_eq!(stopped(&mut session).await, StopReason::PeerDisconnected);
}

/// A task that keeps the paused-time runtime busy: tokio's paused clock only jumps ahead when
/// every task is idle, and real socket I/O makes tasks idle between syscalls. With this running
/// the simulated time moves only through `tokio::time::advance`, however long the test takes in
/// real time.
fn keep_time_frozen() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
}

/// Lets `real` of wall-clock time pass (the runtime keeps polling sockets meanwhile).
#[allow(clippy::disallowed_methods)] // measuring real time is the point
async fn let_real_time_pass(real: Duration) {
    let began = std::time::Instant::now();
    while began.elapsed() < real {
        tokio::task::yield_now().await;
    }
}

/// A server on a simulated clock (paused tokio time).
async fn sim_server(config: ServerConfig) -> (WebSocketServer, CancellationToken) {
    let clock: SharedClock = std::sync::Arc::new(brisvox_core::SimClock::new());
    let parent = CancellationToken::new();
    let server = WebSocketServer::bind(config, clock, &parent).await.unwrap();
    (server, parent)
}

/// Reads server frames, spinning on the simulated runtime, until the connection ends or `real`
/// of wall-clock time passed. Returns the text messages and the close code.
#[allow(clippy::disallowed_methods)] // measuring real time is the point
async fn read_until_closed(ws: &mut Client, real: Duration) -> (Vec<ServerMessage>, Option<u16>) {
    let began = std::time::Instant::now();
    let mut seen = Vec::new();
    while began.elapsed() < real {
        match futures::FutureExt::now_or_never(ws.next()) {
            None => tokio::task::yield_now().await,
            Some(Some(Ok(Message::Text(text)))) => {
                seen.push(ServerMessage::parse(text.as_str()).unwrap());
            }
            Some(Some(Ok(Message::Close(frame)))) => {
                return (seen, frame.map(|f| u16::from(f.code)));
            }
            Some(Some(Ok(_))) => {}
            Some(Some(Err(_)) | None) => return (seen, None),
        }
    }
    panic!("the connection stayed open for {real:?} (saw {seen:?})");
}

#[tokio::test(start_paused = true)]
async fn start_timeout_fires_on_simulated_time_only() {
    let (server, parent) = sim_server(ServerConfig {
        start_timeout: Duration::from_millis(200),
        ..fast_config()
    })
    .await;
    let busy = keep_time_frozen();
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/call", server.local_addr()))
            .await
            .unwrap();
    // Three times the timeout passes in real time; on the simulated clock no time passed.
    let_real_time_pass(Duration::from_millis(600)).await;
    assert!(
        futures::FutureExt::now_or_never(ws.next()).is_none(),
        "the start timeout fired without simulated time passing"
    );
    assert_eq!(server.active_calls(), 1);

    tokio::time::advance(Duration::from_millis(250)).await;
    let (seen, close) = read_until_closed(&mut ws, Duration::from_secs(3)).await;
    assert!(
        seen.iter().any(|m| matches!(
            m,
            ServerMessage::Error { code, fatal: true, .. } if code == code::START_TIMEOUT
        )),
        "{seen:?}"
    );
    assert_eq!(close, Some(CLOSE_POLICY));
    busy.abort();
    parent.cancel();
}

#[tokio::test(start_paused = true)]
async fn handshake_timeout_fires_on_simulated_time_only() {
    let (server, parent) = sim_server(ServerConfig {
        handshake_timeout: Duration::from_millis(200),
        ..fast_config()
    })
    .await;
    let busy = keep_time_frozen();
    // A TCP connection that never speaks websocket.
    let mut tcp = TcpStream::connect(server.local_addr()).await.unwrap();
    let_real_time_pass(Duration::from_millis(600)).await;
    let mut buf = [0u8; 1];
    assert!(
        futures::FutureExt::now_or_never(tokio::io::AsyncReadExt::read(&mut tcp, &mut buf))
            .is_none(),
        "the handshake timeout fired without simulated time passing"
    );
    tokio::time::advance(Duration::from_millis(250)).await;
    let mut closed = std::pin::pin!(tokio::io::AsyncReadExt::read(&mut tcp, &mut buf));
    let read = loop {
        if let Some(read) = futures::FutureExt::now_or_never(closed.as_mut()) {
            break read;
        }
        tokio::task::yield_now().await;
    };
    assert!(matches!(read, Ok(0) | Err(_)));
    busy.abort();
    parent.cancel();
}

#[tokio::test(start_paused = true)]
async fn ping_timeout_fires_on_simulated_time_only() {
    let (server, parent) = sim_server(ServerConfig {
        ping_interval: Duration::from_millis(100),
        ping_timeout: Duration::from_millis(300),
        ..fast_config()
    })
    .await;
    let busy = keep_time_frozen();
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/call", server.local_addr()))
            .await
            .unwrap();
    let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
    ws.send(Message::text(start.to_json())).await.unwrap();
    // The call is open (its `started` is on the wire), then the client goes silent: it does not
    // read, so it answers no ping.
    let_real_time_pass(Duration::from_millis(900)).await;
    assert_eq!(server.active_calls(), 1);
    // Nearly the timeout of simulated time: still alive.
    tokio::time::advance(Duration::from_millis(250)).await;
    let_real_time_pass(Duration::from_millis(100)).await;
    assert_eq!(
        server.active_calls(),
        1,
        "ended before the simulated timeout"
    );
    // Past it: the call ends with ping_timeout.
    tokio::time::advance(Duration::from_millis(150)).await;
    let (seen, close) = read_until_closed(&mut ws, Duration::from_secs(3)).await;
    assert!(
        seen.iter().any(|m| matches!(
            m,
            ServerMessage::Error { code, fatal: true, .. } if code == code::PING_TIMEOUT
        )),
        "{seen:?}"
    );
    assert_eq!(close, Some(CLOSE_POLICY));
    busy.abort();
    parent.cancel();
}

/// Every timer and timeout of the websocket module runs on the injected clock (a simulated clock
/// must drive them all). The tests above prove it for the start, handshake and ping timers; this
/// guards the rest: no source line of the module may use tokio's own timers or read real time.
#[test]
fn websocket_sources_use_no_timer_but_the_injected_clock() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/websocket");
    let banned = [
        "tokio::time::timeout",
        "tokio::time::sleep",
        "tokio::time::interval",
        "tokio::time::Interval",
        "time::timeout(",
        "std::time::Instant",
        "SystemTime",
        "Instant::now",
    ];
    let mut offences = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        // Unit tests at the end of a file may measure real time; the code above them may not.
        let code = source.split("#[cfg(test)]").next().unwrap();
        for (number, line) in code.lines().enumerate() {
            let text = line.trim_start();
            if text.starts_with("//") {
                continue;
            }
            for pattern in banned {
                if text.contains(pattern) {
                    offences.push(format!("{}:{}: {text}", path.display(), number + 1));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "timers outside the injected Clock:\n{}",
        offences.join("\n")
    );
}

#[tokio::test]
async fn reference_client_reads_the_socket_while_it_sends_a_long_input_at_full_speed() {
    // The server writes 32 MiB of bot audio before it reads a single input frame, so the client
    // must read while it sends: if the send loop starved the receive side (--no-realtime), both
    // directions would fill up, and the client's write would time out.
    const AUDIO_FRAMES: usize = 1024;
    const AUDIO_FRAME: usize = 32 * 1024;
    let (url, server) = mock_server(|mut ws| async move {
        let frame = Bytes::from(vec![4u8; AUDIO_FRAME]);
        let write = async {
            for _ in 0..AUDIO_FRAMES {
                ws.feed(Message::Binary(frame.clone())).await.unwrap();
            }
            ws.flush().await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(20), write)
            .await
            .expect("the client never read the bot audio");
        // Now take the input until the client stops.
        let mut input_frames = 0usize;
        let take = async {
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Binary(_) => input_frames += 1,
                    Message::Text(_) | Message::Close(_) => break,
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), take)
            .await
            .expect("the client never finished its input");
        let stop = ServerMessage::Stop {
            reason: stop::ENDED.to_owned(),
        };
        let _ = ws.send(Message::text(stop.to_json())).await;
        let _ = ws.send(Message::Close(None)).await;
        input_frames
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.realtime = false;
    config.linger = Duration::from_millis(100);
    config.io_timeout = Duration::from_secs(2);
    let input = ramp(8 * 1024 * 1024); // 16 MiB: 26k frames of 20 ms
    let report = client::run(
        SystemClock::shared(),
        &config,
        &input,
        &CancellationToken::new(),
    )
    .await
    .expect("the call ran");
    assert_eq!(report.failure(), None, "{report:?}");
    assert!(report.input_complete);
    assert_eq!(report.audio_frames_received, AUDIO_FRAMES as u64);
    assert_eq!(report.audio.len(), AUDIO_FRAMES * AUDIO_FRAME);
    let heard = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u64::try_from(heard).unwrap(), report.frames_sent);
}

#[tokio::test]
async fn reference_client_looks_at_the_socket_between_the_frames_it_sends_at_full_speed() {
    // The server sends a mark at once. A client that serves its socket between two sent frames
    // echoes it within a few frames; one that sends its whole input first echoes it at the end.
    let (url, server) = mock_server(|mut ws| async move {
        let mark = ServerMessage::Mark {
            id: "m1".to_owned(),
        };
        ws.send(Message::text(mark.to_json())).await.unwrap();
        let mut frames = 0usize;
        let mut frames_at_echo = None;
        let take = async {
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Binary(_) => frames += 1,
                    Message::Text(text) => match ClientMessage::parse(text.as_str()) {
                        Ok(ClientMessage::Mark { .. }) => frames_at_echo = Some(frames),
                        _ => break,
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), take)
            .await
            .expect("the client never finished");
        let stop = ServerMessage::Stop {
            reason: stop::ENDED.to_owned(),
        };
        let _ = ws.send(Message::text(stop.to_json())).await;
        let _ = ws.send(Message::Close(None)).await;
        (frames, frames_at_echo)
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.realtime = false;
    config.linger = Duration::from_millis(100);
    config.io_timeout = Duration::from_secs(2);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(160_000), // 10 s: 500 frames
        &CancellationToken::new(),
    )
    .await
    .expect("the call ran");
    assert_eq!(report.failure(), None, "{report:?}");
    assert_eq!(report.marks_echoed, ["m1"]);
    let (frames, frames_at_echo) = tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    assert_eq!(frames, 500);
    let at_echo = frames_at_echo.expect("the mark was echoed");
    assert!(
        at_echo <= 10,
        "the mark was echoed only after {at_echo} of {frames} frames"
    );
}

#[tokio::test]
async fn reference_client_reports_a_clean_close_before_all_input_was_sent_as_a_failure() {
    // A bare close frame, no stop, while the input is still going out.
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 2).await;
        ws.send(Message::Close(None)).await.unwrap();
        let drain = async { while let Some(Ok(_)) = ws.next().await {} };
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.io_timeout = Duration::from_secs(1);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(16_000), // 1 s: 50 frames at real-time speed
        &CancellationToken::new(),
    )
    .await
    .expect("a close after the call started is a report, not an error");
    assert!(report.server_closed);
    assert!(!report.input_complete);
    assert_eq!(report.stop_reason, None);
    assert_eq!(report.transport_error, None, "it was a clean close");
    assert!(report.errors.is_empty());
    let failure = report.failure().expect("not a success");
    assert!(failure.contains("before all input was sent"), "{failure}");
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn reference_client_accepts_a_clean_close_after_all_input_was_sent() {
    let (url, server) = mock_server(|mut ws| async move {
        read_frames(&mut ws, 3).await;
        ws.send(Message::Close(None)).await.unwrap();
        let drain = async { while let Some(Ok(_)) = ws.next().await {} };
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    })
    .await;
    let mut config = ClientConfig::new(url);
    config.io_timeout = Duration::from_secs(1);
    let report = client::run(
        SystemClock::shared(),
        &config,
        &ramp(960), // 60 ms: exactly 3 frames
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.frames_sent, 3);
    assert!(report.input_complete);
    assert!(report.server_closed);
    assert_eq!(report.stop_reason, None);
    assert_eq!(report.failure(), None, "{report:?}");
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
}

/// Runs the `ws_client` binary with `args` and waits for it.
async fn run_ws_client(args: Vec<std::ffi::OsString>) -> std::process::Output {
    let run = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ws_client"))
            .args(args)
            .output()
            .unwrap()
    });
    tokio::time::timeout(WAIT, run).await.unwrap().unwrap()
}

fn os(text: impl Into<std::ffi::OsString>) -> std::ffi::OsString {
    text.into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ws_client_binary_honours_its_options() {
    let dir = std::env::temp_dir().join(format!("brisvox-ws-client-opts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input_path = dir.join("in8k.wav");
    let output_path = dir.join("out8k.wav");
    // 8 kHz mono, 0.2 s: not the client's default rate, so the WAV's rate must be used.
    let input = ramp(1_600);
    std::fs::write(&input_path, wav::encode(8_000, &input)).unwrap();

    let rig = Rig::start(fast_config()).await;
    let url = rig.url();
    let clock = rig.clock.clone();
    let engine = tokio::spawn(echo_engine(rig));
    let began = clock.now();
    let output = run_ws_client(vec![
        os(url),
        os(&input_path),
        os(&output_path),
        os("--encoding"),
        os("mulaw"),
        os("--dtmf"),
        os("7"),
        os("--meta"),
        os("tenant=acme"),
        os("--meta"),
        os("lang=tr"),
        os("--no-realtime"),
        os("--linger-ms"),
        os("300"),
    ])
    .await;
    let took = clock.now() - began;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // --linger-ms is honoured: the default linger alone is 2 s.
    assert!(took < Duration::from_millis(1800), "took {took:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sent 10 frames"), "{stdout}");

    let (echoed, events) = tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
    assert_eq!(echoed, 10);
    let TransportEvent::Started(info) = &events[0] else {
        panic!("first event {:?}", events[0]);
    };
    let meta = |key: &str| info.metadata.get(key).map(String::as_str);
    assert_eq!(meta("sample_rate"), Some("8000"), "the WAV's rate is used");
    assert_eq!(meta("encoding"), Some("mulaw"));
    assert_eq!(meta("tenant"), Some("acme"));
    assert_eq!(meta("lang"), Some("tr"));
    assert!(
        events.iter().any(|e| matches!(
            e,
            TransportEvent::Dtmf { digit, .. } if digit.as_char() == '7'
        )),
        "no key press in {events:?}"
    );

    // The recording is the input through the mu-law round trip, at the WAV's rate.
    let mut encoded = vec![0u8; Codec::Ulaw.encoded_len(input.len())];
    Codec::Ulaw.encode(&input, &mut encoded).unwrap();
    let mut expected = vec![0u8; Codec::Ulaw.decoded_len(encoded.len())];
    Codec::Ulaw.decode(&encoded, &mut expected).unwrap();
    let recorded = wav::parse(&std::fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(recorded.sample_rate, 8_000);
    assert_eq!(recorded.pcm.as_ref(), expected.as_slice());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ws_client_binary_exits_with_2_on_bad_usage_and_unusable_files() {
    let dir = std::env::temp_dir().join(format!("brisvox-ws-client-exit2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input_path = dir.join("in.wav");
    std::fs::write(&input_path, wav::encode(16_000, &ramp(3_200))).unwrap();
    let url = "ws://127.0.0.1:9/never";

    // An unknown option, a bad option value, a missing input file, a file that is not a WAV.
    let garbage = dir.join("garbage.wav");
    std::fs::write(&garbage, b"this is not a wav file").unwrap();
    let cases: Vec<Vec<std::ffi::OsString>> = vec![
        vec![
            os(url),
            os(&input_path),
            os(dir.join("o.wav")),
            os("--frobnicate"),
        ],
        vec![
            os(url),
            os(&input_path),
            os(dir.join("o.wav")),
            os("--encoding"),
            os("opus"),
        ],
        vec![
            os(url),
            os(&input_path),
            os(dir.join("o.wav")),
            os("--linger-ms"),
            os("soon"),
        ],
        vec![
            os(url),
            os(&input_path),
            os(dir.join("o.wav")),
            os("--meta"),
            os("novalue"),
        ],
        vec![os(url), os(dir.join("missing.wav")), os(dir.join("o.wav"))],
        vec![os(url), os(&garbage), os(dir.join("o.wav"))],
    ];
    for args in cases {
        let output = run_ws_client(args.clone()).await;
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}: no message");
    }

    // An output that cannot be written is also exit 2, after a call that went fine.
    let rig = Rig::start(fast_config()).await;
    let url = rig.url();
    let engine = tokio::spawn(echo_engine(rig));
    let output = run_ws_client(vec![
        os(url),
        os(&input_path),
        os(dir.join("no/such/dir/out.wav")),
        os("--linger-ms"),
        os("200"),
    ])
    .await;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot write"), "{stderr}");
    tokio::time::timeout(WAIT, engine).await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Admission gate

/// A gate with `room` reservations; a ticket gives its reservation back when dropped.
struct CountingGate {
    room: Arc<AtomicUsize>,
    shutting_down: AtomicBool,
}

impl CountingGate {
    fn new(room: &Arc<AtomicUsize>) -> Self {
        Self {
            room: Arc::clone(room),
            shutting_down: AtomicBool::new(false),
        }
    }
}

struct Reservation(Arc<AtomicUsize>);

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.fetch_add(1, SeqCst);
    }
}

impl AdmissionGate for CountingGate {
    fn try_admit(&self) -> Result<AdmissionTicket, Refusal> {
        if self.shutting_down.load(SeqCst) {
            return Err(Refusal::ShuttingDown);
        }
        let mut room = self.room.load(SeqCst);
        loop {
            let left = room.checked_sub(1).ok_or(Refusal::Busy)?;
            match self.room.compare_exchange(room, left, SeqCst, SeqCst) {
                Ok(_) => break,
                Err(now) => room = now,
            }
        }
        Ok(AdmissionTicket::new(Reservation(Arc::clone(&self.room))))
    }
}

/// Connects to `url` and expects an HTTP refusal instead of an upgrade.
async fn expect_refused(url: &str) -> (u16, bool) {
    match tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .expect("connect timed out")
    {
        Err(WsError::Http(response)) => (
            response.status().as_u16(),
            response.headers().contains_key("retry-after"),
        ),
        other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
    }
}

async fn wait_for_room(room: &AtomicUsize, expected: usize) {
    let wait = async {
        while room.load(SeqCst) != expected {
            pause(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WAIT, wait)
        .await
        .unwrap_or_else(|_| panic!("room stayed at {}", room.load(SeqCst)));
}

#[tokio::test]
async fn a_gate_admits_before_the_upgrade_and_its_ticket_travels_with_the_call() {
    let room = Arc::new(AtomicUsize::new(1));
    let gate = Arc::new(CountingGate::new(&room));
    let parent = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let shared_gate: SharedGate = gate.clone();
    let mut server = WebSocketServer::serve_gated(
        listener,
        fast_config(),
        SystemClock::shared(),
        &parent,
        Some(shared_gate),
    )
    .unwrap();
    let url = format!("ws://{}/call", server.local_addr());

    // The one reservation: the call is admitted and its link carries the ticket.
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(&url))
        .await
        .unwrap()
        .unwrap();
    send_json(
        &mut ws,
        &ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)),
    )
    .await;
    let started = next_server_message(&mut ws).await;
    assert!(
        matches!(started, ServerMessage::Started { .. }),
        "{started:?}"
    );
    let mut link = tokio::time::timeout(WAIT, server.accept(&parent))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(room.load(SeqCst), 0);

    // No room left: the next connection is refused before the upgrade, with Retry-After.
    assert_eq!(expect_refused(&url).await, (503, true));
    // Shutting down: 503 without Retry-After (the client should go elsewhere).
    gate.shutting_down.store(true, SeqCst);
    assert_eq!(expect_refused(&url).await, (503, false));

    // The ticket is the gate's reservation; dropping it gives the room back.
    let ticket = link
        .take_ticket()
        .expect("the link carries the gate's ticket");
    assert!(link.take_ticket().is_none());
    let reservation = ticket.downcast::<Reservation>().unwrap();
    assert_eq!(room.load(SeqCst), 0);
    drop(reservation);
    assert_eq!(room.load(SeqCst), 1);
    drop(link);
    server.shutdown(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_connection_that_never_becomes_a_call_gives_its_reservation_back() {
    let room = Arc::new(AtomicUsize::new(1));
    let gate: SharedGate = Arc::new(CountingGate::new(&room));
    let config = ServerConfig {
        handshake_timeout: Duration::from_millis(200),
        ..fast_config()
    };
    let parent = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server =
        WebSocketServer::serve_gated(listener, config, SystemClock::shared(), &parent, Some(gate))
            .unwrap();
    // A TCP connection that never speaks websocket holds the reservation until its handshake
    // times out, then gives it back.
    let _tcp = TcpStream::connect(server.local_addr()).await.unwrap();
    wait_for_room(&room, 0).await;
    wait_for_room(&room, 1).await;
    server.shutdown(WAIT).await.unwrap();
}

#[tokio::test]
async fn a_call_left_in_the_accept_queue_gives_its_reservation_back_at_shutdown() {
    let room = Arc::new(AtomicUsize::new(1));
    let gate: SharedGate = Arc::new(CountingGate::new(&room));
    let parent = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = WebSocketServer::serve_gated(
        listener,
        fast_config(),
        SystemClock::shared(),
        &parent,
        Some(gate),
    )
    .unwrap();
    let url = format!("ws://{}/call", server.local_addr());
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(&url))
        .await
        .unwrap()
        .unwrap();
    send_json(
        &mut ws,
        &ClientMessage::Start(Start::new(16_000, Encoding::Pcm16)),
    )
    .await;
    let _started = next_server_message(&mut ws).await;
    assert_eq!(room.load(SeqCst), 0);
    // Nobody accepts the call: dropping the server drops the queued link and its ticket.
    server.shutdown(WAIT).await.unwrap();
    drop(server);
    wait_for_room(&room, 1).await;
}
