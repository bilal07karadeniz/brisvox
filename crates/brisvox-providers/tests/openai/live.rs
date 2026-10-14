//! Live test against the real OpenAI API. Runs only with `BRISVOX_LIVE_TESTS=1` and
//! `OPENAI_API_KEY` in the environment (never printed); skipped otherwise. Optional
//! `OPENAI_MODEL` overrides the model. This is the live check: it confirms the wire
//! shapes `docs/providers/openai.md` marks UNVERIFIED (reasoning replay, strict tools, usage).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SystemClock};
use brisvox_providers::openai::{OpenAiConfig, OpenAiLlm};
use brisvox_providers::{
    FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role, ToolCall, ToolSpec,
};

/// What one live response produced.
#[derive(Default)]
struct Outcome {
    text: String,
    call: Option<(String, String, String)>,
    finish: Option<FinishReason>,
    cached: Option<u32>,
    output_tokens: Option<u32>,
    provider_data: Option<Arc<str>>,
    first_token: Option<Duration>,
}

async fn read(llm: &OpenAiLlm, request: LlmRequest) -> Outcome {
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
                id,
                name,
                arguments,
                ..
            } => {
                outcome.first_token.get_or_insert(clock.now() - start);
                let call = outcome.call.get_or_insert_with(Default::default);
                if let Some(id) = id {
                    call.0 = id.to_string();
                }
                if let Some(name) = name {
                    call.1 = name.to_string();
                }
                call.2.push_str(&arguments);
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
    let Some(key) = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        eprintln!("skipped: OPENAI_API_KEY is not set");
        return;
    };
    let mut config = OpenAiConfig::new(Secret::new(key));
    if let Some(model) = std::env::var("OPENAI_MODEL").ok().filter(|m| !m.is_empty()) {
        config.model = model;
    }
    config.first_byte_timeout = Duration::from_secs(30);
    config.idle_timeout = Duration::from_secs(30);
    let llm = OpenAiLlm::new(config, SystemClock::shared()).expect("provider");
    llm.warm().await.expect("warm");

    // 1. Plain text.
    let plain = read(
        &llm,
        LlmRequest {
            messages: vec![
                Message::new(Role::System, "You are a terse phone assistant."),
                Message::new(Role::User, "Say hello in one short sentence."),
            ],
            tools: Vec::new(),
        },
    )
    .await;
    assert!(!plain.text.trim().is_empty(), "no text");
    assert_eq!(plain.finish, Some(FinishReason::Stop));
    assert!(plain.output_tokens.is_some_and(|n| n > 0), "no usage");
    eprintln!("live: plain first token after {:?}", plain.first_token);

    // 2. A strict tool call, then the tool result replayed statelessly.
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
    let (id, name, arguments) = called.call.expect("a tool call");
    assert_eq!(name, "get_weather");
    let parsed: serde_json::Value = serde_json::from_str(&arguments).expect("arguments are JSON");
    assert!(parsed["city"].is_string(), "{arguments}");
    eprintln!("live: tool call first token after {:?}", called.first_token);

    messages.push(Message::assistant(
        called.text,
        vec![ToolCall {
            id: Arc::from(id.as_str()),
            name: Arc::from(name.as_str()),
            arguments: Arc::from(arguments.as_str()),
        }],
        called.provider_data,
    ));
    messages.push(Message::tool_result(
        id,
        r#"{"temperature_c":18,"sky":"cloudy"}"#,
    ));
    let answered = read(&llm, LlmRequest { messages, tools }).await;
    assert!(
        !answered.text.trim().is_empty(),
        "no answer after the tool result"
    );
    assert_eq!(answered.finish, Some(FinishReason::Stop));
    eprintln!(
        "live: answer first token after {:?}, cached input tokens {:?}",
        answered.first_token, answered.cached
    );
}
