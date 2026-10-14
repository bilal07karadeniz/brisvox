//! Live test against the real Gemini API. Runs only with `BRISVOX_LIVE_TESTS=1` and
//! `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) in the environment (never printed); skipped otherwise.
//! Optional `GEMINI_MODEL` overrides the model. This is the live check: it confirms the
//! wire shapes `docs/providers/gemini.md` marks UNVERIFIED (chunk framing, usage placement,
//! signature replay, whole function calls, Turkish).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SystemClock};
use brisvox_providers::gemini::{GeminiConfig, GeminiLlm};
use brisvox_providers::{
    FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role, ToolCall, ToolSpec,
};

/// What one live response produced.
#[derive(Default)]
struct Outcome {
    text: String,
    calls: Vec<(String, String, String)>,
    finish: Option<FinishReason>,
    cached: Option<u32>,
    output_tokens: Option<u32>,
    provider_data: Option<Arc<str>>,
    first_token: Option<Duration>,
}

async fn read(llm: &GeminiLlm, request: LlmRequest) -> Outcome {
    let clock = SystemClock::shared();
    let start = clock.now();
    let mut stream = llm.stream(request).await.expect("the request is accepted");
    let mut outcome = Outcome::default();
    while let Some(event) = stream.next().await {
        match event {
            LlmEvent::TextDelta(t) => {
                outcome.first_token.get_or_insert(clock.now() - start);
                outcome.text.push_str(&t);
            }
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                outcome.first_token.get_or_insert(clock.now() - start);
                assert_eq!(index as usize, outcome.calls.len(), "calls arrive whole");
                outcome.calls.push((
                    id.expect("an id").to_string(),
                    name.expect("a name").to_string(),
                    arguments.to_string(),
                ));
            }
            LlmEvent::Done {
                finish,
                usage,
                provider_data,
            } => {
                outcome.finish = Some(finish);
                outcome.cached = usage.map(|u| u.cached_input_tokens);
                outcome.output_tokens = usage.map(|u| u.output_tokens);
                outcome.provider_data = provider_data;
            }
            LlmEvent::Error(error) => panic!("the live stream failed: {error}"),
        }
    }
    outcome
}

#[tokio::test]
async fn a_real_session_streams_text_calls_a_tool_and_replays_it() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
        return;
    }
    let Some(key) = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|k| !k.is_empty()))
    else {
        eprintln!("skipped: GEMINI_API_KEY is not set");
        return;
    };
    let mut config = GeminiConfig::new(Secret::new(key));
    if let Some(model) = std::env::var("GEMINI_MODEL").ok().filter(|m| !m.is_empty()) {
        config.model = model;
    }
    config.first_byte_timeout = Duration::from_secs(30);
    config.first_output_timeout = Duration::from_secs(60);
    config.idle_timeout = Duration::from_secs(30);
    let llm = GeminiLlm::new(config, SystemClock::shared()).expect("provider");
    llm.warm().await.expect("warm");

    // 1. Plain text, in Turkish.
    let plain = read(
        &llm,
        LlmRequest {
            messages: vec![
                Message::new(Role::System, "You are a terse phone assistant."),
                Message::new(Role::User, "Merhaba! Tek kisa cumleyle selam ver."),
            ],
            tools: Vec::new(),
        },
    )
    .await;
    assert!(!plain.text.trim().is_empty(), "no text");
    assert_eq!(plain.finish, Some(FinishReason::Stop));
    assert!(plain.output_tokens.is_some_and(|n| n > 0), "no usage");
    eprintln!("live: plain first token after {:?}", plain.first_token);

    // 2. A tool call, then the tool result replayed statelessly (signature echoed).
    let tools = vec![ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Current weather of a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}"#,
        ),
    }];
    let mut messages = vec![
        Message::new(Role::System, "Use tools when they help. Be terse."),
        Message::new(
            Role::User,
            "What is the weather in Paris right now? Use the tool.",
        ),
    ];
    let called = read(
        &llm,
        LlmRequest {
            messages: messages.clone(),
            tools: tools.clone(),
        },
    )
    .await;
    assert_eq!(called.finish, Some(FinishReason::ToolCalls));
    let [(id, name, arguments)] = called.calls.as_slice() else {
        panic!("expected one call, got {}", called.calls.len());
    };
    assert_eq!(name, "get_weather");
    let parsed: serde_json::Value = serde_json::from_str(arguments).expect("arguments are JSON");
    assert!(parsed["city"].is_string(), "{arguments}");
    eprintln!(
        "live: tool call first token after {:?}, provider data kept: {}",
        called.first_token,
        called.provider_data.is_some()
    );

    messages.push(Message::assistant(
        called.text.clone(),
        vec![ToolCall {
            id: Arc::from(id.as_str()),
            name: Arc::from(name.as_str()),
            arguments: Arc::from(arguments.as_str()),
        }],
        called.provider_data.clone(),
    ));
    messages.push(Message::tool_result(
        id.as_str(),
        r#"{"temperature_c":18,"sky":"cloudy"}"#,
    ));
    let answered = read(
        &llm,
        LlmRequest {
            messages: messages.clone(),
            tools: tools.clone(),
        },
    )
    .await;
    assert!(
        !answered.text.trim().is_empty(),
        "no answer after the tool result"
    );
    assert_eq!(answered.finish, Some(FinishReason::Stop));
    eprintln!(
        "live: answer first token after {:?}, cached input tokens {:?}",
        answered.first_token, answered.cached
    );

    // 3. The same history rebuilt without provider data: the documented fallback signature must
    //    be accepted by the API.
    if let Some(last_assistant) = messages
        .iter_mut()
        .rev()
        .find(|m| m.role == Role::Assistant)
    {
        last_assistant.provider_data = None;
    }
    let rebuilt = read(&llm, LlmRequest { messages, tools }).await;
    assert!(
        !rebuilt.text.trim().is_empty(),
        "the fallback signature was not accepted"
    );
}
