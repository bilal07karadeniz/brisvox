//! Protocol-level tests of the OpenAI adapter: the exact request sent, every server event type
//! of `docs/providers/openai.md`, error mapping, keep-alive and timeout behaviour, connection
//! reuse, redirects and secrecy.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Error as CoreError, Secret, SystemClock};
use brisvox_providers::openai::{OpenAiConfig, OpenAiLlm, ReasoningEffort};
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, ToolCall,
    ToolSpec, Usage,
};
use serde_json::{Value, json};

use crate::mock::{Mock, Script, Step};

const KEY: &str = "sk-test-key-0123456789";

fn config(mock: &Mock) -> OpenAiConfig {
    OpenAiConfig::new(Secret::new(KEY.to_owned()))
        .with_base_url(mock.base_url())
        .with_timeout(Duration::from_secs(3))
}

fn provider(mock: &Mock) -> OpenAiLlm {
    OpenAiLlm::new(config(mock), SystemClock::shared()).expect("provider")
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

fn completed(extra: Value) -> String {
    let mut response = json!({"id": "resp_1", "status": "completed", "output": []});
    if let (Some(map), Some(extra)) = (response.as_object_mut(), extra.as_object()) {
        map.extend(extra.clone());
    }
    json!({"type": "response.completed", "response": response}).to_string()
}

fn only_done(mock: &Mock) {
    mock.script(Script::stream(vec![Step::event(completed(json!({})))]));
}

#[tokio::test]
async fn the_request_on_the_wire_is_exactly_the_documented_one() {
    let mock = Mock::start(false).await;
    only_done(&mock);
    let mut cfg = config(&mock);
    cfg.organization = Some("org-1".to_owned());
    cfg.project = Some("proj-1".to_owned());
    cfg.prompt_cache_key = Some("agent-v1".to_owned());
    cfg.max_output_tokens = Some(256);
    cfg.reasoning_effort = Some(ReasoningEffort::Low);
    cfg.parallel_tool_calls = false;
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let strict = r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}"#;
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "You are a phone agent."),
            Message::new(Role::User, "Hallo"),
        ],
        tools: vec![ToolSpec {
            name: Arc::from("get_weather"),
            description: Arc::from("Weather of a city"),
            parameters_schema: Arc::from(strict),
        }],
    };
    collect(llm.stream(request).await.expect("accepted")).await;

    let posts = mock.posts();
    let [post] = posts.as_slice() else {
        panic!("{posts:?}")
    };
    assert_eq!(post.path, "/v1/responses");
    assert_eq!(
        post.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(post.header("content-type"), Some("application/json"));
    assert_eq!(post.header("accept"), Some("text/event-stream"));
    assert_eq!(post.header("openai-organization"), Some("org-1"));
    assert_eq!(post.header("openai-project"), Some("proj-1"));
    assert!(
        post.header("x-client-request-id")
            .is_some_and(|id| id.starts_with("brisvox-") && id.is_ascii())
    );
    assert_eq!(
        post.json(),
        json!({
            "model": "gpt-6-luna",
            "stream": true,
            "store": false,
            "instructions": "You are a phone agent.",
            "input": [{"type": "message", "role": "user", "content": "Hallo"}],
            "tools": [{"type": "function", "name": "get_weather",
                       "description": "Weather of a city",
                       "parameters": serde_json::from_str::<Value>(strict).unwrap(),
                       "strict": true}],
            "parallel_tool_calls": false,
            "reasoning": {"effort": "low"},
            "max_output_tokens": 256,
            "prompt_cache_key": "agent-v1",
            "include": ["reasoning.encrypted_content"],
            "stream_options": {"include_obfuscation": false},
        })
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
    assert!(warm.header("authorization").is_none());
}

/// A stream with every event type the adapter reads or ignores, with comments, `event:` lines,
/// CRLF and LF endings, cut into small HTTP chunks at arbitrary byte positions.
#[tokio::test]
async fn every_documented_server_event_is_parsed_across_arbitrary_chunk_boundaries() {
    let mock = Mock::start(false).await;
    let reasoning_done = json!({"type": "reasoning", "id": "rs_1", "summary": [],
        "encrypted_content": "ENC=="});
    let call_done = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1",
        "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"});
    let events = [
        json!({"type": "response.created", "sequence_number": 0, "response": {"id": "resp_1"}}),
        json!({"type": "response.in_progress", "sequence_number": 1, "response": {}}),
        json!({"type": "response.queued", "response": {}}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "thinking"}),
        json!({"type": "response.reasoning_text.delta", "item_id": "rs_1", "delta": "hm"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_done}),
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.content_part.added", "item_id": "msg_1", "part": {"type": "output_text", "text": ""}}),
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1,
               "content_index": 0, "delta": "Merhaba, ", "logprobs": [], "obfuscation": "xx"}),
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1,
               "content_index": 0, "delta": "Ada! \u{1F44B} \"quoted\"\nline"}),
        json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 1,
               "content_index": 0, "text": "Merhaba, Ada!"}),
        json!({"type": "response.content_part.done", "item_id": "msg_1", "part": {}}),
        json!({"type": "response.output_item.done", "output_index": 1,
               "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": []}}),
        json!({"type": "response.output_item.added", "output_index": 2,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                        "name": "get_weather", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "response_id": "resp_1",
               "item_id": "fc_1", "output_index": 2, "delta": "{\"ci"}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1",
               "output_index": 2, "delta": "ty\":\"Paris\"}"}),
        json!({"type": "response.function_call_arguments.done", "item_id": "fc_1",
               "output_index": 2, "arguments": "{\"city\":\"Paris\"}"}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": call_done}),
        json!({"type": "response.completed", "sequence_number": 99, "response": {
            "id": "resp_1", "status": "completed",
            "output": [{"type": "reasoning"}, {"type": "message"}, {"type": "function_call"}],
            "usage": {"input_tokens": 200, "input_tokens_details": {"cached_tokens": 128, "cache_write_tokens": 0},
                      "output_tokens": 30, "output_tokens_details": {"reasoning_tokens": 10},
                      "total_tokens": 230}}}),
    ];
    let mut wire = String::from(": hello\n\nretry: 3000\n\n");
    for (i, event) in events.iter().enumerate() {
        let name = event["type"].as_str().unwrap();
        match i % 3 {
            0 => wire.push_str(&format!("event: {name}\ndata: {event}\n\n")),
            1 => wire.push_str(&format!("data: {event}\r\n\r\n: keep-alive\r\n\r\n")),
            _ => wire.push_str(&format!("id: {i}\ndata: {event}\n\n")),
        }
    }
    wire.push_str("data: [DONE]\n\n");
    let steps = wire
        .as_bytes()
        .chunks(37)
        .map(|c| Step::raw(c.to_vec()))
        .collect();
    mock.script(Script::stream(steps));

    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "Merhaba, Ada! \u{1F44B} \"quoted\"\nline");
    let call: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => Some((*index, id.clone(), name.clone(), arguments.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(call.len(), 3, "{call:?}");
    assert_eq!(call[0].0, 0);
    assert_eq!(call[0].1.as_deref(), Some("call_1"));
    assert_eq!(call[0].2.as_deref(), Some("get_weather"));
    assert!(call[1..].iter().all(|c| c.1.is_none() && c.2.is_none()));
    assert_eq!(
        call.iter().map(|c| c.3.as_str()).collect::<String>(),
        "{\"city\":\"Paris\"}"
    );
    let Some(LlmEvent::Done {
        finish,
        usage,
        provider_data,
    }) = events.last()
    else {
        panic!("no Done: {events:?}");
    };
    assert_eq!(*finish, FinishReason::ToolCalls);
    assert_eq!(
        *usage,
        Some(Usage {
            input_tokens: 200,
            cached_input_tokens: 128,
            output_tokens: 30
        })
    );
    let data: Value =
        serde_json::from_str(provider_data.as_deref().expect("provider data")).unwrap();
    // The placeholder marks where the assistant message sat between the two items.
    assert_eq!(
        data,
        json!([reasoning_done, {"type": "message"}, call_done])
    );
    assert_eq!(events.len(), call.len() + 2 + 1, "text x2, call x3, Done");
}

#[tokio::test]
async fn text_only_responses_finish_with_stop_and_no_provider_data() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(json!({"type": "response.output_text.delta", "delta": "ok"}).to_string()),
        Step::event(completed(json!({"output": [{"type": "message"}]}))),
    ]));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(
        events.as_slice(),
        [
            LlmEvent::TextDelta(_),
            LlmEvent::Done {
                finish: FinishReason::Stop,
                usage: None,
                provider_data: None
            }
        ]
    ));
}

#[tokio::test]
async fn a_truncated_answer_reports_length() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![Step::event(
        json!({"type": "response.incomplete", "response": {
            "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}})
        .to_string(),
    )]));
    let events = run(&mock, user("hi")).await;
    assert!(matches!(
        events.as_slice(),
        [LlmEvent::Done {
            finish: FinishReason::Length,
            ..
        }]
    ));
}

#[tokio::test]
async fn http_refusals_map_to_rejected_with_the_status_and_are_not_retried() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    for status in [400_u16, 401, 403, 429, 500, 503] {
        let body = json!({"error": {"message": "Incorrect API key provided: sk-test***",
            "type": "invalid_request_error", "code": "invalid_api_key"}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("Retry-After", "7");
        }
        mock.script(script);
        match llm.stream(user("hi")).await {
            Err(Error::Rejected {
                provider: "openai",
                status: got,
            }) => assert_eq!(got, status),
            other => panic!("status {status}: {other:?}"),
        }
    }
    assert_eq!(
        mock.posts().len(),
        6,
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
    let body = json!({"error": {"type": "server_error", "code": "overloaded"}}).to_string();
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
async fn an_oversized_error_body_is_not_read_to_the_end() {
    // 64 KiB arrive of a 16 MiB announced body, then the server stalls. The adapter stops at its
    // 8 KiB cap and answers at once instead of waiting out the one-second bound for the rest.
    let mock = Mock::start(false).await;
    let big = "x".repeat(64 * 1024);
    mock.script(Script::status_then_stall(500, &big, 16 * 1024 * 1024));
    let started = std::time::Instant::now();
    let result = provider(&mock).stream(user("hi")).await;
    let took = started.elapsed();
    assert!(
        matches!(result, Err(Error::Rejected { status: 500, .. })),
        "{result:?}"
    );
    assert!(
        took < Duration::from_millis(600),
        "the error body was read for {took:?}"
    );
    // A big body that does arrive in full is just as prompt.
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
    let text = Step::event(json!({"type": "response.output_text.delta", "delta": "a"}).to_string());
    let cases: Vec<Case> = vec![
        (
            "response.failed rate limit",
            vec![
                text.clone(),
                Step::event(
                    json!({"type": "response.failed", "response": {"status": "failed",
                        "error": {"code": "rate_limit_exceeded", "message": "slow down"}}})
                    .to_string(),
                ),
            ],
            |e| matches!(e, Error::Rejected { status: 429, .. }),
        ),
        (
            "error event",
            vec![
                text.clone(),
                Step::event(
                    json!({"type": "error", "code": "server_error", "message": "x"}).to_string(),
                ),
            ],
            |e| matches!(e, Error::Rejected { status: 500, .. }),
        ),
        (
            "body ends without a terminal event",
            vec![text.clone()],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
        (
            "[DONE] without a terminal event",
            vec![text.clone(), Step::raw(b"data: [DONE]\n\n".to_vec())],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
        (
            "connection cut mid-body",
            vec![text.clone(), Step::Close],
            |e| matches!(e, Error::Disconnected { .. }),
        ),
        (
            "garbage JSON",
            vec![text.clone(), Step::raw(b"data: {oops\n\n".to_vec())],
            |e| matches!(e, Error::Protocol { .. }),
        ),
        (
            "function call without call_id",
            vec![Step::event(
                json!({"type": "response.output_item.added", "output_index": 0,
                       "item": {"type": "function_call", "id": "fc", "name": "t"}})
                .to_string(),
            )],
            |e| matches!(e, Error::Protocol { .. }),
        ),
    ];
    for (name, steps, expect) in cases {
        mock.script(Script::stream(steps));
        let events = run(&mock, user("hi")).await;
        let Some(LlmEvent::Error(error)) = events.last() else {
            panic!("{name}: {events:?}");
        };
        assert!(expect(error), "{name}: {error:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, LlmEvent::Error(_)))
                .count(),
            1,
            "{name}: exactly one error, and it is the last event"
        );
    }
}

#[tokio::test]
async fn an_event_larger_than_the_cap_is_a_protocol_error_not_unbounded_memory() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_event_bytes = 2048;
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut big = b"data: ".to_vec();
    big.extend(std::iter::repeat_n(b'x', 64 * 1024));
    mock.script(Script::stream(vec![Step::raw(big)]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(
        matches!(events.as_slice(), [LlmEvent::Error(Error::Protocol { .. })]),
        "{events:?}"
    );
}

fn timeout_error(event: &LlmEvent) -> Option<&'static str> {
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
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::event(json!({"type": "response.output_text.delta", "delta": "a"}).to_string()),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(events.last().and_then(timeout_error), Some("openai idle"));
}

#[tokio::test]
async fn the_total_duration_of_a_response_is_capped_even_while_keep_alives_arrive() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_secs(5);
    cfg.max_stream_duration = Duration::from_millis(400);
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for _ in 0..40 {
        steps.push(Step::raw(b": ping\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(events.last().and_then(timeout_error), Some("openai stream"));
}

#[tokio::test]
async fn response_headers_that_never_come_time_out_in_stream() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_byte_timeout = Duration::from_millis(300);
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(30);
    mock.script(script);
    assert!(matches!(
        llm.stream(user("hi")).await,
        Err(Error::Core(CoreError::Timeout {
            what: "openai first byte",
            ..
        }))
    ));
}

#[tokio::test]
async fn a_refused_connection_is_a_disconnect_not_a_hang() {
    let mock = Mock::start(false).await;
    let cfg = config(&mock).with_base_url("http://127.0.0.1:1");
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
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
    let mut steps = vec![Step::counted(
        json!({"type": "response.output_text.delta", "delta": "first"}).to_string(),
    )];
    for _ in 0..100 {
        steps.push(Step::Sleep(Duration::from_millis(20)));
        steps.push(Step::counted(
            json!({"type": "response.output_text.delta", "delta": "more"}).to_string(),
        ));
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
async fn tools_and_history_are_byte_stable_across_turns_for_prompt_caching() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let tools = vec![ToolSpec {
        name: Arc::from("lookup"),
        description: Arc::from("Looks something up"),
        parameters_schema: Arc::from(r#"{"type":"object","properties":{"q":{"type":"string"}}}"#),
    }];
    let mut messages = vec![
        Message::new(Role::System, "Stable persona."),
        Message::new(Role::User, "first question"),
    ];
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
    messages.push(Message::new(Role::Assistant, "first answer"));
    messages.push(Message::new(Role::User, "second question"));
    only_done(&mock);
    collect(
        llm.stream(LlmRequest { messages, tools })
            .await
            .expect("accepted"),
    )
    .await;

    let posts = mock.posts();
    let (first, second) = (&posts[0].body, &posts[1].body);
    let end_of_input = first.find(r#"],"tools":"#).expect("tools follow input");
    assert!(
        second.starts_with(&first[..end_of_input]),
        "the second request must extend the first one's prefix:\n{first}\n{second}"
    );
    let tools_of = |body: &str| {
        let from = body.find(r#""tools":"#).unwrap();
        let to = body.find(r#","parallel_tool_calls""#).unwrap();
        body[from..to].to_owned()
    };
    assert_eq!(tools_of(first), tools_of(second));
}

#[tokio::test]
async fn reasoning_and_calls_round_trip_through_provider_data() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1",
        "name": "lookup", "arguments": "{\"q\":\"x\"}", "status": "completed"});
    mock.script(Script::stream(vec![
        Step::event(json!({"type": "response.output_item.added", "output_index": 1,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": ""}}).to_string()),
        Step::event(json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 1,
            "delta": "{\"q\":\"x\"}"}).to_string()),
        Step::event(json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}).to_string()),
        Step::event(json!({"type": "response.output_item.done", "output_index": 1, "item": call}).to_string()),
        Step::event(completed(json!({"output": [{"type": "reasoning"}, {"type": "function_call"}]}))),
    ]));
    let first = collect(llm.stream(user("look up x")).await.expect("accepted")).await;
    let Some(LlmEvent::Done {
        finish: FinishReason::ToolCalls,
        provider_data: Some(data),
        ..
    }) = first.last()
    else {
        panic!("{first:?}");
    };

    only_done(&mock);
    let second = LlmRequest {
        messages: vec![
            Message::new(Role::User, "look up x"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: Arc::from("call_1"),
                    name: Arc::from("lookup"),
                    arguments: Arc::from(r#"{"q":"x"}"#),
                }],
                Some(Arc::clone(data)),
            ),
            Message::tool_result("call_1", "found it"),
        ],
        tools: Vec::new(),
    };
    collect(llm.stream(second).await.expect("accepted")).await;
    assert_eq!(
        mock.posts()[1].json()["input"],
        json!([
            {"type": "message", "role": "user", "content": "look up x"},
            reasoning,
            call,
            {"type": "function_call_output", "call_id": "call_1", "output": "found it"},
        ])
    );
}

#[tokio::test]
async fn a_large_history_is_sent_intact_and_the_request_builder_is_off_the_worker() {
    // 400 KiB of replayed reasoning is far past the size at which the body is built on the
    // blocking pool; the request on the wire must be the same as for a small history.
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    only_done(&mock);
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [],
        "encrypted_content": "E".repeat(400 * 1024)});
    let data: Arc<str> = Arc::from(json!([reasoning, {"type": "message"}]).to_string());
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::User, "hi"),
            Message::assistant("hello", Vec::new(), Some(data)),
            Message::new(Role::User, "and now?"),
        ],
        tools: Vec::new(),
    };
    collect(llm.stream(request).await.expect("accepted")).await;
    assert_eq!(
        mock.posts()[0].json()["input"],
        json!([
            {"type": "message", "role": "user", "content": "hi"},
            reasoning,
            {"type": "message", "role": "assistant", "content": "hello"},
            {"type": "message", "role": "user", "content": "and now?"},
        ])
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
    let bad = OpenAiConfig::new(Secret::new("sk-bad\nkey-SECRETPART".to_owned()));
    let error = OpenAiLlm::new(bad, SystemClock::shared()).expect_err("invalid header");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("SECRETPART") && !shown.contains("sk-bad"),
        "{shown}"
    );

    // Refusals echo key fragments in their message; the adapter's error must not.
    mock.script(Script::status(
        401,
        r#"{"error":{"message":"Incorrect API key provided: sk-test-key-0123***","type":"invalid_request_error","code":"invalid_api_key"}}"#,
    ));
    let error = llm.stream(user("hi")).await.expect_err("rejected");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("sk-") && !shown.contains("Incorrect"),
        "{shown}"
    );
}

#[tokio::test]
async fn requests_with_bad_tools_or_tool_messages_fail_before_any_network_io() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let bad_tool = LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![ToolSpec {
            name: Arc::from("has space"),
            description: Arc::from("d"),
            parameters_schema: Arc::from("{}"),
        }],
    };
    assert!(matches!(
        llm.stream(bad_tool).await,
        Err(Error::Protocol { .. })
    ));
    let orphan = LlmRequest {
        messages: vec![Message::new(Role::Tool, "result without a call")],
        tools: Vec::new(),
    };
    assert!(matches!(
        llm.stream(orphan).await,
        Err(Error::Protocol { .. })
    ));
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn the_adapter_identifies_itself() {
    let mock = Mock::start(false).await;
    assert_eq!(provider(&mock).name(), "openai");
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
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        Step::counted(json!({"type": "response.output_text.delta", "delta": "first"}).to_string()),
        Step::Sleep(Duration::from_secs(30)),
        Step::counted(json!({"type": "response.output_text.delta", "delta": "never"}).to_string()),
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
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for block in 0..3 {
        let mut wire = String::new();
        for i in 0..200 {
            let n = block * 200 + i;
            wire.push_str(&format!(
                "data: {}\n\n",
                json!({"type": "response.output_text.delta", "delta": format!("<{n}>")})
            ));
        }
        steps.push(Step::raw(wire.into_bytes()));
    }
    steps.push(Step::event(completed(json!({}))));
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
    let mut steps: Vec<Step> = (0..events)
        .map(|n| {
            Step::counted(
                json!({"type": "response.output_text.delta", "delta": format!("{n:06}{pad}")})
                    .to_string(),
            )
        })
        .collect();
    steps.push(Step::counted(completed(json!({}))));
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
    // After the terminal event the server trickles bytes forever; the reader must let go of the
    // connection after about half a second in total, not per chunk.
    let mock = Mock::start(true).await;
    let llm = provider(&mock);
    let mut steps = vec![Step::counted(completed(json!({})))];
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
async fn keep_alive_comments_do_not_hold_the_idle_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![Step::event(
        json!({"type": "response.output_text.delta", "delta": "a"}).to_string(),
    )];
    for _ in 0..60 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(100)));
    }
    mock.script(Script::stream(steps));
    let start = std::time::Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "a");
    assert_eq!(events.last().and_then(timeout_error), Some("openai idle"));
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn events_hold_the_idle_deadline_whatever_fills_the_gaps() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    // 900 ms in total, never 400 ms between events (reasoning events count: they are events).
    let mut steps = Vec::new();
    for _ in 0..6 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(150)));
        steps.push(Step::event(
            json!({"type": "response.reasoning_summary_text.delta", "delta": "hm"}).to_string(),
        ));
    }
    steps.push(Step::event(
        json!({"type": "response.output_text.delta", "delta": "late"}).to_string(),
    ));
    steps.push(Step::event(completed(json!({}))));
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(text_of(&events), "late");
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn comments_and_lifecycle_events_do_not_extend_the_first_output_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_output_timeout = Duration::from_millis(400);
    cfg.idle_timeout = Duration::from_secs(5);
    let llm = OpenAiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for i in 0..60 {
        steps.push(Step::raw(b": keep-alive\n\n".to_vec()));
        let kind = if i % 2 == 0 {
            "response.created"
        } else {
            "response.in_progress"
        };
        steps.push(Step::event(
            json!({"type": kind, "response": {"id": "resp_1"}}).to_string(),
        ));
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
        Some("openai first output")
    );
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn heartbeat_events_with_empty_data_do_not_break_the_stream() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::raw(b"data:\n\ndata\n\ndata: \n\n".to_vec()),
        Step::event(json!({"type": "response.output_text.delta", "delta": "ok"}).to_string()),
        Step::raw(b"data:\n\n".to_vec()),
        Step::event(completed(json!({}))),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "ok");
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn odd_typed_fields_on_ignored_events_and_numeric_error_codes_are_handled() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(
            json!({"type": "response.some_future_event", "delta": {"x": 1}, "output_index": "a",
                   "code": [1], "item_id": 5})
            .to_string(),
        ),
        Step::event(json!({"type": "response.output_text.delta", "delta": "ok"}).to_string()),
        Step::event(json!({"type": "error", "code": 429, "message": "slow down"}).to_string()),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "ok");
    assert!(
        matches!(
            events.last(),
            Some(LlmEvent::Error(Error::Rejected { status: 429, .. }))
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_malformed_terminal_response_still_ends_with_done() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(json!({"type": "response.output_text.delta", "delta": "answer"}).to_string()),
        Step::event(
            json!({"type": "response.completed", "response": {
                "usage": {"input_tokens": "many"}, "output": 5}})
            .to_string(),
        ),
    ]));
    let events = run(&mock, user("hi")).await;
    assert_eq!(text_of(&events), "answer");
    assert!(
        matches!(
            events.last(),
            Some(LlmEvent::Done {
                finish: FinishReason::Stop,
                usage: None,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn plain_http_is_refused_for_remote_hosts_unless_opted_in() {
    let key = || Secret::new(KEY.to_owned());
    let build = |url: &str, allow: bool| {
        let mut cfg = OpenAiConfig::new(key()).with_base_url(url);
        cfg.allow_insecure_http = allow;
        OpenAiLlm::new(cfg, SystemClock::shared())
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
        "https://api.openai.com",
        "https://gateway.example.com/prefix",
        "http://localhost:8080",
        "http://LOCALHOST",
        "http://127.0.0.1:1234",
        "http://127.9.9.9",
        "http://[::1]:8080",
    ] {
        assert!(build(url, false).is_ok(), "{url}");
    }
    for url in ["ftp://example.com", "not a url", "", "file:///etc/passwd"] {
        assert!(build(url, true).is_err(), "{url:?}");
    }
}

#[tokio::test]
async fn dangling_tool_calls_and_results_are_refused_before_any_network_io() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let call = |id: &str| ToolCall {
        id: Arc::from(id),
        name: Arc::from("lookup"),
        arguments: Arc::from("{}"),
    };
    let histories = [
        // The tool was cancelled by a barge-in and the caller spoke again.
        vec![
            Message::new(Role::User, "look it up"),
            Message::assistant("", vec![call("c1")], None),
            Message::new(Role::User, "hello?"),
        ],
        // The assistant turn was dropped; its result remains.
        vec![
            Message::new(Role::User, "look it up"),
            Message::assistant("", Vec::new(), None),
            Message::tool_result("c1", "found"),
        ],
    ];
    for messages in histories {
        let result = llm
            .stream(LlmRequest {
                messages,
                tools: Vec::new(),
            })
            .await;
        assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
    }
    assert!(mock.requests().is_empty());
}

/// The HTTP client (and with it the connection pool) is shared by every adapter instance
/// of the process that talks to the same origin the same way, so a call that starts after
/// another one finds a warm connection instead of paying a handshake.
#[tokio::test]
async fn adapter_instances_of_one_origin_share_the_connection_pool() {
    let mock = Mock::start(true).await;
    let before = brisvox_providers::net::pool_stats();
    // Three "sessions": three adapter instances with the same settings.
    for _ in 0..3 {
        provider(&mock).warm().await.expect("warm");
    }
    let after = brisvox_providers::net::pool_stats();
    assert_eq!(mock.connections(), 1, "one connection served all three");
    assert_eq!(mock.requests().len(), 3);
    assert!(after.reused >= before.reused + 2, "{before:?} -> {after:?}");
}

/// Settings that change how the client connects are part of the pool key: a different connect
/// timeout is a different client with its own connection.
#[tokio::test]
async fn a_different_connect_timeout_gets_its_own_pool_entry() {
    let mock = Mock::start(true).await;
    provider(&mock).warm().await.expect("warm");
    let mut other = config(&mock);
    other.connect_timeout = Duration::from_millis(2_345);
    let other = OpenAiLlm::new(other, SystemClock::shared()).expect("provider");
    other.warm().await.expect("warm");
    assert_eq!(mock.connections(), 2);
}
