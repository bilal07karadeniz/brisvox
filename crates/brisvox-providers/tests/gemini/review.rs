//! Tests added in a review round: turn completion without usage, idle semantics,
//! deadlines on the injected clock (every deadline of the adapter), a stalled consumer, a leading model turn, the tool cache
//! and HTTP/2.

use std::sync::Arc;
use std::time::{Duration, Instant};

use brisvox_core::{Error as CoreError, Secret, SharedClock, SystemClock};
use brisvox_providers::gemini::{GeminiConfig, GeminiLlm};
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role, ToolCall, ToolSpec,
};
use serde_json::json;

use crate::clock::SkewedClock;
use crate::h2mock::{H2Mock, Mode};
use crate::mock::{Mock, Script, Step};
use crate::protocol::{
    chunk, collect, config, stop, text_chunk, text_of, timeout_error, usage, user, wait_for_hangups,
};

fn finish_without_usage() -> Step {
    Step::event(chunk(
        json!([{"text": "a"},
               {"functionCall": {"id": "c1", "name": "get_weather", "args": {"city": "Paris"}},
                "thoughtSignature": "SIG"}]),
        Some("STOP"),
        None,
    ))
}

fn assert_complete_turn(events: &[LlmEvent]) {
    assert_eq!(text_of(events), "a");
    let Some(LlmEvent::Done {
        finish,
        usage,
        provider_data,
    }) = events.last()
    else {
        panic!("no Done: {:?}", events.last())
    };
    assert_eq!(*finish, FinishReason::ToolCalls);
    assert_eq!(*usage, None);
    let data: serde_json::Value =
        serde_json::from_str(provider_data.as_deref().expect("provider data")).unwrap();
    assert_eq!(data[1]["thoughtSignature"], json!("SIG"));
}

#[tokio::test]
async fn a_finish_without_usage_on_an_open_connection_completes_after_the_grace_period() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.usage_grace = Duration::from_millis(200);
    cfg.idle_timeout = Duration::from_secs(10);
    cfg.first_output_timeout = Duration::from_secs(10);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        finish_without_usage(),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let start = Instant::now();
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_complete_turn(&events);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "Done after {:?}, not after the 10 s idle deadline",
        start.elapsed()
    );
    // The abandoned response is hung up on.
    assert!(
        wait_for_hangups(&mock, 1, Duration::from_secs(2))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn a_late_usage_chunk_within_the_grace_period_is_still_used() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.usage_grace = Duration::from_millis(1500);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![
        finish_without_usage(),
        Step::Sleep(Duration::from_millis(100)),
        Step::event(json!({"usageMetadata": usage()}).to_string()),
    ]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Done { usage: Some(u), .. }) if u.input_tokens == 120
    ));
}

#[tokio::test]
async fn a_transport_error_after_the_finish_reason_still_completes_the_turn() {
    let mock = Mock::start(false).await;
    // The body is cut (no chunk terminator) right after the finish chunk.
    mock.script(Script::stream(vec![finish_without_usage(), Step::Close]));
    let llm = GeminiLlm::new(config(&mock), SystemClock::shared()).expect("provider");
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_complete_turn(&events);

    // Before the finish the same cut is still a lost connection.
    mock.script(Script::stream(vec![text_chunk("a"), Step::Close]));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(matches!(
        events.last(),
        Some(LlmEvent::Error(Error::Disconnected { .. }))
    ));
}

#[tokio::test]
async fn heartbeats_empty_chunks_and_usage_only_chunks_do_not_hold_the_idle_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = vec![text_chunk("a")];
    for _ in 0..60 {
        steps.push(Step::raw(b"data:\n\n".to_vec()));
        steps.push(Step::event("{}"));
        steps.push(Step::event(
            json!({"usageMetadata": {"promptTokenCount": 3}}).to_string(),
        ));
        steps.push(Step::event(chunk(json!([]), None, None)));
        steps.push(Step::Sleep(Duration::from_millis(100)));
    }
    mock.script(Script::stream(steps));
    let start = Instant::now();
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
async fn output_events_keep_the_idle_deadline_alive() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(400);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    let mut steps = Vec::new();
    for i in 0..10 {
        steps.push(text_chunk(&format!("<{i}>")));
        steps.push(Step::Sleep(Duration::from_millis(150)));
    }
    steps.push(Step::event(stop()));
    mock.script(Script::stream(steps));
    let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

#[tokio::test]
async fn deadlines_are_computed_on_the_injected_clock() {
    // The clock runs 2 s ahead of real time, so every deadline derived from it lies 2 s later
    // in real time. With `Instant::now()` or a tokio sleep in the adapter, each of these would
    // fire after about 300 ms.
    const SKEW: Duration = Duration::from_secs(2);
    let clock = || -> SharedClock { Arc::new(SkewedClock::new(SKEW)) };
    let short = Duration::from_millis(300);

    // Headers that never come: `stream()` times out.
    let first_byte = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.first_byte_timeout = short;
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        let mut script = Script::stream(Vec::new());
        script.pre_delay = Duration::from_secs(30);
        mock.script(script);
        let start = Instant::now();
        let result = llm.stream(user("hi")).await;
        assert!(matches!(
            result,
            Err(Error::Core(CoreError::Timeout {
                what: "gemini first byte",
                ..
            }))
        ));
        start.elapsed()
    };
    // Headers, then silence before any output.
    let first_output = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.first_output_timeout = short;
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        mock.script(Script::stream(vec![Step::Sleep(Duration::from_secs(30))]));
        let start = Instant::now();
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert_eq!(
            events.last().and_then(timeout_error),
            Some("gemini first output")
        );
        start.elapsed()
    };
    // Output, then silence.
    let idle = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.idle_timeout = short;
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        mock.script(Script::stream(vec![
            text_chunk("a"),
            Step::Sleep(Duration::from_secs(30)),
        ]));
        let start = Instant::now();
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert_eq!(events.last().and_then(timeout_error), Some("gemini idle"));
        start.elapsed()
    };
    // The total duration cap, while comments keep arriving.
    let stream_cap = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.max_stream_duration = short;
        cfg.first_output_timeout = Duration::from_secs(20);
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        let mut steps = Vec::new();
        for _ in 0..80 {
            steps.push(Step::raw(b": ping\n\n".to_vec()));
            steps.push(Step::Sleep(Duration::from_millis(50)));
        }
        mock.script(Script::stream(steps));
        let start = Instant::now();
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert_eq!(events.last().and_then(timeout_error), Some("gemini stream"));
        start.elapsed()
    };
    // The usage grace period after a finish without usage.
    let grace = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.usage_grace = short;
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        mock.script(Script::stream(vec![
            finish_without_usage(),
            Step::Sleep(Duration::from_secs(30)),
        ]));
        let start = Instant::now();
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
        start.elapsed()
    };
    // The refusal body that stalls: read for at most ERROR_BODY_TIMEOUT (1 s) of the injected
    // clock, so 3 s of real time here; a tokio sleep would cut it off after 1 s.
    let error_body = async {
        let mock = Mock::start(false).await;
        let llm = GeminiLlm::new(config(&mock), clock()).expect("provider");
        let body = json!({"error": {"code": 503, "status": "UNAVAILABLE"}}).to_string();
        mock.script(Script::status_then_stall(503, &body, 4096));
        let start = Instant::now();
        let result = llm.stream(user("hi")).await;
        assert!(
            matches!(result, Err(Error::Rejected { status: 503, .. })),
            "{result:?}"
        );
        start.elapsed()
    };
    // A consumer that never reads: abandoned after `consumer_stall_timeout` of the injected clock.
    let consumer_stall = async {
        let mock = Mock::start(false).await;
        let mut cfg = config(&mock);
        cfg.consumer_stall_timeout = short;
        cfg.idle_timeout = Duration::from_secs(20);
        cfg.first_output_timeout = Duration::from_secs(20);
        cfg.max_stream_duration = Duration::from_secs(60);
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        let mut wire = String::new();
        for i in 0..300 {
            wire.push_str(&format!(
                "data: {}\n\n",
                chunk(json!([{"text": format!("<{i}>")}]), None, None)
            ));
        }
        mock.script(Script::stream(vec![
            Step::raw(wire.into_bytes()),
            Step::Sleep(Duration::from_secs(30)),
        ]));
        let stream = llm.stream(user("hi")).await.expect("accepted");
        let took = wait_for_hangups(&mock, 1, Duration::from_secs(8)).await;
        drop(stream);
        took.expect("the stalled consumer was abandoned")
    };
    // After the finish, a server that keeps the body open is let go after DRAIN_TIMEOUT
    // (500 ms) of the injected clock, so 2.5 s of real time here.
    let drain = async {
        let mock = Mock::start(false).await;
        let llm = GeminiLlm::new(config(&mock), clock()).expect("provider");
        mock.script(Script::stream(vec![
            Step::event(stop()),
            Step::Sleep(Duration::from_secs(30)),
        ]));
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
        wait_for_hangups(&mock, 1, Duration::from_secs(8))
            .await
            .expect("the drain was bounded")
    };
    // The warm-up HEAD that is never answered: `first_byte_timeout` of the injected clock.
    let warm = async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let accepting = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let mut cfg = GeminiConfig::new(Secret::new("k".to_owned())).with_base_url(url);
        cfg.first_byte_timeout = short;
        let llm = GeminiLlm::new(cfg, clock()).expect("provider");
        let start = Instant::now();
        let result = llm.warm().await;
        assert!(
            matches!(
                result,
                Err(Error::Core(CoreError::Timeout {
                    what: "gemini warm",
                    ..
                }))
            ),
            "{result:?}"
        );
        accepting.abort();
        start.elapsed()
    };
    let (a, b, c, d, e, f, g, h, i) = tokio::join!(
        first_byte,
        first_output,
        idle,
        stream_cap,
        grace,
        error_body,
        consumer_stall,
        drain,
        warm
    );
    for (name, took) in [
        ("first byte", a),
        ("first output", b),
        ("idle", c),
        ("stream cap", d),
        ("usage grace", e),
        ("error body", f),
        ("consumer stall", g),
        ("drain", h),
        ("warm", i),
    ] {
        assert!(
            took >= Duration::from_millis(1800),
            "{name} fired after {took:?}: the deadline did not follow the injected clock"
        );
        assert!(took < Duration::from_secs(4), "{name}: {took:?}");
    }
}

#[tokio::test]
async fn a_consumer_that_stops_reading_cannot_pin_the_response_for_ever() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.consumer_stall_timeout = Duration::from_millis(300);
    cfg.idle_timeout = Duration::from_secs(20);
    cfg.first_output_timeout = Duration::from_secs(20);
    cfg.max_stream_duration = Duration::from_secs(60);
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    // Far more events than the channel holds, then the server idles.
    let mut wire = String::new();
    for i in 0..300 {
        wire.push_str(&format!(
            "data: {}\n\n",
            chunk(json!([{"text": format!("<{i}>")}]), None, None)
        ));
    }
    mock.script(Script::stream(vec![
        Step::raw(wire.into_bytes()),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    // Alive, but never read.
    let stream = llm.stream(user("hi")).await.expect("accepted");
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(3)).await;
    assert!(
        took.is_some_and(|t| t < Duration::from_millis(2000)),
        "the response was abandoned after {took:?}"
    );
    // What was buffered is still delivered, in order, and then the stream ends without a Done.
    let events = collect(stream).await;
    assert!(!events.is_empty());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, LlmEvent::Done { .. } | LlmEvent::Error(_)))
    );
    let text = text_of(&events);
    assert!(text.starts_with("<0><1><2>"), "{text}");
}

fn greeting_request() -> LlmRequest {
    LlmRequest {
        messages: vec![
            Message::new(Role::System, "You are a phone agent."),
            Message::new(Role::Assistant, "Merhaba, nasil yardimci olabilirim?"),
            Message::new(Role::User, "Randevu almak istiyorum."),
        ],
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn a_history_that_starts_with_the_agent_greeting_gets_a_leading_user_content() {
    let mock = Mock::start(false).await;
    let llm = GeminiLlm::new(config(&mock), SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![Step::event(stop())]));
    collect(llm.stream(greeting_request()).await.expect("accepted")).await;
    let body = mock.posts()[0].json();
    assert_eq!(
        body["contents"],
        json!([
            {"role": "user", "parts": [{"text": "(The call has started.)"}]},
            {"role": "model", "parts": [{"text": "Merhaba, nasil yardimci olabilirim?"}]},
            {"role": "user", "parts": [{"text": "Randevu almak istiyorum."}]}
        ])
    );

    // Opting out sends the history as it is.
    let mut cfg = config(&mock);
    cfg.leading_user_placeholder = None;
    let llm = GeminiLlm::new(cfg, SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![Step::event(stop())]));
    collect(llm.stream(greeting_request()).await.expect("accepted")).await;
    assert_eq!(
        mock.posts()[1].json()["contents"][0]["role"],
        json!("model")
    );

    // A history that starts with a user turn is never touched.
    let llm = GeminiLlm::new(config(&mock), SystemClock::shared()).expect("provider");
    mock.script(Script::stream(vec![Step::event(stop())]));
    collect(llm.stream(user("hi")).await.expect("accepted")).await;
    assert_eq!(
        mock.posts()[2].json()["contents"],
        json!([{"role": "user", "parts": [{"text": "hi"}]}])
    );
}

#[tokio::test]
async fn tool_sets_that_alternate_are_each_sent_byte_stable_and_free_form_objects_are_degraded() {
    let mock = Mock::start(false).await;
    let llm = GeminiLlm::new(config(&mock), SystemClock::shared()).expect("provider");
    let tool = |name: &str| ToolSpec {
        name: Arc::from(name),
        description: Arc::from("d"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"meta":{"type":"object","additionalProperties":true}}}"#,
        ),
    };
    for name in ["a", "b", "a", "b", "a"] {
        mock.script(Script::stream(vec![Step::event(stop())]));
        let request = LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![tool(name)],
        };
        collect(llm.stream(request).await.expect("accepted")).await;
    }
    let posts = mock.posts();
    let tools_of = |i: usize| posts[i].json()["tools"].to_string();
    assert_eq!(tools_of(0), tools_of(2));
    assert_eq!(tools_of(0), tools_of(4));
    assert_eq!(tools_of(1), tools_of(3));
    assert_ne!(tools_of(0), tools_of(1));
    // The nested free-form object is not sent as a bare OBJECT.
    let sent = tools_of(0);
    assert!(sent.contains(r#""type":"STRING""#), "{sent}");
    assert!(!sent.contains(r#""meta":{"type":"OBJECT"}"#), "{sent}");
}

// --- HTTP/2 -----------------------------------------------------------------------------

fn h2_provider(mock: &H2Mock) -> GeminiLlm {
    let mut cfg = GeminiConfig::new(Secret::new("AIza-test".to_owned()))
        .with_base_url(mock.base_url())
        .with_timeout(Duration::from_secs(3));
    cfg.http2_prior_knowledge = true;
    GeminiLlm::new(cfg, SystemClock::shared()).expect("provider")
}

async fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    done()
}

#[tokio::test]
async fn over_http2_warm_and_streams_share_one_connection() {
    let mock = H2Mock::start(Mode::Finish).await;
    let llm = h2_provider(&mock);
    llm.warm().await.expect("warm");
    assert_eq!(mock.heads(), 1);
    for _ in 0..3 {
        let events = collect(llm.stream(user("hi")).await.expect("accepted")).await;
        assert_eq!(text_of(&events), "hi");
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    }
    assert_eq!(mock.posts(), 3);
    assert_eq!(
        mock.connections(),
        1,
        "the HEAD warm-up leaves the h2 connection the streams reuse"
    );
}

#[tokio::test]
async fn over_http2_a_burst_of_concurrent_streams_is_multiplexed_on_one_connection() {
    const STREAMS: usize = 40;
    let mock = H2Mock::start(Mode::Paced {
        events: 5,
        pace: Duration::from_millis(60),
    })
    .await;
    let llm = Arc::new(h2_provider(&mock));
    // Open the connection first so the burst cannot race to open several.
    llm.warm().await.expect("warm");
    let mut tasks = Vec::new();
    for _ in 0..STREAMS {
        let llm = Arc::clone(&llm);
        tasks.push(tokio::spawn(async move {
            collect(llm.stream(user("hi")).await.expect("accepted")).await
        }));
    }
    for task in tasks {
        let events = task.await.expect("task");
        assert_eq!(text_of(&events), "hihihihihi");
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    }
    assert_eq!(mock.posts(), STREAMS);
    assert_eq!(mock.connections(), 1, "one connection carried every stream");
    assert!(
        mock.max_active() >= STREAMS / 2,
        "only {} streams were open at once",
        mock.max_active()
    );
}

#[tokio::test]
async fn over_http2_dropping_the_stream_resets_the_stream_and_keeps_the_connection() {
    let mock = H2Mock::start(Mode::Hang).await;
    let llm = h2_provider(&mock);
    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    assert!(matches!(stream.next().await, Some(LlmEvent::TextDelta(_))));
    assert_eq!(mock.resets(), 0, "the server is still waiting");
    let start = Instant::now();
    drop(stream);
    assert!(
        wait_until(Duration::from_secs(2), || mock.resets() == 1).await,
        "no RST_STREAM after the drop"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    // RST_STREAM ends the stream, not the connection: the next request reuses it.
    mock.set_mode(Mode::Finish);
    let events = collect(llm.stream(user("again")).await.expect("accepted")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    assert_eq!(mock.connections(), 1);
}

#[tokio::test]
async fn over_http2_a_tool_result_turn_completes() {
    // Smoke test of a request with a call and its result over h2.
    let mock = H2Mock::start(Mode::Finish).await;
    let llm = h2_provider(&mock);
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::User, "weather?"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: Arc::from("c1"),
                    name: Arc::from("get_weather"),
                    arguments: Arc::from("{}"),
                }],
                None,
            ),
            Message::tool_result("c1", "sunny"),
        ],
        tools: Vec::new(),
    };
    let events = collect(llm.stream(request).await.expect("accepted")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
}

/// Two "sessions" (two adapter instances of the process) talk to one origin over HTTP/2:
/// the second one's warm-up finds the connection the first one opened, and their concurrent
/// streams are multiplexed on that one connection.
#[tokio::test]
async fn over_http2_two_adapter_instances_share_one_connection() {
    const STREAMS_PER_SESSION: usize = 10;
    let mock = H2Mock::start(Mode::Paced {
        events: 3,
        pace: Duration::from_millis(40),
    })
    .await;
    let first = Arc::new(h2_provider(&mock));
    first.warm().await.expect("warm");
    assert_eq!(mock.connections(), 1);
    // The second session is built after the first one's connection exists.
    let second = Arc::new(h2_provider(&mock));
    second.warm().await.expect("warm");
    assert_eq!(mock.heads(), 2);
    assert_eq!(mock.connections(), 1, "the second warm-up reused it");
    let mut tasks = Vec::new();
    for llm in [&first, &second] {
        for _ in 0..STREAMS_PER_SESSION {
            let llm = Arc::clone(llm);
            tasks.push(tokio::spawn(async move {
                collect(llm.stream(user("hi")).await.expect("accepted")).await
            }));
        }
    }
    for task in tasks {
        let events = task.await.expect("task");
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    }
    assert_eq!(mock.posts(), 2 * STREAMS_PER_SESSION);
    assert_eq!(
        mock.connections(),
        1,
        "one connection carried both sessions"
    );
    assert!(
        mock.max_active() >= STREAMS_PER_SESSION,
        "only {} streams were open at once",
        mock.max_active()
    );
}
