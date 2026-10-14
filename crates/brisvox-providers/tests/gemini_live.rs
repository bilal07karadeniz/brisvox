//! Gemini Live adapter against a local mock of the `BidiGenerateContent` WebSocket protocol:
//! handshake and key placement, the `setup` contract, history seeding, audio and
//! control encoding, event mapping (JSON in text AND binary frames), the barge-in round trip
//! (`interrupted`, truncation that has no wire form), the tool round trip, `goAway`, setup
//! rejection and timeouts, liveness and a lost connection.
//!
//! The mock is a plain `TcpListener` plus `tokio_tungstenite::accept_hdr_async`. Real sockets do
//! not work with paused tokio time, so everything runs on the real `SystemClock` with short
//! deadlines.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::result_large_err
)]

#[path = "gemini_live/live.rs"]
mod live;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::gemini_live::{GeminiLive, GeminiLiveConfig};
use brisvox_providers::llm::ToolSpec;
use brisvox_providers::{
    Error, S2sConfig, S2sControl, S2sEvent, S2sHistoryItem, S2sProvider, S2sResponseStatus,
    S2sSession, S2sTurnDetection,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;

const KEY: &str = "AIzaTestKey_0123456789";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a connection of the mock behaves.
#[derive(Debug, Clone, Copy, Default)]
struct Behavior {
    /// Answer the upgrade with this HTTP status instead of accepting it.
    reject: Option<u16>,
    /// Accept the upgrade but never answer the setup.
    silent: bool,
    /// Send an `error` body instead of `setupComplete`.
    refuse_setup: bool,
    /// After the setup stop reading and writing altogether (a half-open connection).
    stall: bool,
}

#[derive(Debug, Clone, Default)]
struct Req {
    uri: String,
    api_key_header: Option<String>,
}

#[derive(Default)]
struct Shared {
    connections: AtomicUsize,
    requests: Mutex<Vec<Req>>,
    /// Text messages the client sent, in order, per connection index (the setup included).
    received: Mutex<Vec<(usize, Value)>>,
    injectors: Mutex<Vec<mpsc::UnboundedSender<Message>>>,
    pings: Mutex<Vec<usize>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

struct Mock {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: AbortHandle,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.accept.abort();
        for task in lock(&self.shared.tasks).drain(..) {
            task.abort();
        }
    }
}

impl Mock {
    async fn start(behaviors: Vec<Behavior>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let accept_shared = Arc::clone(&shared);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let index = accept_shared.connections.fetch_add(1, Ordering::SeqCst);
                let behavior = behaviors.get(index).copied().unwrap_or_default();
                let shared = Arc::clone(&accept_shared);
                let task = tokio::spawn(serve(stream, index, behavior, Arc::clone(&shared)));
                lock(&accept_shared.tasks).push(task.abort_handle());
            }
        })
        .abort_handle();
        Self {
            addr,
            shared,
            accept,
        }
    }

    fn provider(&self, tweak: impl FnOnce(&mut GeminiLiveConfig)) -> GeminiLive {
        let mut cfg = GeminiLiveConfig::new(Secret::new(KEY.to_owned()));
        cfg.base_url = Some(format!("ws://{}", self.addr));
        cfg.io_timeout = Duration::from_secs(2);
        tweak(&mut cfg);
        let clock: SharedClock = SystemClock::shared();
        GeminiLive::new(cfg, clock)
    }

    /// Server-to-client JSON as a text frame.
    fn push(&self, index: usize, event: &Value) {
        self.inject(index, Message::text(event.to_string()));
    }

    /// Server-to-client JSON as a BINARY frame (what the real service does).
    fn push_binary(&self, index: usize, event: &Value) {
        self.inject(index, Message::Binary(Bytes::from(event.to_string())));
    }

    fn inject(&self, index: usize, message: Message) {
        let injector = lock(&self.shared.injectors).get(index).cloned().unwrap();
        injector.send(message).unwrap();
    }

    fn received(&self, index: usize) -> Vec<Value> {
        lock(&self.shared.received)
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// The messages of connection `index` that carry the top-level field `key`.
    fn received_with(&self, index: usize, key: &str) -> Vec<Value> {
        self.received(index)
            .into_iter()
            .filter(|v| v.get(key).is_some())
            .collect()
    }

    fn request(&self, index: usize) -> Req {
        lock(&self.shared.requests)[index].clone()
    }

    async fn wait(&self, what: &str, cond: impl Fn(&Mock) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

async fn serve(stream: TcpStream, index: usize, behavior: Behavior, shared: Arc<Shared>) {
    let callback_shared = Arc::clone(&shared);
    let callback =
        move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            lock(&callback_shared.requests).push(Req {
                uri: request.uri().to_string(),
                api_key_header: request
                    .headers()
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            });
            match behavior.reject {
                Some(status) => {
                    let mut error = ErrorResponse::new(Some("no".to_owned()));
                    *error.status_mut() = http::StatusCode::from_u16(status).unwrap();
                    Err(error)
                }
                None => Ok(response),
            }
        };
    let Ok(mut ws) = accept_hdr_async(stream, callback).await else {
        return;
    };
    let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Message>();
    lock(&shared.injectors).push(inject_tx);
    let mut set_up = false;
    loop {
        tokio::select! {
            injected = inject_rx.recv() => {
                let Some(message) = injected else { break };
                let close = matches!(message, Message::Close(_));
                if ws.send(message).await.is_err() || close {
                    break;
                }
            }
            incoming = ws.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(text.as_str()).unwrap();
                        let is_setup = !set_up && value.get("setup").is_some();
                        lock(&shared.received).push((index, value));
                        if is_setup && !behavior.silent {
                            set_up = true;
                            let answer = if behavior.refuse_setup {
                                json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "no such voice"}})
                            } else {
                                json!({"setupComplete": {}})
                            };
                            // The real service answers in a binary frame.
                            let _ = ws.send(Message::Binary(Bytes::from(answer.to_string()))).await;
                            if behavior.stall {
                                std::future::pending::<()>().await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(_))) => lock(&shared.pings).push(index),
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

fn s2s_config() -> S2sConfig {
    let mut config = S2sConfig::new("You are a test agent.");
    config.tools.push(ToolSpec {
        name: Arc::from("lookup"),
        description: Arc::from("Looks something up."),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}"#,
        ),
    });
    config
}

async fn next_event(session: &mut S2sSession) -> S2sEvent {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("the event stream ended")
}

/// Events up to and including the first one `stop` accepts.
async fn events_until(session: &mut S2sSession, stop: impl Fn(&S2sEvent) -> bool) -> Vec<S2sEvent> {
    let mut seen = Vec::new();
    loop {
        let event = next_event(session).await;
        let done = stop(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

fn audio_chunk(pcm: &[u8]) -> Value {
    json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {
        "mimeType": "audio/pcm;rate=24000",
        "data": STANDARD.encode(pcm),
    }}]}}})
}

fn decode_audio(message: &Value) -> Vec<u8> {
    STANDARD
        .decode(message["realtimeInput"]["audio"]["data"].as_str().unwrap())
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Handshake and setup
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn setup_carries_the_contract_and_the_key_goes_in_the_query() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    assert_eq!(provider.name(), "gemini-live");
    let mut config = s2s_config();
    config.voice = Some(Arc::from("Puck"));
    config.input_transcription = true;
    let _session = provider.connect(config).await.unwrap();
    let request = mock.request(0);
    assert!(request.uri.contains("BidiGenerateContent?key="));
    assert!(request.uri.ends_with(KEY));
    assert!(request.api_key_header.is_none());
    let setup = mock.received_with(0, "setup")[0]["setup"].clone();
    assert_eq!(setup["model"], "models/gemini-3.8-live");
    assert_eq!(
        setup["generationConfig"]["responseModalities"],
        json!(["AUDIO"])
    );
    assert_eq!(
        setup["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
        "Puck"
    );
    assert_eq!(
        setup["systemInstruction"]["parts"][0]["text"],
        "You are a test agent."
    );
    let tool = &setup["tools"][0]["functionDeclarations"][0];
    assert_eq!(tool["name"], "lookup");
    assert_eq!(tool["parametersJsonSchema"]["required"][0], "q");
    assert_eq!(setup["inputAudioTranscription"], json!({}));
    assert_eq!(setup["outputAudioTranscription"], json!({}));
    assert!(setup.get("historyConfig").is_none());
    assert!(mock.received_with(0, "clientContent").is_empty());
}

#[tokio::test]
async fn the_key_can_go_in_a_header_instead() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|c| c.key_in_header = true);
    let _session = provider.connect(s2s_config()).await.unwrap();
    let request = mock.request(0);
    assert!(!request.uri.contains("key="));
    assert_eq!(request.api_key_header.as_deref(), Some(KEY));
}

#[tokio::test]
async fn a_new_session_is_seeded_with_the_history_before_any_audio() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut config = s2s_config();
    config.history = vec![
        S2sHistoryItem::User(Arc::from("Merhaba")),
        S2sHistoryItem::Assistant(Arc::from("Selam, nasil yardimci olabilirim?")),
        S2sHistoryItem::User(Arc::from("Hava nasil?")),
    ];
    let session = provider.connect(config).await.unwrap();
    session
        .audio
        .send(Bytes::from_static(&[1, 2]))
        .await
        .unwrap();
    mock.wait("the audio", |m| {
        m.received_with(0, "realtimeInput").len() == 1
    })
    .await;
    let received = mock.received(0);
    assert!(received[0].get("setup").is_some());
    assert!(
        received[1].get("clientContent").is_some(),
        "history precedes audio"
    );
    assert!(received[2].get("realtimeInput").is_some());
    assert_eq!(
        received[0]["setup"]["historyConfig"]["initialHistoryInClientContent"],
        true
    );
    let content = &received[1]["clientContent"];
    assert_eq!(content["turnComplete"], false);
    let turns = content["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[1]["role"], "model");
    assert_eq!(turns[2]["parts"][0]["text"], "Hava nasil?");
}

#[tokio::test]
async fn setup_failures_are_errors() {
    // The upgrade is refused.
    let mock = Mock::start(vec![Behavior {
        reject: Some(403),
        ..Behavior::default()
    }])
    .await;
    let error = mock
        .provider(|_| {})
        .connect(s2s_config())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 403, .. }),
        "{error:?}"
    );
    assert!(!error.to_string().contains(KEY));
    // The setup is answered with an error body.
    let mock = Mock::start(vec![Behavior {
        refuse_setup: true,
        ..Behavior::default()
    }])
    .await;
    let error = mock
        .provider(|_| {})
        .connect(s2s_config())
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Protocol { detail, .. } if detail.contains("no such voice")));
    // The setup is never answered: the explicit timeout ends the wait.
    let mock = Mock::start(vec![Behavior {
        silent: true,
        ..Behavior::default()
    }])
    .await;
    let provider = mock.provider(|c| c.io_timeout = Duration::from_millis(200));
    let error = provider.connect(s2s_config()).await.unwrap_err();
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error:?}"
    );
    // An unsupported rate fails connect before any socket is opened.
    let mock = Mock::start(vec![]).await;
    let mut config = s2s_config();
    config.output_rate = brisvox_core::frame::SampleRate::Hz16000;
    assert!(matches!(
        mock.provider(|_| {}).connect(config).await,
        Err(Error::Protocol { .. })
    ));
    assert_eq!(mock.shared.connections.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------------------------
// Audio, turns and events
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_turns_stream_audio_and_map_the_events() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut session = provider.connect(s2s_config()).await.unwrap();
    session
        .audio
        .send(Bytes::from_static(&[9, 8, 7, 6]))
        .await
        .unwrap();
    mock.wait("audio", |m| m.received_with(0, "realtimeInput").len() == 1)
        .await;
    let sent = &mock.received_with(0, "realtimeInput")[0];
    assert_eq!(
        sent["realtimeInput"]["audio"]["mimeType"],
        "audio/pcm;rate=24000"
    );
    assert_eq!(decode_audio(sent), [9, 8, 7, 6]);
    // Provider mode never sends activity markers.
    assert!(
        mock.received(0)
            .iter()
            .all(|m| m["realtimeInput"]["activityStart"].is_null())
    );

    mock.push_binary(0, &json!({"voiceActivity": {"type": "ACTIVITY_START"}}));
    mock.push_binary(0, &json!({"voiceActivity": {"type": "ACTIVITY_END"}}));
    mock.push_binary(
        0,
        &json!({"serverContent": {"inputTranscription": {"text": "Merhaba"}}}),
    );
    mock.push_binary(0, &audio_chunk(&[1, 0, 2, 0]));
    mock.push(
        0,
        &json!({"serverContent": {"outputTranscription": {"text": "Selam"}}}),
    );
    mock.push(0, &audio_chunk(&[3, 0]));
    mock.push(0, &json!({"serverContent": {"generationComplete": true}}));
    mock.push(0, &json!({"serverContent": {"turnComplete": true}}));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::ResponseDone { .. })).await;
    let names: Vec<&str> = events
        .iter()
        .map(|e| match e {
            S2sEvent::SpeechStarted => "speech_started",
            S2sEvent::SpeechStopped => "speech_stopped",
            S2sEvent::UserTranscript { .. } => "user_transcript",
            S2sEvent::ResponseStarted { .. } => "response_started",
            S2sEvent::Audio { .. } => "audio",
            S2sEvent::AssistantTranscript {
                is_final: false, ..
            } => "delta",
            S2sEvent::AssistantTranscript { is_final: true, .. } => "final",
            S2sEvent::AudioDone { .. } => "audio_done",
            S2sEvent::ResponseDone { .. } => "response_done",
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        [
            "speech_started",
            "speech_stopped",
            "user_transcript",
            "response_started",
            "audio",
            "delta",
            "audio",
            "audio_done",
            "final",
            "response_done"
        ]
    );
    let pcm: Vec<u8> = events
        .iter()
        .filter_map(|e| match e {
            S2sEvent::Audio { pcm, .. } => Some(pcm.to_vec()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(pcm, [1, 0, 2, 0, 3, 0]);
    assert!(matches!(
        events.last(),
        Some(S2sEvent::ResponseDone {
            status: S2sResponseStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn manual_turns_wrap_the_utterance_in_activity_markers() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut config = s2s_config();
    config.turn_detection = S2sTurnDetection::Manual;
    let session = provider.connect(config).await.unwrap();
    let setup = mock.received_with(0, "setup")[0]["setup"].clone();
    assert_eq!(
        setup["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
        true
    );
    for chunk in [[1_u8, 1], [2, 2], [3, 3]] {
        session
            .audio
            .send(Bytes::copy_from_slice(&chunk))
            .await
            .unwrap();
    }
    session.control.send(S2sControl::Commit).await.unwrap();
    // The commit absorbs the audio queued before it, then ends the activity; the following
    // CreateResponse is absorbed because the server answers by itself.
    session
        .control
        .send(S2sControl::CreateResponse)
        .await
        .unwrap();
    mock.wait("activityEnd", |m| {
        m.received(0)
            .iter()
            .any(|v| !v["realtimeInput"]["activityEnd"].is_null())
    })
    .await;
    session
        .control
        .send(S2sControl::Note {
            text: Arc::from("marker"),
        })
        .await
        .unwrap();
    mock.wait("the note", |m| {
        m.received_with(0, "clientContent").len() == 1
    })
    .await;
    let received = mock.received(0);
    let shape: Vec<&str> = received[1..]
        .iter()
        .map(|v| {
            if !v["realtimeInput"]["activityStart"].is_null() {
                "start"
            } else if !v["realtimeInput"]["activityEnd"].is_null() {
                "end"
            } else if v["realtimeInput"]["audio"].is_object() {
                "audio"
            } else {
                "other"
            }
        })
        .collect();
    assert_eq!(shape, ["start", "audio", "audio", "audio", "end", "other"]);
    let decoded: Vec<Vec<u8>> = received
        .iter()
        .filter(|v| v["realtimeInput"]["audio"].is_object())
        .map(decode_audio)
        .collect();
    assert_eq!(decoded, [vec![1, 1], vec![2, 2], vec![3, 3]]);
    // No response was requested on the wire: the only clientContent is the note.
    let contents = mock.received_with(0, "clientContent");
    assert_eq!(contents.len(), 1);
    assert_eq!(
        contents[0]["clientContent"]["turns"][0]["parts"][0]["text"],
        "[note] marker"
    );
    assert_eq!(contents[0]["clientContent"]["turnComplete"], false);
}

#[tokio::test]
async fn a_response_after_a_note_is_requested_with_an_empty_complete_turn() {
    let mock = Mock::start(vec![]).await;
    let session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    session
        .control
        .send(S2sControl::Note {
            text: Arc::from("answer without tools"),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::CreateResponse)
        .await
        .unwrap();
    mock.wait("the request", |m| {
        m.received_with(0, "clientContent").len() == 2
    })
    .await;
    let contents = mock.received_with(0, "clientContent");
    assert_eq!(contents[1]["clientContent"]["turnComplete"], true);
    assert_eq!(contents[1]["clientContent"]["turns"], json!([]));
}

// ---------------------------------------------------------------------------------------------
// Barge-in and tools
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn barge_in_cancels_the_response_and_truncation_has_no_wire_form() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    mock.push(0, &audio_chunk(&[1, 0, 2, 0]));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::Audio { .. })).await;
    let item = match events.last() {
        Some(S2sEvent::Audio { item, .. }) => Arc::clone(item),
        other => panic!("{other:?}"),
    };
    // The caller talks over the bot: the server reports `interrupted`.
    mock.push(0, &json!({"serverContent": {"interrupted": true}}));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::ResponseDone { .. })).await;
    assert!(matches!(events[0], S2sEvent::SpeechStarted), "{events:?}");
    assert!(matches!(
        events.last(),
        Some(S2sEvent::ResponseDone {
            status: S2sResponseStatus::Cancelled,
            ..
        })
    ));
    // The engine truncates to what was heard: Gemini cannot express that, nothing is sent and the
    // session stays healthy.
    let before = mock.received(0).len();
    session
        .control
        .send(S2sControl::Truncate {
            item,
            audio_end: Duration::from_millis(40),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::Note {
            text: Arc::from("after"),
        })
        .await
        .unwrap();
    mock.wait("the note", |m| m.received(0).len() == before + 1)
        .await;
    assert!(mock.received(0)[before].get("clientContent").is_some());
    // Late audio of the cancelled response never reaches the engine after the new turn began:
    // the next audio opens a new response.
    mock.push(0, &audio_chunk(&[5, 0]));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::Audio { .. })).await;
    assert!(events.iter().any(|e| matches!(e, S2sEvent::SpeechStopped)));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, S2sEvent::ResponseStarted { response } if &**response == "gr2"))
    );
}

#[tokio::test]
async fn a_local_cancel_ends_the_response_and_drops_its_tail() {
    let mock = Mock::start(vec![]).await;
    let mut config = s2s_config();
    config.turn_detection = S2sTurnDetection::Manual;
    let mut session = mock.provider(|_| {}).connect(config).await.unwrap();
    mock.push(0, &audio_chunk(&[1, 0]));
    events_until(&mut session, |e| matches!(e, S2sEvent::Audio { .. })).await;
    session
        .control
        .send(S2sControl::CancelResponse)
        .await
        .unwrap();
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::ResponseDone { .. })).await;
    assert!(matches!(
        events.last(),
        Some(S2sEvent::ResponseDone {
            status: S2sResponseStatus::Cancelled,
            ..
        })
    ));
    // Nothing went over the wire for the cancel.
    assert!(mock.received(0).iter().all(|v| v.get("setup").is_some()));
    // The tail of the cancelled response is dropped; the turn end re-arms the mapper.
    mock.push(0, &audio_chunk(&[2, 0]));
    mock.push(0, &json!({"serverContent": {"turnComplete": true}}));
    mock.push(0, &audio_chunk(&[3, 0]));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::Audio { .. })).await;
    let pcm: Vec<Vec<u8>> = events
        .iter()
        .filter_map(|e| match e {
            S2sEvent::Audio { pcm, .. } => Some(pcm.to_vec()),
            _ => None,
        })
        .collect();
    assert_eq!(pcm, [vec![3, 0]]);
}

#[tokio::test]
async fn a_tool_call_round_trips_once() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    let call = json!({"toolCall": {"functionCalls": [
        {"id": "fc-1", "name": "lookup", "args": {"q": "weather"}},
        {"id": "fc-2", "name": "lookup", "args": {"q": "time"}},
    ]}});
    mock.push_binary(0, &call);
    mock.push_binary(0, &call); // a repeat is not delivered again
    mock.push(0, &json!({"serverContent": {"turnComplete": true}}));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::ResponseDone { .. })).await;
    let calls: Vec<(String, String, String)> = events
        .iter()
        .filter_map(|e| match e {
            S2sEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => Some((call_id.to_string(), name.to_string(), arguments.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            (
                "fc-1".to_owned(),
                "lookup".to_owned(),
                r#"{"q":"weather"}"#.to_owned()
            ),
            (
                "fc-2".to_owned(),
                "lookup".to_owned(),
                r#"{"q":"time"}"#.to_owned()
            ),
        ]
    );
    // The engine answers both calls; the answers go out as ONE toolResponse after the second.
    session
        .control
        .send(S2sControl::ToolResult {
            call_id: Arc::from("fc-1"),
            output: Arc::from(r#"{"temp":21}"#),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::ToolResult {
            call_id: Arc::from("fc-2"),
            output: Arc::from("noon"),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::CreateResponse)
        .await
        .unwrap();
    mock.wait("the tool response", |m| {
        !m.received_with(0, "toolResponse").is_empty()
    })
    .await;
    let responses = mock.received_with(0, "toolResponse");
    assert_eq!(responses.len(), 1);
    let answers = responses[0]["toolResponse"]["functionResponses"]
        .as_array()
        .unwrap();
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0]["id"], "fc-1");
    assert_eq!(answers[0]["name"], "lookup");
    assert_eq!(answers[0]["response"]["result"]["temp"], 21);
    assert_eq!(answers[1]["response"]["result"], "noon");
    // The server continues by itself: CreateResponse sent no clientContent.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(mock.received_with(0, "clientContent").is_empty());
    // The continuation is a new response.
    mock.push(0, &audio_chunk(&[1, 0]));
    let events = events_until(&mut session, |e| matches!(e, S2sEvent::Audio { .. })).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, S2sEvent::ResponseStarted { response } if &**response == "gr2"))
    );
}

#[tokio::test]
async fn the_result_of_an_interrupted_call_is_not_sent() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    mock.push(
        0,
        &json!({"toolCall": {"functionCalls": [{"id": "fc-9", "name": "lookup", "args": {}}]}}),
    );
    events_until(&mut session, |e| matches!(e, S2sEvent::ResponseDone { .. })).await;
    // The caller interrupts before the tool finished: the server discards the call.
    mock.push(0, &json!({"serverContent": {"interrupted": true}}));
    events_until(&mut session, |e| matches!(e, S2sEvent::SpeechStarted)).await;
    session
        .control
        .send(S2sControl::ToolResult {
            call_id: Arc::from("fc-9"),
            output: Arc::from("late"),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::Note {
            text: Arc::from("sync"),
        })
        .await
        .unwrap();
    mock.wait("the note", |m| {
        !m.received_with(0, "clientContent").is_empty()
    })
    .await;
    assert!(mock.received_with(0, "toolResponse").is_empty());
}

// ---------------------------------------------------------------------------------------------
// Limits, liveness, failure
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn go_away_is_reported_and_the_session_limit_is_the_connection_lifetime() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|c| c.connection_lifetime = Duration::from_secs(540));
    assert_eq!(provider.session_limit(), Some(Duration::from_secs(540)));
    let mut session = provider.connect(s2s_config()).await.unwrap();
    mock.push_binary(0, &json!({"goAway": {"timeLeft": "55s"}}));
    match next_event(&mut session).await {
        S2sEvent::SessionEnding { time_left } => assert_eq!(time_left, Duration::from_secs(55)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_silent_server_is_detected_by_the_liveness_ping() {
    let mock = Mock::start(vec![Behavior {
        stall: true,
        ..Behavior::default()
    }])
    .await;
    let provider = mock.provider(|c| {
        c.keepalive_interval = Duration::from_millis(100);
        c.io_timeout = Duration::from_millis(300);
    });
    let mut session = provider.connect(s2s_config()).await.unwrap();
    match next_event(&mut session).await {
        S2sEvent::Error(Error::Core(brisvox_core::Error::Timeout { what, .. })) => {
            assert!(what.contains("liveness"), "{what}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        session.events.recv().await.is_none(),
        "the error is the last event"
    );
}

#[tokio::test]
async fn a_lost_connection_ends_the_stream_with_an_error() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    mock.inject(0, Message::Close(None));
    match next_event(&mut session).await {
        S2sEvent::Error(error) => {
            assert!(
                matches!(error, Error::Disconnected { .. } | Error::Rejected { .. }),
                "{error:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(session.events.recv().await.is_none());
}

#[tokio::test]
async fn a_malformed_message_fails_the_session_and_server_errors_are_warnings() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    mock.push(0, &json!({"error": {"code": 429, "message": "slow down"}}));
    match next_event(&mut session).await {
        S2sEvent::Warning { code, message } => {
            assert_eq!(&*code, "429");
            assert_eq!(&*message, "slow down");
        }
        other => panic!("{other:?}"),
    }
    mock.inject(0, Message::text("not json"));
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::Error(Error::Protocol { .. })
    ));
}

#[tokio::test]
async fn dropping_the_senders_closes_the_connection() {
    let mock = Mock::start(vec![]).await;
    let session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    let S2sSession {
        audio,
        control,
        mut events,
    } = session;
    drop(audio);
    drop(control);
    let ended = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap();
    assert!(ended.is_none());
}
