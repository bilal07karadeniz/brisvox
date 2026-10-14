//! Builds the body of `POST .../models/{model}:streamGenerateContent?alt=sse` from an
//! [`LlmRequest`].
//!
//! Stateless replay: the whole history is sent each turn, built from the engine's own messages,
//! so after an interruption the model sees only what was played. Layout for implicit caching
//! (`docs/providers/gemini.md`): `systemInstruction` and `tools` come first and are byte-stable
//! (the tool array is rendered once and reused), the history is append-only.
//!
//! # Roles
//! Gemini has the roles `user` and `model` only. Leading system messages become
//! `systemInstruction`; a later system message (a note injected mid-call) becomes a `user` text
//! part prefixed with `System note:`; tool results become `functionResponse` parts of a `user`
//! content. Adjacent contents with the same role are merged into one (whether the API accepts
//! two consecutive contents of a role is UNVERIFIED, and dropping an empty assistant turn leaves
//! two user turns in a row). A history that starts with a model turn (the agent's greeting)
//! gets a placeholder user content in front ([`GeminiConfig::leading_user_placeholder`]); whether
//! the API accepts a leading model content is UNVERIFIED.
//!
//! # Function calls and thought signatures
//! An assistant turn is replayed from its `provider_data` (the parts of the response as the
//! adapter recorded them, signatures included) so the `thoughtSignature` on the first
//! `functionCall` goes back exactly as received. The text parts are replaced by the text that
//! was actually played when it differs from the recorded text (their signatures are dropped:
//! the docs call them optional); calls the history no longer contains are dropped with their
//! signature. A call without recorded data is rebuilt from the [`ToolCall`], and when no part of
//! the turn carries a signature the first call gets
//! [`GeminiConfig::fallback_signature`](super::GeminiConfig::fallback_signature). Calls whose id
//! the adapter made up (the API sent none) are replayed without an `id`.

use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::PROVIDER;
use super::config::{GeminiConfig, SchemaMode};
use super::events::{SYNTHETIC_ID_PREFIX, sanitize, synthetic_id};
use super::schema;
use crate::error::{Error, Result};
use crate::llm::{LlmRequest, Message, Role, ToolCall, ToolSpec};

/// Longest tool name the adapter sends (the API allows 128; the agent's `ToolRegistry` allows 64).
const MAX_TOOL_NAME: usize = 64;

/// Longest call arguments the adapter re-parses to restore free-form objects (see
/// [`RenderedTools::restore_args`]); parsing and serialising this much costs well under a
/// millisecond.
const MAX_RESTORE_BYTES: usize = 32 * 1024;

/// Prefix of a system message that arrives after the conversation started.
const SYSTEM_NOTE: &str = "System note: ";

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
    mode: SchemaMode,
    json: Box<RawValue>,
    /// For each tool whose converted schema sends a free-form object as a string: its name and
    /// converted `parameters` (see [`RenderedTools::restore_args`]).
    object_strings: Vec<(String, Value)>,
}

impl RenderedTools {
    /// Whether this rendering is still valid for `specs` under `mode`.
    pub(crate) fn matches(&self, specs: &[ToolSpec], mode: SchemaMode) -> bool {
        self.mode == mode && self.specs == specs
    }

    /// Renders `specs` as the `tools` array: one entry with all `functionDeclarations`.
    pub(crate) fn render(specs: &[ToolSpec], mode: SchemaMode) -> Result<Arc<Self>> {
        let mut declarations = Vec::with_capacity(specs.len());
        let mut object_strings = Vec::new();
        for (at, spec) in specs.iter().enumerate() {
            let name: &str = &spec.name;
            if name.is_empty()
                || name.len() > MAX_TOOL_NAME
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                return Err(protocol(format!(
                    "tool name {name:?} is not 1-{MAX_TOOL_NAME} characters of [A-Za-z0-9_-]"
                )));
            }
            if specs.iter().take(at).any(|other| other.name == spec.name) {
                return Err(protocol(format!("tool {name} is declared twice")));
            }
            let schema: Value = serde_json::from_str(&spec.parameters_schema).map_err(|e| {
                protocol(format!("tool {name}: parameters schema is not JSON: {e}"))
            })?;
            let mut declaration = Map::new();
            declaration.insert("name".to_owned(), Value::String(name.to_owned()));
            declaration.insert(
                "description".to_owned(),
                Value::String(spec.description.to_string()),
            );
            match mode {
                SchemaMode::OpenApiSubset => {
                    let converted = schema::convert_parameters(&schema)
                        .map_err(|why| protocol(format!("tool {name}: {why}")))?;
                    for what in &converted.degraded {
                        // Once per render (the rendering is cached), not per request.
                        tracing::warn!(
                            tool = name,
                            "gemini tool schema degraded: {what} (SchemaMode::JsonSchema sends                              the schema unchanged)"
                        );
                    }
                    if let Some(parameters) = converted.parameters {
                        if converted.has_object_strings {
                            object_strings.push((name.to_owned(), parameters.clone()));
                        }
                        declaration.insert("parameters".to_owned(), parameters);
                    }
                }
                SchemaMode::JsonSchema => {
                    declaration.insert("parametersJsonSchema".to_owned(), schema);
                }
            }
            declarations.push(Value::Object(declaration));
        }
        let json =
            serde_json::value::to_raw_value(&json!([{ "functionDeclarations": declarations }]))
                .map_err(|e| protocol(format!("cannot render tools: {e}")))?;
        Ok(Arc::new(Self {
            specs: specs.to_vec(),
            mode,
            json,
            object_strings,
        }))
    }

    /// The arguments of a call to `name` with every free-form object the model sent as a
    /// JSON-encoded string (the declared type, see the `schema` module) turned back into an
    /// object, so they satisfy the tool's own schema. `None` when nothing needed restoring:
    /// the tool has no such parameter, `arguments` is not a JSON object, nothing was a string
    /// holding an object, or `arguments` is longer than [`MAX_RESTORE_BYTES`] (re-parsing it
    /// would be too much work for a tokio worker; it is forwarded as received).
    pub(crate) fn restore_args(&self, name: &str, arguments: &str) -> Option<String> {
        let (_, schema) = self.object_strings.iter().find(|(n, _)| n == name)?;
        if arguments.len() > MAX_RESTORE_BYTES {
            tracing::warn!(
                tool = name,
                "gemini call arguments too large to restore free-form objects; forwarded as received"
            );
            return None;
        }
        let mut value: Value = serde_json::from_str(arguments).ok()?;
        schema::restore_objects(schema, &mut value)
            .then(|| serde_json::to_string(&value).ok())
            .flatten()
    }
}

/// A part of a request content.
#[derive(Serialize)]
#[serde(untagged)]
enum Part<'a> {
    Text { text: &'a str },
    Json(Value),
}

fn has_signature(part: &Value) -> bool {
    part.get("thoughtSignature").is_some_and(Value::is_string)
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'static str,
    parts: Vec<Part<'a>>,
}

/// Appends `parts` as a content of `role`, merged into the previous content of the same role.
fn push_content<'a>(contents: &mut Vec<Content<'a>>, role: &'static str, parts: Vec<Part<'a>>) {
    if parts.is_empty() {
        return;
    }
    match contents.last_mut() {
        Some(last) if last.role == role => last.parts.extend(parts),
        _ => contents.push(Content { role, parts }),
    }
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: [Part<'a>; 1],
}

#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a RawValue>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<Value>,
    contents: Vec<Content<'a>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<Value>,
}

/// Splits the leading system messages (the `systemInstruction`) from the rest.
fn split_instructions(messages: &[Message]) -> (Option<String>, &[Message]) {
    let leading = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let (system, rest) = messages.split_at(leading);
    let text = system
        .iter()
        .map(|m| &*m.content)
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    ((!text.is_empty()).then_some(text), rest)
}

/// `generationConfig`, or `None` when nothing is configured.
fn generation_config(config: &GeminiConfig) -> Option<Value> {
    let mut map = Map::new();
    if let Some(max) = config.max_output_tokens {
        map.insert("maxOutputTokens".to_owned(), json!(max));
    }
    if let Some(temperature) = config.temperature.filter(|t| t.is_finite()) {
        // Through the shortest decimal form, so 0.7 is sent as 0.7 and not as 0.699999988...
        let shown = temperature.to_string().parse::<f64>().unwrap_or(0.0);
        map.insert("temperature".to_owned(), json!(shown));
    }
    let mut thinking = Map::new();
    if let Some(level) = config.thinking_level {
        thinking.insert("thinkingLevel".to_owned(), json!(level.as_str()));
    }
    if let Some(budget) = config.thinking_budget {
        thinking.insert("thinkingBudget".to_owned(), json!(budget));
    }
    if !thinking.is_empty() {
        map.insert("thinkingConfig".to_owned(), Value::Object(thinking));
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

/// `toolConfig`, or `None` (no mode configured, or no tools in the request).
fn tool_config(config: &GeminiConfig, has_tools: bool) -> Option<Value> {
    let mode = config.function_calling_mode.filter(|_| has_tools)?;
    let mut calling = Map::new();
    calling.insert("mode".to_owned(), json!(mode.as_str()));
    let may_restrict = matches!(
        mode,
        super::FunctionCallingMode::Any | super::FunctionCallingMode::Validated
    );
    if may_restrict && !config.allowed_function_names.is_empty() {
        calling.insert(
            "allowedFunctionNames".to_owned(),
            json!(config.allowed_function_names),
        );
    }
    Some(json!({ "functionCallingConfig": calling }))
}

/// A call of the history that no tool message has answered yet.
struct OpenCall<'a> {
    id: &'a str,
    name: &'a str,
    /// Position among all calls of the history (results are sent in call order).
    seq: usize,
}

/// The `args` object of a call being rebuilt from its [`ToolCall`].
fn args_of(call: &ToolCall) -> Result<Value> {
    if call.arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(&call.arguments) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(protocol(format!(
            "arguments of tool call {} are not a JSON object",
            sanitize(&call.id)
        ))),
    }
}

/// A `functionCall` part rebuilt from the engine's record (no signature).
fn rebuilt_call(call: &ToolCall) -> Result<Value> {
    let mut function_call = Map::new();
    if !call.id.starts_with(SYNTHETIC_ID_PREFIX) {
        function_call.insert("id".to_owned(), json!(&*call.id));
    }
    function_call.insert("name".to_owned(), json!(&*call.name));
    function_call.insert("args".to_owned(), args_of(call)?);
    Ok(json!({ "functionCall": function_call }))
}

/// The recorded parts of a response (`Message::provider_data`), if the data is ours.
fn recorded_parts(provider_data: &str) -> Option<Vec<Value>> {
    let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(provider_data) else {
        return None;
    };
    let ours = parts.iter().all(|p| {
        p.get("text").is_some_and(Value::is_string)
            || p.get("functionCall").is_some_and(Value::is_object)
    });
    (ours && !parts.is_empty()).then_some(parts)
}

/// The parts of the replayed `model` content for one assistant message.
fn assistant_parts(message: &Message, config: &GeminiConfig) -> Result<Vec<Value>> {
    let recorded = config
        .replay_thought_signatures
        .then_some(message.provider_data.as_deref())
        .flatten()
        .and_then(recorded_parts);

    let mut played: Option<Value> =
        (!message.content.is_empty()).then(|| json!({ "text": &*message.content }));
    let mut parts: Vec<Value> = Vec::new();
    let mut used = vec![false; message.tool_calls.len()];
    if let Some(recorded) = recorded {
        let recorded_text: String = recorded
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect();
        let text_unchanged = recorded_text == *message.content;
        if text_unchanged {
            // The recorded text parts say it all, signatures included.
            played = None;
        }
        let mut ordinal = 0usize;
        for part in recorded {
            if part.get("text").is_some() {
                if text_unchanged {
                    parts.push(part);
                } else if let Some(text) = played.take() {
                    // The played text takes the place of the first recorded text part.
                    parts.push(text);
                }
            } else if let Some(function_call) = part.get("functionCall") {
                let id = function_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| synthetic_id(ordinal), str::to_owned);
                ordinal += 1;
                let slot = message
                    .tool_calls
                    .iter()
                    .enumerate()
                    .find(|(at, call)| *call.id == *id && used.get(*at) == Some(&false));
                if let Some((at, _)) = slot {
                    if let Some(flag) = used.get_mut(at) {
                        *flag = true;
                    }
                    // Played text that sat after no text part goes in front of the calls.
                    if let Some(text) = played.take() {
                        parts.push(text);
                    }
                    parts.push(part);
                }
            }
        }
    }
    // Text that had no recorded part to replace (no provider data) leads the turn.
    if let Some(text) = played.take() {
        parts.insert(0, text);
    }
    for (call, was_used) in message.tool_calls.iter().zip(&used) {
        if !was_used {
            parts.push(rebuilt_call(call)?);
        }
    }
    if let Some(signature) = config.fallback_signature.as_deref()
        && !parts.iter().any(has_signature)
        && let Some(first) = parts.iter_mut().find(|p| p.get("functionCall").is_some())
        && let Some(map) = first.as_object_mut()
    {
        map.insert("thoughtSignature".to_owned(), json!(signature));
    }
    Ok(parts)
}

/// The JSON body for `request`.
///
/// # Errors
/// [`Error::Protocol`] for a request the API would refuse after a full round trip: a tool
/// result without its call (or a call without its result), arguments that are not a JSON object,
/// or no conversation content at all.
pub(crate) fn build_body(
    config: &GeminiConfig,
    request: &LlmRequest,
    tools: Option<&RenderedTools>,
) -> Result<Bytes> {
    let (instructions, rest) = split_instructions(&request.messages);
    let mut contents: Vec<Content<'_>> = Vec::with_capacity(rest.len() + 1);
    let mut open: Vec<OpenCall<'_>> = Vec::new();
    let mut seq = 0usize;
    // Consecutive tool results, flushed as one user content in call order.
    let mut results: Vec<(usize, Value)> = Vec::new();
    let flush = |results: &mut Vec<(usize, Value)>, contents: &mut Vec<Content<'_>>| {
        results.sort_by_key(|(at, _)| *at);
        let parts = results.drain(..).map(|(_, v)| Part::Json(v)).collect();
        push_content(contents, "user", parts);
    };
    for message in rest {
        if message.role != Role::Tool {
            flush(&mut results, &mut contents);
        }
        match message.role {
            Role::System => push_content(
                &mut contents,
                "user",
                vec![Part::Json(
                    json!({ "text": format!("{SYSTEM_NOTE}{}", message.content) }),
                )],
            ),
            Role::User => {
                // An empty text part is refused by the API; an empty turn leaves no trace.
                if !message.content.is_empty() {
                    push_content(
                        &mut contents,
                        "user",
                        vec![Part::Text {
                            text: &message.content,
                        }],
                    );
                }
            }
            Role::Assistant => {
                let parts = assistant_parts(message, config)?;
                push_content(
                    &mut contents,
                    "model",
                    parts.into_iter().map(Part::Json).collect(),
                );
                for call in &message.tool_calls {
                    open.push(OpenCall {
                        id: &call.id,
                        name: &call.name,
                        seq,
                    });
                    seq += 1;
                }
            }
            Role::Tool => {
                let Some(call_id) = message.tool_call_id.as_deref() else {
                    return Err(protocol("a tool message has no tool_call_id"));
                };
                // The API answers a dangling call or result with HTTP 400 after a full round
                // trip: refuse it here, where the cause is visible.
                let Some(position) = open.iter().position(|c| c.id == call_id) else {
                    return Err(protocol(format!(
                        "tool result for call {} has no earlier unanswered call in the history \
                         (dropped assistant turn, or answered twice)",
                        sanitize(call_id)
                    )));
                };
                let call = open.remove(position);
                let mut response = Map::new();
                if !call.id.starts_with(SYNTHETIC_ID_PREFIX) {
                    response.insert("id".to_owned(), json!(call.id));
                }
                response.insert("name".to_owned(), json!(call.name));
                response.insert("response".to_owned(), response_object(&message.content));
                results.push((call.seq, json!({ "functionResponse": response })));
            }
        }
    }
    flush(&mut results, &mut contents);
    if let Some(call) = open.first() {
        return Err(protocol(format!(
            "tool call {} has no tool result in the history (cancelled tool?): the caller must \
             answer or drop it before the next request",
            sanitize(call.id)
        )));
    }
    if contents.is_empty() {
        return Err(protocol("the request has no conversation content"));
    }
    // A voice agent normally opens with its own greeting, so the history may start with a model
    // turn. Whether the API accepts that is UNVERIFIED (some Gemini endpoints answer "First
    // content should be with role user"), hence the fail-safe placeholder user turn.
    if let Some(placeholder) = config.leading_user_placeholder.as_deref()
        && contents.first().is_some_and(|c| c.role == "model")
    {
        contents.insert(
            0,
            Content {
                role: "user",
                parts: vec![Part::Text { text: placeholder }],
            },
        );
    }
    let tools = tools.filter(|t| !t.specs.is_empty());
    let body = Body {
        system_instruction: instructions.as_deref().map(|text| SystemInstruction {
            parts: [Part::Text { text }],
        }),
        tools: tools.map(|t| &*t.json),
        tool_config: tool_config(config, tools.is_some()),
        contents,
        generation_config: generation_config(config),
    };
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|e| protocol(format!("cannot serialise the request: {e}")))
}

/// The `response` of a `functionResponse`: it must be a JSON object, so a result that is a JSON
/// object is sent as is and anything else (plain text, a number, a list) is wrapped as
/// `{"result": "<text>"}`.
fn response_object(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ "result": content }),
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::Secret;

    use super::*;
    use crate::llm::ToolCall;

    fn config() -> GeminiConfig {
        GeminiConfig::new(Secret::new("test-key".to_owned()))
    }

    fn body(config: &GeminiConfig, request: &LlmRequest) -> Value {
        let tools = (!request.tools.is_empty())
            .then(|| RenderedTools::render(&request.tools, config.schema_mode).unwrap());
        serde_json::from_slice(&build_body(config, request, tools.as_deref()).unwrap()).unwrap()
    }

    fn spec(name: &str, schema: &str) -> ToolSpec {
        ToolSpec {
            name: Arc::from(name),
            description: Arc::from("does it"),
            parameters_schema: Arc::from(schema),
        }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: Arc::from(id),
            name: Arc::from(name),
            arguments: Arc::from(args),
        }
    }

    fn request(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            messages,
            tools: Vec::new(),
        }
    }

    #[test]
    fn a_plain_conversation_uses_system_instruction_and_user_model_roles() {
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::System, "Be terse."),
                Message::new(Role::System, "Speak Turkish."),
                Message::new(Role::User, "Merhaba"),
                Message::new(Role::Assistant, "Selam!"),
                Message::new(Role::User, "Nasilsin?"),
            ]),
        );
        assert_eq!(
            out,
            json!({
                "systemInstruction": {"parts": [{"text": "Be terse.\n\nSpeak Turkish."}]},
                "contents": [
                    {"role": "user", "parts": [{"text": "Merhaba"}]},
                    {"role": "model", "parts": [{"text": "Selam!"}]},
                    {"role": "user", "parts": [{"text": "Nasilsin?"}]}
                ]
            })
        );
    }

    #[test]
    fn generation_and_tool_config_appear_only_when_configured() {
        let plain = body(&config(), &request(vec![Message::new(Role::User, "hi")]));
        assert!(plain.get("generationConfig").is_none());
        assert!(plain.get("toolConfig").is_none());
        assert!(plain.get("tools").is_none());

        let mut cfg = config().with_thinking_level(super::super::ThinkingLevel::Low);
        cfg.max_output_tokens = Some(256);
        cfg.temperature = Some(0.7);
        cfg.function_calling_mode = Some(super::super::FunctionCallingMode::Any);
        cfg.allowed_function_names = vec!["get_weather".to_owned()];
        let mut req = request(vec![Message::new(Role::User, "hi")]);
        req.tools = vec![spec(
            "get_weather",
            r#"{"type":"object","properties":{"city":{"type":"string"}}}"#,
        )];
        let out = body(&cfg, &req);
        assert_eq!(
            out["generationConfig"],
            json!({"maxOutputTokens": 256, "temperature": 0.7,
                   "thinkingConfig": {"thinkingLevel": "low"}})
        );
        assert_eq!(
            out["toolConfig"],
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}})
        );
        // The mode is meaningless without tools.
        let no_tools = body(&cfg, &request(vec![Message::new(Role::User, "hi")]));
        assert!(no_tools.get("toolConfig").is_none());
    }

    #[test]
    fn tools_are_function_declarations_with_a_converted_schema() {
        let mut req = request(vec![Message::new(Role::User, "hi")]);
        req.tools = vec![
            spec(
                "get_weather",
                r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}"#,
            ),
            spec("ping", r#"{"type":"object","properties":{}}"#),
        ];
        let out = body(&config(), &req);
        assert_eq!(
            out["tools"],
            json!([{"functionDeclarations": [
                {"name": "get_weather", "description": "does it",
                 "parameters": {"type": "OBJECT", "properties": {"city": {"type": "STRING"}}, "required": ["city"]}},
                {"name": "ping", "description": "does it"}
            ]}])
        );
        let mut cfg = config();
        cfg.schema_mode = SchemaMode::JsonSchema;
        let out = body(&cfg, &req);
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn bad_tool_definitions_are_rejected_before_any_request() {
        for name in ["", "has space", "dot.ted", "co:lon", "ünï", &"x".repeat(65)] {
            let error = RenderedTools::render(
                &[spec(name, r#"{"type":"object"}"#)],
                SchemaMode::OpenApiSubset,
            )
            .unwrap_err();
            assert!(matches!(error, Error::Protocol { .. }), "{name}");
        }
        assert!(
            RenderedTools::render(&[spec("a", "not json")], SchemaMode::OpenApiSubset).is_err()
        );
        assert!(
            RenderedTools::render(
                &[
                    spec("a", r#"{"type":"object"}"#),
                    spec("a", r#"{"type":"object"}"#)
                ],
                SchemaMode::OpenApiSubset
            )
            .is_err()
        );
        let error = RenderedTools::render(
            &[spec("a", r#"{"type":"array"}"#)],
            SchemaMode::OpenApiSubset,
        )
        .unwrap_err();
        assert!(error.to_string().contains("tool a"), "{error}");
    }

    #[test]
    fn tool_names_may_use_the_whole_alphabet_of_the_tool_registry() {
        // `ToolRegistry` accepts [A-Za-z0-9_-]{1,64}; a name it accepts must not fail the request.
        for name in ["get-weather", "-", "a_b-C9", &"x".repeat(64)] {
            RenderedTools::render(
                &[spec(name, r#"{"type":"object"}"#)],
                SchemaMode::OpenApiSubset,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let mut req = request(vec![Message::new(Role::User, "hi")]);
        req.tools = vec![spec("get-weather", r#"{"type":"object"}"#)];
        let out = body(&config(), &req);
        assert_eq!(
            out["tools"][0]["functionDeclarations"][0]["name"],
            json!("get-weather")
        );
    }

    #[test]
    fn rendered_tools_are_reused_only_for_identical_specs() {
        let specs = vec![spec("a", r#"{"type":"object"}"#)];
        let rendered = RenderedTools::render(&specs, SchemaMode::OpenApiSubset).unwrap();
        assert!(rendered.matches(&specs, SchemaMode::OpenApiSubset));
        assert!(!rendered.matches(&specs, SchemaMode::JsonSchema));
        assert!(!rendered.matches(
            &[spec("b", r#"{"type":"object"}"#)],
            SchemaMode::OpenApiSubset
        ));
    }

    /// The recorded parts of a signed parallel-call turn.
    fn recorded() -> Arc<str> {
        Arc::from(
            json!([
                {"text": "Checking. "},
                {"functionCall": {"id": "c1", "name": "get_weather", "args": {"city": "Paris"}},
                 "thoughtSignature": "SIG1"},
                {"functionCall": {"id": "c2", "name": "get_time", "args": {}}}
            ])
            .to_string(),
        )
    }

    fn two_calls() -> Vec<ToolCall> {
        vec![
            call("c1", "get_weather", r#"{"city":"Paris"}"#),
            call("c2", "get_time", "{}"),
        ]
    }

    #[test]
    fn a_call_turn_replays_the_recorded_parts_and_orders_the_results_by_call() {
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "weather and time?"),
                Message::assistant("Checking. ", two_calls(), Some(recorded())),
                // Answered out of order on purpose.
                Message::tool_result("c2", "12:00"),
                Message::tool_result("c1", r#"{"temperature_c":18}"#),
            ]),
        );
        assert_eq!(
            out["contents"],
            json!([
                {"role": "user", "parts": [{"text": "weather and time?"}]},
                {"role": "model", "parts": [
                    {"text": "Checking. "},
                    {"functionCall": {"id": "c1", "name": "get_weather", "args": {"city": "Paris"}},
                     "thoughtSignature": "SIG1"},
                    {"functionCall": {"id": "c2", "name": "get_time", "args": {}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "c1", "name": "get_weather", "response": {"temperature_c": 18}}},
                    {"functionResponse": {"id": "c2", "name": "get_time", "response": {"result": "12:00"}}}
                ]}
            ])
        );
    }

    #[test]
    fn an_interrupted_turn_keeps_the_calls_and_their_signature_but_replays_only_played_text() {
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("Chec", two_calls(), Some(recorded())),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
            ]),
        );
        let model = &out["contents"][1]["parts"];
        assert_eq!(model[0], json!({"text": "Chec"}));
        assert_eq!(model[1]["thoughtSignature"], json!("SIG1"));
        assert_eq!(model.as_array().unwrap().len(), 3);
        // Nothing played: no text part, the calls stay.
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("", two_calls(), Some(recorded())),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
            ]),
        );
        let model = out["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(model.len(), 2);
        assert!(model.iter().all(|p| p.get("functionCall").is_some()));
    }

    #[test]
    fn a_signature_on_text_is_dropped_with_the_text_it_signed() {
        let data: Arc<str> =
            Arc::from(json!([{"text": "Hello there", "thoughtSignature": "TS"}]).to_string());
        let full = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "hi"),
                Message::assistant("Hello there", Vec::new(), Some(Arc::clone(&data))),
            ]),
        );
        assert_eq!(
            full["contents"][1]["parts"][0]["thoughtSignature"],
            json!("TS")
        );
        let cut = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "hi"),
                Message::assistant("Hello", Vec::new(), Some(data)),
            ]),
        );
        assert_eq!(cut["contents"][1]["parts"], json!([{"text": "Hello"}]));
    }

    #[test]
    fn a_call_without_provider_data_is_rebuilt_with_the_fallback_signature() {
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("", two_calls(), None),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
            ]),
        );
        let parts = &out["contents"][1]["parts"];
        assert_eq!(
            parts[0],
            json!({"functionCall": {"id": "c1", "name": "get_weather", "args": {"city": "Paris"}},
                   "thoughtSignature": "skip_thought_signature_validator"})
        );
        assert!(
            parts[1].get("thoughtSignature").is_none(),
            "only the first call"
        );
        let mut cfg = config();
        cfg.fallback_signature = None;
        let out = body(
            &cfg,
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("", two_calls(), None),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
            ]),
        );
        assert!(
            out["contents"][1]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );
    }

    #[test]
    fn disabling_signature_replay_ignores_provider_data() {
        let mut cfg = config();
        cfg.replay_thought_signatures = false;
        cfg.fallback_signature = None;
        let out = body(
            &cfg,
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("Checking. ", two_calls(), Some(recorded())),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
            ]),
        );
        let text = out["contents"][1].to_string();
        assert!(!text.contains("SIG1"), "{text}");
        assert!(text.contains("get_time"));
    }

    #[test]
    fn made_up_call_ids_are_replayed_without_an_id() {
        let data: Arc<str> = Arc::from(
            json!([
                {"functionCall": {"name": "get_time", "args": {}}, "thoughtSignature": "S"}
            ])
            .to_string(),
        );
        let synthetic = synthetic_id(0);
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "time?"),
                Message::assistant("", vec![call(&synthetic, "get_time", "{}")], Some(data)),
                Message::tool_result(synthetic.as_str(), "noon"),
            ]),
        );
        assert_eq!(
            out["contents"][1]["parts"][0],
            json!({"functionCall": {"name": "get_time", "args": {}}, "thoughtSignature": "S"})
        );
        assert_eq!(
            out["contents"][2]["parts"][0],
            json!({"functionResponse": {"name": "get_time", "response": {"result": "noon"}}})
        );
        // Rebuilt without provider data: the made-up id is still not sent.
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "time?"),
                Message::assistant("", vec![call(&synthetic, "get_time", "{}")], None),
                Message::tool_result(synthetic.as_str(), "noon"),
            ]),
        );
        assert!(
            out["contents"][1]["parts"][0]["functionCall"]
                .get("id")
                .is_none()
        );
    }

    #[test]
    fn foreign_or_broken_provider_data_is_ignored() {
        for data in [
            "not json",
            r#"{"a":1}"#,
            r#"[{"type":"reasoning","id":"rs_1"}]"#,
            "[]",
        ] {
            let out = body(
                &config(),
                &request(vec![
                    Message::new(Role::User, "hi"),
                    Message::assistant("Hello", Vec::new(), Some(Arc::from(data))),
                ]),
            );
            assert_eq!(
                out["contents"][1],
                json!({"role": "model", "parts": [{"text": "Hello"}]}),
                "{data}"
            );
        }
    }

    #[test]
    fn adjacent_same_role_contents_merge_and_empty_turns_leave_no_trace() {
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "one"),
                // An assistant turn truncated to nothing, then the caller speaks again.
                Message::new(Role::Assistant, ""),
                Message::new(Role::User, "two"),
                Message::new(Role::User, ""),
                Message::new(Role::System, "late note"),
            ]),
        );
        assert_eq!(
            out["contents"],
            json!([{"role": "user", "parts": [
                {"text": "one"}, {"text": "two"}, {"text": "System note: late note"}
            ]}])
        );
    }

    #[test]
    fn tool_results_are_json_objects_or_wrapped_text() {
        assert_eq!(response_object(r#"{"a":1}"#), json!({"a": 1}));
        assert_eq!(response_object("[1,2]"), json!({"result": "[1,2]"}));
        assert_eq!(response_object("42"), json!({"result": "42"}));
        assert_eq!(response_object("sunny"), json!({"result": "sunny"}));
        assert_eq!(response_object(""), json!({"result": ""}));
    }

    fn pairing(messages: Vec<Message>) -> Result<Bytes> {
        build_body(&config(), &request(messages), None)
    }

    #[test]
    fn dangling_calls_and_results_are_refused_before_any_io() {
        let user = || Message::new(Role::User, "go");
        let asks = || Message::assistant("", vec![call("c1", "f", "{}")], None);
        // A call nobody answered.
        assert!(pairing(vec![user(), asks()]).is_err());
        // A result without a call.
        assert!(pairing(vec![user(), Message::tool_result("c9", "x")]).is_err());
        // Answered twice.
        assert!(
            pairing(vec![
                user(),
                asks(),
                Message::tool_result("c1", "x"),
                Message::tool_result("c1", "y"),
            ])
            .is_err()
        );
        // A tool message without an id.
        assert!(pairing(vec![user(), Message::new(Role::Tool, "x")]).is_err());
        // Arguments that are not an object.
        assert!(
            pairing(vec![
                user(),
                Message::assistant("", vec![call("c1", "f", "[1]")], None),
                Message::tool_result("c1", "x"),
            ])
            .is_err()
        );
        // Nothing to say.
        assert!(pairing(vec![Message::new(Role::System, "only a prompt")]).is_err());
        // A well-formed turn passes.
        assert!(pairing(vec![user(), asks(), Message::tool_result("c1", "x")]).is_ok());
    }

    #[test]
    fn the_prefix_is_stable_when_the_history_grows() {
        let mut req = request(vec![
            Message::new(Role::System, "Be terse."),
            Message::new(Role::User, "one"),
        ]);
        req.tools = vec![spec(
            "a",
            r#"{"type":"object","properties":{"x":{"type":"string"}}}"#,
        )];
        let tools = RenderedTools::render(&req.tools, SchemaMode::OpenApiSubset).unwrap();
        let first = build_body(&config(), &req, Some(&tools)).unwrap();
        req.messages.push(Message::new(Role::Assistant, "ok"));
        req.messages.push(Message::new(Role::User, "two"));
        let second = build_body(&config(), &req, Some(&tools)).unwrap();
        let cut = std::str::from_utf8(&first).unwrap().rfind("]}").unwrap();
        let head = &std::str::from_utf8(&first).unwrap()[..cut];
        assert!(
            std::str::from_utf8(&second).unwrap().starts_with(head),
            "systemInstruction, tools and the first contents must be byte-identical"
        );
    }

    #[test]
    fn calls_the_history_no_longer_holds_are_dropped_from_the_replay() {
        // The engine dropped (cancelled) c2: the message holds c1 only, the recorded parts both.
        // Replaying c2 would send two functionCalls against one functionResponse (HTTP 400).
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "weather and time?"),
                Message::assistant(
                    "Checking. ",
                    vec![call("c1", "get_weather", r#"{"city":"Paris"}"#)],
                    Some(recorded()),
                ),
                Message::tool_result("c1", "sunny"),
            ]),
        );
        let model = out["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(
            *model,
            vec![
                json!({"text": "Checking. "}),
                json!({"functionCall": {"id": "c1", "name": "get_weather", "args": {"city": "Paris"}},
                       "thoughtSignature": "SIG1"}),
            ]
        );
        let calls = |content: &Value| {
            content["parts"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p.get("functionCall").is_some() || p.get("functionResponse").is_some())
                .count()
        };
        assert_eq!(calls(&out["contents"][1]), 1);
        assert_eq!(calls(&out["contents"][2]), 1);
        // The signature of a dropped first call goes with it; the fallback covers the rest.
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("", vec![call("c2", "get_time", "{}")], Some(recorded())),
                Message::tool_result("c2", "12:00"),
            ]),
        );
        let model = out["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(model.len(), 1);
        assert_eq!(model[0]["functionCall"]["id"], json!("c2"));
        assert_eq!(
            model[0]["thoughtSignature"],
            json!(crate::gemini::SKIP_SIGNATURE_VALIDATOR)
        );
    }

    #[test]
    fn a_call_the_record_does_not_hold_is_rebuilt_next_to_the_recorded_ones() {
        // The inverse: the message holds c3, which the recorded parts never saw.
        let mut calls = two_calls();
        calls.push(call("c3", "get_news", r#"{"topic":"ai"}"#));
        let out = body(
            &config(),
            &request(vec![
                Message::new(Role::User, "go"),
                Message::assistant("Checking. ", calls, Some(recorded())),
                Message::tool_result("c1", "{}"),
                Message::tool_result("c2", "{}"),
                Message::tool_result("c3", "{}"),
            ]),
        );
        let model = out["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(model.len(), 4, "{model:?}");
        assert_eq!(model[1]["thoughtSignature"], json!("SIG1"));
        assert_eq!(
            model[3],
            json!({"functionCall": {"id": "c3", "name": "get_news", "args": {"topic": "ai"}}})
        );
    }

    #[test]
    fn turkish_text_survives_the_request_and_an_interruption() {
        // Two- and three-byte characters (and a dotted capital I) in every place text goes.
        let said = "Merhaba, size nasıl yardımcı olabilirim? Şu an müşteri hizmetlerindeyim.";
        let played = "Merhaba, size nasıl yardımcı olabilirim? Şu an müş";
        assert!(!said.is_ascii() && said.starts_with(played));
        let data: Arc<str> = Arc::from(
            json!([
                {"text": said},
                {"functionCall": {"id": "c1", "name": "randevu_al", "args": {"gün": "Çarşamba", "kişi": "İlker Öztürk"}},
                 "thoughtSignature": "SIG"}
            ])
            .to_string(),
        );
        let calls = vec![call(
            "c1",
            "randevu_al",
            r#"{"gün":"Çarşamba","kişi":"İlker Öztürk"}"#,
        )];
        let bytes = build_body(
            &config(),
            &request(vec![
                Message::new(Role::System, "Türkçe konuş."),
                Message::new(
                    Role::User,
                    "Çarşamba için randevu istiyorum, şöyle olsun: ğüşıöç",
                ),
                Message::assistant(played, calls, Some(data)),
                Message::tool_result("c1", r#"{"durum":"onaylandı"}"#),
            ]),
            None,
        )
        .unwrap();
        // Raw UTF-8 on the wire, not \u escapes or lossy replacements.
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(wire.contains("ğüşıöç") && wire.contains("müş\""), "{wire}");
        assert!(!wire.contains('\u{fffd}'));
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            out["systemInstruction"]["parts"][0]["text"],
            json!("Türkçe konuş.")
        );
        let model = &out["contents"][1]["parts"];
        assert_eq!(model[0], json!({"text": played}), "only the played text");
        assert_eq!(
            model[1]["functionCall"]["args"],
            json!({"gün": "Çarşamba", "kişi": "İlker Öztürk"})
        );
        assert_eq!(
            out["contents"][2]["parts"][0]["functionResponse"]["response"],
            json!({"durum": "onaylandı"})
        );
    }
}
