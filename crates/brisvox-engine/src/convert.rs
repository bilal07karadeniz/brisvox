//! Pure converters between the agent's provider-neutral types and the provider adapters' types
//! (`docs/PIPELINE.md` section 4).
//!
//! The agent keeps the conversation as a [`Context`] (stable internal tool-call ids, arguments as
//! JSON values, `provider_data` as a JSON value). The adapters speak
//! [`brisvox_providers::Message`] (the provider's own call ids, arguments and `provider_data` as
//! JSON text). The functions here translate between them and nothing else: no IO, no state.
//!
//! # Rules
//! - **Tool calls.** An assistant message's calls carry the provider's id (`provider_id`) when the
//!   call had one, otherwise the internal id; the [`Role::Tool`] message that answers a call
//!   carries the same id as `tool_call_id`, found through the call's internal id. A tool result
//!   whose call is not in the context keeps its internal id (the context was invalid anyway).
//! - **`provider_data`** is a JSON value in the context and JSON text for the adapters; the text
//!   is the value's compact serialization.
//! - **Tool specs** are name-ordered, so a request is byte-identical across rounds (prompt-cache
//!   friendly) whatever order the tools were registered in. The schema is the compact JSON text.
//! - **Events.** [`delta_of`] maps text and tool-call fragments of an [`LlmEvent`] to the
//!   assembler's [`Delta`]; [`provider_data_delta`] maps the `provider_data` of `Done`.
//!
//! The reverse direction ([`from_provider_messages`], [`from_provider_tool`]) exists for tests,
//! snapshots and tooling: the provider form has no internal ids, so they are minted again
//! (`t<epoch>.<seq>`) and the provider's id is kept as `provider_id`. A tool spec's kind, timeout
//! and filler delay are not part of the provider form and come from the caller.
//!
//! Round trips preserve [`Context::validate`]: converting a valid context to the provider form and
//! back gives a valid context with the same texts, calls and arguments.

use std::collections::HashMap;
use std::sync::Arc;

use brisvox_agent::assemble::Delta;
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::tools::{ToolKind, ToolSpec};
use brisvox_core::TurnEpoch;
use brisvox_providers as providers;
use brisvox_providers::{LlmEvent, Role};
use serde_json::Value;

/// Why a provider-form value could not be converted to the agent's form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConvertError {
    /// The arguments of a call, the schema of a tool or a `provider_data` is not valid JSON.
    #[error("{what} is not valid JSON: {detail}")]
    InvalidJson {
        /// What was being parsed.
        what: &'static str,
        /// The parser's message (never the text itself).
        detail: String,
    },
    /// The arguments of a call are valid JSON but not an object.
    #[error("the arguments of call {call:?} are not a JSON object")]
    ArgumentsNotAnObject {
        /// The provider's id of the call.
        call: String,
    },
    /// A tool message answers a call that no earlier assistant message made.
    #[error("tool message answers unknown call {call:?}")]
    UnknownToolCall {
        /// The `tool_call_id` of the message.
        call: String,
    },
    /// A tool message has no `tool_call_id`.
    #[error("tool message without a call id")]
    MissingToolCallId,
}

fn invalid_json(what: &'static str, error: &serde_json::Error) -> ConvertError {
    ConvertError::InvalidJson {
        what,
        detail: error.to_string(),
    }
}

/// The JSON text of a value. Serializing a [`Value`] cannot fail; the fallback keeps the function
/// total.
fn json_text(value: &Value) -> Arc<str> {
    Arc::from(serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()))
}

/// The provider-side form of `context`, in order.
///
/// Costs one allocation per call and per `provider_data` (argument text); texts are shared.
pub fn to_provider_messages(context: &Context) -> Vec<providers::Message> {
    let mut provider_ids: HashMap<&ToolCallId, Arc<str>> = HashMap::new();
    let mut out = Vec::with_capacity(context.len());
    for message in context.messages() {
        out.push(match message {
            Message::System { text } => providers::Message::new(Role::System, Arc::clone(text)),
            Message::User { text } => providers::Message::new(Role::User, Arc::clone(text)),
            Message::Assistant {
                text,
                tool_calls,
                provider_data,
            } => {
                let calls = tool_calls
                    .iter()
                    .map(|call| {
                        let id: Arc<str> = call
                            .provider_id
                            .clone()
                            .unwrap_or_else(|| Arc::from(call.id.as_str()));
                        provider_ids.insert(&call.id, Arc::clone(&id));
                        providers::ToolCall {
                            id,
                            name: Arc::clone(&call.name),
                            arguments: json_text(&call.arguments),
                        }
                    })
                    .collect();
                providers::Message::assistant(
                    Arc::clone(text),
                    calls,
                    provider_data.as_ref().map(json_text),
                )
            }
            Message::Tool {
                call_id, content, ..
            } => {
                let id = provider_ids
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| Arc::from(call_id.as_str()));
                providers::Message::tool_result(id, Arc::clone(content))
            }
        });
    }
    out
}

/// The agent-side form of provider `messages`; tool-call ids are minted for `epoch`
/// (`t<epoch>.0`, `t<epoch>.1`, ...) and the provider's ids are kept as `provider_id`.
///
/// # Errors
/// [`ConvertError`] for arguments or `provider_data` that are not JSON (or arguments that are not
/// an object), a tool message with no call id, or one that answers a call no assistant message
/// made.
pub fn from_provider_messages(
    messages: &[providers::Message],
    epoch: TurnEpoch,
) -> Result<Context, ConvertError> {
    let mut context = Context::new();
    let mut internal: HashMap<Arc<str>, ToolCallId> = HashMap::new();
    let mut seq = 0u32;
    for message in messages {
        match message.role {
            Role::System => context.push(Message::System {
                text: Arc::clone(&message.content),
            }),
            Role::User => context.push(Message::User {
                text: Arc::clone(&message.content),
            }),
            Role::Assistant => {
                let mut calls = Vec::with_capacity(message.tool_calls.len());
                for call in &message.tool_calls {
                    let arguments: Value = serde_json::from_str(&call.arguments)
                        .map_err(|e| invalid_json("tool call arguments", &e))?;
                    if !arguments.is_object() {
                        return Err(ConvertError::ArgumentsNotAnObject {
                            call: call.id.to_string(),
                        });
                    }
                    let id = ToolCallId::new(epoch, seq);
                    seq = seq.saturating_add(1);
                    internal.insert(Arc::clone(&call.id), id.clone());
                    calls.push(ToolCall {
                        id,
                        provider_id: Some(Arc::clone(&call.id)),
                        name: Arc::clone(&call.name),
                        arguments,
                        provider_data: None,
                    });
                }
                let provider_data = message
                    .provider_data
                    .as_deref()
                    .map(serde_json::from_str::<Value>)
                    .transpose()
                    .map_err(|e| invalid_json("provider_data", &e))?;
                context.push(Message::Assistant {
                    text: Arc::clone(&message.content),
                    tool_calls: calls,
                    provider_data,
                });
            }
            Role::Tool => {
                let provider_id = message
                    .tool_call_id
                    .as_ref()
                    .ok_or(ConvertError::MissingToolCallId)?;
                let call_id = internal.get(provider_id).cloned().ok_or_else(|| {
                    ConvertError::UnknownToolCall {
                        call: provider_id.to_string(),
                    }
                })?;
                context.push(Message::tool_result(call_id, Arc::clone(&message.content)));
            }
        }
    }
    Ok(context)
}

/// The provider-side form of one tool spec: the schema as compact JSON text.
pub fn to_provider_tool(spec: &ToolSpec) -> providers::ToolSpec {
    providers::ToolSpec {
        name: Arc::clone(&spec.name),
        description: Arc::clone(&spec.description),
        parameters_schema: json_text(&spec.parameters),
    }
}

/// The provider-side form of `specs`, ordered by tool name whatever the input order (stable for
/// prompt caching).
pub fn to_provider_tools<'a>(
    specs: impl IntoIterator<Item = &'a ToolSpec>,
) -> Vec<providers::ToolSpec> {
    let mut tools: Vec<providers::ToolSpec> = specs.into_iter().map(to_provider_tool).collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// The agent-side form of a provider tool spec. The provider form carries neither the kind nor
/// the timeout nor the filler delay: `kind` is the caller's, the other two are the defaults of
/// [`ToolSpec::new`].
///
/// # Errors
/// [`ConvertError::InvalidJson`] when the schema is not JSON.
pub fn from_provider_tool(
    spec: &providers::ToolSpec,
    kind: ToolKind,
) -> Result<ToolSpec, ConvertError> {
    let parameters: Value = serde_json::from_str(&spec.parameters_schema)
        .map_err(|e| invalid_json("tool parameter schema", &e))?;
    Ok(ToolSpec::new(
        Arc::clone(&spec.name),
        Arc::clone(&spec.description),
        parameters,
        kind,
    ))
}

/// The assembler delta of a streamed text or tool-call fragment; `None` for `Done` and `Error`
/// (which end the stream; `Done`'s `provider_data` goes through [`provider_data_delta`]).
///
/// Per-call `provider_data` does not exist in the provider event (only the response's), so the
/// delta carries none.
pub fn delta_of(event: &LlmEvent) -> Option<Delta> {
    match event {
        LlmEvent::TextDelta(text) => Some(Delta::Text(Arc::clone(text))),
        LlmEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => Some(Delta::ToolCall {
            index: *index,
            id: id.clone(),
            name: name.clone(),
            arguments: Arc::clone(arguments),
            provider_data: None,
        }),
        LlmEvent::Done { .. } | LlmEvent::Error(_) => None,
    }
}

/// The assembler delta of the `provider_data` of a `Done` event (JSON text in the event, a JSON
/// value in the context); `None` when there is none.
///
/// # Errors
/// [`ConvertError::InvalidJson`] when the text is not JSON.
pub fn provider_data_delta(provider_data: Option<&str>) -> Result<Option<Delta>, ConvertError> {
    provider_data
        .map(|text| {
            serde_json::from_str::<Value>(text)
                .map(Delta::ProviderData)
                .map_err(|e| invalid_json("provider_data", &e))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use brisvox_agent::tools::{DEFAULT_FILLER_AFTER, DEFAULT_TOOL_TIMEOUT};
    use serde_json::json;

    use super::*;

    const T1: TurnEpoch = TurnEpoch::new(1);

    fn call(id: ToolCallId, provider_id: Option<&str>, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id,
            provider_id: provider_id.map(Arc::from),
            name: Arc::from(name),
            arguments: args,
            provider_data: None,
        }
    }

    /// A turn with two calls (one with a provider id, one without), results in the reverse
    /// order, provider data on the assistant message and a closing answer.
    fn tool_turn() -> Context {
        let mut c = Context::with_system("You are a helpful agent.");
        c.push(Message::user("Weather in Ankara and Izmir?"));
        let ids = c.next_tool_call_ids(T1, 2);
        c.push(Message::Assistant {
            text: Arc::from("Let me check."),
            tool_calls: vec![
                call(
                    ids[0].clone(),
                    Some("call_abc"),
                    "weather",
                    json!({"city": "Ankara"}),
                ),
                call(ids[1].clone(), None, "weather", json!({"city": "Izmir"})),
            ],
            provider_data: Some(json!({"reasoning": [{"id": "r1"}]})),
        });
        c.push(Message::tool_result(ids[1].clone(), r#"{"temp":24}"#));
        c.push(Message::tool_error(ids[0].clone(), "timeout"));
        c.push(Message::assistant("Ankara timed out; Izmir is 24 degrees."));
        c
    }

    #[test]
    fn a_context_converts_message_by_message_with_provider_call_ids() {
        let context = tool_turn();
        context.validate().unwrap();
        let messages = to_provider_messages(&context);
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                Role::System,
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Tool,
                Role::Assistant
            ]
        );
        let assistant = &messages[2];
        assert_eq!(&*assistant.content, "Let me check.");
        assert_eq!(assistant.tool_calls.len(), 2);
        assert_eq!(&*assistant.tool_calls[0].id, "call_abc");
        assert_eq!(&*assistant.tool_calls[0].name, "weather");
        assert_eq!(&*assistant.tool_calls[0].arguments, r#"{"city":"Ankara"}"#);
        // A call without a provider id is addressed by its internal id.
        assert_eq!(&*assistant.tool_calls[1].id, "t1.1");
        assert_eq!(
            assistant.provider_data.as_deref(),
            Some(r#"{"reasoning":[{"id":"r1"}]}"#)
        );
        // Results carry the id of the call they answer, whatever order they come in.
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("t1.1"));
        assert_eq!(&*messages[3].content, r#"{"temp":24}"#);
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("call_abc"));
        assert_eq!(
            &*messages[5].content,
            "Ankara timed out; Izmir is 24 degrees."
        );
        assert!(messages[5].tool_calls.is_empty() && messages[5].provider_data.is_none());
    }

    #[test]
    fn a_round_trip_preserves_the_context_and_its_validity() {
        let context = tool_turn();
        let back = from_provider_messages(&to_provider_messages(&context), T1).unwrap();
        back.validate().unwrap();
        assert_eq!(back.len(), context.len());
        // Same texts, names, arguments and provider data; the ids are minted again but equal
        // here because the original used the same scheme; the provider ids are kept.
        for (a, b) in context.messages().iter().zip(back.messages()) {
            match (a, b) {
                (
                    Message::Assistant {
                        text: ta,
                        tool_calls: ca,
                        provider_data: pa,
                    },
                    Message::Assistant {
                        text: tb,
                        tool_calls: cb,
                        provider_data: pb,
                    },
                ) => {
                    assert_eq!(ta, tb);
                    assert_eq!(pa, pb);
                    assert_eq!(ca.len(), cb.len());
                    for (x, y) in ca.iter().zip(cb) {
                        assert_eq!((&x.name, &x.arguments), (&y.name, &y.arguments));
                    }
                }
                (Message::Tool { content: ca, .. }, Message::Tool { content: cb, .. }) => {
                    assert_eq!(ca, cb);
                }
                _ => assert_eq!(a.text(), b.text()),
            }
        }
        assert_eq!(back.messages()[2].text(), "Let me check.");
        let Message::Assistant { tool_calls, .. } = &back.messages()[2] else {
            panic!("assistant expected");
        };
        assert_eq!(tool_calls[0].provider_id.as_deref(), Some("call_abc"));
        // Converting the round-tripped context again gives the same provider form.
        assert_eq!(
            to_provider_messages(&back),
            to_provider_messages(&context),
            "provider form is stable"
        );
    }

    #[test]
    fn plain_and_empty_contexts_round_trip() {
        let empty = Context::new();
        assert!(to_provider_messages(&empty).is_empty());
        assert_eq!(from_provider_messages(&[], T1).unwrap(), empty);
        let mut chat = Context::with_system("p");
        chat.push(Message::user("hi"));
        chat.push(Message::assistant("hello"));
        let back = from_provider_messages(&to_provider_messages(&chat), T1).unwrap();
        assert_eq!(back, chat);
        back.validate().unwrap();
    }

    #[test]
    fn the_reverse_direction_rejects_what_it_cannot_represent() {
        let bad_args = providers::Message::assistant(
            "",
            vec![providers::ToolCall {
                id: Arc::from("c"),
                name: Arc::from("t"),
                arguments: Arc::from("{not json"),
            }],
            None,
        );
        assert!(matches!(
            from_provider_messages(&[bad_args], T1),
            Err(ConvertError::InvalidJson { .. })
        ));
        let array_args = providers::Message::assistant(
            "",
            vec![providers::ToolCall {
                id: Arc::from("c"),
                name: Arc::from("t"),
                arguments: Arc::from("[1]"),
            }],
            None,
        );
        assert!(matches!(
            from_provider_messages(&[array_args], T1),
            Err(ConvertError::ArgumentsNotAnObject { .. })
        ));
        let orphan = providers::Message::tool_result("nope", "x");
        assert_eq!(
            from_provider_messages(&[orphan], T1),
            Err(ConvertError::UnknownToolCall {
                call: "nope".to_owned()
            })
        );
        let mut no_id = providers::Message::new(Role::Tool, "x");
        no_id.tool_call_id = None;
        assert_eq!(
            from_provider_messages(&[no_id], T1),
            Err(ConvertError::MissingToolCallId)
        );
        let bad_data = providers::Message::assistant("a", Vec::new(), Some(Arc::from("{x")));
        assert!(matches!(
            from_provider_messages(&[bad_data], T1),
            Err(ConvertError::InvalidJson {
                what: "provider_data",
                ..
            })
        ));
    }

    fn spec(name: &str, kind: ToolKind) -> ToolSpec {
        ToolSpec::new(
            name,
            format!("does {name}"),
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            kind,
        )
    }

    #[test]
    fn tool_specs_are_name_ordered_whatever_the_input_order() {
        let specs = [
            spec("zeta", ToolKind::ReadOnly),
            spec("alpha", ToolKind::SideEffect),
            spec("mid", ToolKind::ReadOnly),
        ];
        let names = |tools: &[providers::ToolSpec]| -> Vec<String> {
            tools.iter().map(|t| t.name.to_string()).collect()
        };
        let forward = to_provider_tools(&specs);
        assert_eq!(names(&forward), ["alpha", "mid", "zeta"]);
        let reversed = to_provider_tools(specs.iter().rev());
        assert_eq!(
            forward, reversed,
            "byte-identical regardless of input order"
        );
        assert_eq!(
            &*forward[0].parameters_schema,
            r#"{"properties":{"q":{"type":"string"}},"type":"object"}"#,
            "compact schema text"
        );
    }

    #[test]
    fn a_tool_spec_round_trips_except_what_the_provider_form_cannot_hold() {
        let original = spec("lookup", ToolKind::SideEffect);
        let provider = to_provider_tool(&original);
        let back = from_provider_tool(&provider, ToolKind::SideEffect).unwrap();
        assert_eq!(back, original, "defaults equal here");
        assert_eq!(back.timeout, DEFAULT_TOOL_TIMEOUT);
        assert_eq!(back.filler_after, Some(DEFAULT_FILLER_AFTER));
        let broken = providers::ToolSpec {
            parameters_schema: Arc::from("nope"),
            ..provider
        };
        assert!(from_provider_tool(&broken, ToolKind::ReadOnly).is_err());
    }

    #[test]
    fn events_map_to_deltas() {
        let text = LlmEvent::TextDelta(Arc::from("hi "));
        assert_eq!(delta_of(&text), Some(Delta::Text(Arc::from("hi "))));
        let fragment = LlmEvent::ToolCallDelta {
            index: 2,
            id: Some(Arc::from("c1")),
            name: None,
            arguments: Arc::from(r#"{"a":"#),
        };
        assert_eq!(
            delta_of(&fragment),
            Some(Delta::ToolCall {
                index: 2,
                id: Some(Arc::from("c1")),
                name: None,
                arguments: Arc::from(r#"{"a":"#),
                provider_data: None,
            })
        );
        let done = LlmEvent::Done {
            finish: providers::FinishReason::Stop,
            usage: None,
            provider_data: None,
        };
        assert_eq!(delta_of(&done), None);
        assert_eq!(provider_data_delta(None), Ok(None));
        assert_eq!(
            provider_data_delta(Some(r#"{"sig":"x"}"#)),
            Ok(Some(Delta::ProviderData(json!({"sig": "x"}))))
        );
        assert!(provider_data_delta(Some("{")).is_err());
    }

    #[test]
    fn done_provider_data_lands_in_the_assistant_message_and_comes_back_verbatim() {
        use brisvox_agent::assemble::ResponseAssembler;
        let mut assembler = ResponseAssembler::new();
        assembler.push(Delta::Text(Arc::from("ok"))).unwrap();
        let data = r#"{"reasoning":[{"id":"r9","summary":[]}]}"#;
        assembler
            .push(provider_data_delta(Some(data)).unwrap().unwrap())
            .unwrap();
        let mut context = Context::with_system("p");
        context.push(Message::user("q"));
        let response = assembler.finish(T1, &context);
        context.push(response.assistant_message().unwrap());
        context.validate().unwrap();
        let messages = to_provider_messages(&context);
        assert_eq!(messages[2].provider_data.as_deref(), Some(data));
    }
}
