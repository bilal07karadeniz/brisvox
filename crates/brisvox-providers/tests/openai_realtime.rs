//! OpenAI Realtime adapter against a local mock of the Realtime WebSocket protocol:
//! handshake and auth, the `session.update` contract, audio and control encoding, event
//! mapping (tool call once, response status), the barge-in truncation round trip, error
//! classification, timeouts and a lost connection.
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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::llm::ToolSpec;
use brisvox_providers::openai_realtime::{OpenAiRealtime, OpenAiRealtimeConfig};
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

const KEY: &str = "sk-test-key-0123456789";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a connection of the mock behaves.
#[derive(Debug, Clone, Copy, Default)]
struct Behavior {
    /// Answer the upgrade with this HTTP status instead of accepting it.
    reject: Option<u16>,
    /// Accept the upgrade but never send `session.created`.
    silent: bool,
    /// Send an `error` event instead of `session.updated`.
    refuse_update: bool,
    /// After the handshake stop reading and writing altogether (a half-open connection: no
    /// `Pong`, no `Close`, nothing).
    stall: bool,
}

#[derive(Debug, Clone, Default)]
struct Req {
    uri: String,
    authorization: Option<String>,
    beta: Option<String>,
    safety: Option<String>,
}

#[derive(Default)]
struct Shared {
    connections: AtomicUsize,
    requests: Mutex<Vec<Req>>,
    /// Text messages the client sent after the handshake, in order, per connection index.
    received: Mutex<Vec<(usize, Value)>>,
    /// Server-side injection channel of each connection.
    injectors: Mutex<Vec<mpsc::UnboundedSender<Message>>>,
    closed: Mutex<Vec<usize>>,
    /// Connection index of every WebSocket `Ping` the client sent.
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

    fn provider(&self, tweak: impl FnOnce(&mut OpenAiRealtimeConfig)) -> OpenAiRealtime {
        let mut cfg = OpenAiRealtimeConfig::new(Secret::new(KEY.to_owned()));
        cfg.base_url = Some(format!("ws://{}", self.addr));
        cfg.io_timeout = Duration::from_secs(2);
        tweak(&mut cfg);
        let clock: SharedClock = SystemClock::shared();
        OpenAiRealtime::new(cfg, clock)
    }

    /// Server-to-client event on connection `index`.
    fn push(&self, index: usize, event: Value) {
        let injector = lock(&self.shared.injectors).get(index).cloned().unwrap();
        injector.send(Message::text(event.to_string())).unwrap();
    }

    fn received(&self, index: usize) -> Vec<Value> {
        lock(&self.shared.received)
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn received_of(&self, index: usize, kind: &str) -> Vec<Value> {
        self.received(index)
            .into_iter()
            .filter(|v| v["type"] == kind)
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
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            lock(&callback_shared.requests).push(Req {
                uri: request.uri().to_string(),
                authorization: header("authorization"),
                beta: header("openai-beta"),
                safety: header("openai-safety-identifier"),
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
    if !behavior.silent {
        let created = json!({"type": "session.created", "session": {"id": "sess_1"}});
        let _ = ws.send(Message::text(created.to_string())).await;
    }
    let mut updated = false;
    loop {
        tokio::select! {
            injected = inject_rx.recv() => {
                let Some(message) = injected else { break };
                if matches!(message, Message::Close(_)) {
                    let _ = ws.send(message).await;
                    break;
                }
                if ws.send(message).await.is_err() {
                    break;
                }
            }
            incoming = ws.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(text.as_str()).unwrap();
                        if !updated && value["type"] == "session.update" {
                            updated = true;
                            lock(&shared.received).push((index, value));
                            let answer = if behavior.refuse_update {
                                json!({"type": "error", "error": {"type": "invalid_request_error", "code": "bad_session", "message": "no such voice"}})
                            } else {
                                json!({"type": "session.updated", "session": {"id": "sess_1"}})
                            };
                            let _ = ws.send(Message::text(answer.to_string())).await;
                            if behavior.stall {
                                std::future::pending::<()>().await;
                            }
                        } else {
                            lock(&shared.received).push((index, value));
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(_))) => lock(&shared.pings).push(index),
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    lock(&shared.closed).push(index);
}

fn s2s_config() -> S2sConfig {
    let mut config = S2sConfig::new("You are a test agent.");
    config.tools.push(ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Weather for a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
        ),
    });
    config
}

async fn next_event(session: &mut S2sSession) -> S2sEvent {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv())
        .await
        .expect("no event within 5 s")
        .expect("the event stream ended")
}

// ---------------------------------------------------------------------------------------------
// Handshake and session.update
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn connect_authenticates_with_a_bearer_header_and_configures_the_session() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|cfg| cfg.safety_identifier = Some("user-hash-1".to_owned()));
    let _session = provider.connect(s2s_config()).await.unwrap();

    let request = mock.request(0);
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer sk-test-key-0123456789")
    );
    assert_eq!(request.uri, "/v1/realtime?model=gpt-realtime-2.1");
    assert_eq!(
        request.beta, None,
        "the GA interface must not send OpenAI-Beta"
    );
    assert_eq!(request.safety.as_deref(), Some("user-hash-1"));
    assert!(
        !request.uri.contains("sk-test"),
        "the key never goes into the URL"
    );

    let update = mock.received_of(0, "session.update").remove(0);
    let session = &update["session"];
    assert_eq!(session["type"], "realtime");
    assert_eq!(session["instructions"], "You are a test agent.");
    assert_eq!(session["tools"][0]["name"], "get_weather");
    assert_eq!(session["tools"][0]["parameters"]["required"][0], "city");
    assert_eq!(session["audio"]["input"]["format"]["rate"], 24000);
    assert_eq!(
        session["audio"]["input"]["turn_detection"]["type"],
        "server_vad"
    );
    assert_eq!(
        session["audio"]["input"]["turn_detection"]["interrupt_response"],
        true
    );
    assert_eq!(session["audio"]["output"]["voice"], "marin");
}

#[tokio::test]
async fn manual_turn_detection_turns_the_server_vad_off() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut config = s2s_config();
    config.turn_detection = S2sTurnDetection::Manual;
    let _session = provider.connect(config).await.unwrap();
    let update = mock.received_of(0, "session.update").remove(0);
    assert!(update["session"]["audio"]["input"]["turn_detection"].is_null());
}

#[tokio::test]
async fn an_unsupported_rate_fails_before_any_connection() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut config = s2s_config();
    config.input_rate = brisvox_core::frame::SampleRate::Hz16000;
    let error = provider.connect(config).await.unwrap_err();
    assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    assert_eq!(mock.shared.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_http_refusal_is_a_rejection_with_its_status() {
    let mock = Mock::start(vec![Behavior {
        reject: Some(401),
        ..Behavior::default()
    }])
    .await;
    let error = mock
        .provider(|_| {})
        .connect(s2s_config())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 401, .. }),
        "{error:?}"
    );
    assert!(!format!("{error}").contains(KEY));
}

#[tokio::test]
async fn a_server_that_never_greets_times_out() {
    let mock = Mock::start(vec![Behavior {
        silent: true,
        ..Behavior::default()
    }])
    .await;
    let provider = mock.provider(|cfg| cfg.io_timeout = Duration::from_millis(250));
    let started = Instant::now();
    let error = provider.connect(s2s_config()).await.unwrap_err();
    assert!(
        matches!(error, Error::Core(brisvox_core::Error::Timeout { .. })),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_refused_session_update_fails_connect_without_leaking_the_key() {
    let mock = Mock::start(vec![Behavior {
        refuse_update: true,
        ..Behavior::default()
    }])
    .await;
    let error = mock
        .provider(|_| {})
        .connect(s2s_config())
        .await
        .unwrap_err();
    match &error {
        Error::Protocol { detail, .. } => {
            assert!(detail.contains("bad_session"), "{detail}");
            assert!(!detail.contains(KEY));
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Client side: audio and control
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn audio_is_sent_as_base64_appends_and_control_as_documented_events() {
    let mock = Mock::start(vec![]).await;
    let session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();

    session
        .audio
        .send(Bytes::from_static(&[1, 2, 3, 4]))
        .await
        .unwrap();
    for control in [
        S2sControl::Commit,
        S2sControl::CreateResponse,
        S2sControl::CancelResponse,
        S2sControl::Truncate {
            item: Arc::from("item_9"),
            audio_end: Duration::from_millis(1234),
        },
        S2sControl::ToolResult {
            call_id: Arc::from("call_7"),
            output: Arc::from(r#"{"temp":21}"#),
        },
        S2sControl::Note {
            text: Arc::from("late result"),
        },
    ] {
        session.control.send(control).await.unwrap();
    }
    mock.wait("all client events", |m| m.received(0).len() >= 8)
        .await;

    let append = mock.received_of(0, "input_audio_buffer.append").remove(0);
    assert_eq!(append["audio"], "AQIDBA==");
    assert_eq!(mock.received_of(0, "input_audio_buffer.commit").len(), 1);
    assert_eq!(mock.received_of(0, "response.create").len(), 1);
    assert_eq!(mock.received_of(0, "response.cancel").len(), 1);
    let truncate = mock.received_of(0, "conversation.item.truncate").remove(0);
    assert_eq!(truncate["item_id"], "item_9");
    assert_eq!(truncate["content_index"], 0);
    assert_eq!(truncate["audio_end_ms"], 1234);
    let items = mock.received_of(0, "conversation.item.create");
    assert_eq!(items[0]["item"]["type"], "function_call_output");
    assert_eq!(items[0]["item"]["call_id"], "call_7");
    assert_eq!(items[0]["item"]["output"], r#"{"temp":21}"#);
    assert_eq!(items[1]["item"]["role"], "system");
    assert_eq!(items[1]["item"]["content"][0]["text"], "late result");
}

#[tokio::test]
async fn dropping_the_senders_closes_the_connection() {
    let mock = Mock::start(vec![]).await;
    let session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    drop(session);
    mock.wait("the server to see the close", |m| {
        !lock(&m.shared.closed).is_empty()
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// Server side: events
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn server_events_map_to_neutral_events_in_order() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();

    mock.push(
        0,
        json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 10, "item_id": "u1"}),
    );
    mock.push(
        0,
        json!({"type": "input_audio_buffer.speech_stopped", "audio_end_ms": 900, "item_id": "u1"}),
    );
    mock.push(0, json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}));
    mock.push(0, json!({"type": "response.output_audio.delta", "response_id": "resp_1", "item_id": "item_1", "output_index": 0, "content_index": 0, "delta": "AAECAw=="}));
    mock.push(0, json!({"type": "response.output_audio_transcript.delta", "response_id": "resp_1", "item_id": "item_1", "output_index": 0, "content_index": 0, "delta": "Hel"}));
    mock.push(0, json!({"type": "response.output_audio_transcript.done", "response_id": "resp_1", "item_id": "item_1", "output_index": 0, "content_index": 0, "transcript": "Hello"}));
    mock.push(0, json!({"type": "response.output_audio.done", "response_id": "resp_1", "item_id": "item_1", "output_index": 0, "content_index": 0}));
    mock.push(0, json!({"type": "response.done", "response": {"id": "resp_1", "status": "completed", "output": []}}));

    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::SpeechStarted
    ));
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::SpeechStopped
    ));
    assert!(
        matches!(next_event(&mut session).await, S2sEvent::ResponseStarted { response } if &*response == "resp_1")
    );
    match next_event(&mut session).await {
        S2sEvent::Audio {
            response,
            item,
            pcm,
        } => {
            assert_eq!(&*response, "resp_1");
            assert_eq!(&*item, "item_1");
            assert_eq!(&pcm[..], &[0, 1, 2, 3]);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(next_event(&mut session).await, S2sEvent::AssistantTranscript { text, is_final: false, .. } if &*text == "Hel")
    );
    assert!(
        matches!(next_event(&mut session).await, S2sEvent::AssistantTranscript { text, is_final: true, .. } if &*text == "Hello")
    );
    assert!(
        matches!(next_event(&mut session).await, S2sEvent::AudioDone { item, .. } if &*item == "item_1")
    );
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::ResponseDone {
            status: S2sResponseStatus::Completed,
            ..
        }
    ));
}

/// The tool-call round trip at the protocol level: the call arrives (once, although the server
/// reports it twice), the result goes back as a `function_call_output`, and `response.create`
/// follows so the model speaks it.
#[tokio::test]
async fn a_tool_call_is_delivered_once_and_answered_with_output_plus_response_create() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();

    mock.push(0, json!({"type": "response.created", "response": {"id": "resp_2", "status": "in_progress", "output": []}}));
    mock.push(0, json!({"type": "response.function_call_arguments.done", "response_id": "resp_2", "item_id": "item_f", "output_index": 0, "call_id": "call_42", "name": "get_weather", "arguments": "{\"city\":\"Ankara\"}"}));
    mock.push(0, json!({"type": "response.done", "response": {"id": "resp_2", "status": "completed", "output": [
        {"type": "function_call", "call_id": "call_42", "name": "get_weather", "arguments": "{\"city\":\"Ankara\"}"}
    ]}}));

    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::ResponseStarted { .. }
    ));
    match next_event(&mut session).await {
        S2sEvent::ToolCall {
            call_id,
            name,
            arguments,
            response,
        } => {
            assert_eq!(&*call_id, "call_42");
            assert_eq!(&*name, "get_weather");
            assert_eq!(&*arguments, r#"{"city":"Ankara"}"#);
            assert_eq!(&*response, "resp_2");
        }
        other => panic!("{other:?}"),
    }
    // The next event is the end of the response, not a second copy of the call.
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::ResponseDone {
            status: S2sResponseStatus::Completed,
            ..
        }
    ));

    session
        .control
        .send(S2sControl::ToolResult {
            call_id: Arc::from("call_42"),
            output: Arc::from(r#"{"temp":21}"#),
        })
        .await
        .unwrap();
    session
        .control
        .send(S2sControl::CreateResponse)
        .await
        .unwrap();
    mock.wait("output and response.create", |m| {
        !m.received_of(0, "response.create").is_empty()
    })
    .await;
    let order: Vec<String> = mock
        .received(0)
        .iter()
        .map(|v| v["type"].as_str().unwrap().to_owned())
        .collect();
    let output_at = order
        .iter()
        .position(|t| t == "conversation.item.create")
        .unwrap();
    let create_at = order.iter().position(|t| t == "response.create").unwrap();
    assert!(output_at < create_at, "{order:?}");
}

/// The barge-in round trip at the protocol level: the model's audio is playing, the server's VAD
/// reports the caller speaking, the response ends cancelled, and the client cuts the item to what
/// was heard.
#[tokio::test]
async fn barge_in_ends_the_response_cancelled_and_the_client_truncates_the_item() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();

    mock.push(0, json!({"type": "response.created", "response": {"id": "resp_3", "status": "in_progress", "output": []}}));
    mock.push(0, json!({"type": "response.output_audio.delta", "response_id": "resp_3", "item_id": "item_3", "output_index": 0, "content_index": 0, "delta": "AAAA"}));
    mock.push(0, json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 5000, "item_id": "u2"}));
    mock.push(0, json!({"type": "response.done", "response": {"id": "resp_3", "status": "cancelled", "status_details": {"type": "cancelled", "reason": "turn_detected"}, "output": []}}));

    let mut item = None;
    let mut saw_speech = false;
    loop {
        match next_event(&mut session).await {
            S2sEvent::Audio { item: i, .. } => item = Some(i),
            S2sEvent::SpeechStarted => saw_speech = true,
            S2sEvent::ResponseDone { status, .. } => {
                assert_eq!(status, S2sResponseStatus::Cancelled);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_speech);
    session
        .control
        .send(S2sControl::Truncate {
            item: item.unwrap(),
            audio_end: Duration::from_millis(750),
        })
        .await
        .unwrap();
    mock.wait("the truncate", |m| {
        !m.received_of(0, "conversation.item.truncate").is_empty()
    })
    .await;
    let truncate = mock.received_of(0, "conversation.item.truncate").remove(0);
    assert_eq!(truncate["item_id"], "item_3");
    assert_eq!(truncate["audio_end_ms"], 750);
}

// ---------------------------------------------------------------------------------------------
// Errors and a lost connection
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn errors_answering_our_cancel_are_swallowed_and_others_are_warnings() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();

    session
        .control
        .send(S2sControl::CancelResponse)
        .await
        .unwrap();
    mock.wait("the cancel", |m| {
        !m.received_of(0, "response.cancel").is_empty()
    })
    .await;
    let cancel = mock.received_of(0, "response.cancel").remove(0);
    let event_id = cancel["event_id"].as_str().unwrap().to_owned();
    // The cancel raced a response that had already finished.
    mock.push(0, json!({"type": "error", "error": {"type": "invalid_request_error", "code": "response_cancel_not_active", "message": "nothing to cancel", "event_id": event_id}}));
    // An unrelated problem.
    mock.push(0, json!({"type": "error", "error": {"type": "server_error", "code": "overloaded", "message": "try later", "event_id": "someone_else"}}));

    match next_event(&mut session).await {
        S2sEvent::Warning { code, message } => {
            assert_eq!(&*code, "overloaded");
            assert_eq!(&*message, "try later");
        }
        other => panic!("the racy error must be swallowed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_lost_connection_ends_the_stream_with_an_error() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    lock(&mock.shared.injectors)[0]
        .send(Message::Close(None))
        .unwrap();
    match next_event(&mut session).await {
        S2sEvent::Error(error) => assert!(
            matches!(error, Error::Disconnected { .. } | Error::Rejected { .. }),
            "{error:?}"
        ),
        other => panic!("{other:?}"),
    }
    let end = tokio::time::timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap();
    assert!(end.is_none(), "no event after the error");
}

#[tokio::test]
async fn a_malformed_server_message_ends_the_session() {
    let mock = Mock::start(vec![]).await;
    let mut session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    lock(&mock.shared.injectors)[0]
        .send(Message::text("this is not json"))
        .unwrap();
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::Error(Error::Protocol { .. })
    ));
}

#[tokio::test]
async fn a_new_session_after_a_lost_one_is_a_fresh_connection() {
    // The adapter does not reconnect by itself; the engine opens a second session.
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut first = provider.connect(s2s_config()).await.unwrap();
    lock(&mock.shared.injectors)[0]
        .send(Message::Close(None))
        .unwrap();
    assert!(matches!(next_event(&mut first).await, S2sEvent::Error(_)));
    let mut second = provider.connect(s2s_config()).await.unwrap();
    mock.push(
        1,
        json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 0, "item_id": "u"}),
    );
    assert!(matches!(
        next_event(&mut second).await,
        S2sEvent::SpeechStarted
    ));
    assert_eq!(mock.shared.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn keepalive_pings_go_out_when_the_server_is_quiet_and_the_pongs_keep_the_session_alive() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|cfg| {
        cfg.keepalive_interval = Duration::from_millis(100);
        cfg.io_timeout = Duration::from_millis(400);
    });
    let mut session = provider.connect(s2s_config()).await.unwrap();
    // Several probes over more than one io_timeout: only a working Pong round trip keeps the
    // session open this long.
    mock.wait("three pings", |m| lock(&m.shared.pings).len() >= 3)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(lock(&mock.shared.pings).iter().all(|i| *i == 0));
    mock.push(
        0,
        json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 0, "item_id": "u"}),
    );
    assert!(matches!(
        next_event(&mut session).await,
        S2sEvent::SpeechStarted
    ));
}

#[tokio::test]
async fn outbound_audio_does_not_postpone_the_liveness_probe() {
    // Streaming caller audio into a dead peer must not hide the missing answers.
    let mock = Mock::start(vec![Behavior {
        stall: true,
        ..Behavior::default()
    }])
    .await;
    let provider = mock.provider(|cfg| {
        cfg.keepalive_interval = Duration::from_millis(100);
        cfg.io_timeout = Duration::from_millis(300);
    });
    let mut session = provider.connect(s2s_config()).await.unwrap();
    let audio = session.audio.clone();
    let feeder = tokio::spawn(async move {
        while audio.send(Bytes::from_static(&[0; 320])).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let started = Instant::now();
    match next_event(&mut session).await {
        S2sEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. })) => {}
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    feeder.abort();
}

#[tokio::test]
async fn a_half_open_connection_is_detected_and_ends_the_stream_with_a_timeout() {
    let mock = Mock::start(vec![Behavior {
        stall: true,
        ..Behavior::default()
    }])
    .await;
    let provider = mock.provider(|cfg| {
        cfg.keepalive_interval = Duration::from_millis(100);
        cfg.io_timeout = Duration::from_millis(300);
    });
    let mut session = provider.connect(s2s_config()).await.unwrap();
    let started = Instant::now();
    match next_event(&mut session).await {
        S2sEvent::Error(Error::Core(brisvox_core::Error::Timeout { .. })) => {}
        other => panic!("{other:?}"),
    }
    // Quiet for 100 ms, then 300 ms for the Pong.
    assert!(started.elapsed() >= Duration::from_millis(350));
    assert!(started.elapsed() < Duration::from_secs(2));
    let end = tokio::time::timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap();
    assert!(end.is_none(), "no event after the error");
}

#[tokio::test]
async fn audio_queued_before_a_commit_is_sent_before_it() {
    // Control is served before audio; a commit must still follow the audio queued ahead of it.
    let mock = Mock::start(vec![]).await;
    let session = mock.provider(|_| {}).connect(s2s_config()).await.unwrap();
    for _ in 0..5 {
        session
            .audio
            .try_send(Bytes::from_static(&[1; 320]))
            .unwrap();
    }
    session.control.try_send(S2sControl::Commit).unwrap();
    mock.wait("the commit", |m| {
        !m.received_of(0, "input_audio_buffer.commit").is_empty()
    })
    .await;
    let kinds: Vec<String> = mock
        .received(0)
        .iter()
        .map(|v| v["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        [
            "session.update",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
            "input_audio_buffer.commit",
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Re-seeding a new session
// ---------------------------------------------------------------------------------------------

fn history() -> Vec<S2sHistoryItem> {
    vec![
        S2sHistoryItem::User(Arc::from("Ankara'da hava nasil?")),
        S2sHistoryItem::ToolCall {
            call_id: Arc::from("call_1"),
            name: Arc::from("get_weather"),
            arguments: Arc::from(r#"{"city":"Ankara"}"#),
        },
        S2sHistoryItem::ToolResult {
            call_id: Arc::from("call_1"),
            output: Arc::from(r#"{"temp":21}"#),
        },
        S2sHistoryItem::Assistant(Arc::from("Ankara'da 21 derece.")),
        S2sHistoryItem::Note(Arc::from("late result")),
    ]
}

#[tokio::test]
async fn a_new_session_is_seeded_with_the_history_before_any_audio() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|_| {});
    let mut config = s2s_config();
    config.history = history();
    let session = provider.connect(config).await.unwrap();
    session
        .audio
        .send(Bytes::from_static(&[1, 2]))
        .await
        .unwrap();
    mock.wait("the audio", |m| {
        !m.received_of(0, "input_audio_buffer.append").is_empty()
    })
    .await;
    let received = mock.received(0);
    let kinds: Vec<&str> = received
        .iter()
        .map(|v| v["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "session.update",
            "conversation.item.create",
            "conversation.item.create",
            "conversation.item.create",
            "conversation.item.create",
            "conversation.item.create",
            "input_audio_buffer.append"
        ]
    );
    let item = |n: usize| received[n]["item"].clone();
    assert_eq!(item(1)["role"], "user");
    assert_eq!(item(1)["content"][0]["type"], "input_text");
    assert_eq!(item(2)["type"], "function_call");
    assert_eq!(item(2)["call_id"], "call_1");
    assert_eq!(item(2)["arguments"], r#"{"city":"Ankara"}"#);
    assert_eq!(item(3)["type"], "function_call_output");
    assert_eq!(item(3)["output"], r#"{"temp":21}"#);
    assert_eq!(item(4)["role"], "assistant");
    assert_eq!(item(4)["content"][0]["type"], "output_text");
    assert_eq!(item(5)["role"], "system");
}

#[tokio::test]
async fn assistant_history_can_be_seeded_as_a_system_message() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|cfg| cfg.seed_assistant_as_system = true);
    let mut config = s2s_config();
    config.history = history();
    let _session = provider.connect(config).await.unwrap();
    mock.wait("the history", |m| {
        m.received_of(0, "conversation.item.create").len() == 5
    })
    .await;
    let items: Vec<Value> = mock
        .received_of(0, "conversation.item.create")
        .into_iter()
        .map(|v| v["item"].clone())
        .collect();
    assert_eq!(items[3]["role"], "system");
    assert!(
        items[3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("The assistant said earlier:")
    );
}

#[tokio::test]
async fn an_overlong_history_fails_connect_and_the_session_limit_is_reported() {
    let mock = Mock::start(vec![]).await;
    let provider = mock.provider(|cfg| cfg.max_session = Duration::from_secs(3000));
    assert_eq!(provider.session_limit(), Some(Duration::from_secs(3000)));
    let mut config = s2s_config();
    config.history = vec![S2sHistoryItem::User(Arc::from("x")); 129];
    let error = provider.connect(config).await.unwrap_err();
    assert!(matches!(error, Error::Protocol { .. }), "{error:?}");
    assert_eq!(mock.shared.connections.load(Ordering::SeqCst), 0);
}
