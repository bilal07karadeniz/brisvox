//! Protocol-level tests of the Gemini adapter: the exact request sent, every server message
//! shape of `docs/providers/gemini.md`, error mapping, keep-alive and timeout behaviour,
//! connection reuse, redirects, signature replay and secrecy.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Error as CoreError, Secret, SystemClock};
use brisvox_providers::gemini::{FunctionCallingMode, GeminiConfig, GeminiLlm, ThinkingLevel};
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, ToolCall,
    ToolSpec, Usage,
};
use serde_json::{Value, json};

use crate::mock::{Mock, Script, Step};

const KEY: &str = "AIza-test-key-0123456789";

pub fn config(mock: &Mock) -> GeminiConfig {
    GeminiConfig::new(Secret::new(KEY.to_owned()))
        .with_base_url(mock.base_url())
        .with_timeout(Duration::from_secs(3))
}

pub fn provider(mock: &Mock) -> GeminiLlm {
    GeminiLlm::new(config(mock), SystemClock::shared()).expect("provider")
}

pub fn user(text: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, text)],
        tools: Vec::new(),
    }
}

/// Reads a stream to its end (a hang fails the test after five seconds).
pub async fn collect(mut stream: LlmStream) -> Vec<LlmEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("the stream ended")
}

pub async fn run(mock: &Mock, request: LlmRequest) -> Vec<LlmEvent> {
    collect(provider(mock).stream(request).await.expect("accepted")).await
}

pub fn text_of(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(t) => Some(&**t),
            _ => None,
        })
        .collect()
}

pub fn chunk(parts: Value, finish: Option<&str>, usage: Option<Value>) -> String {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    let mut chunk =
        json!({"candidates": [candidate], "modelVersion": "gemini-test", "responseId": "r1"});
    if let Some(usage) = usage {
        chunk["usageMetadata"] = usage;
    }
    chunk.to_string()
}

pub fn usage() -> Value {
    json!({"promptTokenCount": 120, "cachedContentTokenCount": 100,
           "candidatesTokenCount": 9, "thoughtsTokenCount": 1, "totalTokenCount": 130})
}

/// A final chunk that only ends the turn.
pub fn stop() -> String {
    chunk(json!([]), Some("STOP"), Some(usage()))
}

pub fn text_chunk(text: &str) -> Step {
    Step::event(chunk(json!([{"text": text}]), None, None))
}

pub fn only_done(mock: &Mock) {
    mock.script(Script::stream(vec![Step::event(stop())]));
}

pub fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Weather of a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}"#,
        ),
    }
}

#[tokio::test]
async fn the_request_on_the_wire_is_exactly_the_documented_one() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let mut cfg = config(&mock).with_model("gemini-3.8-flash");
    cfg.thinking_level = Some(ThinkingLevel::Low);
    cfg.max_output_tokens = Some(256);
    cfg.function_calling_mode = Some(FunctionCallingMode::Auto);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "You are a phone agent. Answer in Turkish."),
            Message::new(Role::User, "Hava nasil Londra'da?"),
        ],
        tools: vec![weather_tool()],
    };
    collect(llm.stream(request).await.expect("accepted")).await;

    let posts = mock.posts();
    let [post] = posts.as_slice() else {
        panic!("{posts:?}")
    };
    assert_eq!(
        post.path,
        "/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse"
    );
    assert_eq!(post.header("x-goog-api-key"), Some(KEY));
    assert!(!post.path.contains("AIza"), "the key is never in the URL");
    assert!(post.header("authorization").is_none());
    assert_eq!(post.header("content-type"), Some("application/json"));
    assert_eq!(post.header("accept"), Some("text/event-stream"));
    assert_eq!(
        post.json(),
        json!({
            "systemInstruction": {"parts": [{"text": "You are a phone agent. Answer in Turkish."}]},
            "tools": [{"functionDeclarations": [{
                "name": "get_weather",
                "description": "Weather of a city",
                "parameters": {"type": "OBJECT",
                               "properties": {"city": {"type": "STRING"}},
                               "required": ["city"]}
            }]}],
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
            "contents": [{"role": "user", "parts": [{"text": "Hava nasil Londra'da?"}]}],
            "generationConfig": {"maxOutputTokens": 256,
                                 "thinkingConfig": {"thinkingLevel": "low"}}
        })
    );
}

#[tokio::test]
async fn the_default_model_is_the_documented_fast_one_and_no_optional_fields_are_sent() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    run(&mock, user("hi")).await;
    let post = &mock.posts()[0];
    assert_eq!(
        post.path,
        "/v1beta/models/gemini-3.5-flash-lite:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        post.json(),
        json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]})
    );
}

#[tokio::test]
async fn the_warm_up_request_carries_no_credentials() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    llm.warm()
        .await
        .expect("any HTTP answer is a warm connection");
    let requests = mock.requests();
    let [warm] = requests.as_slice() else {
        panic!("{requests:?}")
    };
    assert_eq!(warm.method, "HEAD");
    assert!(warm.header("x-goog-api-key").is_none());
    assert!(!warm.path.contains("AIza"));
}

/// A stream with every message shape the adapter reads or ignores: text, thought summaries,
/// empty signed text, parallel whole function calls (one with a signature, one without an id),
/// comments, `event:` lines, CRLF and LF endings, a `[DONE]` line, cut into small HTTP chunks at
/// arbitrary byte positions.
#[tokio::test]
async fn every_documented_server_message_is_parsed_across_arbitrary_chunk_boundaries() {
    let mock = Mock::start(false).await;
    let wire = [
        ": keep-alive\n\n".to_owned(),
        format!(
            "event: message\r\ndata: {}\r\n\r\n",
            chunk(
                json!([{"text": "Let me think", "thought": true}, {"text": "Hel"}]),
                None,
                Some(json!({"promptTokenCount": 120}))
            )
        ),
        format!("data: {}\n\n", chunk(json!([{"text": "lo, "}]), None, None)),
        format!("data: {}\n\n", json!({"modelStatus": {"modelStage": "STABLE"}})),
        format!(
            "id: 7\ndata: {}\n\n",
            chunk(
                json!([
                    {"text": "world", "thoughtSignature": "TS1"},
                    {"functionCall": {"id": "fc1", "name": "get_weather", "args": {"city": "Paris"}},
                     "thoughtSignature": "SIG"},
                    {"functionCall": {"name": "get_time", "args": {}}}
                ]),
                None,
                None
            )
        ),
        format!(
            "data: {}\n\n",
            chunk(json!([{"text": "", "thoughtSignature": "TS2"}]), Some("STOP"), Some(usage()))
        ),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
    .into_bytes();
    let mut steps = vec![];
    for piece in wire.chunks(13) {
        steps.push(Step::raw(piece.to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(1)));
    }
    mock.script(Script::stream(steps));
    let events = run(&mock, user("hi")).await;

    assert_eq!(text_of(&events), "Hello, world");
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => Some((
                *index,
                id.as_deref().unwrap().to_owned(),
                name.as_deref().unwrap().to_owned(),
                arguments.to_string(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                0,
                "fc1".to_owned(),
                "get_weather".to_owned(),
                r#"{"city":"Paris"}"#.to_owned()
            ),
            (
                1,
                "brisvox_call_1".to_owned(),
                "get_time".to_owned(),
                "{}".to_owned()
            ),
        ]
    );
    let Some(LlmEvent::Done {
        finish,
        usage,
        provider_data,
    }) = events.last()
    else {
        panic!("{:?}", events.last())
    };
    assert_eq!(*finish, FinishReason::ToolCalls);
    assert_eq!(
        *usage,
        Some(Usage {
            input_tokens: 120,
            cached_input_tokens: 100,
            output_tokens: 10
        })
    );
    // The model turn as received: the signed text is kept apart, the made-up id is not recorded.
    let data: Value = serde_json::from_str(provider_data.as_deref().unwrap()).unwrap();
    assert_eq!(
        data,
        json!([
            {"text": "Hello, "},
            {"text": "world", "thoughtSignature": "TS1"},
            {"functionCall": {"id": "fc1", "name": "get_weather", "args": {"city": "Paris"}},
             "thoughtSignature": "SIG"},
            {"functionCall": {"name": "get_time", "args": {}}},
            {"text": "", "thoughtSignature": "TS2"}
        ])
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, LlmEvent::Done { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn text_only_responses_finish_with_stop_and_no_provider_data() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        text_chunk("Merhaba"),
        Step::event(chunk(
            json!([{"text": " dunya"}]),
            Some("STOP"),
            Some(usage()),
        )),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "Merhaba dunya");
    match events.last() {
        Some(LlmEvent::Done {
            finish: FinishReason::Stop,
            usage: Some(u),
            provider_data: None,
        }) => {
            assert_eq!(u.cached_input_tokens, 100);
            assert_eq!(u.input_tokens, 120);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn usage_in_a_later_chunk_or_not_at_all_still_ends_the_turn() {
    let mock = Mock::start(false).await;
    // finishReason first, usage in a trailing chunk.
    mock.script(Script::stream(vec![
        Step::event(chunk(json!([{"text": "a"}]), Some("STOP"), None)),
        Step::event(json!({"usageMetadata": usage()}).to_string()),
    ]));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Done { usage: Some(u), .. }) if u.input_tokens == 120
    ));
    // No usage anywhere: the clean end of the body completes the turn.
    mock.script(Script::stream(vec![Step::event(chunk(
        json!([{"text": "a"}]),
        Some("STOP"),
        None,
    ))]));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Done {
            finish: FinishReason::Stop,
            usage: None,
            ..
        })
    ));
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn a_truncated_answer_reports_length() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![Step::event(chunk(
        json!([{"text": "partial"}]),
        Some("MAX_TOKENS"),
        Some(usage()),
    ))]));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Done {
            finish: FinishReason::Length,
            ..
        })
    ));
}

#[tokio::test]
async fn http_refusals_map_to_rejected_with_the_status_and_are_not_retried() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let statuses = [400_u16, 401, 403, 404, 429, 500, 503, 504];
    for status in statuses {
        let body = json!({"error": {"code": status,
            "message": "API key not valid. Please pass a valid API key: AIza-test***",
            "status": "INVALID_ARGUMENT",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "API_KEY_INVALID", "domain": "googleapis.com"}]}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("Retry-After", "7");
        }
        mock.script(script);
        match llm.stream(user("hi")).await {
            Err(Error::Rejected {
                provider: "gemini",
                status: got,
            }) => assert_eq!(got, status),
            other => panic!("status {status}: {other:?}"),
        }
    }
    assert_eq!(
        mock.posts().len(),
        statuses.len(),
        "exactly one attempt per call: no retries"
    );
}

#[tokio::test]
async fn an_unparsable_error_body_still_maps_to_rejected() {
    let mock = Mock::start(false).await;
    mock.script(Script::status(502, "<html>bad gateway</html>"));
    assert!(matches!(
        provider(&mock).stream(user("hi")).await,
        Err(Error::Rejected { status: 502, .. })
    ));
}

#[tokio::test]
async fn a_stalled_error_body_is_cut_off_after_a_second_and_still_maps_to_rejected() {
    let mock = Mock::start(false).await;
    let body = json!({"error": {"code": 503, "status": "UNAVAILABLE"}}).to_string();
    mock.script(Script::status_then_stall(503, &body, 4096));
    let started = std::time::Instant::now();
    let result = provider(&mock).stream(user("hi")).await;
    let took = started.elapsed();
    assert!(
        matches!(result, Err(Error::Rejected { status: 503, .. })),
        "{result:?}"
    );
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_millis(2500),
        "the stalled body was read for {took:?}"
    );
}

#[tokio::test]
async fn redirects_are_not_followed_so_the_key_never_leaves() {
    let mock = Mock::start(false).await;
    mock.script(Script::status(307, "").header("Location", "http://127.0.0.1:1/steal"));
    assert!(matches!(
        provider(&mock).stream(user("hi")).await,
        Err(Error::Rejected { status: 307, .. })
    ));
    assert_eq!(mock.requests().len(), 1);
}

/// A scripted failure: its name, the body steps and what the final error must be.
type Case = (&'static str, Vec<Step>, fn(&Error) -> bool);

#[tokio::test]
async fn failures_inside_the_stream_arrive_as_error_events_then_the_stream_ends() {
    let mock = Mock::start(false).await;
    let cases: Vec<Case> = vec![
        (
            "in-band rate limit",
            vec![
                text_chunk("a"),
                Step::event(
                    json!({"error": {"code": 429, "message": "quota AIza-test-key",
                                     "status": "RESOURCE_EXHAUSTED"}})
                    .to_string(),
                ),
            ],
            |e| matches!(e, Error::Rejected { status: 429, .. }),
        ),
        (
            "in-band unavailable by status name",
            vec![Step::event(
                json!({"error": {"status": "UNAVAILABLE"}}).to_string(),
            )],
            |e| matches!(e, Error::Rejected { status: 503, .. }),
        ),
        (
            "blocked prompt",
            vec![Step::event(
                json!({"promptFeedback": {"blockReason": "SAFETY"},
                       "usageMetadata": {"promptTokenCount": 3}})
                .to_string(),
            )],
            |e| matches!(e, Error::Rejected { status: 400, .. }),
        ),
        (
            "filtered before any output",
            vec![Step::event(chunk(json!([]), Some("SAFETY"), Some(usage())))],
            |e| matches!(e, Error::Rejected { status: 400, .. }),
        ),
        (
            "malformed function call",
            vec![Step::event(chunk(
                json!([]),
                Some("MALFORMED_FUNCTION_CALL"),
                Some(usage()),
            ))],
            |e| matches!(e, Error::Protocol { detail, .. } if detail.contains("MALFORMED_FUNCTION_CALL")),
        ),
        (
            "missing thought signature",
            vec![Step::event(chunk(
                json!([]),
                Some("MISSING_THOUGHT_SIGNATURE"),
                Some(usage()),
            ))],
            |e| matches!(e, Error::Protocol { .. }),
        ),
        (
            "unsupported language",
            vec![Step::event(chunk(json!([]), Some("LANGUAGE"), None))],
            |e| matches!(e, Error::Protocol { detail, .. } if detail.contains("LANGUAGE")),
        ),
        (
            "invalid JSON",
            vec![text_chunk("a"), Step::raw(b"data: {oops\n\n".to_vec())],
            |e| matches!(e, Error::Protocol { .. }),
        ),
        (
            "call without a name",
            vec![Step::event(chunk(
                json!([{"functionCall": {"id": "x", "args": {}}}]),
                None,
                None,
            ))],
            |e| matches!(e, Error::Protocol { .. }),
        ),
        (
            "done sentinel before a finish reason",
            vec![text_chunk("a"), Step::raw(b"data: [DONE]\n\n".to_vec())],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
        (
            "body ends before a finish reason",
            vec![text_chunk("a")],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
        (
            "connection dropped mid-body",
            vec![text_chunk("a"), Step::Close],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
    ];
    for (name, steps, check) in cases {
        mock.script(Script::stream(steps));
        let events = run(&mock, user("hi")).await;
        match events.last() {
            Some(LlmEvent::Error(error)) => {
                assert!(check(error), "{name}: {error:?}");
                let shown = format!("{error} {error:?}");
                assert!(
                    !shown.contains("AIza") && !shown.contains("quota"),
                    "{name}: {shown}"
                );
            }
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, LlmEvent::Done { .. } | LlmEvent::Error(_)))
                .count(),
            1,
            "{name}: exactly one terminal event"
        );
    }
}

#[tokio::test]
async fn a_filtered_turn_that_already_spoke_ends_as_a_normal_stop() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        text_chunk("Well, "),
        Step::event(chunk(
            json!([{"text": "I"}]),
            Some("RECITATION"),
            Some(usage()),
        )),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "Well, I");
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Done {
            finish: FinishReason::Stop,
            ..
        })
    ));
}

#[tokio::test]
async fn an_event_larger_than_the_cap_is_a_protocol_error_not_unbounded_memory() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_event_bytes = 512;
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![Step::event(chunk(
        json!([{"text": "x".repeat(4096)}]),
        None,
        None,
    ))]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Error(Error::Protocol { .. }))
    ));
}

pub fn timeout_error(event: &LlmEvent) -> Option<&'static str> {
    match event {
        LlmEvent::Error(Error::Core(CoreError::Timeout { what, .. })) => Some(what),
        _ => None,
    }
}

#[tokio::test]
async fn silence_longer_than_the_idle_deadline_is_a_timeout_even_after_events() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        text_chunk("a"),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(events.last().and_then(timeout_error), Some("gemini idle"));
}

#[tokio::test]
async fn the_total_duration_of_a_response_is_capped_even_while_keep_alives_arrive() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_secs(5);
    cfg.first_output_timeout = Duration::from_secs(5);
    cfg.max_stream_duration = Duration::from_millis(400);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for _ in 0..40 {
        steps.push(Step::raw(b": ping\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(events.last().and_then(timeout_error), Some("gemini stream"));
}

#[tokio::test]
async fn response_headers_that_never_come_time_out_in_stream() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_byte_timeout = Duration::from_millis(300);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(30);
    mock.script(script);
    assert!(matches!(
        llm.stream(user("hi")).await,
        Err(Error::Core(CoreError::Timeout {
            what: "gemini first byte",
            ..
        }))
    ));
}

#[tokio::test]
async fn a_refused_connection_is_a_disconnect_not_a_hang() {
    let mock = Mock::start(false).await;
    let cfg = config(&mock).with_base_url("http://127.0.0.1:1");
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    // Concurrently: Windows takes about two seconds to report a refused loopback connect.
    let (streamed, warmed) = tokio::join!(llm.stream(user("hi")), llm.warm());
    assert!(matches!(streamed, Err(Error::Disconnected { .. })));
    assert!(matches!(warmed, Err(Error::Disconnected { .. })));
}

#[tokio::test]
async fn keep_alive_comments_do_not_hold_the_idle_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![text_chunk("a")];
    for _ in 0..60 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(100)));
    }
    mock.script(Script::stream(steps));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(events.last().and_then(timeout_error), Some("gemini idle"));
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn comments_and_chunks_without_output_do_not_extend_the_first_output_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_output_timeout = Duration::from_millis(400);
    cfg.idle_timeout = Duration::from_secs(5);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for _ in 0..60 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::event(
            json!({"usageMetadata": {"promptTokenCount": 3}}).to_string(),
        ));
        steps.push(Step::Sleep(Duration::from_millis(100)));
    }
    mock.script(Script::stream(steps));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("gemini first output")
    );
    assert!(start.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn warm_and_finished_responses_leave_one_pooled_connection_for_the_next_request() {
    let mock = Mock::start(true).await;
    let llm = provider(&mock);
    llm.warm().await.expect("warm");
    assert_eq!(mock.connections(), 1);
    for _ in 0..3 {
        only_done(&mock);
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    }
    assert_eq!(
        mock.connections(),
        1,
        "warm() and three streams share the connection the warm-up opened"
    );
}

#[tokio::test]
async fn dropping_the_stream_closes_the_connection_and_the_server_stops() {
    let mock = Mock::start(true).await;
    let llm = provider(&mock);
    let delta = chunk(json!([{"text": "more"}]), None, None);
    let mut steps = vec![Step::counted(chunk(json!([{"text": "first"}]), None, None))];
    for _ in 0..100 {
        steps.push(Step::Sleep(Duration::from_millis(20)));
        steps.push(Step::counted(delta.clone()));
    }
    mock.script(Script::stream(steps));
    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    assert!(matches!(stream.next().await, Some(LlmEvent::TextDelta(_))));
    drop(stream);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sent = mock.events_sent();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.events_sent(),
        sent,
        "the server stopped after the hang-up"
    );
    assert!(
        sent < 20,
        "{sent} events were generated for a cancelled response"
    );
}

#[tokio::test]
async fn a_server_that_keeps_the_body_open_after_the_finish_is_hung_up_on_after_the_drain_timeout()
{
    // The turn is complete (finishReason and usage); the server then neither ends the body nor
    // closes. The reader must not wait for it: the events were delivered already, and after
    // DRAIN_TIMEOUT (500 ms) the connection is dropped.
    let mock = Mock::start(true).await;
    mock.script(Script::stream(vec![
        text_chunk("done"),
        Step::event(stop()),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "done");
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(4)).await;
    assert!(
        took.is_some_and(|t| t < Duration::from_millis(1800)),
        "hung up after {took:?}; the drain must give up after about 500 ms"
    );
}

#[tokio::test]
async fn a_server_that_keeps_sending_after_the_finish_is_hung_up_on_after_the_drain_limit() {
    // More than DRAIN_LIMIT (16 KiB) of comments right behind the finish, then silence: the
    // limit, not the 500 ms timeout, ends the drain.
    let mock = Mock::start(true).await;
    let mut steps = vec![Step::event(stop())];
    let filler = format!(": {}\n\n", "x".repeat(1000)).into_bytes();
    for _ in 0..40 {
        steps.push(Step::raw(filler.clone()));
    }
    steps.push(Step::Sleep(Duration::from_secs(30)));
    mock.script(Script::stream(steps));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(4)).await;
    assert!(
        took.is_some_and(|t| t < Duration::from_millis(300)),
        "hung up after {took:?}; the drain limit must end it well before the 500 ms timeout"
    );
}

#[tokio::test]
async fn turkish_text_in_raw_utf8_survives_any_chunk_boundary() {
    // Raw (not \u-escaped) 2- and 3-byte characters in text, a signature-free call and a
    // function argument, delivered one byte per HTTP chunk: every multi-byte character is
    // split at every possible position.
    let said = "Merhaba! İstanbul'da şu an hava çok güzel; ağaçlar yeşil, öğle yemeği için ıspanak. ₺150 — “tamam”";
    assert!(said.len() > said.chars().count() + 10);
    let mock = Mock::start(false).await;
    let wire = [
        format!(
            "data: {}\n\n",
            chunk(json!([{"text": "Merhaba! İstanbul'da şu an hava "}]), None, None)
        ),
        format!(
            "data: {}\n\n",
            chunk(
                json!([{"text": "çok güzel; ağaçlar yeşil, öğle yemeği için ıspanak. ₺150 — “tamam”"}]),
                None,
                None
            )
        ),
        format!(
            "data: {}\n\n",
            chunk(
                json!([{"functionCall": {"id": "c1", "name": "get_weather",
                                         "args": {"city": "Çanakkale", "not": "günlük ışık"}}}]),
                Some("STOP"),
                Some(usage())
            )
        ),
    ]
    .concat()
    .into_bytes();
    assert!(std::str::from_utf8(&wire).unwrap().contains("İstanbul"));
    let steps: Vec<Step> = wire.chunks(1).map(|b| Step::raw(b.to_vec())).collect();
    mock.script(Script::stream(steps));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), said);
    let args = events.iter().find_map(|e| match e {
        LlmEvent::ToolCallDelta { arguments, .. } => Some(arguments.to_string()),
        _ => None,
    });
    assert_eq!(
        args.as_deref()
            .map(|a| serde_json::from_str::<Value>(a).unwrap()),
        Some(json!({"city": "Çanakkale", "not": "günlük ışık"}))
    );
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

/// Waits up to `limit` for the mock to see `n` ended connections; returns how long it took.
pub async fn wait_for_hangups(mock: &Mock, n: usize, limit: Duration) -> Option<Duration> {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        if mock.closed_connections() >= n {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

#[tokio::test]
async fn dropping_the_stream_while_the_server_is_silent_hangs_up_at_once() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_secs(10);
    cfg.first_output_timeout = Duration::from_secs(10);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::counted(chunk(json!([{"text": "first"}]), None, None)),
        Step::Sleep(Duration::from_secs(30)),
        Step::counted(chunk(json!([{"text": "never"}]), None, None)),
    ]));
    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    assert!(matches!(stream.next().await, Some(LlmEvent::TextDelta(_))));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.closed_connections(), 0, "the server is still waiting");
    drop(stream);
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(2)).await;
    assert!(
        took.is_some_and(|t| t < Duration::from_millis(300)),
        "hang-up after {took:?}"
    );
    assert_eq!(mock.events_sent(), 1);
}

#[tokio::test]
async fn dropping_the_stream_future_before_the_headers_arrive_aborts_the_request() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(30);
    mock.script(script);
    let attempt = tokio::time::timeout(Duration::from_millis(150), llm.stream(user("hi"))).await;
    assert!(attempt.is_err(), "the server never answers");
    assert_eq!(mock.posts().len(), 1);
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(2)).await;
    assert!(
        took.is_some_and(|t| t < Duration::from_millis(300)),
        "hang-up after {took:?}"
    );
}

#[tokio::test]
async fn a_slow_consumer_gets_every_event_in_order_and_is_not_blamed_by_the_idle_timer() {
    // 600 deltas (about ten times the channel capacity) arrive in three socket writes while the
    // consumer is not reading for longer than the idle deadline.
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.first_output_timeout = Duration::from_millis(300);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for block in 0..3 {
        let mut wire = String::new();
        for i in 0..200 {
            let n = block * 200 + i;
            wire.push_str(&format!(
                "data: {}\n\n",
                chunk(json!([{"text": format!("<{n}>")}]), None, None)
            ));
        }
        steps.push(Step::raw(wire.into_bytes()));
    }
    steps.push(Step::event(stop()));
    mock.script(Script::stream(steps));

    let stream = llm.stream(user("hi")).await.expect("accepted");
    tokio::time::sleep(Duration::from_millis(900)).await;
    let events = collect(stream).await;

    let expected: String = (0..600).map(|n| format!("<{n}>")).collect();
    assert_eq!(text_of(&events), expected, "no delta lost or reordered");
    assert_eq!(events.len(), 601, "{:?}", events.last());
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

/// The first response of a tool conversation: text, a signed call, a made-up-id call.
async fn first_turn(mock: &Mock) -> (Vec<LlmEvent>, Arc<str>) {
    mock.script(Script::stream(vec![Step::event(chunk(
        json!([
            {"text": "Checking. "},
            {"functionCall": {"id": "fc1", "name": "get_weather", "args": {"city": "Paris"}},
             "thoughtSignature": "SIG-ONE"},
            {"functionCall": {"name": "get_time", "args": {}}}
        ]),
        Some("STOP"),
        Some(usage()),
    ))]));
    let events = run(mock, user("weather and time?")).await;
    let Some(LlmEvent::Done {
        provider_data: Some(data),
        ..
    }) = events.last()
    else {
        panic!("{:?}", events.last())
    };
    let data = Arc::clone(data);
    (events, data)
}

fn conversation(data: Arc<str>, content: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![
            Message::new(Role::System, "Be terse."),
            Message::new(Role::User, "weather and time?"),
            Message::assistant(
                content,
                vec![
                    ToolCall {
                        id: Arc::from("fc1"),
                        name: Arc::from("get_weather"),
                        arguments: Arc::from(r#"{"city":"Paris"}"#),
                    },
                    ToolCall {
                        id: Arc::from("brisvox_call_1"),
                        name: Arc::from("get_time"),
                        arguments: Arc::from("{}"),
                    },
                ],
                Some(data),
            ),
            // Answered in the opposite order on purpose.
            Message::tool_result("brisvox_call_1", "12:00"),
            Message::tool_result("fc1", r#"{"temperature_c":18}"#),
        ],
        tools: vec![weather_tool()],
    }
}

#[tokio::test]
async fn function_calls_and_signatures_round_trip_through_provider_data() {
    let mock = Mock::start(false).await;
    let (_, data) = first_turn(&mock).await;
    only_done(&mock);
    run(&mock, conversation(data, "Checking. ")).await;
    let second = &mock.posts()[1].json();
    assert_eq!(
        second["contents"],
        json!([
            {"role": "user", "parts": [{"text": "weather and time?"}]},
            {"role": "model", "parts": [
                {"text": "Checking. "},
                {"functionCall": {"id": "fc1", "name": "get_weather", "args": {"city": "Paris"}},
                 "thoughtSignature": "SIG-ONE"},
                {"functionCall": {"name": "get_time", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "fc1", "name": "get_weather",
                                      "response": {"temperature_c": 18}}},
                {"functionResponse": {"name": "get_time", "response": {"result": "12:00"}}}
            ]}
        ])
    );
}

#[tokio::test]
async fn an_interrupted_turn_replays_only_the_played_text_with_the_call_signature() {
    let mock = Mock::start(false).await;
    let (_, data) = first_turn(&mock).await;
    only_done(&mock);
    run(&mock, conversation(data, "Chec")).await;
    let second = &mock.posts()[1].json();
    let model = &second["contents"][1]["parts"];
    assert_eq!(model[0], json!({"text": "Chec"}));
    assert_eq!(model[1]["thoughtSignature"], json!("SIG-ONE"));
    assert!(
        !second.to_string().contains("Checking"),
        "the unplayed text must not go back to the model"
    );
}

#[tokio::test]
async fn a_history_without_provider_data_gets_the_documented_fallback_signature() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let mut request = conversation(Arc::from("[]"), "");
    if let Some(m) = request.messages.get_mut(2) {
        m.provider_data = None;
    }
    run(&mock, request).await;
    let second = &mock.posts()[0].json();
    assert_eq!(
        second["contents"][1]["parts"][0]["thoughtSignature"],
        json!("skip_thought_signature_validator")
    );
}

#[tokio::test]
async fn tools_and_history_are_byte_stable_across_turns_for_implicit_caching() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "Be terse."),
            Message::new(Role::User, "one"),
        ],
        tools: vec![weather_tool()],
    };
    only_done(&mock);
    collect(llm.stream(request.clone()).await.expect("accepted")).await;
    request.messages.push(Message::new(Role::Assistant, "ok"));
    request.messages.push(Message::new(Role::User, "two"));
    only_done(&mock);
    collect(llm.stream(request).await.expect("accepted")).await;
    let posts = mock.posts();
    let first = &posts[0].body;
    let second = &posts[1].body;
    // Everything but the closing of `contents` is a shared prefix.
    let cut = first.rfind("]}").expect("contents end");
    assert!(
        second.starts_with(&first[..cut]),
        "systemInstruction, tools and the first turn must be byte-identical:\n{first}\n{second}"
    );
}

#[tokio::test]
// That the request builder runs on the blocking pool, not on a worker, is proven by the unit
// tests in `src/gemini/mod.rs` (`OFFLOADED_BUILDS`); this test only proves the offloaded path
// still produces the right bytes.
async fn a_large_history_is_sent_intact() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let data: Arc<str> = Arc::from(
        json!([{"text": "x".repeat(200 * 1024)}, {"text": "", "thoughtSignature": "S"}])
            .to_string(),
    );
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::User, "hi"),
            Message::assistant("x".repeat(200 * 1024), Vec::new(), Some(data)),
            Message::new(Role::User, "again"),
        ],
        tools: Vec::new(),
    };
    run(&mock, request).await;
    let body = mock.posts()[0].json();
    assert_eq!(
        body["contents"][1]["parts"][0]["text"]
            .as_str()
            .map(str::len),
        Some(200 * 1024)
    );
    assert_eq!(
        body["contents"][1]["parts"][1]["thoughtSignature"],
        json!("S")
    );
}

#[tokio::test]
async fn nothing_secret_shows_up_in_debug_output_or_errors() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let shown = format!("{:?} {:?}", llm.config(), llm);
    assert!(
        !shown.contains("0123456789") && shown.contains("[REDACTED]"),
        "{shown}"
    );

    // A key that cannot be an HTTP header value is refused without echoing it.
    let bad = GeminiConfig::new(Secret::new("AIza-bad\nkey-SECRETPART".to_owned()));
    let error = GeminiLlm::new(bad, SystemClock::shared()).expect_err("invalid header");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("SECRETPART") && !shown.contains("AIza-bad"),
        "{shown}"
    );

    // Refusals echo key fragments in their message; the adapter's error must not.
    mock.script(Script::status(
        400,
        r#"{"error":{"code":400,"message":"API key not valid: AIza-test-key-0123***","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#,
    ));
    let error = llm.stream(user("hi")).await.expect_err("rejected");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("AIza") && !shown.contains("not valid"),
        "{shown}"
    );
}

#[tokio::test]
async fn requests_with_bad_tools_or_tool_messages_fail_before_any_network_io() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let tool = |name: &str, schema: &str| LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![ToolSpec {
            name: Arc::from(name),
            description: Arc::from("d"),
            parameters_schema: Arc::from(schema),
        }],
    };
    for bad in [
        tool("has space", r#"{"type":"object"}"#),
        tool("ok", "not json"),
        tool(
            "ok",
            r#"{"type":"object","properties":{"a":{"$ref":"http://x/y"}}}"#,
        ),
        tool("ok", r#"{"type":"string"}"#),
    ] {
        assert!(matches!(llm.stream(bad).await, Err(Error::Protocol { .. })));
    }
    let orphan = LlmRequest {
        messages: vec![Message::new(Role::Tool, "result without a call")],
        tools: Vec::new(),
    };
    assert!(matches!(
        llm.stream(orphan).await,
        Err(Error::Protocol { .. })
    ));
    let dangling = LlmRequest {
        messages: vec![
            Message::new(Role::User, "hi"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: Arc::from("c1"),
                    name: Arc::from("f"),
                    arguments: Arc::from("{}"),
                }],
                None,
            ),
        ],
        tools: Vec::new(),
    };
    assert!(matches!(
        llm.stream(dangling).await,
        Err(Error::Protocol { .. })
    ));
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn tool_schemas_can_be_sent_as_json_schema_unchanged() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let mut cfg = config(&mock);
    cfg.schema_mode = brisvox_providers::gemini::SchemaMode::JsonSchema;
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let request = LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![weather_tool()],
    };
    collect(llm.stream(request).await.expect("accepted")).await;
    let body = mock.posts()[0].json();
    let declaration = &body["tools"][0]["functionDeclarations"][0];
    assert!(declaration.get("parameters").is_none());
    assert_eq!(
        declaration["parametersJsonSchema"]["additionalProperties"],
        json!(false)
    );
}

#[tokio::test]
async fn the_adapter_identifies_itself() {
    let mock = Mock::start(false).await;
    assert_eq!(provider(&mock).name(), "gemini");
}

#[tokio::test]
async fn plain_http_is_refused_for_remote_hosts_unless_opted_in() {
    let key = || Secret::new(KEY.to_owned());
    let build = |url: &str, allow: bool| {
        let mut cfg = GeminiConfig::new(key()).with_base_url(url);
        cfg.allow_insecure_http = allow;
        GeminiLlm::new(cfg, SystemClock::shared())
    };
    for url in [
        "http://gateway.example.com",
        "http://10.0.0.5:8080",
        "http://127.0.0.1@evil.example.com",
        "http://localhost.evil.example.com",
    ] {
        let error = build(url, false).expect_err(url);
        assert!(matches!(error, Error::Protocol { .. }), "{url}");
        assert!(
            !format!("{error}").contains("evil"),
            "the URL is not echoed"
        );
        assert!(build(url, true).is_ok(), "{url} with the opt-in");
    }
    for url in [
        "https://generativelanguage.googleapis.com",
        "https://gateway.example.com/prefix",
        "http://localhost:8080",
        "http://127.0.0.1:1234",
        "http://[::1]:8080",
    ] {
        assert!(build(url, false).is_ok(), "{url}");
    }
    for url in ["ftp://example.com", "not a url", "", "file:///etc/passwd"] {
        assert!(build(url, true).is_err(), "{url:?}");
    }
}

/// The HTTP client (and with it the connection pool) is shared by every adapter instance
/// of the process that talks to the same origin the same way: three instances (three
/// "sessions"), one TCP connection at the server.
#[tokio::test]
async fn adapter_instances_of_one_origin_share_the_connection_pool() {
    let mock = Mock::start(true).await;
    for _ in 0..3 {
        provider(&mock).warm().await.expect("warm");
    }
    assert_eq!(mock.connections(), 1, "one connection served all three");
    assert_eq!(mock.requests().len(), 3);
}
