//! Protocol-level tests of the Anthropic adapter: the exact request sent, every server event type
//! of `docs/providers/anthropic.md`, error mapping, ping/keep-alive and timeout behaviour,
//! connection reuse, redirects, thinking replay and secrecy.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Error as CoreError, Secret, SystemClock};
use brisvox_providers::anthropic::{
    AnthropicConfig, AnthropicLlm, CacheConfig, Effort, ThinkingMode, ToolChoice,
};
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, ToolCall,
    ToolSpec, Usage,
};
use serde_json::{Value, json};

use crate::mock::{Mock, Script, Step};
use crate::wire;

const KEY: &str = "sk-ant-test-key-0123456789";

fn config(mock: &Mock) -> AnthropicConfig {
    AnthropicConfig::new(Secret::new(KEY.to_owned()))
        .with_base_url(mock.base_url())
        .with_timeout(Duration::from_secs(3))
}

fn provider(mock: &Mock) -> AnthropicLlm {
    AnthropicLlm::new(config(mock), SystemClock::shared()).expect("provider")
}

fn user(text: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, text)],
        tools: Vec::new(),
    }
}

/// Reads a stream to its end (a hang fails the test after five seconds).
async fn collect(mut stream: LlmStream) -> Vec<LlmEvent> {
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

async fn run(mock: &Mock, request: LlmRequest) -> Vec<LlmEvent> {
    collect(provider(mock).stream(request).await.expect("accepted")).await
}

fn text_of(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(t) => Some(&**t),
            _ => None,
        })
        .collect()
}

/// The tool calls of a response, fragments joined: `(id, name, arguments)`.
fn calls_of(events: &[LlmEvent]) -> Vec<(String, String, String)> {
    let mut calls: Vec<(u32, String, String, String)> = Vec::new();
    for event in events {
        if let LlmEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } = event
        {
            if let Some(call) = calls.iter_mut().find(|c| c.0 == *index) {
                call.3.push_str(arguments);
            } else {
                calls.push((
                    *index,
                    id.as_deref().unwrap_or_default().to_owned(),
                    name.as_deref().unwrap_or_default().to_owned(),
                    arguments.to_string(),
                ));
            }
        }
    }
    calls.into_iter().map(|c| (c.1, c.2, c.3)).collect()
}

type DoneParts = (FinishReason, Option<Usage>, Option<Arc<str>>);

fn done_of(events: &[LlmEvent]) -> DoneParts {
    match events.last() {
        Some(LlmEvent::Done {
            finish,
            usage,
            provider_data,
        }) => (*finish, *usage, provider_data.clone()),
        other => panic!("no Done: {other:?} in {events:?}"),
    }
}

/// The end of a response: `message_delta` with `stop_reason`, then `message_stop`.
fn finish_steps(stop_reason: &str) -> Vec<Step> {
    vec![
        Step::event(wire::message_delta(stop_reason, 7)),
        Step::counted(wire::message_stop()),
    ]
}

/// A complete one-block text response.
fn text_response(text: &str) -> Vec<Step> {
    let mut steps = vec![
        Step::event(wire::message_start(10)),
        Step::event(wire::text_start(0)),
        Step::counted(wire::text_delta(0, text)),
        Step::event(wire::block_stop(0)),
    ];
    steps.extend(finish_steps("end_turn"));
    steps
}

fn only_done(mock: &Mock) {
    mock.script(Script::stream(finish_steps("end_turn")));
}

fn timeout_error(event: &LlmEvent) -> Option<&'static str> {
    match event {
        LlmEvent::Error(Error::Core(CoreError::Timeout { what, .. })) => Some(what),
        _ => None,
    }
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Weather of a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
        ),
    }
}

#[tokio::test]
async fn the_request_on_the_wire_is_exactly_the_documented_one() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let mut cfg = config(&mock);
    cfg.workspace_id = Some("wrkspc_1".to_owned());
    cfg.beta = vec!["beta-one".to_owned(), "beta-two".to_owned()];
    cfg.max_tokens = 256;
    cfg.effort = Some(Effort::Low);
    cfg.disable_parallel_tool_use = true;
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "You are a phone agent."),
            Message::new(Role::User, "Hallo"),
            Message::new(Role::Assistant, "Guten Tag."),
            Message::new(Role::User, "Wie geht's?"),
        ],
        tools: vec![weather_tool()],
    };
    collect(llm.stream(request).await.expect("accepted")).await;

    let posts = mock.posts();
    let [post] = posts.as_slice() else {
        panic!("{posts:?}")
    };
    assert_eq!(post.path, "/v1/messages");
    assert_eq!(post.header("x-api-key"), Some(KEY));
    assert_eq!(post.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(post.header("content-type"), Some("application/json"));
    assert_eq!(post.header("accept"), Some("text/event-stream"));
    assert_eq!(post.header("anthropic-workspace-id"), Some("wrkspc_1"));
    assert_eq!(post.header("anthropic-beta"), Some("beta-one,beta-two"));
    assert!(post.header("authorization").is_none());
    assert!(
        post.header("user-agent")
            .is_some_and(|ua| ua.starts_with("brisvox/"))
    );
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(
        post.json(),
        json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 256,
            "stream": true,
            "thinking": {"type": "disabled"},
            "output_config": {"effort": "low"},
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "tools": [{
                "name": "get_weather",
                "description": "Weather of a city",
                "input_schema": serde_json::from_str::<Value>(
                    r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#
                ).unwrap(),
                "cache_control": ephemeral,
            }],
            "system": [{"type": "text", "text": "You are a phone agent.", "cache_control": ephemeral}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hallo"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Guten Tag.", "cache_control": ephemeral}]},
                {"role": "user", "content": [{"type": "text", "text": "Wie geht's?"}]},
            ],
        })
    );
}

#[tokio::test]
async fn caching_the_model_and_the_thinking_mode_are_configuration() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.model = "some-future-model".to_owned();
    cfg.thinking = ThinkingMode::BetweenTools;
    cfg.cache = CacheConfig::OFF;
    cfg.tool_choice = ToolChoice::Tool("get_weather".to_owned());
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    only_done(&mock);
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "Persona."),
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ],
        tools: vec![weather_tool()],
    };
    collect(llm.stream(request).await.expect("accepted")).await;
    let body = mock.posts()[0].json();
    assert_eq!(body["model"], "some-future-model");
    assert_eq!(body["thinking"], json!({"type": "between_tools"}));
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": "get_weather"})
    );
    assert!(!body.to_string().contains("cache_control"));
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
    assert!(warm.header("x-api-key").is_none());
    assert!(warm.header("authorization").is_none());
}

/// A stream with every event type the adapter reads or ignores (thinking with a signature, a
/// redacted block, text, a tool call in lumps, server-tool and citation noise, pings, comments,
/// a `[DONE]` sentinel), with `event:` lines, CRLF and LF endings, cut into small HTTP chunks at
/// arbitrary byte positions.
#[tokio::test]
async fn every_documented_server_event_is_parsed_across_arbitrary_chunk_boundaries() {
    let mock = Mock::start(false).await;
    let events = [
        wire::message_start_with(json!({"input_tokens": 25, "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 2000, "output_tokens": 1,
            "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 0}})),
        wire::ping(),
        wire::thinking_start(0),
        wire::thinking_delta(0, "I need to find "),
        wire::thinking_delta(0, "the answer."),
        wire::signature_delta(0, "EqQBCgIYAhIM=="),
        wire::block_stop(0),
        wire::ping(),
        wire::redacted_start(1, "REDACTED=="),
        wire::block_stop(1),
        json!({"type": "content_block_start", "index": 2,
               "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}})
        .to_string(),
        json!({"type": "content_block_delta", "index": 2,
               "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"x\"}"}})
        .to_string(),
        wire::block_stop(2),
        wire::text_start(3),
        wire::text_delta(3, "Merhaba, "),
        wire::text_delta(3, "Ada! \u{1F44B} \"quoted\"\nline"),
        json!({"type": "content_block_delta", "index": 3,
               "delta": {"type": "citations_delta", "citation": {"type": "x"}}})
        .to_string(),
        wire::block_stop(3),
        wire::tool_start(4, "toolu_01T1x1fJ", "get_weather"),
        wire::json_delta(4, ""),
        wire::json_delta(4, "{\"ci"),
        wire::json_delta(4, "ty\":"),
        wire::json_delta(4, " \"Paris\"}"),
        wire::block_stop(4),
        json!({"type": "some_future_event", "x": 1}).to_string(),
        wire::message_delta_with(
            "tool_use",
            json!({"output_tokens": 30, "server_tool_use": {"web_search_requests": 1},
                   "output_tokens_details": {}}),
        ),
        wire::message_stop(),
    ];
    let mut wire_text = String::from(": hello\n\nretry: 3000\n\n");
    for (i, event) in events.iter().enumerate() {
        let name = serde_json::from_str::<Value>(event).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned();
        match i % 3 {
            0 => wire_text.push_str(&format!("event: {name}\ndata: {event}\n\n")),
            1 => wire_text.push_str(&format!("data: {event}\r\n\r\n: keep-alive\r\n\r\n")),
            _ => wire_text.push_str(&format!("id: {i}\ndata: {event}\n\n")),
        }
    }
    wire_text.push_str("data: [DONE]\n\n");
    let steps = wire_text
        .as_bytes()
        .chunks(37)
        .map(|c| Step::raw(c.to_vec()))
        .collect();
    mock.script(Script::stream(steps));

    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "Merhaba, Ada! \u{1F44B} \"quoted\"\nline");
    assert_eq!(
        calls_of(&events),
        vec![(
            "toolu_01T1x1fJ".to_owned(),
            "get_weather".to_owned(),
            "{\"city\": \"Paris\"}".to_owned()
        )],
        "the server tool block is ignored, the call has dense index 0"
    );
    let fragments = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolCallDelta { .. }))
        .count();
    assert_eq!(fragments, 4, "start, then the three non-empty fragments");
    let (finish, usage, data) = done_of(&events);
    assert_eq!(finish, FinishReason::ToolCalls);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 25 + 100 + 2000,
            cached_input_tokens: 2000,
            output_tokens: 30
        })
    );
    let data: Value = serde_json::from_str(data.as_deref().expect("provider data")).unwrap();
    assert_eq!(
        data,
        json!([
            {"type": "thinking", "thinking": "I need to find the answer.", "signature": "EqQBCgIYAhIM=="},
            {"type": "redacted_thinking", "data": "REDACTED=="},
            {"type": "text"},
            {"type": "tool_use", "id": "toolu_01T1x1fJ", "name": "get_weather"},
        ])
    );
    assert_eq!(events.len(), 2 + fragments + 1, "text x2, call x4, Done");
}

#[tokio::test]
async fn text_only_responses_finish_with_stop_usage_and_no_provider_data() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(text_response("Hello")));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "Hello");
    let (finish, usage, data) = done_of(&events);
    assert_eq!(finish, FinishReason::Stop);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 7
        })
    );
    assert!(data.is_none());
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn the_stop_reason_table_maps_to_finish_reasons() {
    let mock = Mock::start(false).await;
    for (reason, expected) in [
        ("end_turn", FinishReason::Stop),
        ("stop_sequence", FinishReason::Stop),
        ("tool_use", FinishReason::ToolCalls),
        ("max_tokens", FinishReason::Length),
        ("model_context_window_exceeded", FinishReason::Length),
        ("refusal", FinishReason::Stop),
        ("pause_turn", FinishReason::Stop),
    ] {
        mock.script(Script::stream(finish_steps(reason)));
        let events = run(&mock, user("hi")).await;
        assert_eq!(done_of(&events).0, expected, "{reason}");
    }
}

#[tokio::test]
async fn an_empty_end_turn_is_a_clean_done_without_text() {
    // The documented "empty response": 2-3 tokens, no content, stop_reason end_turn.
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::message_start(500)),
        Step::event(wire::message_delta("end_turn", 3)),
        Step::counted(wire::message_stop()),
    ]));
    let events = run(&mock, user("hi")).await;
    assert!(text_of(&events).is_empty());
    assert_eq!(done_of(&events).0, FinishReason::Stop);
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn a_message_stop_without_a_message_delta_still_ends_with_done() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::event(wire::text_delta(0, "ok")),
        Step::event(wire::message_stop()),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "ok");
    let (finish, usage, _) = done_of(&events);
    assert_eq!(finish, FinishReason::Stop);
    assert!(usage.is_none());
}

#[tokio::test]
async fn http_refusals_map_to_rejected_with_the_status_and_are_not_retried() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let statuses = [400_u16, 401, 402, 403, 404, 413, 429, 500, 504, 529];
    for status in statuses {
        let body = json!({"type": "error", "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy",
            "error": {"type": "authentication_error",
                      "message": "invalid x-api-key: sk-ant-test-key-0123***"}})
        .to_string();
        let mut script = Script::status(status, &body).header("request-id", "req_011CSHoEeq");
        if status == 429 {
            script = script.header("retry-after", "7");
        }
        mock.script(script);
        match llm.stream(user("hi")).await {
            Err(Error::Rejected {
                provider: "anthropic",
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
    // The head announces 4 KiB, 60 bytes arrive, then silence: without the 1 s bound on reading
    // the error body `stream()` would sit here until some other timeout (3 s here) fired.
    let mock = Mock::start(false).await;
    let body = json!({"type": "error", "error": {"type": "overloaded_error"}}).to_string();
    mock.script(Script::status_then_stall(529, &body, 4096));
    let started = std::time::Instant::now();
    let result = provider(&mock).stream(user("hi")).await;
    let took = started.elapsed();
    assert!(
        matches!(result, Err(Error::Rejected { status: 529, .. })),
        "{result:?}"
    );
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_millis(2500),
        "the stalled body was read for {took:?}"
    );
}

#[tokio::test]
async fn an_oversized_error_body_is_not_read_to_the_end() {
    let mock = Mock::start(false).await;
    let big = "x".repeat(64 * 1024);
    mock.script(Script::status_then_stall(500, &big, 16 * 1024 * 1024));
    let started = std::time::Instant::now();
    let result = provider(&mock).stream(user("hi")).await;
    assert!(matches!(result, Err(Error::Rejected { status: 500, .. })));
    assert!(started.elapsed() < Duration::from_millis(600));
    mock.script(Script::status(500, &"y".repeat(1024 * 1024)));
    let started = std::time::Instant::now();
    let result = provider(&mock).stream(user("hi")).await;
    assert!(matches!(result, Err(Error::Rejected { status: 500, .. })));
    assert!(started.elapsed() < Duration::from_millis(600));
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
    let text = || {
        vec![
            Step::event(wire::text_start(0)),
            Step::event(wire::text_delta(0, "a")),
        ]
    };
    let with = |tail: Step| {
        let mut steps = text();
        steps.push(tail);
        steps
    };
    let cases: Vec<Case> = vec![
        (
            "overloaded_error after the 200",
            with(Step::event(wire::error_event("overloaded_error"))),
            |e| matches!(e, Error::Rejected { status: 529, .. }),
        ),
        (
            "rate_limit_error",
            with(Step::event(wire::error_event("rate_limit_error"))),
            |e| matches!(e, Error::Rejected { status: 429, .. }),
        ),
        (
            "api_error",
            with(Step::event(wire::error_event("api_error"))),
            |e| matches!(e, Error::Rejected { status: 500, .. }),
        ),
        (
            "invalid_request_error",
            with(Step::event(wire::error_event("invalid_request_error"))),
            |e| matches!(e, Error::Rejected { status: 400, .. }),
        ),
        (
            "authentication_error",
            with(Step::event(wire::error_event("authentication_error"))),
            |e| matches!(e, Error::Rejected { status: 401, .. }),
        ),
        (
            "an unknown error type",
            with(Step::event(wire::error_event("brand_new_error"))),
            |e| matches!(e, Error::Protocol { detail, .. } if detail.contains("brand_new_error")),
        ),
        (
            "unparsable JSON",
            with(Step::raw(b"data: {this is not json\n\n".to_vec())),
            |e| matches!(e, Error::Protocol { .. }),
        ),
        (
            "a fragment for a call that never started",
            with(Step::event(wire::json_delta(9, "{}"))),
            |e| matches!(e, Error::Protocol { .. }),
        ),
        ("the connection drops mid-stream", with(Step::Close), |e| {
            matches!(e, Error::Disconnected { .. })
        }),
        ("the body ends without message_stop", text(), |e| {
            matches!(e, Error::Disconnected { .. })
        }),
    ];
    for (name, steps, check) in cases {
        mock.script(Script::stream(steps));
        let events = run(&mock, user("hi")).await;
        assert_eq!(text_of(&events), "a", "{name}");
        assert_eq!(
            events.len(),
            2,
            "{name}: the text, then exactly one error: {events:?}"
        );
        match events.last() {
            Some(LlmEvent::Error(error)) => {
                assert!(check(error), "{name}: {error:?}");
                let shown = format!("{error} {error:?}");
                assert!(!shown.contains("SECRET"), "{name}: {shown}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_event_larger_than_the_cap_is_a_protocol_error_not_unbounded_memory() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_event_bytes = 2048;
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut big = b"data: ".to_vec();
    big.extend(std::iter::repeat_n(b'x', 64 * 1024));
    mock.script(Script::stream(vec![Step::raw(big)]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(
        matches!(events.as_slice(), [LlmEvent::Error(Error::Protocol { .. })]),
        "{events:?}"
    );
}

#[tokio::test]
async fn silence_longer_than_the_idle_deadline_is_a_timeout_even_after_events() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::event(wire::text_delta(0, "a")),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle")
    );
}

#[tokio::test]
async fn pings_and_comments_do_not_hold_the_idle_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![
        Step::event(wire::text_start(0)),
        Step::event(wire::text_delta(0, "a")),
    ];
    for _ in 0..60 {
        steps.push(Step::event(wire::ping()));
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(100)));
    }
    mock.script(Script::stream(steps));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle")
    );
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn content_events_hold_the_idle_deadline_whatever_fills_the_gaps() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    // 900 ms in total, never 400 ms between content events (thinking deltas count: they are
    // content events).
    let mut steps = vec![Step::event(wire::thinking_start(0))];
    for _ in 0..6 {
        steps.push(Step::event(wire::ping()));
        steps.push(Step::Sleep(Duration::from_millis(150)));
        steps.push(Step::event(wire::thinking_delta(0, "hm")));
    }
    steps.push(Step::event(wire::signature_delta(0, "S")));
    steps.push(Step::event(wire::block_stop(0)));
    steps.push(Step::event(wire::text_start(1)));
    steps.push(Step::event(wire::text_delta(1, "late")));
    steps.extend(finish_steps("end_turn"));
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "late");
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn pings_and_message_start_do_not_extend_the_first_output_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_output_timeout = Duration::from_millis(400);
    cfg.idle_timeout = Duration::from_secs(5);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![Step::event(wire::message_start(10))];
    for _ in 0..60 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::event(wire::ping()));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(
        matches!(events.as_slice(), [LlmEvent::Error(_)]),
        "{events:?}"
    );
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic first output")
    );
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn an_open_tool_block_may_pause_longer_than_text_may() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.tool_idle_timeout = Duration::from_millis(1500);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");

    // A 700 ms gap inside a tool call (the model works between key/value lumps) is fine...
    let mut steps = vec![
        Step::event(wire::tool_start(0, "toolu_1", "get_weather")),
        Step::event(wire::json_delta(0, "{\"city\":")),
        Step::Sleep(Duration::from_millis(700)),
        Step::event(wire::json_delta(0, "\"Paris\"}")),
        Step::event(wire::block_stop(0)),
    ];
    steps.extend(finish_steps("tool_use"));
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        calls_of(&events),
        vec![(
            "toolu_1".to_owned(),
            "get_weather".to_owned(),
            "{\"city\":\"Paris\"}".to_owned()
        )]
    );
    assert_eq!(done_of(&events).0, FinishReason::ToolCalls);

    // ... the same gap in a text block is not ...
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::event(wire::text_delta(0, "a")),
        Step::Sleep(Duration::from_millis(700)),
        Step::event(wire::text_delta(0, "b")),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle")
    );

    // ... and the tool allowance is bounded too.
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.tool_idle_timeout = Duration::from_millis(500);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::event(wire::tool_start(0, "toolu_1", "get_weather")),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle in a tool call")
    );
}

#[tokio::test]
async fn the_total_duration_of_a_response_is_capped_even_while_content_keeps_arriving() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_secs(5);
    cfg.max_stream_duration = Duration::from_millis(400);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![Step::event(wire::text_start(0))];
    for _ in 0..40 {
        steps.push(Step::event(wire::text_delta(0, "x")));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic stream")
    );
}

#[tokio::test]
async fn response_headers_that_never_come_time_out_in_stream() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_byte_timeout = Duration::from_millis(300);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(30);
    mock.script(script);
    assert!(matches!(
        llm.stream(user("hi")).await,
        Err(Error::Core(CoreError::Timeout {
            what: "anthropic first byte",
            ..
        }))
    ));
}

#[tokio::test]
async fn a_refused_connection_is_a_disconnect_not_a_hang() {
    let mock = Mock::start(false).await;
    let cfg = config(&mock).with_base_url("http://127.0.0.1:1");
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    // Concurrently: Windows takes about two seconds to report a refused loopback connect.
    let (streamed, warmed) = tokio::join!(llm.stream(user("hi")), llm.warm());
    assert!(matches!(streamed, Err(Error::Disconnected { .. })));
    assert!(matches!(warmed, Err(Error::Disconnected { .. })));
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
    let mut steps = vec![
        Step::event(wire::text_start(0)),
        Step::counted(wire::text_delta(0, "first")),
    ];
    for _ in 0..100 {
        steps.push(Step::Sleep(Duration::from_millis(20)));
        steps.push(Step::counted(wire::text_delta(0, "more")));
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

/// Waits up to `limit` for the mock to see `n` ended connections; returns how long it took.
async fn wait_for_hangups(mock: &Mock, n: usize, limit: Duration) -> Option<Duration> {
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
    // The usual barge-in moment: the model is thinking and nothing arrives. The reader must
    // notice the dropped stream without waiting for the next event or a timeout.
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_secs(10);
    cfg.first_output_timeout = Duration::from_secs(10);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::counted(wire::text_delta(0, "first")),
        Step::Sleep(Duration::from_secs(30)),
        Step::counted(wire::text_delta(0, "never")),
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
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![Step::event(wire::text_start(0))];
    for block in 0..3 {
        let mut wire_text = String::new();
        for i in 0..200 {
            let n = block * 200 + i;
            wire_text.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                wire::text_delta(0, &format!("<{n}>"))
            ));
        }
        steps.push(Step::raw(wire_text.into_bytes()));
    }
    steps.extend(finish_steps("end_turn"));
    mock.script(Script::stream(steps));

    let stream = llm.stream(user("hi")).await.expect("accepted");
    tokio::time::sleep(Duration::from_millis(900)).await;
    let events = collect(stream).await;

    let expected: String = (0..600).map(|n| format!("<{n}>")).collect();
    assert_eq!(text_of(&events), expected, "no delta lost or reordered");
    assert_eq!(events.len(), 601, "{:?}", events.last());
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn a_stalled_consumer_stops_the_reader_instead_of_buffering_the_response() {
    // 16 MiB of deltas are scripted; the consumer does not read at all. The reader may hold what
    // fits in the event channel (64) plus socket buffers, but it must stop reading the socket,
    // so the server's writes block far short of the total. Then the consumer resumes and gets
    // everything, in order.
    const MIN_EVENTS: usize = 4000;
    const PAYLOAD: usize = 4096;
    // What the sockets alone absorb depends on the operating system: under 2000 events of this
    // size on Windows loopback, up to the kernel's autotuned buffer maximum on Linux
    // (`loopback_buffer_bytes`, ~10 MiB: ~2500 events). The bound is that plus 256 events for
    // the reader, and the script is twice as long as the bound, so an unbounded reader (all
    // events sent) still fails it by 2x.
    let bound = if cfg!(target_os = "linux") {
        brisvox_sim::platform::loopback_buffer_bytes() / PAYLOAD + 256
    } else {
        MIN_EVENTS / 2
    };
    let events: usize = (2 * bound).max(MIN_EVENTS);
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let pad = "p".repeat(PAYLOAD - 16);
    let mut steps = vec![Step::event(wire::text_start(0))];
    steps.extend((0..events).map(|n| Step::counted(wire::text_delta(0, &format!("{n:06}{pad}")))));
    steps.push(Step::counted(wire::message_stop()));
    mock.script(Script::stream(steps));

    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    // Let the server run until it blocks (or, if the reader were unbounded, finishes).
    let mut last = usize::MAX;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = mock.events_sent();
        if now == last {
            break;
        }
        last = now;
    }
    let held = mock.events_sent();
    assert!(
        held < bound,
        "{held} of {events} events were sent while nobody consumed (bound {bound}): the reader is not bounded"
    );

    let received = tokio::time::timeout(Duration::from_secs(20), async {
        let mut deltas = 0_usize;
        let mut done = false;
        while let Some(event) = stream.next().await {
            match event {
                LlmEvent::TextDelta(t) => {
                    assert!(
                        t.starts_with(&format!("{deltas:06}")),
                        "out of order at {deltas}"
                    );
                    deltas += 1;
                }
                LlmEvent::Done { .. } => done = true,
                other => panic!("{other:?}"),
            }
        }
        (deltas, done)
    })
    .await
    .expect("the resumed consumer finished");
    assert_eq!(received, (events, true));
}

#[tokio::test]
async fn the_rest_of_a_finished_response_is_drained_within_a_total_bound() {
    // After `message_stop` the server trickles bytes forever; the reader must let go of the
    // connection after about half a second in total, not per chunk.
    let mock = Mock::start(true).await;
    let llm = provider(&mock);
    let mut steps = finish_steps("end_turn");
    for _ in 0..80 {
        steps.push(Step::Sleep(Duration::from_millis(100)));
        steps.push(Step::counted("{}"));
    }
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sent = mock.events_sent();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.events_sent(), sent, "the server was hung up on");
    assert!(sent <= 10, "{sent} events were sent into the drain");
    assert_eq!(mock.closed_connections(), 1);
}

#[tokio::test]
async fn heartbeat_events_with_empty_data_and_done_sentinels_do_not_break_the_stream() {
    let mock = Mock::start(false).await;
    let mut steps = vec![
        Step::raw(b"data:\n\n".to_vec()),
        Step::raw(b"data: [DONE]\n\n".to_vec()),
        Step::event(wire::text_start(0)),
        Step::raw(b"data: \n\n".to_vec()),
        Step::event(wire::text_delta(0, "ok")),
    ];
    steps.extend(finish_steps("end_turn"));
    mock.script(Script::stream(steps));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "ok");
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn plain_http_is_refused_for_remote_hosts_unless_opted_in() {
    let mock = Mock::start(false).await;
    let remote = |allow: bool| {
        let mut cfg = config(&mock).with_base_url("http://api.example.invalid");
        cfg.allow_insecure_http = allow;
        AnthropicLlm::new(cfg, SystemClock::shared())
    };
    let error = remote(false).expect_err("cleartext key to a remote host");
    let shown = format!("{error}");
    assert!(!shown.contains(KEY) && shown.contains("http"), "{shown}");
    assert!(remote(true).is_ok());
    // Loopback, and https anywhere, are fine; so are IPv6 loopback and `localhost`.
    for url in [
        "http://127.0.0.1:9",
        "http://localhost:9",
        "http://[::1]:9",
        "https://api.anthropic.com",
    ] {
        let cfg = config(&mock).with_base_url(url);
        assert!(
            AnthropicLlm::new(cfg, SystemClock::shared()).is_ok(),
            "{url}"
        );
    }
    for url in ["ftp://x", "not a url", ""] {
        let cfg = config(&mock).with_base_url(url);
        assert!(
            AnthropicLlm::new(cfg, SystemClock::shared()).is_err(),
            "{url}"
        );
    }
}

#[tokio::test]
async fn tools_system_and_history_are_byte_stable_across_turns_for_prompt_caching() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let tools = vec![weather_tool()];
    let mut messages = vec![
        Message::new(Role::System, "Stable persona."),
        Message::new(Role::User, "first question"),
    ];
    for answer in ["first answer", "second answer"] {
        only_done(&mock);
        collect(
            llm.stream(LlmRequest {
                messages: messages.clone(),
                tools: tools.clone(),
            })
            .await
            .expect("accepted"),
        )
        .await;
        messages.push(Message::new(Role::Assistant, answer));
        messages.push(Message::new(Role::User, format!("question after {answer}")));
    }
    only_done(&mock);
    collect(
        llm.stream(LlmRequest { messages, tools })
            .await
            .expect("accepted"),
    )
    .await;

    let posts = mock.posts();
    assert_eq!(posts.len(), 3);
    // The raw bytes of the tools array and the system array are identical on every turn.
    let section = |body: &str, from: &str, to: &str| {
        let a = body.find(from).unwrap();
        let b = body.find(to).unwrap();
        body[a..b].to_owned()
    };
    let tools_of = |body: &str| section(body, r#""tools":"#, r#","system":"#);
    let system_of = |body: &str| section(body, r#""system":"#, r#","messages":"#);
    for later in &posts[1..] {
        assert_eq!(tools_of(&posts[0].body), tools_of(&later.body));
        assert_eq!(system_of(&posts[0].body), system_of(&later.body));
    }
    // The history is append-only: the messages of a request, ignoring where the breakpoint
    // sits (it moves; it is not part of the cached prefix), start with the previous request's.
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let messages_of = |post: &crate::mock::Recorded| {
        let mut messages = post.json()["messages"].clone();
        strip(&mut messages);
        messages.as_array().unwrap().clone()
    };
    for pair in posts.windows(2) {
        let (before, after) = (messages_of(&pair[0]), messages_of(&pair[1]));
        assert!(
            after.starts_with(&before),
            "{before:?} is not a prefix of {after:?}"
        );
    }
    // And the breakpoint is on the turn before the newest one each time.
    for post in &posts[1..] {
        let messages = post.json()["messages"].clone();
        let n = messages.as_array().unwrap().len();
        assert!(messages[n - 2]["content"][0].get("cache_control").is_some());
        assert!(messages[n - 1]["content"][0].get("cache_control").is_none());
    }
}

#[tokio::test]
async fn thinking_and_calls_round_trip_through_provider_data() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut steps = vec![
        Step::event(wire::message_start(100)),
        Step::event(wire::thinking_start(0)),
        Step::event(wire::thinking_delta(0, "Weather needs a lookup.")),
        Step::event(wire::signature_delta(0, "SIGNATURE==")),
        Step::event(wire::block_stop(0)),
        Step::event(wire::text_start(1)),
        Step::event(wire::text_delta(1, "One moment.")),
        Step::event(wire::block_stop(1)),
        Step::event(wire::tool_start(2, "toolu_77", "get_weather")),
        Step::event(wire::json_delta(2, "")),
        Step::event(wire::json_delta(2, "{\"city\":")),
        Step::event(wire::json_delta(2, "\"Paris\"}")),
        Step::event(wire::block_stop(2)),
    ];
    steps.extend(finish_steps("tool_use"));
    mock.script(Script::stream(steps));

    let tools = vec![weather_tool()];
    let mut messages = vec![
        Message::new(Role::System, "Persona."),
        Message::new(Role::User, "Weather in Paris?"),
    ];
    let events = collect(
        llm.stream(LlmRequest {
            messages: messages.clone(),
            tools: tools.clone(),
        })
        .await
        .expect("accepted"),
    )
    .await;
    let (finish, _, data) = done_of(&events);
    assert_eq!(finish, FinishReason::ToolCalls);
    let calls = calls_of(&events);
    let [(id, name, arguments)] = calls.as_slice() else {
        panic!("{calls:?}")
    };

    // The caller stores the turn as it was played, with the provider data, and answers the call.
    messages.push(Message::assistant(
        text_of(&events),
        vec![ToolCall {
            id: Arc::from(id.as_str()),
            name: Arc::from(name.as_str()),
            arguments: Arc::from(arguments.as_str()),
        }],
        data,
    ));
    messages.push(Message::tool_result(id.as_str(), "18 degrees, cloudy"));
    mock.script(Script::stream(text_response("It is 18 degrees.")));
    let events = collect(
        llm.stream(LlmRequest { messages, tools })
            .await
            .expect("accepted"),
    )
    .await;
    assert_eq!(text_of(&events), "It is 18 degrees.");

    let posts = mock.posts();
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(
        posts[1].json()["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Weather in Paris?"}]},
            {"role": "assistant", "content": [
                // Unmodified, and in front of the text it led into.
                {"type": "thinking", "thinking": "Weather needs a lookup.", "signature": "SIGNATURE=="},
                {"type": "text", "text": "One moment."},
                {"type": "tool_use", "id": "toolu_77", "name": "get_weather",
                 "input": {"city": "Paris"}, "cache_control": ephemeral},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_77", "content": "18 degrees, cloudy"},
            ]},
        ])
    );
}

#[tokio::test]
async fn parallel_tool_calls_round_trip_with_their_thinking_and_results() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut steps = vec![
        Step::event(wire::message_start(100)),
        Step::event(wire::thinking_start(0)),
        Step::event(wire::signature_delta(0, "S1")),
        Step::event(wire::block_stop(0)),
        Step::event(wire::text_start(1)),
        Step::event(wire::text_delta(1, "Two lookups.")),
        Step::event(wire::block_stop(1)),
        Step::event(wire::tool_start(2, "toolu_a", "get_weather")),
        Step::event(wire::json_delta(2, "{\"city\":")),
        Step::event(wire::json_delta(2, "\"Paris\"}")),
        Step::event(wire::block_stop(2)),
        // Thinking between the calls, then a call with no arguments at all.
        Step::event(wire::thinking_start(3)),
        Step::event(wire::signature_delta(3, "S2")),
        Step::event(wire::block_stop(3)),
        Step::event(wire::tool_start(4, "toolu_b", "get_time")),
        Step::event(wire::block_stop(4)),
    ];
    steps.extend(finish_steps("tool_use"));
    mock.script(Script::stream(steps));

    let mut messages = vec![Message::new(Role::User, "Weather in Paris and the time?")];
    let events = collect(
        llm.stream(LlmRequest {
            messages: messages.clone(),
            tools: vec![weather_tool()],
        })
        .await
        .expect("accepted"),
    )
    .await;
    let (finish, _, data) = done_of(&events);
    assert_eq!(finish, FinishReason::ToolCalls);
    let indexes: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCallDelta { index, id, .. } if id.is_some() => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(indexes, vec![0, 1], "dense indexes, not block indexes");
    let calls = calls_of(&events);
    assert_eq!(
        calls,
        vec![
            (
                "toolu_a".to_owned(),
                "get_weather".to_owned(),
                "{\"city\":\"Paris\"}".to_owned()
            ),
            ("toolu_b".to_owned(), "get_time".to_owned(), "{}".to_owned()),
        ]
    );

    messages.push(Message::assistant(
        text_of(&events),
        calls
            .iter()
            .map(|(id, name, arguments)| ToolCall {
                id: Arc::from(id.as_str()),
                name: Arc::from(name.as_str()),
                arguments: Arc::from(arguments.as_str()),
            })
            .collect(),
        data,
    ));
    messages.push(Message::tool_result("toolu_a", "18 degrees"));
    messages.push(Message::tool_result("toolu_b", "noon"));
    mock.script(Script::stream(text_response("Done.")));
    collect(
        llm.stream(LlmRequest {
            messages,
            tools: vec![weather_tool()],
        })
        .await
        .expect("accepted"),
    )
    .await;

    let sent = mock.posts()[1].json()["messages"].clone();
    let assistant = sent[1]["content"].as_array().expect("blocks");
    let shape: Vec<(&str, &str)> = assistant
        .iter()
        .map(|b| {
            (
                b["type"].as_str().unwrap_or_default(),
                b["id"]
                    .as_str()
                    .or_else(|| b["signature"].as_str())
                    .or_else(|| b["text"].as_str())
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("thinking", "S1"),
            ("text", "Two lookups."),
            ("tool_use", "toolu_a"),
            ("thinking", "S2"),
            ("tool_use", "toolu_b"),
        ]
    );
    assert_eq!(assistant[2]["input"], json!({"city": "Paris"}));
    assert_eq!(assistant[4]["input"], json!({}));
    assert_eq!(
        sent[2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_a", "content": "18 degrees"},
            {"type": "tool_result", "tool_use_id": "toolu_b", "content": "noon"},
        ])
    );
}

#[tokio::test]
async fn an_interrupted_turn_replays_only_what_was_played() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    only_done(&mock);
    let data = r#"[{"type":"thinking","thinking":"plan","signature":"S1"},{"type":"text"}]"#;
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::User, "Tell me a long story"),
            // The caller cut the played text at the barge-in.
            Message::assistant("Once upon a", Vec::new(), Some(Arc::from(data))),
            Message::new(Role::User, "Stop, something else"),
        ],
        tools: Vec::new(),
    };
    collect(llm.stream(request).await.expect("accepted")).await;
    assert_eq!(
        mock.posts()[0].json()["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "plan", "signature": "S1"},
            {"type": "text", "text": "Once upon a", "cache_control": {"type": "ephemeral"}},
        ])
    );
}

#[tokio::test]
async fn a_large_history_is_sent_intact() {
    // Past 32 KiB the body is built on the blocking pool (covered by the unit tests); the
    // result is the same bytes either way.
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    only_done(&mock);
    let big = "word ".repeat(40_000);
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::User, big.as_str()),
            Message::new(Role::Assistant, "ok"),
            Message::new(Role::User, "and now?"),
        ],
        tools: Vec::new(),
    };
    collect(llm.stream(request).await.expect("accepted")).await;
    let body = mock.posts()[0].json();
    assert_eq!(body["messages"][0]["content"][0]["text"], big.as_str());
    assert_eq!(body["messages"][2]["content"][0]["text"], "and now?");
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
    let bad = AnthropicConfig::new(Secret::new("sk-ant-bad\nkey-SECRETPART".to_owned()));
    let error = AnthropicLlm::new(bad, SystemClock::shared()).expect_err("invalid header");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("SECRETPART") && !shown.contains("sk-ant-bad"),
        "{shown}"
    );

    // Refusals echo key fragments in their message; the adapter's error must not.
    mock.script(Script::status(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key sk-ant-test-key-0123***"}}"#,
    ));
    let error = llm.stream(user("hi")).await.expect_err("rejected");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("sk-") && !shown.contains("invalid x-api-key"),
        "{shown}"
    );
}

#[tokio::test]
async fn requests_with_bad_tools_or_tool_messages_fail_before_any_network_io() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let tool = |name: &str, schema: &str| ToolSpec {
        name: Arc::from(name),
        description: Arc::from("d"),
        parameters_schema: Arc::from(schema),
    };
    let requests = vec![
        LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![tool("has space", r#"{"type":"object"}"#)],
        },
        LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![tool("ok", r#"{"type":"string"}"#)],
        },
        LlmRequest {
            messages: vec![Message::new(Role::Tool, "result without a call")],
            tools: Vec::new(),
        },
        LlmRequest::default(),
        LlmRequest {
            messages: vec![
                Message::new(Role::User, "hi"),
                Message::assistant(
                    "",
                    vec![ToolCall {
                        id: Arc::from("toolu_1"),
                        name: Arc::from("t"),
                        arguments: Arc::from("{}"),
                    }],
                    None,
                ),
                Message::new(Role::User, "never answered the call"),
            ],
            tools: Vec::new(),
        },
        LlmRequest {
            messages: vec![
                Message::new(Role::User, "hi"),
                Message::new(Role::Assistant, "prefill is rejected by the API"),
            ],
            tools: Vec::new(),
        },
    ];
    for request in requests {
        assert!(matches!(
            llm.stream(request).await,
            Err(Error::Protocol { .. })
        ));
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn the_adapter_identifies_itself() {
    let mock = Mock::start(false).await;
    assert_eq!(provider(&mock).name(), "anthropic");
}

#[tokio::test]
async fn an_open_thinking_block_may_stay_silent_up_to_the_first_output_allowance() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.first_output_timeout = Duration::from_millis(1500);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");

    // display: omitted: the block opens, then only pings, then one signature when the thinking
    // ends. A 700 ms silence is longer than the 300 ms idle timeout and fine inside thinking.
    let mut steps = vec![
        Step::event(wire::message_start(10)),
        Step::event(wire::thinking_start(0)),
        Step::event(wire::ping()),
        Step::Sleep(Duration::from_millis(700)),
        Step::event(wire::ping()),
        Step::event(wire::signature_delta(0, "SIG")),
        Step::event(wire::block_stop(0)),
        Step::event(wire::text_start(1)),
        Step::event(wire::text_delta(1, "answer")),
    ];
    steps.extend(finish_steps("end_turn"));
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "answer");
    let (finish, _, data) = done_of(&events);
    assert_eq!(finish, FinishReason::Stop);
    assert!(data.expect("thinking is kept for replay").contains("SIG"));

    // The same silence after the thinking block closed is held to the plain idle timeout.
    mock.script(Script::stream(vec![
        Step::event(wire::thinking_start(0)),
        Step::event(wire::signature_delta(0, "SIG")),
        Step::event(wire::block_stop(0)),
        Step::event(wire::text_start(1)),
        Step::event(wire::text_delta(1, "a")),
        Step::Sleep(Duration::from_millis(700)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle")
    );

    // And the allowance is bounded.
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.first_output_timeout = Duration::from_millis(600);
    let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::event(wire::thinking_start(0)),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle in thinking")
    );
    assert!(start.elapsed() < Duration::from_secs(3));

    // A redacted block is a thinking block too.
    mock.script(Script::stream(vec![
        Step::event(wire::redacted_start(0, "RED")),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        events.last().and_then(timeout_error),
        Some("anthropic idle in thinking")
    );
}

#[tokio::test]
async fn a_message_stop_inside_an_open_tool_call_is_a_protocol_error_not_a_truncated_call() {
    let mock = Mock::start(false).await;
    let mut steps = vec![
        Step::event(wire::tool_start(0, "toolu_1", "get_weather")),
        Step::event(wire::json_delta(0, "{\"city\":")),
    ];
    steps.extend(finish_steps("tool_use"));
    mock.script(Script::stream(steps));
    let events = run(&mock, user("hi")).await;
    assert!(
        matches!(events.last(), Some(LlmEvent::Error(Error::Protocol { .. }))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, LlmEvent::Done { .. })));
}

#[tokio::test]
async fn a_malformed_event_does_not_put_its_data_into_the_error() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::event(
            json!({"type": "content_block_delta", "index": "MARKER-secret-value", "delta": {}})
                .to_string(),
        ),
    ]));
    let events = run(&mock, user("hi")).await;
    let Some(LlmEvent::Error(error)) = events.last() else {
        panic!("{events:?}");
    };
    assert!(matches!(error, Error::Protocol { .. }));
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("MARKER") && !shown.contains("secret"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_late_system_note_before_an_interrupted_empty_assistant_turn_is_folded_on_the_wire() {
    let mock = Mock::start(false).await;
    let history = || LlmRequest {
        messages: vec![
            Message::new(Role::User, "A"),
            Message::new(Role::System, "note"),
            // Interrupted before any audio, no calls: this turn leaves no trace.
            Message::new(Role::Assistant, ""),
            Message::new(Role::User, "B"),
        ],
        tools: Vec::new(),
    };
    for enabled in [false, true] {
        only_done(&mock);
        let mut cfg = config(&mock);
        cfg.cache = CacheConfig::OFF;
        cfg.mid_conversation_system = enabled;
        let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
        collect(llm.stream(history()).await.expect("accepted")).await;
        let body = mock.posts().last().expect("post").json();
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "A"},
                {"type": "text", "text": "note"},
                {"type": "text", "text": "B"},
            ]}]),
            "mid_conversation_system = {enabled}: a system message in front of a user turn is a 400"
        );
    }
}

#[tokio::test]
async fn the_mid_conversation_system_role_is_opt_in() {
    let mock = Mock::start(false).await;
    let request = || LlmRequest {
        messages: vec![
            Message::new(Role::User, "one"),
            Message::new(Role::System, "note"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ],
        tools: Vec::new(),
    };
    for (enabled, expected_roles) in [
        (false, vec!["user", "assistant", "user"]),
        (true, vec!["user", "system", "assistant", "user"]),
    ] {
        only_done(&mock);
        let mut cfg = config(&mock);
        cfg.mid_conversation_system = enabled;
        let llm = AnthropicLlm::new(cfg, SystemClock::shared()).expect("provider");
        collect(llm.stream(request()).await.expect("accepted")).await;
        let body = mock.posts().last().expect("post").json();
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, expected_roles, "enabled = {enabled}");
    }
}

#[tokio::test]
async fn documented_400_combinations_and_credentials_in_the_base_url_are_refused_up_front() {
    let mock = Mock::start(false).await;
    for thinking in [ThinkingMode::Disabled, ThinkingMode::BetweenTools] {
        for effort in [Effort::Xhigh, Effort::Max] {
            let mut cfg = config(&mock);
            cfg.thinking = thinking;
            cfg.effort = Some(effort);
            assert!(
                matches!(
                    AnthropicLlm::new(cfg, SystemClock::shared()),
                    Err(Error::Protocol { .. })
                ),
                "{thinking:?} {effort:?}"
            );
        }
    }
    // Fine: high effort needs thinking on, and low effort works with it off.
    for (thinking, effort) in [
        (ThinkingMode::Adaptive, Effort::Max),
        (ThinkingMode::ModelDefault, Effort::Xhigh),
        (ThinkingMode::Disabled, Effort::High),
        (ThinkingMode::BetweenTools, Effort::Low),
    ] {
        let mut cfg = config(&mock);
        cfg.thinking = thinking;
        cfg.effort = Some(effort);
        assert!(
            AnthropicLlm::new(cfg, SystemClock::shared()).is_ok(),
            "{thinking:?} {effort:?}"
        );
    }

    // Credentials in the URL would reach logs through transport errors.
    for url in [
        "https://user:hunter2@gateway.example.invalid",
        "http://user:hunter2@127.0.0.1:9",
        "https://onlyuser@gateway.example.invalid",
    ] {
        let cfg = config(&mock).with_base_url(url);
        let error = AnthropicLlm::new(cfg, SystemClock::shared()).expect_err(url);
        let shown = format!("{error} {error:?}");
        assert!(
            matches!(error, Error::Protocol { .. })
                && !shown.contains("hunter2")
                && !shown.contains("onlyuser"),
            "{shown}"
        );
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
