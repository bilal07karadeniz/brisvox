//! Maps Responses API stream events (the JSON `data` of each SSE event) to [`LlmEvent`]s.
//!
//! Dispatch is on the JSON `type` field only; unknown types are ignored (the union has 59
//! members and grows). An event is parsed in two steps: first only its `type` (and whether it
//! carries an `error`), then just the fields of that kind. An event the mapper ignores is
//! therefore never parsed beyond its `type`, however odd its other fields are, and the fields of
//! error events and of the terminal response object are read leniently (a numeric `code`, a
//! string `error`, an unreadable `usage` do not turn a good answer into a failure). Several
//! fields are UNVERIFIED in `docs/providers/openai.md`; the fields the mapping cannot do without
//! are checked and a violation is [`Error::Protocol`]. Free-text error messages are never copied
//! into errors or logs: they can echo prompt content (and, for a 401, a fragment of the key).
//! Only the machine-readable error `code` is used.

use std::borrow::Cow;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use super::PROVIDER;
use crate::error::Error;
use crate::llm::{FinishReason, LlmEvent, Usage};

/// Most tool calls one response may open; more is a protocol error (bounds memory).
const MAX_CALLS: usize = 128;

/// Most replayable output items (reasoning, function calls, the message marker) one response may
/// carry; more is a protocol error (bounds memory).
const MAX_REPLAY_ITEMS: usize = 256;

/// Most bytes of replayable output items one response may carry (16 MiB).
const MAX_REPLAY_BYTES: usize = 16 * 1024 * 1024;

/// Most bytes of arguments one tool call may stream (1 MiB). A call that exceeds it is a protocol
/// error: the adapter keeps a copy of every call's arguments, so without a cap a misbehaving
/// endpoint could grow it for the whole `max_stream_duration`.
pub(crate) const MAX_CALL_ARGS_BYTES: usize = 1024 * 1024;

/// Most bytes of arguments all tool calls of one response may stream together (4 MiB).
pub(crate) const MAX_TOTAL_ARGS_BYTES: usize = 4 * 1024 * 1024;

/// The placeholder `provider_data` holds where the response's assistant message sat among its
/// reasoning and function-call items. It is not a real input item: the request builder uses it
/// to put the played text at the same position and never sends it.
pub(crate) const MESSAGE_MARKER: &str = r#"{"type":"message"}"#;

/// What the pump does after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep reading.
    Continue,
    /// A terminal event was mapped (`Done` is in the output): stop reading.
    Finished,
}

/// A JSON string, borrowed when it has no escapes.
#[derive(Deserialize)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

/// `raw` as text, `None` when it is absent or not a JSON string.
fn text(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    serde_json::from_str::<Text<'_>>(raw?.get())
        .ok()
        .map(|t| t.0)
}

/// The first look at an event: its `type` and whether it has an `error` member.
#[derive(Deserialize)]
struct Head<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<&'a RawValue>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

/// `response.output_text.delta`, `response.refusal.delta`.
#[derive(Deserialize)]
struct TextEvent<'a> {
    #[serde(default, borrow)]
    delta: Option<Cow<'a, str>>,
}

/// `response.function_call_arguments.delta`.
#[derive(Deserialize)]
struct ArgsDeltaEvent<'a> {
    #[serde(default, borrow)]
    delta: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    item_id: Option<Cow<'a, str>>,
    #[serde(default)]
    output_index: Option<u32>,
}

/// `response.function_call_arguments.done`.
#[derive(Deserialize)]
struct ArgsDoneEvent<'a> {
    #[serde(default, borrow)]
    arguments: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    item_id: Option<Cow<'a, str>>,
    #[serde(default)]
    output_index: Option<u32>,
}

/// `response.output_item.added` / `.done`.
#[derive(Deserialize)]
struct ItemEvent<'a> {
    #[serde(default, borrow)]
    item: Option<&'a RawValue>,
    #[serde(default)]
    output_index: Option<u32>,
}

/// `response.completed` / `.incomplete`.
#[derive(Deserialize)]
struct ResponseEvent<'a> {
    #[serde(default, borrow)]
    response: Option<&'a RawValue>,
}

/// `response.failed`.
type FailedEvent<'a> = ResponseEvent<'a>;

/// An `error` event: `code` at the top level, or inside an `error` object.
#[derive(Deserialize)]
struct ErrorEvent<'a> {
    #[serde(default, borrow)]
    code: Option<&'a RawValue>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

/// `error` objects (`response.error`, a nested stream `error`): only the code matters.
#[derive(Deserialize)]
struct ErrorBody<'a> {
    #[serde(default, borrow)]
    code: Option<&'a RawValue>,
}

/// The code of an error value, whatever its JSON type: a string, or a number (an HTTP-like
/// status). Anything else is no code.
fn code_of(raw: Option<&RawValue>) -> Option<String> {
    match serde_json::from_str::<Value>(raw?.get()).ok()? {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The code of an `error` member that is an object (`{"code": ...}`); a string or anything else
/// has none.
fn code_in_object(raw: Option<&RawValue>) -> Option<String> {
    let body: ErrorBody<'_> = serde_json::from_str(raw?.get()).ok()?;
    code_of(body.code)
}

/// The parts of an output item the adapter needs to place it.
#[derive(Deserialize)]
struct ItemKind<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<&'a RawValue>,
}

/// A `function_call` output item.
#[derive(Deserialize)]
struct Item<'a> {
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    call_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    arguments: Option<Cow<'a, str>>,
}

/// The `Response` object of a terminal event, one level deep: each member is parsed on its own
/// so that an unreadable one costs only itself.
#[derive(Deserialize)]
struct ResponseParts<'a> {
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
    #[serde(default, borrow)]
    incomplete_details: Option<&'a RawValue>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
    #[serde(default, borrow)]
    output: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct IncompleteDetails<'a> {
    #[serde(default, borrow)]
    reason: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct UsageObject {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

pub(crate) use crate::http_llm::sanitize;

/// Maps an OpenAI error `code` (from `response.failed` or an `error` event) to an [`Error`]:
/// rate limits are `Rejected { 429 }`, server-side failures `Rejected { 500 / 503 }`, rejected
/// prompts `Rejected { 400 }`, a numeric HTTP-like code (400-599) is that status; anything else
/// is a protocol error naming the (sanitised) code.
pub(crate) fn error_for_code(code: Option<&str>) -> Error {
    let status = match code {
        Some("rate_limit_exceeded" | "slow_down" | "insufficient_quota") => Some(429),
        Some("server_error") => Some(500),
        Some("server_is_overloaded" | "service_unavailable") => Some(503),
        Some(
            "invalid_prompt"
            | "invalid_request_error"
            | "context_length_exceeded"
            | "data_residency_mismatch",
        ) => Some(400),
        Some(other) => other
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|s| (400..600).contains(s)),
        None => None,
    };
    match (status, code) {
        (Some(status), _) => Error::Rejected {
            provider: PROVIDER,
            status,
        },
        (None, Some(code)) => protocol(format!("the response failed with code {}", sanitize(code))),
        (None, None) => protocol("the response failed without an error code"),
    }
}

/// A function call being streamed.
#[derive(Debug)]
struct CallSlot {
    item_id: Option<String>,
    output_index: Option<u32>,
    /// Position among the function calls of the response (the `index` of `ToolCallDelta`).
    index: u32,
    /// Arguments forwarded so far.
    args: String,
}

/// Per-response state of the mapping.
#[derive(Debug, Default)]
pub(crate) struct Mapper {
    calls: Vec<CallSlot>,
    /// Raw `reasoning` and `function_call` output items, plus one [`MESSAGE_MARKER`] where the
    /// assistant message sat, in arrival order: the response's `provider_data`.
    replay: Vec<Box<RawValue>>,
    /// Bytes held by `replay`.
    replay_bytes: usize,
    /// Bytes of arguments held by `calls` together.
    args_bytes: usize,
    /// Whether the marker is in `replay`.
    marked: bool,
}

impl Mapper {
    /// A mapper for a new response.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Maps one event's `data` into `out` (appended; events of this call only).
    ///
    /// Empty (or whitespace-only) data is a heartbeat and is ignored.
    ///
    /// # Errors
    /// [`Error::Protocol`] for data that is not JSON or violates the protocol,
    /// [`Error::Rejected`] for a failed response or error event, [`Error::Disconnected`] for
    /// a `[DONE]` sentinel before a terminal event.
    pub(crate) fn handle(&mut self, data: &str, out: &mut Vec<LlmEvent>) -> Result<Flow, Error> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Flow::Continue);
        }
        if data == "[DONE]" {
            return Err(Error::Disconnected { provider: PROVIDER });
        }
        let head: Head<'_> = serde_json::from_str(data)
            .map_err(|e| protocol(format!("an event is not valid JSON: {e}")))?;
        let kind = text(head.kind);
        let kind = match kind.as_deref() {
            Some(kind) => kind,
            // The SDK treats any object with an `error` key as an error, even without a type.
            None if head.error.is_some() => "error",
            None => return Ok(Flow::Continue),
        };
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                // A refusal is spoken like any answer: the caller should hear it.
                let event: TextEvent<'_> = parse(data, kind)?;
                if let Some(delta) = event.delta.as_deref().filter(|d| !d.is_empty()) {
                    out.push(LlmEvent::TextDelta(Arc::from(delta)));
                }
            }
            "response.output_item.added" => self.item_added(&parse(data, kind)?, out)?,
            "response.function_call_arguments.delta" => {
                let event: ArgsDeltaEvent<'_> = parse(data, kind)?;
                if let Some(delta) = event.delta.as_deref().filter(|d| !d.is_empty()) {
                    let slot = self
                        .slot(event.item_id.as_deref(), event.output_index)
                        .ok_or_else(|| protocol("arguments for a call that was never announced"))?;
                    self.push_arguments(slot, delta, out)?;
                }
            }
            "response.function_call_arguments.done" => {
                let event: ArgsDoneEvent<'_> = parse(data, kind)?;
                if let (Some(full), Some(slot)) = (
                    event.arguments.as_deref(),
                    self.slot(event.item_id.as_deref(), event.output_index),
                ) {
                    self.reconcile_arguments(slot, full, out)?;
                }
            }
            "response.output_item.done" => self.item_done(&parse(data, kind)?, out)?,
            "response.completed" | "response.incomplete" => {
                let event: ResponseEvent<'_> = parse(data, kind)?;
                self.finish(kind == "response.incomplete", &event, out);
                return Ok(Flow::Finished);
            }
            "response.failed" => {
                let event: FailedEvent<'_> = parse(data, kind)?;
                let code = event
                    .response
                    .and_then(|r| serde_json::from_str::<ResponseParts<'_>>(r.get()).ok())
                    .and_then(|r| code_in_object(r.error));
                return Err(error_for_code(code.as_deref()));
            }
            "error" => {
                // Lenient: a numeric code, a string `error`, a missing code are all handled.
                let event: Option<ErrorEvent<'_>> = serde_json::from_str(data).ok();
                let code = event.and_then(|e| code_of(e.code).or_else(|| code_in_object(e.error)));
                return Err(error_for_code(code.as_deref()));
            }
            other => tracing::trace!(kind = other, "ignored openai stream event"),
        }
        Ok(Flow::Continue)
    }

    /// The slot of the call `item_id` / `output_index` refers to. A call announced without an
    /// item id is found by its output index even when the delta carries an item id.
    fn slot(&self, item_id: Option<&str>, output_index: Option<u32>) -> Option<usize> {
        let by_index = |c: &CallSlot| output_index.is_some() && c.output_index == output_index;
        self.calls.iter().position(|c| match item_id {
            Some(id) => c.item_id.as_deref() == Some(id) || (c.item_id.is_none() && by_index(c)),
            None => by_index(c),
        })
    }

    /// Forwards a fragment of call `slot`.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the call, or all calls together, would exceed the argument caps
    /// ([`MAX_CALL_ARGS_BYTES`], [`MAX_TOTAL_ARGS_BYTES`]); nothing is appended then.
    fn push_arguments(
        &mut self,
        slot: usize,
        fragment: &str,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let held = self.calls.get(slot).map_or(0, |c| c.args.len());
        self.charge_arguments(held, fragment.len())?;
        if let Some(call) = self.calls.get_mut(slot) {
            call.args.push_str(fragment);
            out.push(LlmEvent::ToolCallDelta {
                index: call.index,
                id: None,
                name: None,
                arguments: Arc::from(fragment),
            });
        }
        Ok(())
    }

    /// Accounts `add` more argument bytes for a call that already holds `held`.
    fn charge_arguments(&mut self, held: usize, add: usize) -> Result<(), Error> {
        let total = self.args_bytes.saturating_add(add);
        if held.saturating_add(add) > MAX_CALL_ARGS_BYTES || total > MAX_TOTAL_ARGS_BYTES {
            return Err(protocol(format!(
                "the response streams more than {MAX_CALL_ARGS_BYTES} bytes of arguments for \
                 one tool call or {MAX_TOTAL_ARGS_BYTES} bytes for all of them"
            )));
        }
        self.args_bytes = total;
        Ok(())
    }

    /// The full arguments of a call arrived (`...arguments.done`, or the finished item): forward
    /// what the deltas missed, so a server that skips deltas still delivers the arguments.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the argument caps would be exceeded.
    fn reconcile_arguments(
        &mut self,
        slot: usize,
        full: &str,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let missing = self
            .calls
            .get(slot)
            .and_then(|c| full.strip_prefix(c.args.as_str()))
            .filter(|rest| !rest.is_empty())
            .map(str::to_owned);
        match missing {
            Some(rest) => self.push_arguments(slot, &rest, out),
            None => Ok(()),
        }
    }

    /// Opens call `item` (a `function_call` item) and returns its slot.
    fn open_call(
        &mut self,
        item: &Item<'_>,
        output_index: Option<u32>,
        out: &mut Vec<LlmEvent>,
    ) -> Result<usize, Error> {
        if self.calls.len() >= MAX_CALLS {
            return Err(protocol(format!(
                "the response opens more than {MAX_CALLS} tool calls"
            )));
        }
        let (Some(call_id), Some(name)) = (item.call_id.as_deref(), item.name.as_deref()) else {
            return Err(protocol("a function_call item has no call_id or name"));
        };
        let index = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        let args = item.arguments.as_deref().unwrap_or_default();
        self.charge_arguments(0, args.len())?;
        self.calls.push(CallSlot {
            item_id: item.id.as_deref().map(str::to_owned),
            output_index,
            index,
            args: args.to_owned(),
        });
        out.push(LlmEvent::ToolCallDelta {
            index,
            id: Some(Arc::from(call_id)),
            name: Some(Arc::from(name)),
            arguments: Arc::from(args),
        });
        Ok(self.calls.len() - 1)
    }

    /// Records a replayable item, within the count and size caps.
    fn keep(&mut self, raw: Box<RawValue>) -> Result<(), Error> {
        self.replay_bytes = self.replay_bytes.saturating_add(raw.get().len());
        if self.replay.len() >= MAX_REPLAY_ITEMS || self.replay_bytes > MAX_REPLAY_BYTES {
            return Err(protocol(format!(
                "the response carries more than {MAX_REPLAY_ITEMS} replayable items or \
                 {MAX_REPLAY_BYTES} bytes of them"
            )));
        }
        self.replay.push(raw);
        Ok(())
    }

    /// The kind of an output item.
    fn kind_of(raw: &RawValue) -> Result<Option<String>, Error> {
        let head: ItemKind<'_> = serde_json::from_str(raw.get())
            .map_err(|e| protocol(format!("an output item is not valid JSON: {e}")))?;
        Ok(text(head.kind).map(Cow::into_owned))
    }

    /// Parses a `function_call` output item.
    fn call_item(raw: &RawValue) -> Result<Item<'_>, Error> {
        serde_json::from_str(raw.get())
            .map_err(|e| protocol(format!("a function_call item is not valid: {e}")))
    }

    fn item_added(&mut self, event: &ItemEvent<'_>, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let Some(raw) = event.item else {
            return Ok(());
        };
        if Self::kind_of(raw)?.as_deref() == Some("function_call") {
            self.open_call(&Self::call_item(raw)?, event.output_index, out)?;
        }
        Ok(())
    }

    fn item_done(&mut self, event: &ItemEvent<'_>, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let Some(raw) = event.item else {
            return Ok(());
        };
        match Self::kind_of(raw)?.as_deref() {
            Some("reasoning") => self.keep(raw.to_owned())?,
            Some("message") if !self.marked => {
                let marker = RawValue::from_string(MESSAGE_MARKER.to_owned())
                    .map_err(|e| protocol(format!("cannot build the message marker: {e}")))?;
                self.keep(marker)?;
                self.marked = true;
            }
            Some("function_call") => {
                let item = Self::call_item(raw)?;
                match self.slot(item.id.as_deref(), event.output_index) {
                    Some(slot) => {
                        if let Some(full) = item.arguments.as_deref() {
                            self.reconcile_arguments(slot, full, out)?;
                        }
                    }
                    // Never announced (a server that sends only the finished item): open it now
                    // with its complete arguments.
                    None => {
                        let full = item.arguments.as_deref().unwrap_or_default().to_owned();
                        let announced = Item {
                            arguments: None,
                            ..item
                        };
                        let slot = self.open_call(&announced, event.output_index, out)?;
                        self.reconcile_arguments(slot, &full, out)?;
                    }
                }
                self.keep(raw.to_owned())?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Maps a `response.completed` / `response.incomplete` event to `Done`. The response object
    /// is read leniently: whatever is unreadable (usage, details, output) is skipped, because
    /// the answer has been delivered by now and a bad bookkeeping field must not discard it.
    fn finish(&mut self, incomplete: bool, event: &ResponseEvent<'_>, out: &mut Vec<LlmEvent>) {
        let parts: Option<ResponseParts<'_>> = event.response.and_then(|raw| {
            serde_json::from_str(raw.get())
                .inspect_err(|_| tracing::debug!("openai terminal response object is unreadable"))
                .ok()
        });
        let usage = parts
            .as_ref()
            .and_then(|p| p.usage)
            .and_then(|raw| {
                serde_json::from_str::<UsageObject>(raw.get())
                    .inspect_err(|_| tracing::debug!("openai usage object is unreadable"))
                    .ok()
            })
            .map(|u| Usage {
                input_tokens: clamp(u.input_tokens),
                cached_input_tokens: u
                    .input_tokens_details
                    .as_ref()
                    .map_or(0, |d| clamp(d.cached_tokens)),
                output_tokens: clamp(u.output_tokens),
            });
        let output_has_call = parts
            .as_ref()
            .and_then(|p| p.output)
            .and_then(|raw| serde_json::from_str::<Vec<ItemKind<'_>>>(raw.get()).ok())
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|i| text(i.kind).as_deref() == Some("function_call"))
            });
        let had_calls = !self.calls.is_empty() || output_has_call;
        let reason = parts
            .as_ref()
            .and_then(|p| p.incomplete_details)
            .and_then(|raw| serde_json::from_str::<IncompleteDetails<'_>>(raw.get()).ok())
            .and_then(|d| text(d.reason).map(Cow::into_owned));
        let finish = if incomplete && reason.as_deref() == Some("max_output_tokens") {
            // Possibly cut in the middle of a call's arguments: the caller must not trust them.
            FinishReason::Length
        } else if had_calls {
            FinishReason::ToolCalls
        } else {
            if incomplete {
                // `content_filter`, `max_messages`, `steered`: no brisvox variant; the answer
                // simply ends.
                tracing::debug!(
                    reason = reason.as_deref().map(sanitize),
                    "openai response incomplete"
                );
            }
            FinishReason::Stop
        };
        out.push(LlmEvent::Done {
            finish,
            usage,
            provider_data: self.provider_data(),
        });
    }

    /// The JSON array of the replayable output items (with the message marker), `None` if there
    /// is nothing to replay: a text-only answer needs no data.
    fn provider_data(&self) -> Option<Arc<str>> {
        if self.replay.iter().all(|i| i.get() == MESSAGE_MARKER) {
            return None;
        }
        let mut json = String::from("[");
        for (i, item) in self.replay.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(item.get());
        }
        json.push(']');
        Some(Arc::from(json))
    }
}

/// Parses the fields of one event kind. The error names the kind, never the data.
fn parse<'a, T: Deserialize<'a>>(data: &'a str, kind: &str) -> Result<T, Error> {
    serde_json::from_str(data).map_err(|e| protocol(format!("a {kind} event is malformed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(events: &[&str]) -> (Vec<LlmEvent>, Result<Flow, Error>) {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let mut last = Ok(Flow::Continue);
        for event in events {
            last = mapper.handle(event, &mut out);
            if last.as_ref().map_or(true, |f| *f == Flow::Finished) {
                break;
            }
        }
        (out, last)
    }

    fn text(out: &[LlmEvent]) -> String {
        out.iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta(t) => Some(&**t),
                _ => None,
            })
            .collect()
    }

    const ADDED: &str = r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"get_weather","arguments":""}}"#;

    #[test]
    fn text_deltas_and_a_completed_response_with_usage() {
        let (out, last) = run(&[
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            r#"{"type":"response.in_progress","response":{}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.content_part.added","part":{}}"#,
            r#"{"type":"response.output_text.delta","delta":"Hel","item_id":"msg_1","output_index":0}"#,
            r#"{"type":"response.output_text.delta","delta":"lo \"you\"\n","item_id":"msg_1"}"#,
            r#"{"type":"response.output_text.delta","delta":""}"#,
            r#"{"type":"response.output_text.done","text":"Hello"}"#,
            r#"{"type":"response.some_future_event","x":1}"#,
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message"}],"usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100,"cache_write_tokens":0},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":127}}}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        assert_eq!(text(&out), "Hello \"you\"\n");
        let Some(LlmEvent::Done {
            finish,
            usage,
            provider_data,
        }) = out.last()
        else {
            panic!("no Done: {out:?}");
        };
        assert_eq!(*finish, FinishReason::Stop);
        assert_eq!(
            *usage,
            Some(Usage {
                input_tokens: 120,
                cached_input_tokens: 100,
                output_tokens: 7
            })
        );
        assert!(provider_data.is_none());
    }

    #[test]
    fn a_function_call_streams_with_a_dense_index_and_ends_with_tool_calls() {
        let (out, last) = run(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC"}}"#,
            ADDED,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","output_index":1,"delta":"{\"loc"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","output_index":1,"delta":"ation\":\"Paris\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_9","output_index":1,"arguments":"{\"location\":\"Paris\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"get_weather","arguments":"{\"location\":\"Paris\"}","status":"completed"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning"},{"type":"function_call"}],"usage":{"input_tokens":10,"output_tokens":3}}}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        let fragments: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments,
                } => Some((
                    *index,
                    id.as_deref().map(str::to_owned),
                    name.as_deref().map(str::to_owned),
                    arguments.to_string(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            fragments,
            vec![
                (
                    0,
                    Some("call_9".to_owned()),
                    Some("get_weather".to_owned()),
                    String::new()
                ),
                (0, None, None, "{\"loc".to_owned()),
                (0, None, None, "ation\":\"Paris\"}".to_owned()),
            ],
            "the call is keyed by call_id, indexed densely (output_index 1 -> 0); done adds nothing"
        );
        let Some(LlmEvent::Done {
            finish,
            provider_data: Some(data),
            ..
        }) = out.last()
        else {
            panic!("no Done with provider data: {out:?}");
        };
        assert_eq!(*finish, FinishReason::ToolCalls);
        let items: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            items[0]["encrypted_content"], "ENC",
            "taken from the done event"
        );
        assert_eq!(items[1]["call_id"], "call_9");
        assert_eq!(items.as_array().unwrap().len(), 2);
    }

    #[test]
    fn parallel_calls_interleave_by_item_id() {
        let (out, _) = run(&[
            ADDED,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_10","call_id":"call_10","name":"b","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_10","delta":"{}"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{\"a\":1}"}"#,
        ]);
        let indexes: Vec<u32> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indexes, vec![0, 1, 1, 0]);
    }

    #[test]
    fn the_done_event_completes_arguments_the_deltas_missed() {
        let (out, _) = run(&[
            ADDED,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{\"a\":"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_9","arguments":"{\"a\":1}"}"#,
        ]);
        let joined: String = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { arguments, .. } => Some(&**arguments),
                _ => None,
            })
            .collect();
        assert_eq!(joined, "{\"a\":1}");
    }

    #[test]
    fn a_call_announced_only_by_its_finished_item_is_opened_with_full_arguments() {
        let (out, _) = run(&[
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"t","arguments":"{\"x\":2}"}}"#,
        ]);
        let [
            LlmEvent::ToolCallDelta {
                id: Some(id),
                name: Some(name),
                arguments: first,
                ..
            },
            LlmEvent::ToolCallDelta {
                arguments: second, ..
            },
        ] = out.as_slice()
        else {
            panic!("{out:?}");
        };
        assert_eq!(
            (&**id, &**name, &**first, &**second),
            ("call_1", "t", "", "{\"x\":2}")
        );
    }

    #[test]
    fn arguments_for_an_unknown_call_are_a_protocol_error() {
        let (_, last) = run(&[
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_x","delta":"{}"}"#,
        ]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        let (_, last) = run(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc"}}"#,
        ]);
        assert!(
            matches!(last, Err(Error::Protocol { .. })),
            "no call_id or name"
        );
    }

    #[test]
    fn incomplete_responses_map_to_length_or_stop() {
        let (out, last) = run(&[
            ADDED,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done {
                finish: FinishReason::Length,
                ..
            })
        ));
        let (out, _) = run(&[
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
        ]);
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done {
                finish: FinishReason::Stop,
                usage: None,
                ..
            })
        ));
    }

    #[test]
    fn refusals_are_spoken_as_text() {
        let (out, _) = run(&[r#"{"type":"response.refusal.delta","delta":"I can't."}"#]);
        assert_eq!(text(&out), "I can't.");
    }

    #[test]
    fn failures_map_to_rejected_or_protocol_without_copying_messages() {
        let secret = "Incorrect API key provided: sk-abcdEFGH";
        for (code, status) in [
            ("rate_limit_exceeded", 429),
            ("server_error", 500),
            ("server_is_overloaded", 503),
            ("invalid_prompt", 400),
        ] {
            let failed = format!(
                r#"{{"type":"response.failed","response":{{"status":"failed","error":{{"code":"{code}","message":"{secret}"}}}}}}"#
            );
            let (_, last) = run(&[&failed]);
            assert!(
                matches!(last, Err(Error::Rejected { status: s, .. }) if s == status),
                "{code}"
            );
            let event = format!(r#"{{"type":"error","code":"{code}","message":"{secret}"}}"#);
            let (_, last) = run(&[&event]);
            assert!(matches!(last, Err(Error::Rejected { status: s, .. }) if s == status));
        }
        let (_, last) = run(&[
            r#"{"type":"response.failed","response":{"error":{"code":"weird code!","message":"sk-abcdEFGH"}}}"#,
        ]);
        let Err(e) = last else { panic!() };
        let shown = e.to_string();
        assert!(
            shown.contains("weirdcode") && !shown.contains("sk-"),
            "{shown}"
        );
        // The SDK's untyped mid-stream error and a nested error object.
        let (_, last) = run(&[r#"{"error":{"code":"server_error","message":"x"}}"#]);
        assert!(matches!(last, Err(Error::Rejected { status: 500, .. })));
        let (_, last) = run(&[r#"{"type":"error","error":{"code":"rate_limit_exceeded"}}"#]);
        assert!(matches!(last, Err(Error::Rejected { status: 429, .. })));
        let (_, last) = run(&[r#"{"type":"error","message":"boom"}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
    }

    #[test]
    fn garbage_and_early_done_sentinels_are_errors() {
        let (_, last) = run(&["{not json"]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        let (_, last) = run(&["[DONE]"]);
        assert!(matches!(last, Err(Error::Disconnected { .. })));
        let (_, last) = run(&[r#"{"type":"response.output_item.added","item":5}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
    }

    #[test]
    fn too_many_calls_are_refused() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let mut result = Ok(Flow::Continue);
        for i in 0..=MAX_CALLS {
            let event = format!(
                r#"{{"type":"response.output_item.added","output_index":{i},"item":{{"type":"function_call","id":"fc_{i}","call_id":"c{i}","name":"t"}}}}"#
            );
            result = mapper.handle(&event, &mut out);
        }
        assert!(matches!(result, Err(Error::Protocol { .. })));
    }

    #[test]
    fn heartbeats_with_empty_data_are_ignored() {
        let (out, last) = run(&[
            "",
            "  \n",
            r#"{"type":"response.output_text.delta","delta":"x"}"#,
        ]);
        assert_eq!(text(&out), "x");
        assert_eq!(last.unwrap(), Flow::Continue);
    }

    #[test]
    fn a_delta_whose_call_was_announced_without_an_item_id_is_found_by_output_index() {
        let (out, last) = run(&[
            r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","call_id":"c","name":"t"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_late","output_index":3,"delta":"{}"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Continue);
        assert_eq!(out.len(), 2, "{out:?}");
    }

    #[test]
    fn ignored_events_are_never_parsed_beyond_their_type() {
        // Every field of a kind the mapper ignores may have any JSON type.
        let (out, last) = run(&[
            r#"{"type":"response.reasoning_summary_text.delta","delta":{"x":1},"item_id":7,"output_index":"two","code":[1],"error":"e"}"#,
            r#"{"type":5,"delta":"not a type"}"#,
            r#"{"delta":"no type at all"}"#,
            r#"{"type":"response.output_text.done","text":null,"output_index":-1}"#,
            r#"{"type":"response.output_text.delta","delta":"ok","output_index":"ignored for text"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Continue);
        assert_eq!(text(&out), "ok");
        // A mapped kind with a malformed field is still a protocol error.
        let (_, last) = run(&[r#"{"type":"response.output_text.delta","delta":{"x":1}}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
    }

    #[test]
    fn error_events_accept_numeric_codes_and_string_errors() {
        let (_, last) = run(&[r#"{"type":"error","code":429,"message":"slow"}"#]);
        assert!(matches!(last, Err(Error::Rejected { status: 429, .. })));
        let (_, last) = run(&[r#"{"type":"error","error":{"code":503,"message":"x"}}"#]);
        assert!(matches!(last, Err(Error::Rejected { status: 503, .. })));
        let (_, last) = run(&[r#"{"error":"something sk-secret went wrong"}"#]);
        let Err(e) = last else { panic!() };
        assert!(matches!(e, Error::Protocol { .. }), "{e:?}");
        assert!(!e.to_string().contains("sk-"), "{e}");
        let (_, last) = run(&[r#"{"type":"error","code":{"a":1},"error":[1]}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        let (_, last) =
            run(&[r#"{"type":"response.failed","response":{"error":{"code":500,"message":"m"}}}"#]);
        assert!(matches!(last, Err(Error::Rejected { status: 500, .. })));
        let (_, last) = run(&[r#"{"type":"response.failed","response":{"error":"flat"}}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        let (_, last) = run(&[r#"{"type":"response.failed","response":7}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
    }

    #[test]
    fn a_malformed_terminal_response_still_delivers_done() {
        for response in [
            r#"{"usage":{"input_tokens":"many","output_tokens":-3},"output":{"x":1},"incomplete_details":5}"#,
            r#"{"usage":"lots","output":"none"}"#,
            "7",
            "null",
        ] {
            let event = format!(r#"{{"type":"response.completed","response":{response}}}"#);
            let (out, last) = run(&[
                r#"{"type":"response.output_text.delta","delta":"answer"}"#,
                &event,
            ]);
            assert_eq!(last.unwrap(), Flow::Finished, "{response}");
            assert_eq!(text(&out), "answer");
            assert!(
                matches!(
                    out.last(),
                    Some(LlmEvent::Done {
                        finish: FinishReason::Stop,
                        usage: None,
                        ..
                    })
                ),
                "{response}: {out:?}"
            );
        }
        // One bad member costs only itself.
        let (out, _) = run(&[
            r#"{"type":"response.incomplete","response":{"usage":{"input_tokens":4,"output_tokens":1},"incomplete_details":{"reason":5}}}"#,
        ]);
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done {
                usage: Some(Usage {
                    input_tokens: 4,
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn the_message_position_is_recorded_for_replay_but_text_only_answers_carry_no_data() {
        let (out, _) = run(&[
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[]}}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"message","id":"msg_2","content":[]}}"#,
            r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"fc_1","call_id":"c1","name":"t","arguments":"{}"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ]);
        let Some(LlmEvent::Done {
            provider_data: Some(data),
            ..
        }) = out.last()
        else {
            panic!("{out:?}");
        };
        let items: serde_json::Value = serde_json::from_str(data).unwrap();
        let kinds: Vec<_> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(kinds, ["reasoning", "message", "function_call"]);
        let (out, _) = run(&[
            r#"{"type":"response.output_item.done","item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ]);
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done {
                provider_data: None,
                ..
            })
        ));
    }

    #[test]
    fn the_replayable_items_of_one_response_are_bounded() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let mut result = Ok(Flow::Continue);
        for i in 0..=MAX_REPLAY_ITEMS {
            let event = format!(
                r#"{{"type":"response.output_item.done","output_index":{i},"item":{{"type":"reasoning","id":"rs_{i}"}}}}"#
            );
            result = mapper.handle(&event, &mut out);
        }
        assert!(matches!(result, Err(Error::Protocol { .. })));
        // The byte cap bites before the count cap for large items.
        let mut mapper = Mapper::new();
        let big = "x".repeat(MAX_REPLAY_BYTES / 4 + 1);
        let event = format!(
            r#"{{"type":"response.output_item.done","item":{{"type":"reasoning","encrypted_content":"{big}"}}}}"#
        );
        let results: Vec<_> = (0..4).map(|_| mapper.handle(&event, &mut out)).collect();
        assert!(results[..3].iter().all(Result::is_ok));
        assert!(matches!(results[3], Err(Error::Protocol { .. })));
    }

    fn delta(item_id: &str, fragment: &str) -> String {
        format!(
            r#"{{"type":"response.function_call_arguments.delta","item_id":"{item_id}","output_index":1,"delta":"{fragment}"}}"#
        )
    }

    #[test]
    fn the_arguments_of_one_call_are_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        mapper.handle(ADDED, &mut out).unwrap();
        // Exactly the cap is fine; one more byte is a protocol error and is not buffered.
        let chunk = "a".repeat(64 * 1024);
        for _ in 0..(MAX_CALL_ARGS_BYTES / chunk.len()) {
            mapper.handle(&delta("fc_9", &chunk), &mut out).unwrap();
        }
        let result = mapper.handle(&delta("fc_9", "b"), &mut out);
        assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
        assert_eq!(mapper.calls[0].args.len(), MAX_CALL_ARGS_BYTES);
    }

    #[test]
    fn the_arguments_of_all_calls_together_are_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let chunk = "a".repeat(64 * 1024);
        let per_call = MAX_CALL_ARGS_BYTES / 2;
        let calls = MAX_TOTAL_ARGS_BYTES / per_call + 1;
        let mut result = Ok(Flow::Continue);
        'calls: for i in 0..calls {
            let added = format!(
                r#"{{"type":"response.output_item.added","output_index":{i},"item":{{"type":"function_call","id":"fc_{i}","call_id":"call_{i}","name":"t","arguments":""}}}}"#
            );
            mapper.handle(&added, &mut out).unwrap();
            for _ in 0..(per_call / chunk.len()) {
                let event = format!(
                    r#"{{"type":"response.function_call_arguments.delta","item_id":"fc_{i}","output_index":{i},"delta":"{chunk}"}}"#
                );
                result = mapper.handle(&event, &mut out);
                if result.is_err() {
                    break 'calls;
                }
            }
        }
        assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
        assert!(mapper.args_bytes <= MAX_TOTAL_ARGS_BYTES);
    }

    #[test]
    fn oversized_arguments_in_a_finished_item_are_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let big = "a".repeat(MAX_CALL_ARGS_BYTES + 1);
        let done = format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"function_call","id":"fc_0","call_id":"call_0","name":"t","arguments":"{big}"}}}}"#
        );
        let result = mapper.handle(&done, &mut out);
        assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
    }
}
