//! Protocol-level tests of the OpenAI-compatible adapter: the exact request sent per preset, the
//! chunk shapes of every vendor in `docs/providers/openai-compatible.md` (one mock, one dialect
//! per preset), usage and finish handling, error mapping, deadlines, cancellation, secrecy.

use std::sync::Arc;
use std::time::{Duration, Instant};

use brisvox_core::{Error as CoreError, SystemClock};
use brisvox_providers::openai_compatible::{
    AuthStyle, MaxTokensField, OpenAiCompatibleConfig, OpenAiCompatibleLlm, Preset, Quirks,
    ToolChoice, UsagePlacement,
};
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role, ToolCall,
    ToolSpec, Usage,
};
use brisvox_sim::contract::{Response, ToolCall as ScriptedCall};
use serde_json::{Value, json};

use crate::mock::{Mock, Script, Step};
use crate::wire::{Dialect, ToolShape, UsageWire, render, script_for, usage_json};

const KEY: &str = "test-key-0123456789";

fn config_for(preset: Preset, mock: &Mock) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig::preset(preset)
        .with_api_key(KEY)
        .with_base_url(format!("{}/v1", mock.base_url()))
        .with_timeout(Duration::from_secs(3))
}

fn config(mock: &Mock) -> OpenAiCompatibleConfig {
    config_for(Preset::Groq, mock)
}

fn provider_with(config: OpenAiCompatibleConfig) -> OpenAiCompatibleLlm {
    OpenAiCompatibleLlm::new(config, SystemClock::shared()).expect("provider")
}

fn provider(mock: &Mock) -> OpenAiCompatibleLlm {
    provider_with(config(mock))
}

fn user(text: &str) -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, text)],
        tools: Vec::new(),
    }
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: Arc::from("get_weather"),
        description: Arc::from("Current weather for a city"),
        parameters_schema: Arc::from(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
        ),
    }
}

/// Reads a stream to its end (a hang fails the test after ten seconds).
async fn collect(mut stream: LlmStream) -> Vec<LlmEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("the stream ended")
}

async fn run(llm: &OpenAiCompatibleLlm, request: LlmRequest) -> Vec<LlmEvent> {
    collect(llm.stream(request).await.expect("accepted")).await
}

/// What a response said, folded.
#[derive(Debug, Default)]
struct Summary {
    text: String,
    /// `(id, name, arguments, fragments)` by call index.
    calls: Vec<(Option<String>, Option<String>, String, usize)>,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    error: Option<Error>,
}

fn summarize(events: Vec<LlmEvent>) -> Summary {
    let mut s = Summary::default();
    let mut ended = false;
    for event in events {
        assert!(!ended, "an event after the end of the response");
        match event {
            LlmEvent::TextDelta(t) => s.text.push_str(&t),
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let index = index as usize;
                assert!(index <= s.calls.len(), "call index {index} skips a call");
                if index == s.calls.len() {
                    s.calls.push((None, None, String::new(), 0));
                }
                let call = &mut s.calls[index];
                if let Some(id) = id {
                    assert!(call.0.is_none(), "id repeated");
                    call.0 = Some(id.to_string());
                }
                if let Some(name) = name {
                    assert!(call.1.is_none(), "name repeated");
                    call.1 = Some(name.to_string());
                }
                call.2.push_str(&arguments);
                call.3 += 1;
            }
            LlmEvent::Done { finish, usage, .. } => {
                s.finish = Some(finish);
                s.usage = usage;
                ended = true;
            }
            LlmEvent::Error(e) => {
                s.error = Some(e);
                ended = true;
            }
        }
    }
    s
}

fn chunk_with(delta: Value, finish: Value) -> String {
    json!({"id": "c", "object": "chat.completion.chunk",
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    .to_string()
}

fn text_chunk(text: &str) -> String {
    chunk_with(json!({"content": text}), Value::Null)
}

fn finish_chunk(word: &str) -> String {
    chunk_with(json!({}), json!(word))
}

/// A script of SSE events, one HTTP chunk each, `[DONE]` last.
fn events_script(chunks: &[String]) -> Script {
    let mut steps: Vec<Step> = chunks.iter().cloned().map(Step::event).collect();
    steps.push(Step::event("[DONE]"));
    Script::stream(steps)
}

fn scripted_response() -> Response {
    Response {
        text: vec!["Hel".to_owned(), "lo".to_owned()],
        tool_calls: vec![
            ScriptedCall {
                id: "call_a".to_owned(),
                name: "get_weather".to_owned(),
                arguments: r#"{"city":"Paris"}"#.to_owned(),
            },
            ScriptedCall {
                id: "call_b".to_owned(),
                name: "get_time".to_owned(),
                arguments: r#"{"tz":"UTC"}"#.to_owned(),
            },
        ],
        faults: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Presets and the request
// ---------------------------------------------------------------------------------------------

#[test]
fn presets_carry_the_documented_endpoints_and_quirks() {
    let table = [
        (Preset::Groq, "groq", "https://api.groq.com/openai/v1"),
        (Preset::Together, "together", "https://api.together.ai/v1"),
        (
            Preset::Fireworks,
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
        ),
        (Preset::Vllm, "vllm", "http://localhost:8000/v1"),
        (Preset::Ollama, "ollama", "http://localhost:11434/v1"),
        (
            Preset::OpenRouter,
            "openrouter",
            "https://openrouter.ai/api/v1",
        ),
    ];
    for (preset, name, url) in table {
        let config = OpenAiCompatibleConfig::preset(preset);
        assert_eq!((config.name, config.base_url.as_str()), (name, url));
        assert_eq!(config.quirks, preset.quirks());
        assert!(!config.model.is_empty());
        assert!(config.api_key.is_none());
        assert!(
            OpenAiCompatibleLlm::new(config, SystemClock::shared()).is_ok(),
            "{name}"
        );
    }
    assert_eq!(Preset::ALL.len(), table.len());
    // The documented quirks of each vendor, pinned as literals (docs/providers/
    // openai-compatible.md, "Implementation notes"): a preset flipped by mistake fails here.
    let quirks = |max_tokens_field, request_stream_usage, usage| Quirks {
        max_tokens_field,
        request_stream_usage,
        usage,
        parallel_tool_calls: None,
        tool_choice: None,
        synthesize_missing_ids: true,
    };
    assert_eq!(
        Preset::Groq.quirks(),
        quirks(
            MaxTokensField::MaxCompletionTokens,
            true,
            UsagePlacement::TrailingChunk
        )
    );
    assert_eq!(
        Preset::Together.quirks(),
        quirks(
            MaxTokensField::MaxTokens,
            false,
            UsagePlacement::FinishChunk
        )
    );
    assert_eq!(
        Preset::Fireworks.quirks(),
        quirks(
            MaxTokensField::MaxTokens,
            false,
            UsagePlacement::TrailingChunk
        )
    );
    assert_eq!(
        Preset::Vllm.quirks(),
        quirks(
            MaxTokensField::MaxTokens,
            true,
            UsagePlacement::TrailingChunk
        )
    );
    assert_eq!(
        Preset::Ollama.quirks(),
        quirks(
            MaxTokensField::MaxTokens,
            true,
            UsagePlacement::TrailingChunk
        )
    );
    assert_eq!(
        Preset::OpenRouter.quirks(),
        quirks(
            MaxTokensField::MaxTokens,
            true,
            UsagePlacement::TrailingChunk
        )
    );
}

#[tokio::test]
async fn the_request_on_the_wire_is_what_each_preset_needs() {
    for preset in Preset::ALL {
        let mock = Mock::start(false).await;
        mock.script(events_script(&[finish_chunk("stop")]));
        let mut cfg = config_for(preset, &mock);
        cfg.max_tokens = Some(64);
        cfg.temperature = Some(0.5);
        let llm = provider_with(cfg.clone());
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::System, "Be brief."),
                Message::new(Role::User, "hello"),
            ],
            tools: vec![weather_tool()],
        };
        run(&llm, request).await;

        let posts = mock.posts();
        assert_eq!(posts.len(), 1, "{preset:?}");
        let post = &posts[0];
        assert_eq!(post.path, "/v1/chat/completions");
        assert_eq!(
            post.header("authorization"),
            Some("Bearer test-key-0123456789")
        );
        assert_eq!(post.header("content-type"), Some("application/json"));
        assert_eq!(post.header("accept"), Some("text/event-stream"));

        let mut expected = json!({
            "model": cfg.model,
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "hello"}
            ],
            "stream": true,
            "temperature": 0.5,
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "Current weather for a city",
                "parameters": {"type": "object",
                               "properties": {"city": {"type": "string"}},
                               "required": ["city"]}}}],
        });
        // What each vendor needs, as literals (not derived from the preset under test).
        if matches!(
            preset,
            Preset::Groq | Preset::Vllm | Preset::Ollama | Preset::OpenRouter
        ) {
            expected["stream_options"] = json!({"include_usage": true});
        }
        if preset == Preset::Groq {
            expected["max_completion_tokens"] = json!(64);
        } else {
            expected["max_tokens"] = json!(64);
        }
        assert_eq!(post.json(), expected, "{preset:?}");
        // Nothing else that a vendor could choke on.
        let body = post.json();
        for key in [
            "parallel_tool_calls",
            "tool_choice",
            "n",
            "logprobs",
            "user",
        ] {
            assert!(body.get(key).is_none(), "{preset:?} sent {key}");
        }
    }
}

#[tokio::test]
async fn auth_header_scheme_extra_headers_and_body_fields_are_configurable() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[finish_chunk("stop")]));
    let mut cfg = config(&mock)
        .with_auth(AuthStyle::header("x-api-key"))
        .with_header("X-Gateway-Route", "voice")
        .with_body_field("reasoning_effort", json!("low"))
        .with_body_field("chat_template_kwargs", json!({"enable_thinking": false}))
        .with_quirks(Quirks {
            parallel_tool_calls: Some(false),
            tool_choice: Some(ToolChoice::Auto),
            ..Quirks::default()
        });
    cfg.chat_path = "/chat".to_owned();
    let llm = provider_with(cfg.clone());
    let request = LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![weather_tool()],
    };
    run(&llm, request).await;
    let post = &mock.posts()[0];
    assert_eq!(post.path, "/v1/chat");
    assert_eq!(post.header("x-api-key"), Some(KEY));
    assert!(post.header("authorization").is_none());
    assert_eq!(post.header("x-gateway-route"), Some("voice"));
    let body = post.json();
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["tool_choice"], "auto");

    // Without tools the two tool-only fields are left out; a custom scheme is honoured.
    mock.script(events_script(&[finish_chunk("stop")]));
    let mut cfg = config(&mock).with_quirks(Quirks {
        parallel_tool_calls: Some(true),
        tool_choice: Some(ToolChoice::Required),
        ..Quirks::default()
    });
    cfg.auth = AuthStyle {
        header: "Authorization".to_owned(),
        scheme: Some("Token".to_owned()),
    };
    run(&provider_with(cfg), user("hi")).await;
    let post = &mock.posts()[1];
    assert_eq!(
        post.header("authorization"),
        Some("Token test-key-0123456789")
    );
    let body = post.json();
    assert!(body.get("tools").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[tokio::test]
async fn a_keyless_server_gets_no_auth_header() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[finish_chunk("stop")]));
    let mut cfg = config_for(Preset::Ollama, &mock);
    cfg.api_key = None;
    run(&provider_with(cfg), user("hi")).await;
    let post = &mock.posts()[0];
    assert!(post.header("authorization").is_none());
    assert!(post.header("x-api-key").is_none());
}

#[tokio::test]
async fn the_warm_up_request_carries_no_credentials() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock).config().clone();
    let llm = provider_with(llm.with_header("x-secret-route", "abc"));
    llm.warm().await.expect("warm");
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "HEAD");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert!(requests[0].header("authorization").is_none());
    assert!(requests[0].header("x-secret-route").is_none());
}

#[tokio::test]
async fn history_is_replayed_in_the_chat_completions_shape() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[finish_chunk("stop")]));
    let llm = provider(&mock);
    let request = LlmRequest {
        messages: vec![
            Message::new(Role::System, "sys"),
            Message::new(Role::User, "weather?"),
            Message::assistant(
                "",
                vec![
                    ToolCall {
                        id: Arc::from("call_a"),
                        name: Arc::from("get_weather"),
                        arguments: Arc::from(r#"{"city":"Paris"}"#),
                    },
                    ToolCall {
                        id: Arc::from("call_b"),
                        name: Arc::from("get_time"),
                        arguments: Arc::from(""),
                    },
                ],
                Some(Arc::from("not ours")),
            ),
            Message::tool_result("call_a", "18C"),
            Message::tool_result("call_b", "noon"),
            Message::assistant(
                "Let me check once more.",
                vec![ToolCall {
                    id: Arc::from("call_c"),
                    name: Arc::from("get_weather"),
                    arguments: Arc::from(r#"{"city":"Rome"}"#),
                }],
                None,
            ),
            Message::tool_result("call_c", "21C"),
            Message::assistant("It is 18C at noon.", Vec::new(), None),
            Message::new(Role::User, "thanks"),
        ],
        tools: vec![weather_tool()],
    };
    run(&llm, request).await;
    let body = mock.posts()[0].json();
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_a", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                {"id": "call_b", "type": "function",
                 "function": {"name": "get_time", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_a", "content": "18C"},
            {"role": "tool", "tool_call_id": "call_b", "content": "noon"},
            // Text that was spoken before a call stays in the replayed context.
            {"role": "assistant", "content": "Let me check once more.", "tool_calls": [
                {"id": "call_c", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}]},
            {"role": "tool", "tool_call_id": "call_c", "content": "21C"},
            {"role": "assistant", "content": "It is 18C at noon."},
            {"role": "user", "content": "thanks"}
        ])
    );
}

#[tokio::test]
async fn a_base_url_with_a_query_or_fragment_keeps_the_chat_path_before_the_query() {
    // `?key=` on the base URL is a supported shape (it is redacted from Debug and logs): the
    // chat path goes into the URL path, and the query stays a query, on POST and on HEAD.
    for (base, path, want) in [
        (
            "/v1?key=abc",
            "/chat/completions",
            "/v1/chat/completions?key=abc",
        ),
        (
            "/v1/?key=abc",
            "/chat/completions",
            "/v1/chat/completions?key=abc",
        ),
        ("/v1#frag", "/chat/completions", "/v1/chat/completions"),
        (
            "/v1?key=abc#frag",
            "/chat?api-version=2",
            "/v1/chat?key=abc&api-version=2",
        ),
    ] {
        let mock = Mock::start(false).await;
        mock.script(events_script(&[finish_chunk("stop")]));
        let mut cfg = config(&mock).with_base_url(format!("{}{base}", mock.base_url()));
        cfg.chat_path = path.to_owned();
        let llm = provider_with(cfg);
        run(&llm, user("hi")).await;
        llm.warm().await.expect("warm");
        let requests = mock.requests();
        let sent: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(sent, [want, want], "{base} + {path}");
    }
}

#[tokio::test]
async fn requests_with_bad_tools_or_tool_messages_fail_before_any_network_io() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    for schema in ["not json", "[1]", "\"x\"", ""] {
        let bad = LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![ToolSpec {
                name: Arc::from("t"),
                description: Arc::from("d"),
                parameters_schema: Arc::from(schema),
            }],
        };
        assert!(
            matches!(llm.stream(bad).await, Err(Error::Protocol { .. })),
            "{schema:?}"
        );
    }
    let nameless = LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![ToolSpec {
            name: Arc::from(""),
            description: Arc::from("d"),
            parameters_schema: Arc::from("{}"),
        }],
    };
    assert!(matches!(
        llm.stream(nameless).await,
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
async fn tools_are_rendered_once_and_a_changed_tool_set_is_re_rendered() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut other = weather_tool();
    other.description = Arc::from("changed");
    for tools in [
        vec![weather_tool()],
        vec![weather_tool()],
        vec![other.clone()],
        Vec::new(),
    ] {
        mock.script(events_script(&[finish_chunk("stop")]));
        run(
            &llm,
            LlmRequest {
                messages: vec![Message::new(Role::User, "hi")],
                tools,
            },
        )
        .await;
    }
    let posts = mock.posts();
    assert_eq!(posts[0].body, posts[1].body);
    assert_ne!(posts[1].body, posts[2].body);
    assert_eq!(
        posts[2].json()["tools"][0]["function"]["description"],
        "changed"
    );
    assert!(posts[3].json().get("tools").is_none());
}

#[tokio::test]
async fn a_large_history_is_sent_intact() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[finish_chunk("stop")]));
    let llm = provider(&mock);
    let big = "é".repeat(40 * 1024);
    run(&llm, user(&big)).await;
    let body = mock.posts()[0].json();
    assert_eq!(body["messages"][0]["content"], big.as_str());
}

#[tokio::test]
async fn the_adapter_identifies_itself_by_its_preset() {
    let mock = Mock::start(false).await;
    assert_eq!(provider(&mock).name(), "groq");
    let generic = OpenAiCompatibleConfig::new(mock.base_url(), "m");
    assert_eq!(provider_with(generic).name(), "openai-compatible");
}

// ---------------------------------------------------------------------------------------------
// Reading: every dialect
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_dialect_round_trips_text_two_tool_calls_usage_and_the_finish_reason() {
    for dialect in Dialect::all() {
        let mock = Mock::start(false).await;
        mock.script(script_for(&dialect, &scripted_response()));
        let llm = provider_with(config_for(dialect.preset, &mock));
        let s = summarize(run(&llm, user("hi")).await);
        let who = format!("{:?}", dialect.preset);
        assert!(s.error.is_none(), "{who}: {:?}", s.error);
        assert_eq!(s.text, "Hello", "{who}: reasoning must not be spoken");
        assert_eq!(s.calls.len(), 2, "{who}");
        assert_eq!(
            s.calls[0],
            (
                Some("call_a".to_owned()),
                Some("get_weather".to_owned()),
                r#"{"city":"Paris"}"#.to_owned(),
                s.calls[0].3
            ),
            "{who}"
        );
        assert_eq!(s.calls[1].0.as_deref(), Some("call_b"), "{who}");
        assert_eq!(s.calls[1].1.as_deref(), Some("get_time"), "{who}");
        assert_eq!(s.calls[1].2, r#"{"tz":"UTC"}"#, "{who}");
        match dialect.tools {
            ToolShape::Whole => assert_eq!(s.calls[0].3, 1, "{who}: one delta per whole call"),
            ToolShape::Fragmented => {
                assert!(s.calls[0].3 >= 3, "{who}: open plus two fragments");
            }
        }
        // vLLM keeps `stop` for a named tool_choice; the adapter still reports the calls.
        assert_eq!(s.finish, Some(FinishReason::ToolCalls), "{who}");
        let want = (dialect.usage != UsageWire::Never).then_some(Usage {
            input_tokens: 12,
            cached_input_tokens: 8,
            output_tokens: 5,
        });
        assert_eq!(s.usage, want, "{who}");
    }
}

#[tokio::test]
async fn a_plain_answer_finishes_with_stop_whatever_the_vendors_word_for_it() {
    for dialect in Dialect::all() {
        let mock = Mock::start(false).await;
        let response = Response {
            text: vec!["Hi".to_owned()],
            tool_calls: Vec::new(),
            faults: Vec::new(),
        };
        mock.script(script_for(&dialect, &response));
        let llm = provider_with(config_for(dialect.preset, &mock));
        let s = summarize(run(&llm, user("hi")).await);
        assert_eq!(s.finish, Some(FinishReason::Stop), "{:?}", dialect.preset);
        assert_eq!(s.text, "Hi");
        assert!(s.calls.is_empty());
    }
}

#[tokio::test]
async fn missing_ids_get_synthetic_ones_and_a_null_index_is_tolerated() {
    for base in Dialect::all() {
        let dialect = base.with(|d| {
            d.missing_ids = true;
            d.null_index = true;
        });
        let mock = Mock::start(false).await;
        mock.script(script_for(&dialect, &scripted_response()));
        let llm = provider_with(config_for(dialect.preset, &mock));
        let s = summarize(run(&llm, user("hi")).await);
        let who = format!("{:?}", dialect.preset);
        assert!(s.error.is_none(), "{who}: {:?}", s.error);
        assert_eq!(s.calls.len(), 2, "{who}");
        assert_eq!(s.calls[0].0.as_deref(), Some("call_1"), "{who}");
        assert_eq!(s.calls[1].0.as_deref(), Some("call_2"), "{who}");
        assert_eq!(s.calls[0].2, r#"{"city":"Paris"}"#, "{who}");
        assert_eq!(s.calls[1].2, r#"{"tz":"UTC"}"#, "{who}");
        assert_eq!(s.finish, Some(FinishReason::ToolCalls), "{who}");
    }
}

#[tokio::test]
async fn synthetic_ids_can_be_forbidden() {
    let mock = Mock::start(false).await;
    let dialect = Dialect::of(Preset::Vllm).with(|d| d.missing_ids = true);
    mock.script(script_for(&dialect, &scripted_response()));
    let mut cfg = config_for(Preset::Vllm, &mock);
    cfg.quirks.synthesize_missing_ids = false;
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert!(
        matches!(s.error, Some(Error::Protocol { .. })),
        "{:?}",
        s.error
    );
    assert!(s.finish.is_none());
}

#[tokio::test]
async fn a_length_cut_stays_length_even_with_a_call_in_flight() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[
        chunk_with(
            json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": "{\"x"}}]}),
            Value::Null,
        ),
        finish_chunk("length"),
    ]));
    let s = summarize(run(&provider(&mock), user("hi")).await);
    assert_eq!(s.finish, Some(FinishReason::Length));
}

#[tokio::test]
async fn whole_calls_without_index_or_id_and_object_arguments_are_accepted() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[
        chunk_with(
            json!({"tool_calls": [{"function": {"name": "f", "arguments": {"k": 1}}}]}),
            Value::Null,
        ),
        chunk_with(
            json!({"tool_calls": [{"function": {"name": "g", "arguments": "{}"}}]}),
            Value::Null,
        ),
        finish_chunk("stop"),
    ]));
    let s = summarize(
        run(
            &provider_with(config_for(Preset::Ollama, &mock)),
            user("hi"),
        )
        .await,
    );
    assert_eq!(s.calls.len(), 2);
    assert_eq!(s.calls[0].2, r#"{"k":1}"#);
    assert_eq!(s.calls[1].1.as_deref(), Some("g"));
    assert_eq!(s.finish, Some(FinishReason::ToolCalls));
}

#[tokio::test]
async fn whole_calls_sharing_one_index_and_no_id_stay_separate_calls() {
    // Ollama ids are `omitempty`: two parallel whole calls may both say index 0 and carry no id.
    let mock = Mock::start(false).await;
    let whole = |name: &str, arguments: &str| {
        chunk_with(
            json!({"tool_calls": [{"index": 0, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            Value::Null,
        )
    };
    mock.script(events_script(&[
        whole("get_weather", r#"{"city":"Paris"}"#),
        whole("get_time", r#"{"tz":"UTC"}"#),
        finish_chunk("stop"),
    ]));
    let s = summarize(
        run(
            &provider_with(config_for(Preset::Ollama, &mock)),
            user("hi"),
        )
        .await,
    );
    assert!(s.error.is_none(), "{s:?}");
    assert_eq!(s.finish, Some(FinishReason::ToolCalls));
    let got: Vec<_> = s
        .calls
        .iter()
        .map(|c| (c.0.as_deref(), c.1.as_deref(), c.2.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Some("call_1"), Some("get_weather"), r#"{"city":"Paris"}"#),
            (Some("call_2"), Some("get_time"), r#"{"tz":"UTC"}"#),
        ]
    );
}

#[tokio::test]
async fn sse_framing_is_independent_of_chunk_boundaries_and_line_endings() {
    let dialect = Dialect::of(Preset::Fireworks);
    let rendered = render(&dialect, &scripted_response());
    let mut expected = None;
    for terminator in ["\n", "\r\n"] {
        for piece in [1_usize, 3, 7, 64, 100_000] {
            let mut bytes = Vec::new();
            for json in rendered
                .head
                .iter()
                .chain(&rendered.counted)
                .chain(&rendered.tail)
            {
                bytes.extend_from_slice(format!(": keep-alive{terminator}{terminator}").as_bytes());
                bytes.extend_from_slice(
                    format!("event: message{terminator}data: {json}{terminator}{terminator}")
                        .as_bytes(),
                );
            }
            let mock = Mock::start(false).await;
            mock.script(Script::stream(
                bytes.chunks(piece).map(|c| Step::raw(c.to_vec())).collect(),
            ));
            let llm = provider_with(config_for(Preset::Fireworks, &mock));
            let s = summarize(run(&llm, user("hi")).await);
            let got = format!("{s:?}");
            assert!(s.error.is_none(), "{terminator:?} {piece}: {got}");
            match &expected {
                None => expected = Some(got),
                Some(first) => assert_eq!(&got, first, "{terminator:?} split every {piece}"),
            }
        }
    }
    assert!(expected.is_some_and(|e| e.contains("Hello")));
}

#[tokio::test]
async fn odd_chunks_are_ignored_not_fatal() {
    let mock = Mock::start(false).await;
    mock.script(events_script(&[
        // Keep-alive style and bookkeeping chunks.
        r#"{"choices":[]}"#.to_owned(),
        r#"{"choices":null}"#.to_owned(),
        r#"{"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}],"usage":null,"warnings":[{"message":"w"}]}"#.to_owned(),
        r#"{"choices":[{"delta":{"refusal":null,"reasoning":"hmm","annotations":[]}}]}"#.to_owned(),
        text_chunk(""),
        text_chunk("ok"),
        finish_chunk("stop"),
    ]));
    let s = summarize(run(&provider(&mock), user("hi")).await);
    assert_eq!(s.text, "ok");
    assert_eq!(s.finish, Some(FinishReason::Stop));
}

// ---------------------------------------------------------------------------------------------
// The end of a response: usage, [DONE], EOF
// ---------------------------------------------------------------------------------------------

/// Reads events until `Done` or an error; returns them with the time the last one took to come.
async fn until_end(mut stream: LlmStream, limit: Duration) -> (Vec<LlmEvent>, Duration) {
    let start = Instant::now();
    let mut events = Vec::new();
    let mut at = Duration::ZERO;
    let end = tokio::time::timeout(limit, async {
        while let Some(event) = stream.next().await {
            at = start.elapsed();
            events.push(event);
        }
    })
    .await;
    assert!(end.is_ok(), "no end of stream within {limit:?}: {events:?}");
    (events, at)
}

fn usage_chunk() -> String {
    json!({"choices": [], "usage": usage_json()}).to_string()
}

/// A script that sends `chunks` and then neither usage, `[DONE]` nor the end of the body.
fn hang(chunks: &[String]) -> Script {
    let mut steps: Vec<Step> = chunks.iter().cloned().map(Step::event).collect();
    steps.push(Step::Sleep(Duration::from_secs(20)));
    Script::stream(steps)
}

#[tokio::test]
async fn a_missing_usage_never_delays_done_by_more_than_the_grace() {
    // The margins are wide on purpose (a correct early Done is more than 2x faster than a wrong
    // one) because the test runs next to other real-socket tests.
    let mock = Mock::start(false).await;
    let mut cfg = config_for(Preset::Vllm, &mock);
    cfg.usage_grace = Duration::from_secs(1);
    let llm = provider_with(cfg.clone());

    // Trailing usage expected: Done comes when the grace runs out, without usage. The timer
    // cannot fire early, so the lower bound is exact; the upper bound is the stream limit.
    mock.script(hang(&[text_chunk("hi"), finish_chunk("stop")]));
    let (events, at) = until_end(
        llm.stream(user("hi")).await.expect("accepted"),
        Duration::from_secs(8),
    )
    .await;
    let s = summarize(events);
    assert_eq!(s.finish, Some(FinishReason::Stop));
    assert_eq!(s.usage, None);
    assert!(at >= Duration::from_millis(900), "Done after {at:?}");

    // A usage chunk inside the grace is picked up and ends the response at once, not at the
    // end of the (here 4 s) grace.
    let mut slow = cfg.clone();
    slow.usage_grace = Duration::from_secs(4);
    mock.script(Script::stream(vec![
        Step::event(text_chunk("hi")),
        Step::event(finish_chunk("stop")),
        Step::Sleep(Duration::from_millis(40)),
        Step::event(usage_chunk()),
        Step::Sleep(Duration::from_secs(20)),
    ]));
    let (events, at) = until_end(
        provider_with(slow)
            .stream(user("hi"))
            .await
            .expect("accepted"),
        Duration::from_secs(8),
    )
    .await;
    let s = summarize(events);
    assert_eq!(s.usage.map(|u| u.output_tokens), Some(5));
    assert!(at < Duration::from_secs(2), "Done after {at:?}");

    // A vendor that reports usage on the finish chunk or never does is not waited for at all.
    for placement in [UsagePlacement::FinishChunk, UsagePlacement::Absent] {
        let mut cfg = cfg.clone();
        cfg.quirks.usage = placement;
        cfg.usage_grace = Duration::from_secs(10);
        mock.script(hang(&[text_chunk("hi"), finish_chunk("stop")]));
        let (events, at) = until_end(
            provider_with(cfg)
                .stream(user("hi"))
                .await
                .expect("accepted"),
            Duration::from_secs(8),
        )
        .await;
        assert_eq!(summarize(events).finish, Some(FinishReason::Stop));
        assert!(
            at < Duration::from_secs(4),
            "{placement:?}: Done after {at:?}"
        );
    }
}

#[tokio::test]
async fn finish_chunk_vendors_end_at_the_finish_chunk_with_its_usage() {
    // Together: usage on the finish chunk or not at all. The response ends at that chunk and
    // reports the usage it carries.
    let mock = Mock::start(false).await;
    let mut cfg = config_for(Preset::Together, &mock);
    cfg.usage_grace = Duration::from_secs(10);
    let llm = provider_with(cfg);
    let mut finish: Value = serde_json::from_str(&finish_chunk("eos")).unwrap();
    finish["usage"] = usage_json();
    mock.script(hang(&[text_chunk("hi"), finish.to_string()]));
    let (events, at) = until_end(
        llm.stream(user("hi")).await.expect("accepted"),
        Duration::from_secs(8),
    )
    .await;
    let s = summarize(events);
    assert_eq!(s.finish, Some(FinishReason::Stop));
    assert_eq!(s.usage.map(|u| u.output_tokens), Some(5));
    assert!(at < Duration::from_secs(4), "Done after {at:?}");

    // By design a separate usage chunk after the finish chunk is not waited for: the quirk says
    // there is none, and a wrong preset must not add a grace to every turn. (A vendor that
    // does send one belongs on `TrailingChunk`.)
    mock.script(hang(&[
        text_chunk("hi"),
        finish_chunk("eos"),
        usage_chunk(),
    ]));
    let (events, at) = until_end(
        llm.stream(user("hi")).await.expect("accepted"),
        Duration::from_secs(8),
    )
    .await;
    assert_eq!(summarize(events).finish, Some(FinishReason::Stop));
    assert!(at < Duration::from_secs(4), "Done after {at:?}");
}

#[tokio::test]
async fn running_usage_on_content_chunks_does_not_end_the_response_early() {
    // vLLM with `continuous_usage_stats`: partial totals ride every chunk, the final one comes
    // in the trailing chunk. The response waits for it and reports it.
    let mock = Mock::start(false).await;
    let running = |text: &str, completion: u64| {
        let mut chunk: Value = serde_json::from_str(&text_chunk(text)).unwrap();
        chunk["usage"] = json!({"prompt_tokens": 3, "completion_tokens": completion});
        chunk.to_string()
    };
    mock.script(events_script(&[
        running("a", 1),
        running("b", 2),
        finish_chunk("stop"),
        usage_chunk(),
    ]));
    let s = summarize(run(&provider_with(config_for(Preset::Vllm, &mock)), user("hi")).await);
    assert_eq!(s.text, "ab");
    assert_eq!(s.finish, Some(FinishReason::Stop));
    assert_eq!(s.usage.map(|u| u.output_tokens), Some(5), "{s:?}");
}

#[tokio::test]
async fn a_connection_reset_after_the_finish_chunk_keeps_the_complete_answer() {
    let mock = Mock::start(false).await;
    let llm = provider_with(config_for(Preset::Vllm, &mock));
    // Reset while the usage chunk is awaited.
    mock.script(Script::stream(vec![
        Step::event(text_chunk("hi")),
        Step::event(finish_chunk("stop")),
        Step::Close,
    ]));
    let s = summarize(run(&llm, user("hi")).await);
    assert!(s.error.is_none(), "{s:?}");
    assert_eq!(s.text, "hi");
    assert_eq!(s.finish, Some(FinishReason::Stop));
    assert_eq!(s.usage, None);

    // A call in flight, reset right after its finish chunk: still a tool-call turn.
    mock.script(Script::stream(vec![
        Step::event(chunk_with(
            json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                "function": {"name": "f", "arguments": "{}"}}]}),
            Value::Null,
        )),
        Step::event(finish_chunk("tool_calls")),
        Step::Close,
    ]));
    let s = summarize(run(&llm, user("hi")).await);
    assert!(s.error.is_none(), "{s:?}");
    assert_eq!(s.finish, Some(FinishReason::ToolCalls));
    assert_eq!(s.calls.len(), 1);
}

#[tokio::test]
async fn done_marker_and_eof_complete_a_response_but_a_cut_does_not() {
    let mock = Mock::start(false).await;
    let llm = provider(&mock);

    // finish chunk then EOF (no `[DONE]`, no usage): complete.
    mock.script(Script::stream(vec![
        Step::event(text_chunk("hi")),
        Step::event(finish_chunk("stop")),
    ]));
    let s = summarize(run(&llm, user("hi")).await);
    assert_eq!(s.finish, Some(FinishReason::Stop));

    // `[DONE]` without any finish_reason: complete, as a stop.
    mock.script(events_script(&[text_chunk("hi")]));
    let s = summarize(run(&llm, user("hi")).await);
    assert_eq!(s.finish, Some(FinishReason::Stop));
    assert_eq!(s.text, "hi");

    // `[DONE]` while a tool call is open and no finish_reason came: a gateway aborted the
    // generation mid call, the arguments are truncated JSON. A cut, never a tool-call turn.
    mock.script(events_script(&[chunk_with(
        json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"Par"}}]}),
        Value::Null,
    )]));
    let s = summarize(run(&llm, user("hi")).await);
    assert!(matches!(s.error, Some(Error::Disconnected { .. })), "{s:?}");
    assert!(s.finish.is_none(), "{s:?}");

    // EOF before any finish_reason: cut off.
    mock.script(Script::stream(vec![Step::event(text_chunk("hi"))]));
    let s = summarize(run(&llm, user("hi")).await);
    assert!(matches!(s.error, Some(Error::Disconnected { .. })), "{s:?}");
    assert!(s.finish.is_none());

    // The connection dropped abruptly.
    mock.script(Script::stream(vec![
        Step::event(text_chunk("hi")),
        Step::Close,
    ]));
    let s = summarize(run(&llm, user("hi")).await);
    assert!(matches!(s.error, Some(Error::Disconnected { .. })), "{s:?}");
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn http_refusals_map_to_rejected_with_the_status_and_are_not_retried() {
    let bodies = [
        r#"{"error":{"message":"Incorrect API key provided: test-key-0123***","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        r#"{"error":"the model failed to generate a response test-key-0123"}"#,
        r#"{"error":{"message":"x","type":"rate_limit_error","code":429}}"#,
        "<html>bad gateway test-key-0123456789</html>",
        "",
    ];
    // Everything the doc lists for the five vendors, plus the odd ones (Together 403 for a
    // context overflow, Groq 498/499, Fireworks 520, Together 524/529).
    let statuses = [
        400_u16, 401, 402, 403, 404, 405, 408, 412, 413, 422, 424, 429, 498, 499, 500, 502, 503,
        504, 520, 524, 529,
    ];
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let mut sent = 0;
    for (n, status) in statuses.iter().enumerate() {
        let body = bodies[n % bodies.len()];
        mock.script(Script::status(*status, body));
        let error = llm.stream(user("hi")).await.expect_err("rejected");
        sent += 1;
        assert!(
            matches!(&error, Error::Rejected { status: got, provider } if got == status && *provider == "groq"),
            "{status}: {error:?}"
        );
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("test-key")
                && !shown.contains("Incorrect")
                && !shown.contains("failed to generate"),
            "{shown}"
        );
    }
    assert_eq!(mock.posts().len(), sent, "no retry");
}

#[tokio::test]
async fn a_stalled_error_body_is_cut_off_after_a_second_and_still_maps_to_rejected() {
    let mock = Mock::start(false).await;
    let body = json!({"error": {"type": "server_error", "code": "overloaded"}}).to_string();
    mock.script(Script::status_then_stall(503, &body, 4096));
    let started = Instant::now();
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
    let target = Mock::start(false).await;
    let mock = Mock::start(false).await;
    mock.script(
        Script::status(307, "").header("Location", &format!("{}/steal", target.base_url())),
    );
    assert!(matches!(
        provider(&mock).stream(user("hi")).await,
        Err(Error::Rejected { status: 307, .. })
    ));
    assert!(target.requests().is_empty());
}

#[tokio::test]
async fn failures_inside_the_stream_arrive_as_error_events_then_the_stream_ends() {
    let cases: [(&str, Option<u16>); 8] = [
        (
            r#"{"error":{"message":"secret test-key-0123","type":"rate_limit_exceeded","code":null}}"#,
            Some(429),
        ),
        (
            r#"{"error":{"message":"m","type":"server_error","code":500}}"#,
            Some(500),
        ),
        (r#"{"error":{"message":"m","code":"503"}}"#, Some(503)),
        (
            r#"{"x_groq":{"error":{"message":"m","code":429}}}"#,
            Some(429),
        ),
        (
            r#"{"error":{"message":"m","type":"invalid_request_error"}}"#,
            Some(400),
        ),
        (
            r#"{"error":"the model failed to generate a response"}"#,
            None,
        ),
        (r#"{"error":{"message":"m","type":"weird thing!"}}"#, None),
        ("{this is not json test-key-0123", None),
    ];
    for (body, status) in cases {
        let mock = Mock::start(false).await;
        // Text first, then the failure, then a `[DONE]` that must not matter (vLLM sends one).
        mock.script(events_script(&[text_chunk("par"), body.to_owned()]));
        let s = summarize(run(&provider(&mock), user("hi")).await);
        assert_eq!(s.text, "par");
        assert!(s.finish.is_none(), "{body}");
        let error = s.error.expect("an error event");
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("test-key") && !shown.contains("failed to generate"),
            "{shown}"
        );
        match (status, &error) {
            (Some(want), Error::Rejected { status, .. }) => assert_eq!(*status, want, "{body}"),
            (None, Error::Protocol { .. }) => {}
            _ => panic!("{body}: {error:?}"),
        }
    }
}

#[tokio::test]
async fn an_oversized_event_is_a_protocol_error_not_unbounded_memory() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_event_bytes = 1024;
    mock.script(events_script(&[text_chunk(&"x".repeat(8 * 1024))]));
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert!(matches!(s.error, Some(Error::Protocol { .. })), "{s:?}");
}

#[tokio::test]
async fn runaway_tool_call_arguments_are_a_protocol_error() {
    let mock = Mock::start(false).await;
    let open = chunk_with(
        json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": ""}}]}),
        Value::Null,
    );
    let mut chunks = vec![open];
    let piece = "a".repeat(64 * 1024);
    for _ in 0..20 {
        chunks.push(chunk_with(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": piece}}]}),
            Value::Null,
        ));
    }
    mock.script(events_script(&chunks));
    let s = summarize(run(&provider(&mock), user("hi")).await);
    assert!(matches!(s.error, Some(Error::Protocol { .. })), "{s:?}");
}

// ---------------------------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------------------------

fn timeout_of(error: &Option<Error>) -> Option<&'static str> {
    match error {
        Some(Error::Core(CoreError::Timeout { what, .. })) => Some(what),
        _ => None,
    }
}

#[tokio::test]
async fn silence_longer_than_the_idle_deadline_is_a_timeout_even_after_events() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    mock.script(Script::stream(vec![
        Step::event(text_chunk("hi")),
        Step::Sleep(Duration::from_secs(20)),
    ]));
    let started = Instant::now();
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert_eq!(timeout_of(&s.error), Some("llm idle"), "{s:?}");
    assert_eq!(s.text, "hi");
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_millis(1500),
        "{took:?}"
    );
}

#[tokio::test]
async fn comments_and_reasoning_do_not_extend_the_first_output_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_output_timeout = Duration::from_millis(400);
    cfg.idle_timeout = Duration::from_secs(10);
    let mut steps = Vec::new();
    for _ in 0..40 {
        steps.push(Step::raw(b": ping\n\n".to_vec()));
        steps.push(Step::event(chunk_with(
            json!({"reasoning": "thinking"}),
            Value::Null,
        )));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let started = Instant::now();
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert_eq!(timeout_of(&s.error), Some("llm first output"), "{s:?}");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn keep_alive_comments_do_not_hold_the_idle_deadline() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    let mut steps = vec![Step::event(text_chunk("hi"))];
    for _ in 0..60 {
        steps.push(Step::raw(b": ping\n\n".to_vec()));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let started = Instant::now();
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert_eq!(timeout_of(&s.error), Some("llm idle"), "{s:?}");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn the_total_duration_of_a_response_is_capped() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_stream_duration = Duration::from_millis(500);
    let mut steps = Vec::new();
    for _ in 0..100 {
        steps.push(Step::event(text_chunk("x")));
        steps.push(Step::Sleep(Duration::from_millis(50)));
    }
    mock.script(Script::stream(steps));
    let started = Instant::now();
    let s = summarize(run(&provider_with(cfg), user("hi")).await);
    assert_eq!(timeout_of(&s.error), Some("llm stream"), "{s:?}");
    assert!(started.elapsed() < Duration::from_millis(2000));
}

#[tokio::test]
async fn response_headers_that_never_come_time_out_in_stream() {
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.first_byte_timeout = Duration::from_millis(300);
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(20);
    mock.script(script);
    let started = Instant::now();
    let result = provider_with(cfg).stream(user("hi")).await;
    assert!(
        matches!(result, Err(Error::Core(CoreError::Timeout { .. }))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn a_refused_connection_is_a_disconnect_not_a_hang() {
    let mock = Mock::start(false).await;
    let cfg = config(&mock).with_base_url("http://127.0.0.1:1/v1");
    let llm = provider_with(cfg);
    // Concurrently: Windows takes about two seconds to report a refused loopback connect.
    let (streamed, warmed) = tokio::join!(llm.stream(user("hi")), llm.warm());
    assert!(matches!(streamed, Err(Error::Disconnected { .. })));
    assert!(matches!(warmed, Err(Error::Disconnected { .. })));
}

// ---------------------------------------------------------------------------------------------
// Cancellation, back-pressure, connections
// ---------------------------------------------------------------------------------------------

/// Waits up to `limit` for the mock to see `n` ended connections; returns how long it took.
async fn wait_for_hangups(mock: &Mock, n: usize, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
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
    let llm = provider_with(cfg);
    mock.script(Script::stream(vec![
        Step::counted(text_chunk("first")),
        Step::Sleep(Duration::from_secs(30)),
        Step::counted(text_chunk("never")),
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
async fn a_stalled_consumer_stops_the_reader_instead_of_buffering_the_response() {
    // Thousands of 2 KiB deltas are scripted; the consumer does not read at all. The reader may
    // hold what fits in the event channel (64) plus socket buffers, but it must stop reading
    // the socket, so the server's writes block far short of the total. Then the consumer
    // resumes and gets everything, in order.
    //
    // How many events the sockets alone absorb depends on the operating system. Measured on
    // Windows loopback: about 270 events (64 in the channel, the rest in socket buffers, ~2 KiB
    // each), so 1000 leaves ~4x headroom and fails for any internal queue of ~1000 events or
    // more, let alone an unbounded one. Measured on Linux loopback: 2000-2500 events (2027 on
    // a GitHub runner, 2480 on WSL2), all of them kernel buffers (`brisvox_sim::platform::loopback_buffer_bytes`), which
    // can grow to ~5000. There the bound
    // is the kernel's maximum plus 256 events for the reader, and the script is twice as long as
    // that bound, so an unbounded reader (everything sent) still fails it by a wide margin.
    const DELTA_BYTES: usize = 2048;
    let bound = if cfg!(target_os = "linux") {
        brisvox_sim::platform::loopback_buffer_bytes() / DELTA_BYTES + 256
    } else {
        1000
    };
    let events: usize = (2 * bound).max(6000);
    let mock = Mock::start(false).await;
    let llm = provider(&mock);
    let pad = "p".repeat(DELTA_BYTES);
    let mut steps: Vec<Step> = (0..events)
        .map(|n| Step::counted(text_chunk(&format!("{n:06}{pad}"))))
        .collect();
    steps.push(Step::counted(finish_chunk("stop")));
    mock.script(Script::stream(steps));

    let mut stream = llm.stream(user("hi")).await.expect("accepted");
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
    let received = tokio::time::timeout(Duration::from_secs(30), async {
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
async fn a_consumer_that_never_reads_cannot_hold_the_connection_past_the_stream_cap() {
    // The reader is blocked handing an event to a consumer that holds the stream without
    // reading. It still gives up at the response's duration cap: the connection is closed and
    // the stream ends without a terminal event (a cut-off, as documented on `LlmStream::next`).
    const EVENTS: usize = 6000;
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.max_stream_duration = Duration::from_millis(700);
    let llm = provider_with(cfg);
    let pad = "p".repeat(2048);
    let steps: Vec<Step> = (0..EVENTS)
        .map(|n| Step::counted(text_chunk(&format!("{n:06}{pad}"))))
        .collect();
    mock.script(Script::stream(steps));
    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    let took = wait_for_hangups(&mock, 1, Duration::from_secs(6)).await;
    assert!(took.is_some(), "the reader never gave up the connection");
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends")
    {
        events.push(event);
    }
    assert!(!events.is_empty() && events.len() <= 70, "{}", events.len());
    assert!(
        events.iter().all(|e| matches!(e, LlmEvent::TextDelta(_))),
        "a terminal event was invented"
    );
}

#[tokio::test]
async fn a_slow_consumer_is_not_blamed_by_the_idle_timer() {
    // More events than the channel holds (64), so the reader is blocked handing events over for
    // the whole 900 ms the consumer sleeps, with both server timers at 300 ms. The timers must
    // be armed after the hand-over, not from when the events arrived.
    const CHUNKS: usize = 200;
    let mock = Mock::start(false).await;
    let mut cfg = config(&mock);
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.first_output_timeout = Duration::from_millis(300);
    let llm = provider_with(cfg);
    let mut chunks: Vec<String> = (0..CHUNKS).map(|_| text_chunk("a")).collect();
    chunks.push(finish_chunk("stop"));
    mock.script(events_script(&chunks));
    let mut stream = llm.stream(user("hi")).await.expect("accepted");
    tokio::time::sleep(Duration::from_millis(900)).await;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let s = summarize(events);
    assert!(s.error.is_none(), "{s:?}");
    assert_eq!(s.text, "a".repeat(CHUNKS));
    assert_eq!(s.finish, Some(FinishReason::Stop));
}

#[tokio::test]
async fn warm_and_finished_responses_leave_one_pooled_connection_for_the_next_request() {
    let mock = Mock::start(true).await;
    let llm = provider(&mock);
    llm.warm().await.expect("warm");
    assert_eq!(mock.connections(), 1);
    for _ in 0..3 {
        mock.script(events_script(&[text_chunk("x"), finish_chunk("stop")]));
        let events = run(&llm, user("hi")).await;
        assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    }
    assert_eq!(
        mock.connections(),
        1,
        "warm() and three streams share the connection the warm-up opened"
    );
}

// ---------------------------------------------------------------------------------------------
// Secrets and the base URL
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nothing_secret_shows_up_in_debug_output_or_errors() {
    let mock = Mock::start(false).await;
    let cfg = config(&mock)
        .with_header("x-tenant-token", "TENANTSECRET")
        .with_auth(AuthStyle::bearer());
    let llm = provider_with(cfg);
    let shown = format!("{:?} {:?}", llm.config(), llm);
    assert!(
        !shown.contains("0123456789") && !shown.contains("TENANTSECRET"),
        "{shown}"
    );
    assert!(shown.contains("[REDACTED]"), "{shown}");

    // A key or header that cannot be an HTTP header value is refused without echoing it.
    let bad_key = OpenAiCompatibleConfig::new("https://example.com/v1", "m")
        .with_api_key("sk-bad\nkey-SECRETPART");
    let error = OpenAiCompatibleLlm::new(bad_key, SystemClock::shared()).expect_err("invalid");
    let shown = format!("{error} {error:?}");
    assert!(
        !shown.contains("SECRETPART") && !shown.contains("sk-bad"),
        "{shown}"
    );
    let bad_header = OpenAiCompatibleConfig::new("https://example.com/v1", "m")
        .with_header("x-a", "tok\nSECRETPART");
    let error = OpenAiCompatibleLlm::new(bad_header, SystemClock::shared()).expect_err("invalid");
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("SECRETPART"), "{shown}");

    // The request body never carries the key.
    mock.script(events_script(&[finish_chunk("stop")]));
    run(&llm, user("hi")).await;
    assert!(!mock.posts()[0].body.contains("0123456789"));
}

/// A `tracing` writer that keeps everything logged while its guard is alive.
#[derive(Clone, Default)]
struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut buffer) = self.0.lock() {
            buffer.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;

    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Everything this test binary logs at debug level or above, from the first call on. A global
/// subscriber, not a scoped one: tests run in parallel and a scoped subscriber can miss a
/// callsite that another thread registered first.
fn captured_log() -> &'static LogBuf {
    static LOG: std::sync::OnceLock<LogBuf> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let log = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        log
    })
}

#[tokio::test]
async fn credentials_in_the_base_url_never_reach_logs_debug_output_or_errors() {
    // The base URL and the chat path are the user's: userinfo, a `?key=` query or a secret path
    // segment must not show up anywhere. A server that hangs up makes reqwest build an error
    // whose Display carries the URL, and the adapter logs transport errors at debug level.
    let log = captured_log();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hang_up = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });

    let mut cfg =
        OpenAiCompatibleConfig::new(format!("http://user:pw-URLSECRET@127.0.0.1:{port}/v1"), "m")
            .with_api_key(KEY)
            .with_body_field("metadata", json!({"tenant": "URLSECRET"}))
            .with_timeout(Duration::from_secs(3));
    cfg.name = "urlcred";
    cfg.chat_path = "/chat/completions?key=URLSECRET".to_owned();
    let llm = provider_with(cfg);
    let shown = format!("{:?} {:?}", llm.config(), llm);
    assert!(
        !shown.contains("URLSECRET") && !shown.contains(KEY),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("http://127.0.0.1:{port}")),
        "{shown}"
    );

    let error = llm
        .stream(user("hi"))
        .await
        .expect_err("the server hangs up");
    assert!(matches!(error, Error::Disconnected { .. }), "{error:?}");
    let warm = llm.warm().await.expect_err("the server hangs up");
    hang_up.abort();
    let shown = format!("{error} {error:?} {warm} {warm:?}");
    assert!(!shown.contains("URLSECRET"), "{shown}");

    let logged = log.text();
    assert!(
        logged
            .lines()
            .any(|l| l.contains("LLM transport error") && l.contains("urlcred")),
        "the failure is logged: {logged}"
    );
    assert!(!logged.contains("URLSECRET"), "{logged}");
    assert!(!logged.contains(KEY), "{logged}");
}

#[tokio::test]
async fn a_refusal_is_logged_with_status_retry_after_type_and_code_but_never_the_message() {
    let log = captured_log();
    let mock = Mock::start(false).await;
    let body = json!({"error": {
        "message": "Incorrect API key provided: MSGMARKER-sk-test-123",
        "type": "invalid_request_error",
        "code": "invalid_api_key",
    }})
    .to_string();
    mock.script(Script::status(429, &body).header("Retry-After", "7"));
    let mut cfg = config(&mock);
    cfg.name = "reflog";
    let error = provider_with(cfg)
        .stream(user("hi"))
        .await
        .expect_err("rejected");
    assert!(
        matches!(error, Error::Rejected { status: 429, .. }),
        "{error:?}"
    );

    let logged = log.text();
    let line = logged
        .lines()
        .find(|l| l.contains("the LLM server rejected the request") && l.contains("reflog"))
        .unwrap_or_else(|| panic!("the refusal is logged: {logged}"));
    for field in [
        "status=429",
        "retry_after_secs=7",
        "error_type=\"invalid_request_error\"",
        "error_code=\"invalid_api_key\"",
    ] {
        // `tracing` prints strings quoted or bare depending on the version: accept both.
        let bare = field.replace('"', "");
        assert!(
            line.contains(field) || line.contains(&bare),
            "{field} missing: {line}"
        );
    }
    assert!(!logged.contains("MSGMARKER"), "{logged}");
    assert!(!logged.contains("sk-test-123"), "{logged}");
}

#[tokio::test]
async fn the_request_never_carries_a_key_twice() {
    let mock = Mock::start(false).await;
    let count = |body: &str, key: &str| body.matches(&format!("\"{key}\":")).count();

    // Keys the adapter serialises itself are refused in `extra_body`, while it serialises them.
    let mut with_temperature = config(&mock).with_body_field("temperature", json!(0.9));
    with_temperature.temperature = Some(0.5);
    assert!(matches!(
        OpenAiCompatibleLlm::new(with_temperature, SystemClock::shared()),
        Err(Error::Protocol { .. })
    ));
    let mut with_parallel = config(&mock).with_body_field("parallel_tool_calls", json!(false));
    with_parallel.quirks.parallel_tool_calls = Some(true);
    assert!(matches!(
        OpenAiCompatibleLlm::new(with_parallel, SystemClock::shared()),
        Err(Error::Protocol { .. })
    ));

    // The adapter's own field alone: once, on the raw body.
    let mut cfg = config(&mock);
    cfg.temperature = Some(0.5);
    cfg.quirks.parallel_tool_calls = Some(true);
    mock.script(events_script(&[finish_chunk("stop")]));
    let request = LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: vec![weather_tool()],
    };
    run(&provider_with(cfg), request.clone()).await;
    let raw = &mock.posts()[0].body;
    assert_eq!(count(raw, "temperature"), 1, "{raw}");
    assert_eq!(count(raw, "parallel_tool_calls"), 1, "{raw}");

    // The same keys in `extra_body` when the adapter does not set them: once, with that value.
    let cfg = config(&mock)
        .with_body_field("temperature", json!(0.9))
        .with_body_field("parallel_tool_calls", json!(false));
    mock.script(events_script(&[finish_chunk("stop")]));
    run(&provider_with(cfg), request).await;
    let post = &mock.posts()[1];
    assert_eq!(count(&post.body, "temperature"), 1, "{}", post.body);
    assert_eq!(count(&post.body, "parallel_tool_calls"), 1, "{}", post.body);
    assert_eq!(post.json()["temperature"], json!(0.9));
    assert_eq!(post.json()["parallel_tool_calls"], json!(false));
}

#[tokio::test]
async fn plain_http_is_refused_for_remote_hosts_unless_opted_in() {
    let build = |url: &str, allow: bool| {
        let mut cfg = OpenAiCompatibleConfig::new(url, "m").with_api_key(KEY);
        cfg.allow_insecure_http = allow;
        OpenAiCompatibleLlm::new(cfg, SystemClock::shared())
    };
    for url in [
        "http://gateway.example.com/v1",
        "http://10.0.0.5:8000/v1",
        "http://127.0.0.1@evil.example.com/v1",
        "http://localhost.evil.example.com/v1",
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
        "https://api.groq.com/openai/v1",
        "http://localhost:11434/v1/",
        "http://LOCALHOST/v1",
        "http://127.0.0.1:8000/v1",
        "http://[::1]:8000/v1",
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
