//! MockLlm: scripted streaming, latency profiles and every fault kind, under paused time.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)] // test helpers outside #[test] fns

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{SharedClock, SimClock};
use brisvox_providers::{Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::leak::TaskCounter;
use brisvox_sim::mock_llm::{LlmFault, LlmScript, MockLlm};

const MS: Duration = Duration::from_millis(1);

fn request(text: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, text)],
        tools: Vec::new(),
    }
}

/// Drains a stream, returning (simulated ms since start, event) pairs.
async fn collect(
    clock: &SharedClock,
    mut stream: brisvox_providers::LlmStream,
) -> Vec<(u128, LlmEvent)> {
    let start = clock.now();
    let mut out = Vec::new();
    while let Some(event) = stream.next().await {
        out.push(((clock.now() - start).as_millis(), event));
    }
    out
}

fn mock(clock: &SharedClock, first: u64, gap: u64) -> MockLlm {
    MockLlm::new(clock.clone()).with_latency(
        LatencyProfile::Fixed(Duration::from_millis(first)),
        LatencyProfile::Fixed(Duration::from_millis(gap)),
        0,
    )
}

/// Concatenated arguments per tool-call index, plus (id, name) from the first fragment.
fn assemble(events: &[(u128, LlmEvent)]) -> Vec<(String, String, String)> {
    let mut calls: Vec<(String, String, String)> = Vec::new();
    for (_, event) in events {
        if let LlmEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } = event
        {
            let i = usize::try_from(*index).unwrap();
            if calls.len() <= i {
                calls.resize(i + 1, Default::default());
            }
            let call = &mut calls[i];
            if let Some(id) = id {
                call.0 = id.to_string();
            }
            if let Some(name) = name {
                call.1 = name.to_string();
            }
            call.2.push_str(arguments);
        }
    }
    calls
}

#[tokio::test(start_paused = true)]
async fn text_stream_follows_the_latency_profile_exactly() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 300, 20);
    llm.push(LlmScript::text(["Hello", " there", "!"]));
    let events = collect(&clock, llm.stream(request("hi")).await.unwrap()).await;
    let timeline: Vec<(u128, String)> = events
        .iter()
        .map(|(t, e)| {
            let label = match e {
                LlmEvent::TextDelta(text) => text.to_string(),
                LlmEvent::Done { finish, .. } => format!("{finish:?}"),
                other => format!("{other:?}"),
            };
            (*t, label)
        })
        .collect();
    assert_eq!(
        timeline,
        [
            (300, "Hello".to_owned()),
            (320, " there".to_owned()),
            (340, "!".to_owned()),
            (360, "Stop".to_owned()),
        ]
    );
    assert_eq!(llm.requests(), [request("hi")]);
}

#[tokio::test(start_paused = true)]
async fn tool_calls_stream_as_argument_deltas() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 0, 0);
    let args = r#"{"city":"İstanbul","unit":"celsius"}"#;
    llm.push(
        LlmScript::text(["Checking."])
            .with_tool_call("call_1", "weather", args)
            .with_tool_call("call_2", "time", r#"{"tz":"Europe/Istanbul"}"#),
    );
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    let calls = assemble(&events);
    assert_eq!(
        calls,
        [
            ("call_1".into(), "weather".into(), args.into()),
            (
                "call_2".into(),
                "time".into(),
                r#"{"tz":"Europe/Istanbul"}"#.into()
            ),
        ]
    );
    let deltas = events
        .iter()
        .filter(|(_, e)| matches!(e, LlmEvent::ToolCallDelta { .. }))
        .count();
    assert!(deltas > 4, "arguments should be split into several deltas");
    assert!(matches!(
        events.last(),
        Some((
            _,
            LlmEvent::Done {
                finish: FinishReason::ToolCalls,
                ..
            }
        ))
    ));
}

#[tokio::test(start_paused = true)]
async fn interleaved_deltas_still_assemble_by_index() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 0, 0);
    llm.push(
        LlmScript::text(Vec::<String>::new())
            .with_tool_call("a", "f", r#"{"x":1234567890}"#)
            .with_tool_call("b", "g", r#"{"y":"abcdefghij"}"#)
            .with_fault(LlmFault::InterleavedToolDeltas),
    );
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    let indices: Vec<u32> = events
        .iter()
        .filter_map(|(_, e)| match e {
            LlmEvent::ToolCallDelta { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(&indices[..4], [0, 1, 0, 1], "deltas must interleave");
    let calls = assemble(&events);
    assert_eq!(calls[0].2, r#"{"x":1234567890}"#);
    assert_eq!(calls[1].2, r#"{"y":"abcdefghij"}"#);
}

#[tokio::test(start_paused = true)]
async fn malformed_tool_arguments_are_not_valid_json() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 0, 0);
    llm.push(
        LlmScript::text(Vec::<String>::new())
            .with_tool_call("a", "book", r#"{"room":12,"nights":3}"#)
            .with_fault(LlmFault::MalformedToolArguments),
    );
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    let args = &assemble(&events)[0].2;
    assert!(args.starts_with('{') && !args.ends_with('}'), "{args}");
}

#[tokio::test(start_paused = true)]
async fn error_response_is_returned_before_streaming() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 0, 0);
    llm.push(LlmScript::text(["never"]).with_fault(LlmFault::ErrorResponse { status: 429 }));
    let result = llm.stream(request("x")).await;
    assert!(matches!(
        result,
        Err(Error::Rejected {
            provider: "mock-llm",
            status: 429
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn disconnect_mid_stream_ends_without_done() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 10, 10);
    llm.push(
        LlmScript::text(["a", "b", "c", "d"]).with_fault(LlmFault::Disconnect { after_events: 2 }),
    );
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(matches!(events[0].1, LlmEvent::TextDelta(_)));
    assert!(matches!(events[1].1, LlmEvent::TextDelta(_)));
    assert!(matches!(
        events[2].1,
        LlmEvent::Error(Error::Disconnected { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn stall_delays_one_event() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 100, 10);
    llm.push(
        LlmScript::text(["a", "b", "c"]).with_fault(LlmFault::Stall {
            before_event: 1,
            duration: 2_000 * MS,
        }),
    );
    let times: Vec<u128> = collect(&clock, llm.stream(request("x")).await.unwrap())
        .await
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(times, [100, 2_110, 2_120, 2_130]);
}

#[tokio::test(start_paused = true)]
async fn seeded_random_latency_is_reproducible() {
    let run = |seed| async move {
        let clock: SharedClock = Arc::new(SimClock::new());
        let llm = MockLlm::new(clock.clone()).with_latency(
            LatencyProfile::LogNormal {
                median: 400 * MS,
                sigma: 0.5,
            },
            LatencyProfile::Normal {
                mean: 30 * MS,
                std_dev: 10 * MS,
            },
            seed,
        );
        llm.set_fallback(LlmScript::text(["a", "b", "c", "d", "e"]));
        let mut all = Vec::new();
        for _ in 0..3 {
            let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
            all.extend(events.into_iter().map(|(t, _)| t));
        }
        all
    };
    assert_eq!(run(42).await, run(42).await);
    assert_ne!(run(42).await, run(43).await);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_stops_the_producer() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let counter = TaskCounter::start().unwrap();
    let llm = mock(&clock, 50, 50);
    llm.set_fallback(LlmScript::text(vec!["token"; 1_000]));
    let mut stream = llm.stream(request("x")).await.unwrap();
    assert!(matches!(stream.next().await, Some(LlmEvent::TextDelta(_))));
    drop(stream);
    counter
        .wait_for_baseline(clock.as_ref(), Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn protocol_error_and_disconnect_arrive_at_the_right_time() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 100, 10);
    llm.push(
        LlmScript::text(["a", "b", "c"]).with_fault(LlmFault::ProtocolError { after_events: 1 }),
    );
    llm.push(LlmScript::text(["a", "b", "c"]).with_fault(LlmFault::Disconnect { after_events: 0 }));
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[1],
        (110, LlmEvent::Error(Error::Protocol { .. }))
    ));
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    assert!(matches!(
        events.as_slice(),
        [(100, LlmEvent::Error(Error::Disconnected { .. }))]
    ));
}

#[tokio::test(start_paused = true)]
async fn connect_latency_applies_before_rejection_and_streaming() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 100, 10).with_connect_latency(LatencyProfile::Fixed(50 * MS), 0);
    llm.push(LlmScript::text(["a"]).with_fault(LlmFault::ErrorResponse { status: 500 }));
    llm.push(LlmScript::text(["a"]));
    let start = clock.now();
    assert!(llm.stream(request("x")).await.is_err());
    assert_eq!(clock.now() - start, 50 * MS);
    let stream = llm.stream(request("x")).await.unwrap();
    assert_eq!(clock.now() - start, 100 * MS);
    let first = collect(&clock, stream).await[0].0;
    assert_eq!(first, 100, "first token 100 ms after stream() returned");
}

#[tokio::test(start_paused = true)]
async fn stall_before_the_first_event_delays_time_to_first_token() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 100, 10);
    llm.push(LlmScript::text(["a"]).with_fault(LlmFault::Stall {
        before_event: 0,
        duration: 900 * MS,
    }));
    let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
    assert_eq!(events[0].0, 1_000);
}

#[tokio::test(start_paused = true)]
async fn queued_scripts_play_in_order_then_the_fallback() {
    let clock: SharedClock = Arc::new(SimClock::new());
    let llm = mock(&clock, 0, 0);
    llm.push(LlmScript::text(["first"]));
    llm.push(LlmScript::text(["second"]));
    llm.set_fallback(LlmScript::text(["fallback"]));
    let mut texts = Vec::new();
    for _ in 0..4 {
        let events = collect(&clock, llm.stream(request("x")).await.unwrap()).await;
        if let (_, LlmEvent::TextDelta(text)) = &events[0] {
            texts.push(text.to_string());
        }
        assert!(matches!(
            events.last(),
            Some((_, LlmEvent::Done { usage: Some(_), .. }))
        ));
    }
    assert_eq!(texts, ["first", "second", "fallback", "fallback"]);
}
