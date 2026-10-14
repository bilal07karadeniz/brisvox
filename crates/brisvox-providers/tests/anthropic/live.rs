//! Live test against the real Anthropic API. Runs only with `BRISVOX_LIVE_TESTS=1` and
//! `ANTHROPIC_API_KEY` in the environment (never printed); skipped otherwise. Optional
//! `ANTHROPIC_MODEL` overrides the model. This is the live check: it confirms the wire
//! shapes `docs/providers/anthropic.md` marks UNVERIFIED (usage fields, the tool-call stream,
//! thinking replay, prompt-cache hits) and prints the time to the first token for each setting
//! (`cargo test -- --nocapture` shows it).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::{Secret, SystemClock};
use brisvox_providers::anthropic::{AnthropicConfig, AnthropicLlm, Effort, ThinkingMode};
use brisvox_providers::{
    FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role, ToolCall, ToolSpec, Usage,
};

/// What one live response produced.
#[derive(Default)]
struct Outcome {
    text: String,
    call: Option<(String, String, String)>,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    provider_data: Option<Arc<str>>,
    first_token: Option<Duration>,
}

async fn read(llm: &AnthropicLlm, request: LlmRequest) -> Outcome {
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
                outcome.usage = usage;
                outcome.provider_data = provider_data;
            }
            LlmEvent::Error(error) => panic!("the live stream failed: {error}"),
        }
    }
    outcome
}

fn live_config() -> Option<AnthropicConfig> {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
        return None;
    }
    let Some(key) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        eprintln!("skipped: ANTHROPIC_API_KEY is not set");
        return None;
    };
    let mut config = AnthropicConfig::new(Secret::new(key));
    if let Some(model) = std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
    {
        config.model = model;
    }
    config.first_byte_timeout = Duration::from_secs(30);
    config.idle_timeout = Duration::from_secs(30);
    config.first_output_timeout = Duration::from_secs(60);
    Some(config)
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Current weather of a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
        ),
    }
}

/// A system prompt comfortably over the 512-token minimum cacheable length of the 5.5 models.
fn long_persona() -> String {
    let rule = "Always answer in one short spoken sentence, without markdown, lists or emoji. ";
    format!(
        "You are a friendly phone assistant for a small bakery. {}",
        rule.repeat(80)
    )
}

#[tokio::test]
async fn a_real_session_streams_text_calls_a_tool_replays_it_and_hits_the_cache() {
    let Some(config) = live_config() else {
        return;
    };
    let llm = AnthropicLlm::new(config, SystemClock::shared()).expect("provider");
    llm.warm().await.expect("warm");
    let persona = long_persona();

    // 1. Plain text, and the cache write of the persona.
    let first = read(
        &llm,
        LlmRequest {
            messages: vec![
                Message::new(Role::System, persona.as_str()),
                Message::new(Role::User, "Say hello in one short sentence."),
            ],
            tools: Vec::new(),
        },
    )
    .await;
    assert!(!first.text.trim().is_empty(), "no text");
    assert_eq!(first.finish, Some(FinishReason::Stop));
    assert!(first.usage.is_some_and(|u| u.output_tokens > 0), "no usage");
    eprintln!(
        "live: plain first token after {:?}, usage {:?}",
        first.first_token, first.usage
    );

    // 2. The same prefix again: the persona is a cache hit.
    let second = read(
        &llm,
        LlmRequest {
            messages: vec![
                Message::new(Role::System, persona.as_str()),
                Message::new(Role::User, "Say hello in one short sentence."),
                Message::assistant(first.text.as_str(), Vec::new(), None),
                Message::new(Role::User, "And say goodbye."),
            ],
            tools: Vec::new(),
        },
    )
    .await;
    eprintln!(
        "live: second turn first token after {:?}, usage {:?}",
        second.first_token, second.usage
    );
    assert!(
        second.usage.is_some_and(|u| u.cached_input_tokens > 0),
        "no cache read on the second turn: {:?}",
        second.usage
    );

    // 3. A tool call, then the tool result replayed statelessly.
    let tools = vec![weather_tool()];
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
        "no answer after the tool result (the documented empty end_turn?)"
    );
    assert_eq!(answered.finish, Some(FinishReason::Stop));
    eprintln!(
        "live: answer first token after {:?}, usage {:?}",
        answered.first_token, answered.usage
    );
}

/// Adaptive thinking with medium effort: the thinking blocks (or their absence) round-trip
/// through a tool turn without a 400. Confirms the UNVERIFIED streaming shape of thinking blocks
/// and the `display` default.
#[tokio::test]
async fn a_real_thinking_turn_round_trips_its_blocks() {
    let Some(mut config) = live_config() else {
        return;
    };
    config.thinking = ThinkingMode::Adaptive;
    config.effort = Some(Effort::High);
    let llm = AnthropicLlm::new(config, SystemClock::shared()).expect("provider");
    let tools = vec![weather_tool()];
    // A question that needs a little reasoning before the call, so adaptive thinking produces
    // blocks to echo (a trivial "weather in Rome" turn is answered without thinking).
    let mut messages = vec![
        Message::new(Role::System, "Use tools when they help. Be terse."),
        Message::new(
            Role::User,
            "Of Cairo, Oslo, Lima and Rome, work out which is farthest from the equator, then              get the weather there with the tool.",
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
    let (id, name, arguments) = called.call.expect("a tool call");
    eprintln!(
        "live: thinking provider_data present: {}, first token after {:?}",
        called.provider_data.is_some(),
        called.first_token
    );
    assert!(
        called.provider_data.is_some(),
        "no thinking blocks to replay: the turn does not test the round trip"
    );
    messages.push(Message::assistant(
        called.text,
        vec![ToolCall {
            id: Arc::from(id.as_str()),
            name: Arc::from(name.as_str()),
            arguments: Arc::from(arguments.as_str()),
        }],
        called.provider_data,
    ));
    messages.push(Message::tool_result(id, r#"{"temperature_c":24}"#));
    let answered = read(&llm, LlmRequest { messages, tools }).await;
    assert!(!answered.text.trim().is_empty(), "no answer");
}
