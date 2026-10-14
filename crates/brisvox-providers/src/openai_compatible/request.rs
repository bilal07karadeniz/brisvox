//! Builds the Chat Completions request body.
//!
//! Only what is needed is sent (docs/providers/openai-compatible.md, deviation 6): `model`,
//! `messages`, `stream`, `tools` (omitted when empty), the token limit under the configured
//! field name, `temperature`, and, behind [`Quirks`], `stream_options`, `parallel_tool_calls`,
//! `tool_choice`. Several vendors answer 400 to fields they do not know.

use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use super::config::{MaxTokensField, OpenAiCompatibleConfig};
use crate::error::{Error, Result};
use crate::http_llm::{protocol, sanitize};
use crate::llm::{LlmRequest, Message, Role, ToolSpec};

/// The tool array of a request, rendered once and reused while the tools are unchanged.
#[derive(Debug)]
pub(crate) struct RenderedTools {
    specs: Vec<ToolSpec>,
    json: Box<RawValue>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    r#type: &'static str,
    function: WireToolFn<'a>,
}

#[derive(Serialize)]
struct WireToolFn<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a RawValue,
}

impl RenderedTools {
    /// Renders `specs`.
    ///
    /// # Errors
    /// [`Error::Protocol`] if a tool name is empty or a schema is not a JSON object.
    pub(crate) fn render(provider: &'static str, specs: &[ToolSpec]) -> Result<Arc<Self>> {
        let mut schemas: Vec<Box<RawValue>> = Vec::with_capacity(specs.len());
        for spec in specs {
            if spec.name.is_empty() {
                return Err(protocol(provider, "a tool has an empty name"));
            }
            let schema: Box<RawValue> = serde_json::from_str(&spec.parameters_schema)
                .map_err(|_| invalid_schema(provider, &spec.name))?;
            if !schema.get().starts_with('{') {
                return Err(invalid_schema(provider, &spec.name));
            }
            schemas.push(schema);
        }
        let wire: Vec<WireTool<'_>> = specs
            .iter()
            .zip(&schemas)
            .map(|(spec, schema)| WireTool {
                r#type: "function",
                function: WireToolFn {
                    name: &spec.name,
                    description: &spec.description,
                    parameters: schema,
                },
            })
            .collect();
        let json = serde_json::value::to_raw_value(&wire)
            .map_err(|_| protocol(provider, "cannot serialise the tools"))?;
        Ok(Arc::new(Self {
            specs: specs.to_vec(),
            json,
        }))
    }

    /// Whether this rendering is of exactly `specs`.
    pub(crate) fn matches(&self, specs: &[ToolSpec]) -> bool {
        self.specs == specs
    }
}

fn invalid_schema(provider: &'static str, name: &str) -> Error {
    protocol(
        provider,
        format!(
            "the parameter schema of tool {} is not a JSON object",
            sanitize(name)
        ),
    )
}

#[derive(Serialize)]
struct WireCall<'a> {
    id: &'a str,
    r#type: &'static str,
    function: WireCallFn<'a>,
}

#[derive(Serialize)]
struct WireCallFn<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    /// Always present: `null` for an assistant message that only calls tools.
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireCall<'a>>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(flatten)]
    extra: &'a Map<String, Value>,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn wire_message<'a>(provider: &'static str, message: &'a Message) -> Result<WireMessage<'a>> {
    let tool_call_id = match message.role {
        Role::Tool => Some(
            message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| protocol(provider, "a tool message has no tool_call_id"))?,
        ),
        _ => None,
    };
    let tool_calls: Vec<WireCall<'_>> = if message.role == Role::Assistant {
        message
            .tool_calls
            .iter()
            .map(|call| WireCall {
                id: &call.id,
                r#type: "function",
                function: WireCallFn {
                    name: &call.name,
                    // Servers parse the argument string of a replayed call: an empty string is
                    // not JSON, `{}` is the call without arguments.
                    arguments: if call.arguments.trim().is_empty() {
                        "{}"
                    } else {
                        &call.arguments
                    },
                },
            })
            .collect()
    } else {
        Vec::new()
    };
    let content = if message.role == Role::Assistant
        && !tool_calls.is_empty()
        && message.content.is_empty()
    {
        None
    } else {
        Some(&*message.content)
    };
    Ok(WireMessage {
        role: role_name(message.role),
        content,
        tool_call_id,
        tool_calls,
    })
}

/// The JSON body for `request`.
///
/// # Errors
/// [`Error::Protocol`] for a tool message without an id or if serialisation fails.
pub(crate) fn build_body(
    config: &OpenAiCompatibleConfig,
    extra: &Map<String, Value>,
    request: &LlmRequest,
    tools: Option<&RenderedTools>,
) -> Result<Bytes> {
    let provider = config.name;
    let quirks = &config.quirks;
    let messages = request
        .messages
        .iter()
        .map(|m| wire_message(provider, m))
        .collect::<Result<Vec<_>>>()?;
    let has_tools = tools.is_some();
    let (max_tokens, max_completion_tokens) = match quirks.max_tokens_field {
        MaxTokensField::MaxTokens => (config.max_tokens, None),
        MaxTokensField::MaxCompletionTokens => (None, config.max_tokens),
    };
    let body = Body {
        model: &config.model,
        messages,
        stream: true,
        stream_options: quirks.request_stream_usage.then_some(StreamOptions {
            include_usage: true,
        }),
        tools: tools.map(|t| &*t.json),
        tool_choice: quirks.tool_choice.filter(|_| has_tools).map(|c| c.as_str()),
        parallel_tool_calls: quirks.parallel_tool_calls.filter(|_| has_tools),
        max_tokens,
        max_completion_tokens,
        temperature: config.temperature,
        extra,
    };
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|_| protocol(provider, "cannot serialise the request"))
}
