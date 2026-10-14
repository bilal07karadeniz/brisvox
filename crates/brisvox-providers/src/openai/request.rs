//! Builds the body of `POST /v1/responses` from an [`LlmRequest`].
//!
//! Stateless replay (`store: false`, no `previous_response_id`): the whole history is sent each
//! turn, built from the engine's own messages, so after an interruption the model sees only what
//! was played. The request is laid out for prompt caching (see `docs/providers/openai.md`):
//! tool definitions are rendered once and reused byte for byte, `instructions` come from the
//! leading system messages, and the history is append-only.

use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use super::PROVIDER;
use super::config::{OpenAiConfig, StrictTools};
use super::events::sanitize;
use crate::error::{Error, Result};
use crate::llm::{LlmRequest, Message, Role, ToolSpec};

/// Longest tool name OpenAI accepts.
const MAX_TOOL_NAME: usize = 64;

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

/// The tool definitions of a request, rendered once and reused while the tools do not change.
#[derive(Debug)]
pub(crate) struct RenderedTools {
    specs: Vec<ToolSpec>,
    strict: StrictTools,
    json: Box<RawValue>,
}

impl RenderedTools {
    /// Whether this rendering is still valid for `specs` under `strict`.
    pub(crate) fn matches(&self, specs: &[ToolSpec], strict: StrictTools) -> bool {
        self.strict == strict && self.specs == specs
    }

    /// Renders `specs` as the Responses `tools` array.
    pub(crate) fn render(specs: &[ToolSpec], strict: StrictTools) -> Result<Arc<Self>> {
        #[derive(Serialize)]
        struct Tool<'a> {
            r#type: &'static str,
            name: &'a str,
            description: &'a str,
            parameters: &'a RawValue,
            strict: bool,
        }
        let mut parsed = Vec::with_capacity(specs.len());
        for spec in specs {
            let name: &str = &spec.name;
            if name.is_empty()
                || name.len() > MAX_TOOL_NAME
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(protocol(format!(
                    "tool name {name:?} is not 1-{MAX_TOOL_NAME} characters of [A-Za-z0-9_-]"
                )));
            }
            let raw: &RawValue = serde_json::from_str(&spec.parameters_schema).map_err(|e| {
                protocol(format!("tool {name}: parameters schema is not JSON: {e}"))
            })?;
            let compliant = strict == StrictTools::WhenCompliant
                && serde_json::from_str::<Value>(raw.get())
                    .is_ok_and(|schema| complies_with_strict_mode(&schema));
            parsed.push((spec, raw, compliant));
        }
        let tools: Vec<Tool<'_>> = parsed
            .iter()
            .map(|(spec, raw, compliant)| Tool {
                r#type: "function",
                name: &spec.name,
                description: &spec.description,
                parameters: raw,
                strict: *compliant,
            })
            .collect();
        let json = serde_json::value::to_raw_value(&tools)
            .map_err(|e| protocol(format!("cannot render tools: {e}")))?;
        Ok(Arc::new(Self {
            specs: specs.to_vec(),
            strict,
            json,
        }))
    }
}

/// Whether `schema` already satisfies OpenAI's strict function-calling rules: an object at the
/// root, `additionalProperties: false` on every object, every property listed in `required`,
/// and none of the keywords strict mode does not support. Conservative: when in doubt, `false`
/// (the tool is then sent with `strict: false`, best-effort).
pub(crate) fn complies_with_strict_mode(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("object") && schema_complies(schema)
}

fn schema_complies(schema: &Value) -> bool {
    let Some(map) = schema.as_object() else {
        return false;
    };
    // Keywords OpenAI's structured outputs reject (or that make a schema unverifiable here):
    // when in doubt the tool is sent with `strict: false` instead of failing every request.
    const UNSUPPORTED: [&str; 15] = [
        "oneOf",
        "allOf",
        "not",
        "patternProperties",
        "if",
        "then",
        "else",
        "dependentSchemas",
        "dependentRequired",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "minContains",
        "maxContains",
    ];
    if UNSUPPORTED.iter().any(|k| map.contains_key(*k)) {
        return false;
    }
    // Every sub-schema must declare its shape (`type`, `$ref` or `anyOf`). A typeless leaf such
    // as `{"enum": [..]}`, `{"description": ".."}` or `{}` is something the strict validator may
    // reject ("schema must have a 'type' key"), and one rejected tool fails every request, so
    // such tools are sent with `strict: false`.
    let declares_shape = match map.get("type") {
        Some(Value::String(_)) => true,
        Some(Value::Array(types)) => !types.is_empty() && types.iter().all(Value::is_string),
        Some(_) => false,
        None => map.contains_key("$ref") || map.contains_key("anyOf"),
    };
    if !declares_shape {
        return false;
    }
    let is_object = match map.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("object")),
        _ => map.contains_key("properties"),
    };
    if is_object {
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            return false;
        }
        let properties = map.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if let Some(properties) = properties {
            if !properties.keys().all(|k| required.contains(&k.as_str())) {
                return false;
            }
            if !properties.values().all(schema_complies) {
                return false;
            }
        }
    }
    if map
        .get("items")
        .is_some_and(|items| !schema_complies(items))
    {
        return false;
    }
    if let Some(options) = map.get("anyOf") {
        match options.as_array() {
            Some(options) if options.iter().all(schema_complies) => {}
            _ => return false,
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(defs) = map.get(key) {
            match defs.as_object() {
                Some(defs) if defs.values().all(schema_complies) => {}
                _ => return false,
            }
        }
    }
    true
}

#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

#[derive(Serialize)]
struct StreamOptions {
    include_obfuscation: bool,
}

/// One element of `input`.
#[derive(Serialize)]
#[serde(untagged)]
enum InputItem<'a> {
    Message {
        r#type: &'static str,
        role: &'static str,
        content: &'a str,
    },
    FunctionCall {
        r#type: &'static str,
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    FunctionCallOutput {
        r#type: &'static str,
        call_id: &'a str,
        output: &'a str,
    },
    /// An output item of an earlier response, echoed exactly as received.
    Raw(&'a RawValue),
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<Cow<'a, str>>,
    input: Vec<InputItem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<[&'static str; 1]>,
    stream_options: StreamOptions,
}

/// The fields of a replayed output item the adapter needs to place it.
#[derive(Deserialize)]
struct ItemHead<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    call_id: Option<Cow<'a, str>>,
}

/// One replayable output item of an earlier response.
enum ReplayItem<'a> {
    /// An opaque `reasoning` item.
    Reasoning(&'a RawValue),
    /// A `function_call` item, with its `call_id`.
    Call(String, &'a RawValue),
    /// Where the assistant message sat among the other items (the adapter's own placeholder, see
    /// `events::MESSAGE_MARKER`); never sent.
    Message,
}

/// The replayable output items of an earlier response (`Message::provider_data`), in the order
/// the server produced them.
#[derive(Default)]
struct Replay<'a> {
    items: Vec<ReplayItem<'a>>,
}

impl<'a> Replay<'a> {
    /// Parses `provider_data`. Data that is not an array of OpenAI output items (for example
    /// from another provider) yields nothing: adapters ignore data that is not theirs.
    fn parse(provider_data: &'a str) -> Self {
        let mut replay = Self::default();
        let Ok(items) = serde_json::from_str::<Vec<&'a RawValue>>(provider_data) else {
            return replay;
        };
        for raw in items {
            let Ok(head) = serde_json::from_str::<ItemHead<'_>>(raw.get()) else {
                continue;
            };
            match (head.kind.as_ref(), head.call_id) {
                ("reasoning", _) => replay.items.push(ReplayItem::Reasoning(raw)),
                ("function_call", Some(call_id)) => replay
                    .items
                    .push(ReplayItem::Call(call_id.into_owned(), raw)),
                ("message", _)
                    if !replay
                        .items
                        .iter()
                        .any(|i| matches!(i, ReplayItem::Message)) =>
                {
                    replay.items.push(ReplayItem::Message);
                }
                _ => {}
            }
        }
        replay
    }

    /// The index of the item the played text goes before, when the data has no message marker
    /// (older or foreign data): before the reasoning run that leads into the first call, so no
    /// reasoning item is separated from the item it belongs to; after everything if there is no
    /// call.
    fn text_position(&self) -> usize {
        let Some(first_call) = self
            .items
            .iter()
            .position(|i| matches!(i, ReplayItem::Call(..)))
        else {
            return self.items.len();
        };
        let mut at = first_call;
        while at > 0 && matches!(self.items.get(at - 1), Some(ReplayItem::Reasoning(_))) {
            at -= 1;
        }
        at
    }
}

/// Splits the leading system messages (the `instructions`) from the rest.
fn split_instructions(messages: &[Message]) -> (Option<Cow<'_, str>>, &[Message]) {
    let leading = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let (system, rest) = messages.split_at(leading);
    let instructions = match system {
        [] => None,
        [only] => Some(Cow::Borrowed(&*only.content)),
        many => Some(Cow::Owned(
            many.iter()
                .map(|m| &*m.content)
                .collect::<Vec<_>>()
                .join("\n\n"),
        )),
    };
    (instructions, rest)
}

/// The JSON body for `request`, plus the `Replay` borrows it needs, serialised.
pub(crate) fn build_body(
    config: &OpenAiConfig,
    request: &LlmRequest,
    tools: Option<&RenderedTools>,
) -> Result<Bytes> {
    let (instructions, rest) = split_instructions(&request.messages);
    // Parsed first so the borrows outlive the items that point into them.
    let replays: Vec<Option<Replay<'_>>> = rest
        .iter()
        .map(|m| {
            (m.role == Role::Assistant)
                .then_some(m.provider_data.as_deref())
                .flatten()
                .map(Replay::parse)
        })
        .collect();
    let mut input = Vec::with_capacity(rest.len() + 2);
    // Calls made by the history so far that no tool message has answered yet.
    let mut open: Vec<&str> = Vec::new();
    for (message, replay) in rest.iter().zip(&replays) {
        match message.role {
            // A system message after the conversation started (a late note): a developer item.
            Role::System => input.push(InputItem::Message {
                r#type: "message",
                role: "developer",
                content: &message.content,
            }),
            Role::User => input.push(InputItem::Message {
                r#type: "message",
                role: "user",
                content: &message.content,
            }),
            Role::Assistant => {
                push_assistant(&mut input, message, replay.as_ref(), config);
                // A turn with calls is never dropped, so each of its calls is in the input.
                open.extend(message.tool_calls.iter().map(|c| &*c.id));
            }
            Role::Tool => {
                let Some(call_id) = message.tool_call_id.as_deref() else {
                    return Err(protocol("a tool message has no tool_call_id"));
                };
                // The API answers a dangling call or result with HTTP 400 after a full round
                // trip: refuse it here, where the cause is visible.
                let Some(position) = open.iter().position(|id| *id == call_id) else {
                    return Err(protocol(format!(
                        "tool result for call {} has no earlier unanswered call in the history \
                         (dropped assistant turn, or answered twice)",
                        sanitize(call_id)
                    )));
                };
                open.remove(position);
                input.push(InputItem::FunctionCallOutput {
                    r#type: "function_call_output",
                    call_id,
                    output: &message.content,
                });
            }
        }
    }
    if let Some(call_id) = open.first() {
        return Err(protocol(format!(
            "tool call {} has no tool result in the history (cancelled tool?): the caller must \
             answer or drop it before the next request",
            sanitize(call_id)
        )));
    }
    let has_tools = tools.is_some_and(|t| !t.specs.is_empty());
    let body = Body {
        model: &config.model,
        stream: true,
        store: false,
        instructions,
        input,
        tools: tools.filter(|t| !t.specs.is_empty()).map(|t| &*t.json),
        parallel_tool_calls: has_tools.then_some(config.parallel_tool_calls),
        reasoning: config
            .reasoning_effort
            .map(|e| Reasoning { effort: e.as_str() }),
        max_output_tokens: config.max_output_tokens,
        prompt_cache_key: config.prompt_cache_key.as_deref(),
        service_tier: config.service_tier.as_deref(),
        include: config
            .replay_reasoning
            .then_some(["reasoning.encrypted_content"]),
        stream_options: StreamOptions {
            include_obfuscation: config.include_obfuscation,
        },
    };
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|e| protocol(format!("cannot serialise the request: {e}")))
}

/// Replays one assistant turn: the reasoning items, the text that was played and the tool calls,
/// in the order the server produced them (a reasoning item stays directly in front of the item
/// it belongs to). With `replay_reasoning` off, or without provider data, the turn is rebuilt
/// from the engine's own record: the text, then one plain `function_call` per call (no `id`, no
/// `status`).
fn push_assistant<'a>(
    input: &mut Vec<InputItem<'a>>,
    message: &'a Message,
    replay: Option<&Replay<'a>>,
    config: &OpenAiConfig,
) {
    let has_text = !message.content.is_empty();
    if !has_text && message.tool_calls.is_empty() {
        // Nothing was played and nothing was called: the turn leaves no trace.
        return;
    }
    let text = InputItem::Message {
        r#type: "message",
        role: "assistant",
        content: &message.content,
    };
    let mut text = has_text.then_some(text);
    let mut sent_calls: Vec<&str> = Vec::new();
    if let (Some(replay), true) = (replay, config.replay_reasoning) {
        let marker = replay
            .items
            .iter()
            .position(|i| matches!(i, ReplayItem::Message));
        let text_at = marker.unwrap_or_else(|| replay.text_position());
        // Reasoning waiting for the item that follows it; dropped with that item.
        let mut pending: Vec<&'a RawValue> = Vec::new();
        for at in 0..=replay.items.len() {
            if at == text_at
                && let Some(text) = text.take()
            {
                input.extend(pending.drain(..).map(InputItem::Raw));
                input.push(text);
            }
            match replay.items.get(at) {
                None => break,
                Some(ReplayItem::Reasoning(raw)) => pending.push(raw),
                // The message was not played (or is placed above): reasoning that led into it
                // has no follower.
                Some(ReplayItem::Message) => pending.clear(),
                Some(ReplayItem::Call(call_id, raw)) => {
                    let wanted = message.tool_calls.iter().any(|c| *c.id == **call_id)
                        && !sent_calls.contains(&call_id.as_str());
                    if wanted {
                        input.extend(pending.drain(..).map(InputItem::Raw));
                        input.push(InputItem::Raw(raw));
                        sent_calls.push(call_id);
                    } else {
                        pending.clear();
                    }
                }
            }
        }
    }
    input.extend(text);
    for call in &message.tool_calls {
        if sent_calls.contains(&&*call.id) {
            continue;
        }
        input.push(InputItem::FunctionCall {
            r#type: "function_call",
            call_id: &call.id,
            name: &call.name,
            arguments: &call.arguments,
        });
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::Secret;
    use serde_json::json;

    use super::*;
    use crate::llm::ToolCall;

    fn config() -> OpenAiConfig {
        OpenAiConfig::new(Secret::new("sk-test".to_owned()))
    }

    fn body(config: &OpenAiConfig, request: &LlmRequest) -> Value {
        let tools = RenderedTools::render(&request.tools, config.strict_tools).unwrap();
        let bytes = build_body(config, request, Some(&tools)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn spec(name: &str, schema: &str) -> ToolSpec {
        ToolSpec {
            name: Arc::from(name),
            description: Arc::from("does a thing"),
            parameters_schema: Arc::from(schema),
        }
    }

    #[test]
    fn a_plain_conversation_is_sent_stateless_with_leading_system_as_instructions() {
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::System, "Be brief."),
                Message::new(Role::System, "Speak Turkish."),
                Message::new(Role::User, "Merhaba"),
                Message::new(Role::Assistant, "Selam!"),
                Message::new(Role::System, "late note"),
                Message::new(Role::User, "Nasılsın?"),
            ],
            tools: Vec::new(),
        };
        assert_eq!(
            body(&config(), &request),
            json!({
                "model": "gpt-6-luna",
                "stream": true,
                "store": false,
                "instructions": "Be brief.\n\nSpeak Turkish.",
                "input": [
                    {"type": "message", "role": "user", "content": "Merhaba"},
                    {"type": "message", "role": "assistant", "content": "Selam!"},
                    {"type": "message", "role": "developer", "content": "late note"},
                    {"type": "message", "role": "user", "content": "Nasılsın?"},
                ],
                "reasoning": {"effort": "none"},
                "include": ["reasoning.encrypted_content"],
                "stream_options": {"include_obfuscation": false},
            })
        );
    }

    #[test]
    fn optional_settings_appear_only_when_configured() {
        let mut config = config();
        config.reasoning_effort = None;
        config.replay_reasoning = false;
        config.max_output_tokens = Some(300);
        config.prompt_cache_key = Some("agent-v1".to_owned());
        config.service_tier = Some("fast".to_owned());
        let request = LlmRequest {
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![spec("t", r#"{"type":"object"}"#)],
        };
        let json = body(&config, &request);
        assert_eq!(json["max_output_tokens"], 300);
        assert_eq!(json["prompt_cache_key"], "agent-v1");
        assert_eq!(json["service_tier"], "fast");
        assert_eq!(json["parallel_tool_calls"], true);
        assert!(json.get("reasoning").is_none());
        assert!(json.get("include").is_none());
        assert!(json.get("instructions").is_none());
        // No tools: neither `tools` nor `parallel_tool_calls`.
        let json = body(
            &config,
            &LlmRequest {
                messages: vec![Message::new(Role::User, "hi")],
                tools: Vec::new(),
            },
        );
        assert!(json.get("tools").is_none() && json.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn a_tool_turn_replays_the_call_and_its_output_by_call_id() {
        let call = ToolCall {
            id: Arc::from("call_1"),
            name: Arc::from("get_weather"),
            arguments: Arc::from(r#"{"city":"Paris"}"#),
        };
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::User, "weather?"),
                Message::assistant("Let me check.", vec![call], None),
                Message::tool_result("call_1", r#"{"c":18}"#),
            ],
            tools: Vec::new(),
        };
        assert_eq!(
            body(&config(), &request)["input"],
            json!([
                {"type": "message", "role": "user", "content": "weather?"},
                {"type": "message", "role": "assistant", "content": "Let me check."},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                 "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "{\"c\":18}"},
            ])
        );
    }

    #[test]
    fn provider_data_echoes_reasoning_and_the_exact_call_item() {
        let data = json!([
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "t",
             "arguments": "{}", "status": "completed"},
            {"type": "function_call", "id": "fc_2", "call_id": "call_dropped", "name": "t",
             "arguments": "{}"},
        ]);
        let call = ToolCall {
            id: Arc::from("call_1"),
            name: Arc::from("t"),
            arguments: Arc::from("{}"),
        };
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::User, "go"),
                Message::assistant("", vec![call], Some(Arc::from(data.to_string()))),
                Message::tool_result("call_1", "ok"),
            ],
            tools: Vec::new(),
        };
        assert_eq!(
            body(&config(), &request)["input"],
            json!([
                {"type": "message", "role": "user", "content": "go"},
                {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "t",
                 "arguments": "{}", "status": "completed"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            ]),
            "a function_call item without a call in the message is not replayed"
        );
        // replay_reasoning off: the reasoning item is dropped, the call stays.
        let mut off = config();
        off.replay_reasoning = false;
        let input = body(&off, &request)["input"].clone();
        assert!(
            input
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["type"] != "reasoning")
        );
        assert_eq!(input.as_array().unwrap().len(), 3);
    }

    #[test]
    fn reasoning_of_a_turn_that_left_nothing_is_not_orphaned() {
        // Interrupted before anything was played and no call: the whole turn disappears,
        // reasoning included (the API rejects a reasoning item without its follower).
        let data = json!([{"type": "reasoning", "id": "rs_1", "summary": []}]);
        let request = LlmRequest {
            messages: vec![
                Message::new(Role::User, "a"),
                Message::assistant("", Vec::new(), Some(Arc::from(data.to_string()))),
                Message::new(Role::User, "b"),
            ],
            tools: Vec::new(),
        };
        assert_eq!(
            body(&config(), &request)["input"],
            json!([
                {"type": "message", "role": "user", "content": "a"},
                {"type": "message", "role": "user", "content": "b"},
            ])
        );
    }

    #[test]
    fn foreign_or_broken_provider_data_is_ignored() {
        for data in ["not json", r#"{"a":1}"#, r#"[1,"x",{"type":"thinking"}]"#] {
            let request = LlmRequest {
                messages: vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("hi", Vec::new(), Some(Arc::from(data))),
                ],
                tools: Vec::new(),
            };
            assert_eq!(
                body(&config(), &request)["input"].as_array().unwrap().len(),
                2
            );
        }
    }

    #[test]
    fn a_tool_message_without_a_call_id_is_rejected() {
        let request = LlmRequest {
            messages: vec![Message::new(Role::Tool, "orphan")],
            tools: Vec::new(),
        };
        assert!(matches!(
            build_body(&config(), &request, None),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn tools_are_strict_only_when_their_schema_complies() {
        let strict = r#"{"type":"object","properties":{"city":{"type":"string"},"unit":{"type":["string","null"]}},"required":["city","unit"],"additionalProperties":false}"#;
        let loose = r#"{"type":"object","properties":{"city":{"type":"string"}}}"#;
        let tools = RenderedTools::render(
            &[spec("strict_one", strict), spec("loose_one", loose)],
            StrictTools::WhenCompliant,
        )
        .unwrap();
        let json: Value = serde_json::from_str(tools.json.get()).unwrap();
        assert_eq!(json[0]["strict"], true);
        assert_eq!(json[1]["strict"], false);
        assert_eq!(json[0]["type"], "function");
        assert_eq!(json[0]["name"], "strict_one");
        assert_eq!(json[0]["parameters"]["required"], json!(["city", "unit"]));
        let never =
            RenderedTools::render(&[spec("strict_one", strict)], StrictTools::Never).unwrap();
        let json: Value = serde_json::from_str(never.json.get()).unwrap();
        assert_eq!(json[0]["strict"], false);
    }

    #[test]
    fn strict_compliance_checks_nested_schemas() {
        let ok = json!({"type":"object","additionalProperties":false,
            "properties":{"a":{"type":"array","items":{"type":"object","additionalProperties":false,
            "properties":{"b":{"type":"string"}},"required":["b"]}}},"required":["a"]});
        assert!(complies_with_strict_mode(&ok));
        let nested_open = json!({"type":"object","additionalProperties":false,
            "properties":{"a":{"type":"object","properties":{"b":{"type":"string"}},"required":["b"]}},
            "required":["a"]});
        assert!(!complies_with_strict_mode(&nested_open));
        let missing_required = json!({"type":"object","additionalProperties":false,
            "properties":{"a":{"type":"string"}},"required":[]});
        assert!(!complies_with_strict_mode(&missing_required));
        let one_of = json!({"type":"object","additionalProperties":false,"properties":{},
            "oneOf":[{"type":"object"}]});
        assert!(!complies_with_strict_mode(&one_of));
        assert!(!complies_with_strict_mode(&json!({"type":"string"})));
        assert!(complies_with_strict_mode(
            &json!({"type":"object","additionalProperties":false})
        ));
    }

    #[test]
    fn sub_schemas_without_a_type_are_not_strict_compliant() {
        // A typeless leaf is something the strict validator may reject for every request.
        for leaf in [
            json!({"enum":["a","b"]}),
            json!({"description":"x"}),
            json!({}),
            json!({"type": 7}),
            json!({"type": []}),
        ] {
            let schema = json!({"type":"object","additionalProperties":false,
                "properties":{"mode": leaf},"required":["mode"]});
            assert!(!complies_with_strict_mode(&schema), "{schema}");
        }
        let typeless_items = json!({"type":"object","additionalProperties":false,
            "properties":{"a":{"type":"array","items":{}}},"required":["a"]});
        assert!(!complies_with_strict_mode(&typeless_items));
        let typeless_any_of = json!({"type":"object","additionalProperties":false,
            "properties":{"a":{"anyOf":[{"type":"string"},{"enum":[1]}]}},"required":["a"]});
        assert!(!complies_with_strict_mode(&typeless_any_of));
        let typeless_def = json!({"type":"object","additionalProperties":false,
            "properties":{},"$defs":{"d":{"enum":[1]}}});
        assert!(!complies_with_strict_mode(&typeless_def));
        // Typed leaves, `$ref` and `anyOf` declare their shape.
        let typed = json!({"type":"object","additionalProperties":false,
            "properties":{"mode":{"type":"string","enum":["a","b"]},
                          "n":{"type":["integer","null"]},
                          "r":{"$ref":"#/$defs/d"},
                          "u":{"anyOf":[{"type":"string"},{"type":"null"}]}},
            "required":["mode","n","r","u"],
            "$defs":{"d":{"type":"string"}}});
        assert!(complies_with_strict_mode(&typed));
    }

    #[test]
    fn bad_tool_definitions_are_rejected_before_any_request() {
        for (name, schema) in [("bad name", "{}"), ("", "{}"), ("ok", "{not json")] {
            assert!(
                matches!(
                    RenderedTools::render(&[spec(name, schema)], StrictTools::Never),
                    Err(Error::Protocol { .. })
                ),
                "{name:?} {schema:?}"
            );
        }
        let long = "n".repeat(MAX_TOOL_NAME + 1);
        assert!(RenderedTools::render(&[spec(&long, "{}")], StrictTools::Never).is_err());
    }

    #[test]
    fn rendered_tools_are_reused_only_for_identical_specs() {
        let a = [spec("a", r#"{"type":"object"}"#)];
        let rendered = RenderedTools::render(&a, StrictTools::WhenCompliant).unwrap();
        assert!(rendered.matches(&a, StrictTools::WhenCompliant));
        assert!(!rendered.matches(&a, StrictTools::Never));
        assert!(!rendered.matches(
            &[spec("a", r#"{"type":"object","x":1}"#)],
            StrictTools::WhenCompliant
        ));
        assert!(!rendered.matches(&[], StrictTools::WhenCompliant));
    }

    #[test]
    fn the_prefix_is_stable_when_the_history_grows() {
        // Prompt caching needs an append-only history: the bytes of turn N are a prefix of the
        // bytes of turn N+1 up to the end of `input`.
        let mut messages = vec![
            Message::new(Role::System, "sys"),
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "reply one"),
        ];
        let first = String::from_utf8(
            build_body(
                &config(),
                &LlmRequest {
                    messages: messages.clone(),
                    tools: Vec::new(),
                },
                None,
            )
            .unwrap()
            .to_vec(),
        )
        .unwrap();
        messages.push(Message::new(Role::User, "two"));
        let second = String::from_utf8(
            build_body(
                &config(),
                &LlmRequest {
                    messages,
                    tools: Vec::new(),
                },
                None,
            )
            .unwrap()
            .to_vec(),
        )
        .unwrap();
        let cut = first.find("]").unwrap();
        assert_eq!(first[..cut], second[..cut]);
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: Arc::from(id),
            name: Arc::from("t"),
            arguments: Arc::from("{}"),
        }
    }

    fn rs(n: u32) -> Value {
        json!({"type": "reasoning", "id": format!("rs_{n}"), "summary": [], "encrypted_content": "E"})
    }

    fn fc(n: u32) -> Value {
        json!({"type": "function_call", "id": format!("fc_{n}"), "call_id": format!("call_{n}"),
               "name": "t", "arguments": "{}", "status": "completed"})
    }

    fn turn(text: &str, calls: &[u32], data: &Value) -> Vec<Message> {
        let mut messages = vec![
            Message::new(Role::User, "go"),
            Message::assistant(
                text,
                calls.iter().map(|n| call(&format!("call_{n}"))).collect(),
                Some(Arc::from(data.to_string())),
            ),
        ];
        messages.extend(
            calls
                .iter()
                .map(|n| Message::tool_result(format!("call_{n}"), "ok")),
        );
        messages
    }

    fn input_of(config: &OpenAiConfig, messages: Vec<Message>) -> Vec<Value> {
        let request = LlmRequest {
            messages,
            tools: Vec::new(),
        };
        body(config, &request)["input"].as_array().unwrap().clone()
    }

    #[test]
    fn parallel_calls_keep_each_reasoning_item_in_front_of_its_call() {
        // rs1 fc1 rs2 fc2: replayed in this order, never rs1 rs2 fc1 fc2.
        let data = json!([rs(1), fc(1), rs(2), fc(2)]);
        let input = input_of(&config(), turn("", &[1, 2], &data));
        assert_eq!(input[1..5], [rs(1), fc(1), rs(2), fc(2)], "{input:#?}");
        assert_eq!(input[5]["type"], "function_call_output");
    }

    #[test]
    fn the_played_text_goes_where_the_message_sat_in_the_response() {
        // rs1 message rs2 fc: the text sits between the reasoning items.
        let marker = json!({"type": "message"});
        let data = json!([rs(1), marker, rs(2), fc(1)]);
        let input = input_of(&config(), turn("Let me look.", &[1], &data));
        assert_eq!(input[1], rs(1));
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"], "Let me look.");
        assert_eq!(input[3], rs(2));
        assert_eq!(input[4], fc(1));
        // The marker itself is never sent: one assistant message only.
        assert_eq!(input.iter().filter(|i| i["role"] == "assistant").count(), 1);
        assert_eq!(input.len(), 6);
    }

    #[test]
    fn without_a_marker_the_text_goes_before_the_reasoning_that_leads_into_the_first_call() {
        let data = json!([rs(1), fc(1), rs(2), fc(2)]);
        let input = input_of(&config(), turn("Checking.", &[1, 2], &data));
        assert_eq!(input[1]["content"], "Checking.");
        assert_eq!(input[2..6], [rs(1), fc(1), rs(2), fc(2)]);
        // No calls: after the reasoning.
        let data = json!([rs(1)]);
        let input = input_of(&config(), turn("Hello", &[], &data));
        assert_eq!(input[1], rs(1));
        assert_eq!(input[2]["content"], "Hello");
    }

    #[test]
    fn reasoning_of_an_unplayed_message_or_a_dropped_call_is_dropped_with_it() {
        // The message was cut (no text), only call 2 survives: rs1+message and rs2+fc1 go away
        // with their followers, rs3 stays in front of fc2.
        let data = json!([rs(1), {"type": "message"}, rs(2), fc(1), rs(3), fc(2)]);
        let input = input_of(&config(), turn("", &[2], &data));
        assert_eq!(input[1..3], [rs(3), fc(2)], "{input:#?}");
        assert_eq!(input.len(), 4);
    }

    #[test]
    fn without_reasoning_replay_the_call_is_rebuilt_without_server_ids() {
        let mut off = config();
        off.replay_reasoning = false;
        let data = json!([rs(1), fc(1)]);
        let input = input_of(&off, turn("", &[1], &data));
        assert_eq!(
            input[1],
            json!({"type": "function_call", "call_id": "call_1", "name": "t", "arguments": "{}"})
        );
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
        assert!(input.iter().all(|i| i.get("id").is_none()));
    }

    fn pairing(messages: Vec<Message>) -> Result<Bytes> {
        build_body(
            &config(),
            &LlmRequest {
                messages,
                tools: Vec::new(),
            },
            None,
        )
    }

    #[test]
    fn dangling_calls_and_results_are_refused_before_any_io() {
        let c1 = || Message::assistant("", vec![call("c1")], None);
        // Answered once: fine, also with a late system note in between.
        assert!(
            pairing(vec![
                Message::new(Role::User, "a"),
                c1(),
                Message::new(Role::System, "note"),
                Message::tool_result("c1", "ok"),
            ])
            .is_ok()
        );
        // The tool was cancelled by a barge-in and the user spoke again.
        let unanswered = pairing(vec![
            Message::new(Role::User, "a"),
            c1(),
            Message::new(Role::User, "hello?"),
        ]);
        assert!(
            matches!(&unanswered, Err(Error::Protocol { detail, .. }) if detail.contains("c1"))
        );
        // The call is the last message of the history.
        assert!(pairing(vec![Message::new(Role::User, "a"), c1()]).is_err());
        // The assistant turn was dropped (nothing played, nothing called); its result remains.
        let orphan = pairing(vec![
            Message::new(Role::User, "a"),
            Message::assistant("", Vec::new(), None),
            Message::tool_result("c1", "ok"),
        ]);
        assert!(matches!(&orphan, Err(Error::Protocol { detail, .. }) if detail.contains("c1")));
        // A result before its call, a result for an unknown call, a result answered twice.
        assert!(
            pairing(vec![
                Message::tool_result("c1", "ok"),
                Message::new(Role::User, "a"),
                c1()
            ])
            .is_err()
        );
        assert!(
            pairing(vec![
                Message::new(Role::User, "a"),
                c1(),
                Message::tool_result("other", "ok"),
            ])
            .is_err()
        );
        assert!(
            pairing(vec![
                Message::new(Role::User, "a"),
                c1(),
                Message::tool_result("c1", "ok"),
                Message::tool_result("c1", "again"),
            ])
            .is_err()
        );
        // Parallel calls answered out of order are fine.
        assert!(
            pairing(vec![
                Message::new(Role::User, "a"),
                Message::assistant("", vec![call("c1"), call("c2")], None),
                Message::tool_result("c2", "ok"),
                Message::tool_result("c1", "ok"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn schemas_with_keywords_strict_mode_rejects_are_sent_non_strict() {
        for keyword in [
            r#""dependentRequired":{"a":["b"]}"#,
            r#""then":{"required":["a"]}"#,
            r#""else":{"required":["a"]}"#,
            r#""propertyNames":{"pattern":"^a"}"#,
            r#""unevaluatedProperties":false"#,
            r#""contains":{"type":"string"}"#,
            r#""dependentSchemas":{"a":{}}"#,
            r#""if":{"required":["a"]}"#,
        ] {
            let schema: Value = serde_json::from_str(&format!(
                r#"{{"type":"object","additionalProperties":false,"properties":{{"a":{{"type":"string"}},"b":{{"type":"string"}}}},"required":["a","b"],{keyword}}}"#
            ))
            .unwrap();
            assert!(!complies_with_strict_mode(&schema), "{keyword}");
            // Also when nested.
            let nested = json!({"type":"object","additionalProperties":false,
                "properties":{"x": schema},"required":["x"]});
            assert!(!complies_with_strict_mode(&nested), "{keyword} nested");
        }
    }
}
