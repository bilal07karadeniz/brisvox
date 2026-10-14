//! Live test against real OpenAI-compatible servers. Runs only with `BRISVOX_LIVE_TESTS=1`,
//! and only against the vendors whose key is in the environment (never printed):
//! `GROQ_API_KEY`, `TOGETHER_API_KEY`, `FIREWORKS_API_KEY`, and any server through
//! `OPENAI_COMPATIBLE_BASE_URL` + `OPENAI_COMPATIBLE_MODEL` (+ optional
//! `OPENAI_COMPATIBLE_API_KEY`; a local vLLM or Ollama needs no key). `<VENDOR>_MODEL`
//! overrides a preset's suggested model. Skipped otherwise.
//!
//! This is the live check: it confirms the wire shapes `docs/providers/openai-compatible.md`
//! marks UNVERIFIED (tool-call fragmentation, usage placement, `finish_reason` words).

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_providers::openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleLlm, Preset};
use brisvox_providers::{
    FinishReason, LlmEvent, LlmProvider, LlmRequest, Message, Role, ToolCall, ToolSpec, Usage,
};

/// What one live response produced.
#[derive(Default)]
struct Outcome {
    text: String,
    /// `(id, name, arguments)` of the first tool call.
    call: Option<(String, String, String)>,
    fragments: usize,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    first_token: Option<Duration>,
}

async fn read(llm: &OpenAiCompatibleLlm, request: LlmRequest) -> Outcome {
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
                outcome.fragments += 1;
                let call = outcome.call.get_or_insert_with(Default::default);
                if let Some(id) = id {
                    call.0 = id.to_string();
                }
                if let Some(name) = name {
                    call.1 = name.to_string();
                }
                call.2.push_str(&arguments);
            }
            LlmEvent::Done { finish, usage, .. } => {
                outcome.finish = Some(finish);
                outcome.usage = usage;
            }
            LlmEvent::Error(error) => panic!("the live stream failed: {error}"),
        }
    }
    outcome
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The configurations to try: one per vendor whose key is set.
fn configs() -> Vec<(String, OpenAiCompatibleConfig)> {
    let mut found = Vec::new();
    for (preset, key_var, model_var) in [
        (Preset::Groq, "GROQ_API_KEY", "GROQ_MODEL"),
        (Preset::Together, "TOGETHER_API_KEY", "TOGETHER_MODEL"),
        (Preset::Fireworks, "FIREWORKS_API_KEY", "FIREWORKS_MODEL"),
    ] {
        let Some(key) = env(key_var) else { continue };
        let mut config = OpenAiCompatibleConfig::preset(preset).with_api_key(key);
        if let Some(model) = env(model_var) {
            config.model = model;
        }
        found.push((preset.name().to_owned(), config));
    }
    if let (Some(base), Some(model)) = (
        env("OPENAI_COMPATIBLE_BASE_URL"),
        env("OPENAI_COMPATIBLE_MODEL"),
    ) {
        let mut config = OpenAiCompatibleConfig::new(base, model);
        config.api_key = env("OPENAI_COMPATIBLE_API_KEY").map(brisvox_core::Secret::new);
        config.allow_insecure_http = true;
        found.push(("custom".to_owned(), config));
    }
    found
}

#[tokio::test]
async fn a_real_session_streams_text_and_calls_a_tool() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
        return;
    }
    let configs = configs();
    if configs.is_empty() {
        eprintln!(
            "skipped: none of GROQ_API_KEY, TOGETHER_API_KEY, FIREWORKS_API_KEY or \
             OPENAI_COMPATIBLE_BASE_URL + OPENAI_COMPATIBLE_MODEL is set"
        );
        return;
    }
    for (name, mut config) in configs {
        config.first_byte_timeout = Duration::from_secs(30);
        config.first_output_timeout = Duration::from_secs(60);
        config.idle_timeout = Duration::from_secs(30);
        config.max_tokens = Some(256);
        let llm = OpenAiCompatibleLlm::new(config, SystemClock::shared()).expect("provider");
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
        assert!(!plain.text.trim().is_empty(), "{name}: no text");
        assert_eq!(plain.finish, Some(FinishReason::Stop), "{name}");
        eprintln!(
            "live {name}: plain first token after {:?}, usage {:?}",
            plain.first_token, plain.usage
        );

        // 2. A tool call, then the tool result replayed.
        let tools = vec![ToolSpec {
            name: Arc::from("get_weather"),
            description: Arc::from("Current weather of a city"),
            parameters_schema: Arc::from(
                r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
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
        assert_eq!(called.finish, Some(FinishReason::ToolCalls), "{name}");
        let (id, tool, arguments) = called.call.expect("a tool call");
        assert_eq!(tool, "get_weather", "{name}");
        let parsed: serde_json::Value =
            serde_json::from_str(&arguments).expect("arguments are JSON");
        assert!(parsed["city"].is_string(), "{name}: {arguments}");
        eprintln!(
            "live {name}: tool call in {} fragment(s), first token after {:?}",
            called.fragments, called.first_token
        );

        messages.push(Message::assistant(
            called.text,
            vec![ToolCall {
                id: Arc::from(id.as_str()),
                name: Arc::from(tool.as_str()),
                arguments: Arc::from(arguments.as_str()),
            }],
            None,
        ));
        messages.push(Message::tool_result(
            id,
            r#"{"temperature_c":18,"sky":"cloudy"}"#,
        ));
        let answered = read(&llm, LlmRequest { messages, tools }).await;
        assert!(
            !answered.text.trim().is_empty(),
            "{name}: no answer after the tool result"
        );
        assert_eq!(answered.finish, Some(FinishReason::Stop), "{name}");
    }
}

/// Voice-style latency of a local Ollama model: a short prompt, warm model, `OLLAMA_RUNS` runs
/// (default 5): time to the first text token and to the end of the first sentence (the first of
/// `.`, `!`, `?` followed by more text, else the end of the stream), p50 and the runs.
/// `OLLAMA_MODEL` is required (skips otherwise); `OLLAMA_BASE_URL` (default the preset's);
/// `OLLAMA_REASONING_EFFORT` (default `none`, empty omits the field, which lets a thinking model
/// think first).
#[tokio::test]
async fn ollama_voice_latency() {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
        return;
    }
    let Some(model) = env("OLLAMA_MODEL") else {
        eprintln!("skipped: OLLAMA_MODEL is not set");
        return;
    };
    let runs: usize = env("OLLAMA_RUNS").and_then(|v| v.parse().ok()).unwrap_or(5);
    let mut config = OpenAiCompatibleConfig::preset(Preset::Ollama).with_model(model.clone());
    if let Some(base) = env("OLLAMA_BASE_URL") {
        config = config.with_base_url(base);
    }
    config.allow_insecure_http = true;
    config.max_tokens = Some(
        env("OLLAMA_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(200),
    );
    config.first_byte_timeout = Duration::from_secs(180);
    config.first_output_timeout = Duration::from_secs(180);
    config.idle_timeout = Duration::from_secs(60);
    let effort = std::env::var("OLLAMA_REASONING_EFFORT").unwrap_or_else(|_| "none".to_owned());
    if !effort.is_empty() {
        config
            .extra_body
            .push(("reasoning_effort".to_owned(), serde_json::json!(effort)));
    }
    let llm = OpenAiCompatibleLlm::new(config, SystemClock::shared()).expect("provider");
    let clock = SystemClock::shared();

    let prompts = [
        "What can you help me with today?",
        "Tell me something interesting about octopuses.",
        "How do I make a good cup of tea?",
        "Why is the sky blue?",
        "Give me a quick tip for sleeping better.",
        "What is a good name for a cat?",
    ];
    let mut first_tokens = Vec::new();
    let mut first_sentences = Vec::new();
    // Run 0 loads the model and is not counted; the runs after it are warm.
    for run in 0..=runs {
        let request = LlmRequest {
            messages: vec![
                Message::new(
                    Role::System,
                    "You are a friendly voice assistant on a phone call. Answer in one or two \
                     short spoken sentences.",
                ),
                Message::new(Role::User, prompts[run % prompts.len()]),
            ],
            tools: Vec::new(),
        };
        let start = clock.now();
        let mut stream = llm.stream(request).await.expect("the request is accepted");
        let mut text = String::new();
        let (mut first_token, mut first_sentence) = (None, None);
        while let Some(event) = stream.next().await {
            match event {
                LlmEvent::TextDelta(t) => {
                    first_token.get_or_insert(clock.now() - start);
                    text.push_str(&t);
                    let ended = first_sentence.is_none()
                        && text
                            .char_indices()
                            .any(|(i, c)| matches!(c, '.' | '!' | '?') && i + 1 < text.len());
                    if ended {
                        first_sentence = Some(clock.now() - start);
                    }
                }
                LlmEvent::Done { .. } => {
                    first_sentence.get_or_insert(clock.now() - start);
                }
                LlmEvent::Error(error) => panic!("the live stream failed: {error}"),
                _ => {}
            }
        }
        let (Some(token), Some(sentence)) = (first_token, first_sentence) else {
            panic!("{model}: run {run} produced no text");
        };
        eprintln!(
            "ollama {model} run {run}{}: first token {:.0} ms, first sentence {:.0} ms, {:?}",
            if run == 0 { " (cold)" } else { "" },
            token.as_secs_f64() * 1e3,
            sentence.as_secs_f64() * 1e3,
            text.chars().take(70).collect::<String>()
        );
        if run > 0 {
            first_tokens.push(token);
            first_sentences.push(sentence);
        }
    }
    let p50 = |v: &mut Vec<Duration>| {
        v.sort();
        v.get(v.len() / 2).copied().unwrap_or_default()
    };
    eprintln!(
        "LIVE RESULT ollama {model} (reasoning_effort {effort:?}): TTFT p50 {:.0} ms, first sentence p50 {:.0} ms over {runs} warm runs",
        p50(&mut first_tokens).as_secs_f64() * 1e3,
        p50(&mut first_sentences).as_secs_f64() * 1e3,
    );
}
