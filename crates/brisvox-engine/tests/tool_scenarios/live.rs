//! The scenarios against real providers. Runs only with `BRISVOX_LIVE_TESTS=1` and the keys in the
//! environment (never printed); each provider is skipped, with a message, when its key is missing.
//!
//! ```text
//! BRISVOX_LIVE_TESTS=1 cargo test -p brisvox-engine --test tool_scenarios live_ -- --nocapture --test-threads=1
//! ```
//!
//! * OpenRouter (the OpenAI-compatible adapter): `OPENAI_COMPATIBLE_API_KEY`, optional
//!   `OPENAI_COMPATIBLE_BASE_URL` (default OpenRouter), `OPENAI_COMPATIBLE_MODEL` (default
//!   `openai/gpt-6-luna`) and `OPENAI_COMPATIBLE_REASONING_EFFORT` (default `none`, empty omits it).
//! * Ollama (the same adapter with `Preset::Ollama`, no key): `OLLAMA_MODEL` (required; the test
//!   skips without it), optional `OLLAMA_BASE_URL` (default `http://127.0.0.1:11434/v1`) and
//!   `OLLAMA_REASONING_EFFORT` (default `none`, empty omits it: a thinking model then spends its
//!   token budget on thoughts), `OLLAMA_MAX_TOKENS` (default 600). Run it once per model; a first request that has to load the model
//!   gets two minutes.
//! * Anthropic: `ANTHROPIC_API_KEY`, optional `ANTHROPIC_MODEL`.
//! * Gemini: `GEMINI_API_KEY` (or `GOOGLE_API_KEY`), optional `GEMINI_MODEL` (default
//!   `gemini-3.1-flash-lite`). The free tier allows about five requests a minute and twenty a day
//!   per model, so requests are spaced 13 s apart, a 429 skips the scenario with a message, and
//!   by default only scenarios 1 to 3 run.
//!
//! `BRISVOX_LIVE_SCENARIOS` (for example `1,2,6`) picks the scenarios for every provider;
//! `GEMINI_SCENARIOS` overrides it for Gemini (and `OPENROUTER_SCENARIOS`, `ANTHROPIC_SCENARIOS` for the others). Per scenario the log prints every round's
//! time to first token, every tool's start and finish, and the runner's counters; a summary line
//! per scenario (`LIVE RESULT ...`) closes each test.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use brisvox_core::{Secret, SystemClock};
use brisvox_providers::anthropic::{AnthropicConfig, AnthropicLlm};
use brisvox_providers::gemini::{GeminiConfig, GeminiLlm};
use brisvox_providers::openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleLlm, Preset};
use futures::FutureExt;
use serde_json::json;

use crate::harness::Provider;
use crate::observed::AnyLlm;
use crate::scenarios as s;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn enabled() -> bool {
    if std::env::var("BRISVOX_LIVE_TESTS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: BRISVOX_LIVE_TESTS is not 1");
    false
}

fn openrouter() -> Option<Provider> {
    let Some(key) = env("OPENAI_COMPATIBLE_API_KEY") else {
        eprintln!("skipped: OPENAI_COMPATIBLE_API_KEY is not set");
        return None;
    };
    let base = env("OPENAI_COMPATIBLE_BASE_URL")
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_owned());
    let model = env("OPENAI_COMPATIBLE_MODEL").unwrap_or_else(|| "openai/gpt-6-luna".to_owned());
    let mut config = OpenAiCompatibleConfig::new(base, model);
    config.api_key = Some(Secret::new(key));
    config.max_tokens = Some(600);
    config.first_byte_timeout = Duration::from_secs(30);
    config.first_output_timeout = Duration::from_secs(60);
    config.idle_timeout = Duration::from_secs(30);
    let effort =
        std::env::var("OPENAI_COMPATIBLE_REASONING_EFFORT").unwrap_or_else(|_| "none".to_owned());
    if !effort.is_empty() {
        config
            .extra_body
            .push(("reasoning_effort".to_owned(), json!(effort)));
    }
    let llm = OpenAiCompatibleLlm::new(config, SystemClock::shared()).expect("provider");
    Some(Provider::live(AnyLlm::OpenAi(llm), Duration::ZERO))
}

fn ollama() -> Option<Provider> {
    let Some(model) = env("OLLAMA_MODEL") else {
        eprintln!("skipped: OLLAMA_MODEL is not set");
        return None;
    };
    let mut config = OpenAiCompatibleConfig::preset(Preset::Ollama).with_model(model);
    if let Some(base) = env("OLLAMA_BASE_URL") {
        config = config.with_base_url(base);
    }
    config.allow_insecure_http = true;
    config.max_tokens = Some(
        env("OLLAMA_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    );
    // A local model may have to be loaded into the GPU first.
    config.first_byte_timeout = Duration::from_secs(120);
    config.first_output_timeout = Duration::from_secs(120);
    config.idle_timeout = Duration::from_secs(60);
    let effort = std::env::var("OLLAMA_REASONING_EFFORT").unwrap_or_else(|_| "none".to_owned());
    if !effort.is_empty() {
        config
            .extra_body
            .push(("reasoning_effort".to_owned(), json!(effort)));
    }
    let llm = OpenAiCompatibleLlm::new(config, SystemClock::shared()).expect("provider");
    Some(Provider::live(AnyLlm::OpenAi(llm), Duration::ZERO))
}

fn anthropic() -> Option<Provider> {
    let Some(key) = env("ANTHROPIC_API_KEY") else {
        eprintln!("skipped: ANTHROPIC_API_KEY is not set");
        return None;
    };
    let mut config = AnthropicConfig::new(Secret::new(key));
    if let Some(model) = env("ANTHROPIC_MODEL") {
        config.model = model;
    }
    config.first_byte_timeout = Duration::from_secs(30);
    config.first_output_timeout = Duration::from_secs(60);
    config.idle_timeout = Duration::from_secs(30);
    let llm = AnthropicLlm::new(config, SystemClock::shared()).expect("provider");
    Some(Provider::live(AnyLlm::Anthropic(llm), Duration::ZERO))
}

fn gemini() -> Option<Provider> {
    let Some(key) = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .find_map(|name| env(name))
    else {
        eprintln!("skipped: GEMINI_API_KEY is not set");
        return None;
    };
    let mut config = GeminiConfig::new(Secret::new(key));
    config.model = env("GEMINI_MODEL").unwrap_or_else(|| "gemini-3.1-flash-lite".to_owned());
    config.first_byte_timeout = Duration::from_secs(30);
    config.first_output_timeout = Duration::from_secs(60);
    config.idle_timeout = Duration::from_secs(30);
    let llm = GeminiLlm::new(config, SystemClock::shared()).expect("provider");
    Some(Provider::live(AnyLlm::Gemini(llm), Duration::from_secs(13)))
}

/// The scenario ids to run: `var`, else `BRISVOX_LIVE_SCENARIOS`, else `default`.
fn picked(var: &str, default: &str) -> Vec<String> {
    env(var)
        .or_else(|| env("BRISVOX_LIVE_SCENARIOS"))
        .unwrap_or_else(|| default.to_owned())
        .split(',')
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Runs scenario `id` against a fresh provider; `None` for an unknown id.
fn scenario(id: &str, provider: Provider) -> Option<std::pin::Pin<Box<dyn Future<Output = ()>>>> {
    Some(match id {
        "1" => Box::pin(s::s1(provider)),
        "2" => Box::pin(s::s2(provider)),
        "3" => Box::pin(s::s3(provider)),
        "4" => Box::pin(s::s4(provider)),
        "5" => Box::pin(s::s5(provider)),
        "6" => Box::pin(s::s6(provider)),
        _ => return None,
    })
}

/// Runs the picked scenarios one after the other; every one runs even if an earlier one failed,
/// then the test fails if any did.
async fn run(name: &str, make: fn() -> Option<Provider>, ids: Vec<String>) {
    if !enabled() || make().is_none() {
        return;
    }
    let mut failed = Vec::new();
    for id in ids {
        let Some(provider) = make() else { return };
        let Some(future) = scenario(&id, provider) else {
            eprintln!("LIVE RESULT {name} scenario {id}: unknown scenario id");
            continue;
        };
        let started = Instant::now();
        let outcome = AssertUnwindSafe(future).catch_unwind().await;
        let took = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("LIVE RESULT {name} scenario {id}: ok ({took:.1} s)"),
            Err(_) => {
                eprintln!("LIVE RESULT {name} scenario {id}: FAILED ({took:.1} s)");
                failed.push(id);
            }
        }
    }
    assert!(failed.is_empty(), "{name}: scenarios failed: {failed:?}");
}

#[tokio::test]
async fn live_openrouter() {
    run(
        "openrouter",
        openrouter,
        picked("OPENROUTER_SCENARIOS", "1,2,3,4,5,6"),
    )
    .await;
}

/// A local model through Ollama: `OLLAMA_MODEL=qwen3.5:4b BRISVOX_LIVE_TESTS=1 cargo test ...
/// live_ollama`. Scenarios `OLLAMA_SCENARIOS` (default all six).
#[tokio::test]
async fn live_ollama() {
    run("ollama", ollama, picked("OLLAMA_SCENARIOS", "1,2,3,4,5,6")).await;
}

#[tokio::test]
async fn live_anthropic() {
    run(
        "anthropic",
        anthropic,
        picked("ANTHROPIC_SCENARIOS", "1,2,3,4,5,6"),
    )
    .await;
}

#[tokio::test]
async fn live_gemini() {
    run("gemini", gemini, picked("GEMINI_SCENARIOS", "1,2,3")).await;
}

/// Gemini Live (the speech-to-speech stage), scenarios 1 and 2: real speech goes in, the model
/// calls tools through the real tool engine. `GEMINI_LIVE_MODEL` (default `gemini-3.8-live`),
/// `GEMINI_LIVE_SCENARIOS` (default `1,2`).
#[tokio::test]
async fn live_gemini_live() {
    if !enabled() {
        return;
    }
    let mut failed = Vec::new();
    for id in picked("GEMINI_LIVE_SCENARIOS", "1,2") {
        let Ok(n) = id.parse::<u8>() else { continue };
        let started = Instant::now();
        let outcome = AssertUnwindSafe(crate::s2s::live_gemini_live(n))
            .catch_unwind()
            .await;
        let took = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("LIVE RESULT gemini-live scenario {id}: ok ({took:.1} s)"),
            Err(_) => {
                eprintln!("LIVE RESULT gemini-live scenario {id}: FAILED ({took:.1} s)");
                failed.push(id);
            }
        }
    }
    assert!(
        failed.is_empty(),
        "gemini-live: scenarios failed: {failed:?}"
    );
}
