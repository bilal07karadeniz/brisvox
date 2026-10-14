//! Streaming response assembly: turns the provider-neutral deltas of one LLM response
//! into text plus complete tool calls.
//!
//! Provider adapters translate their stream events into [`Delta`]s and feed a
//! [`ResponseAssembler`]:
//!
//! - Text deltas are appended in order.
//! - Tool-call fragments are buffered per `index`. Fragments of different calls may interleave
//!   with each other and with text, and the JSON of a call may be split anywhere (inside a
//!   string, an escape, a number). `id` and `name` may arrive on any fragment of their call:
//!   the first non-empty value wins, and a later, different value is an error
//!   ([`AssembleError`]). An empty string counts as "absent", because some providers repeat
//!   empty fields on every chunk.
//! - Nothing is parsed while streaming. Only [`ResponseAssembler::finish`] parses, so partial
//!   JSON never reaches a tool (TOOLS.md item 3).
//! - Buffered bytes are capped (default [`DEFAULT_MAX_BYTES`]); a delta that would exceed the cap
//!   is rejected and not applied, so a runaway stream cannot grow memory without bound.
//! - The number of calls is capped too (default [`DEFAULT_MAX_CALLS`]). A fragment that would
//!   open a call beyond the cap is rejected with [`AssembleError::TooManyCalls`], so a single
//!   response cannot fan out into thousands of tool executions.
//! - Opaque provider data (for example a reasoning signature) travels with the response: on the
//!   whole response through [`Delta::ProviderData`], or on one call through the `provider_data`
//!   field of [`Delta::ToolCall`]. It is kept verbatim, including for invalid calls, and lands in
//!   [`Response::assistant_message`].
//! - Cancelling is dropping: [`ResponseAssembler::cancel`] (or any drop) discards everything, and
//!   a cancelled response yields no tool calls and no text.
//!
//! [`ResponseAssembler::finish`] mints the internal ids with
//! [`Context::next_tool_call_ids`], in index order, and parses each call's arguments. A call
//! that cannot be used (missing name, arguments that are not valid JSON, arguments that are not
//! an object) becomes an [`Assembled::Invalid`] entry with its id, raw text and error, so the
//! engine can answer it with a structured error result while the valid siblings still run.
//!
//! Two caller obligations that the types cannot express:
//!
//! - Ids are counted from the calls already in the context. Push each response's assistant
//!   message (see [`Response::assistant_message`]) before finishing the next response of the same
//!   epoch; two responses finished against the same context before either is pushed get the same
//!   ids, and [`Context::validate`] then rejects the second push.
//! - `finish` parses linearly in the buffered bytes. Typical arguments take microseconds, but a
//!   response near the cap can take far longer, so run `finish` on the inference pool
//!   (`brisvox-inference`), not on a tokio worker (CONTRIBUTING.md, CPU work above about 50 µs).

use std::collections::BTreeMap;
use std::sync::Arc;

use brisvox_core::TurnEpoch;
use serde_json::{Map, Value};

use crate::context::{Context, Message, ToolCall, ToolCallId};

/// Default cap on the bytes one response may buffer (text, argument fragments, ids and names):
/// 256 KiB.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Default cap on the number of tool calls one response may open: 32. Each call is executed
/// (SideEffect ones too) and answered in the context, so the cap bounds the fan-out of one LLM
/// response. Raise it with [`ResponseAssembler::with_limits`] only with a reason.
pub const DEFAULT_MAX_CALLS: usize = 32;

/// Bytes charged for each tool-call slot, on top of its content, so that a stream of empty
/// fragments with ever-new indexes is capped too.
const CALL_SLOT_BYTES: usize = 64;

/// One provider-neutral piece of a streamed LLM response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    /// A piece of spoken text.
    Text(Arc<str>),
    /// Opaque provider data of the whole response (for example a reasoning signature that is
    /// not tied to one call). It is kept verbatim and lands in the assistant message. A `null`
    /// value counts as absent; a later, different non-null value is an error
    /// ([`AssembleError::ConflictingResponseProviderData`]). Send each value whole: it is not
    /// concatenated, unlike text and arguments.
    ProviderData(serde_json::Value),
    /// A fragment of tool call `index`. Fragments with the same index concatenate; `id` and
    /// `name` may appear on any of them.
    ToolCall {
        /// Position of the call in the response (gaps are fine; calls are ordered by it).
        index: u32,
        /// The provider's id for the call, if this fragment carries it.
        id: Option<Arc<str>>,
        /// The tool name, if this fragment carries it.
        name: Option<Arc<str>>,
        /// A raw piece of the arguments JSON (may split the JSON anywhere).
        arguments: Arc<str>,
        /// Opaque provider data of this call (for example a signature the provider requires to
        /// be echoed back with it). Send it whole, on one fragment: the first non-null value
        /// wins, a later different one is an error ([`AssembleError::ConflictingCallProviderData`]).
        provider_data: Option<serde_json::Value>,
    },
}

/// Why a response cannot be assembled. A failed [`ResponseAssembler::push`] does not apply the
/// delta; the response should then be cancelled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AssembleError {
    /// The response would buffer more than the configured cap.
    #[error("response exceeds the {limit}-byte buffer cap")]
    TooLarge {
        /// The configured cap, in bytes.
        limit: usize,
    },
    /// The response would open more tool calls than the configured cap.
    #[error("response opens more than {limit} tool calls")]
    TooManyCalls {
        /// The configured cap on calls per response.
        limit: usize,
    },
    /// A later fragment names a different provider id for the same call.
    #[error("tool call {index}: conflicting id {later:?} after {first:?}")]
    ConflictingId {
        /// The call's index.
        index: u32,
        /// The id seen first.
        first: Arc<str>,
        /// The conflicting id.
        later: Arc<str>,
    },
    /// A later fragment names a different tool for the same call.
    #[error("tool call {index}: conflicting name {later:?} after {first:?}")]
    ConflictingName {
        /// The call's index.
        index: u32,
        /// The name seen first.
        first: Arc<str>,
        /// The conflicting name.
        later: Arc<str>,
    },
    /// A later fragment carries different provider data for the same call.
    #[error("tool call {index}: conflicting provider data")]
    ConflictingCallProviderData {
        /// The call's index.
        index: u32,
    },
    /// A later delta carries different provider data for the whole response.
    #[error("the response carries conflicting provider data")]
    ConflictingResponseProviderData,
}

/// Why a buffered call is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// No name arrived for the call.
    #[error("the call has no tool name")]
    MissingName,
    /// The arguments are not valid JSON (for example, the stream was cut off).
    #[error("arguments are not valid JSON: {0}")]
    InvalidJson(String),
    /// The arguments are valid JSON but not an object.
    #[error("arguments must be a JSON object")]
    NotAnObject,
}

/// A tool call that could not be used, kept so the engine can answer it with an error result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCall {
    /// The call's index in the response.
    pub index: u32,
    /// The internal id minted for the call (the error result answers this id).
    pub id: ToolCallId,
    /// The provider's id, if one arrived.
    pub provider_id: Option<Arc<str>>,
    /// The tool name, if one arrived.
    pub name: Option<Arc<str>>,
    /// The arguments exactly as streamed.
    pub raw_arguments: Arc<str>,
    /// The call's opaque provider data, if any arrived (echoed back in the assistant message).
    pub provider_data: Option<serde_json::Value>,
    /// Why the call is unusable.
    pub error: CallError,
}

impl InvalidCall {
    /// A failed tool result for this call, for the context: a JSON object that names the tool
    /// and the problem, so the model can retry with valid arguments.
    pub fn error_result(&self) -> Message {
        let mut object = Map::new();
        object.insert(
            "error".to_owned(),
            Value::String("invalid tool call".to_owned()),
        );
        let tool = self
            .name
            .as_deref()
            .map_or(Value::Null, |name| Value::String(name.to_owned()));
        object.insert("tool".to_owned(), tool);
        object.insert("detail".to_owned(), Value::String(self.error.to_string()));
        Message::tool_error(self.id.clone(), Value::Object(object).to_string())
    }
}

/// One call of a finished response.
#[derive(Debug, Clone, PartialEq)]
pub enum Assembled {
    /// A complete call with parsed object arguments, ready to validate and execute.
    Complete(ToolCall),
    /// A call that cannot run; answer it with [`InvalidCall::error_result`].
    Invalid(InvalidCall),
}

impl Assembled {
    /// The internal id of the call.
    pub fn id(&self) -> &ToolCallId {
        match self {
            Self::Complete(call) => &call.id,
            Self::Invalid(call) => &call.id,
        }
    }
}

/// The finished response of one LLM turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    text: Arc<str>,
    calls: Vec<Assembled>,
    provider_data: Option<serde_json::Value>,
}

impl Response {
    /// The spoken text, exactly as streamed.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The tool calls, in index order.
    pub fn calls(&self) -> &[Assembled] {
        &self.calls
    }

    /// The opaque provider data of the whole response, if any arrived.
    pub fn provider_data(&self) -> Option<&serde_json::Value> {
        self.provider_data.as_ref()
    }

    /// Whether there is neither text nor a tool call. Provider data alone does not make a
    /// response non-empty: with nothing to say or answer, there is no assistant message to carry it.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.calls.is_empty()
    }

    /// Splits the response into its text, calls and whole-response provider data.
    pub fn into_parts(self) -> (Arc<str>, Vec<Assembled>, Option<serde_json::Value>) {
        (self.text, self.calls, self.provider_data)
    }

    /// The assistant message to append to the context for this response, or `None` when the
    /// response is empty (the context rejects empty assistant messages).
    ///
    /// Every call is included, because each one must be answered. Usable calls appear as they
    /// are. An invalid call appears with an empty object as its arguments (providers require an
    /// object, and the raw text is not one) and the name `unknown_tool` when no name arrived; its
    /// [`InvalidCall::error_result`] explains what went wrong. Provider data (the response's and
    /// each call's, invalid calls included) is carried over verbatim, so adapters can echo it back.
    /// Callers record only the text that was actually played, so this message is for
    /// responses that were fully generated.
    pub fn assistant_message(&self) -> Option<Message> {
        if self.is_empty() {
            return None;
        }
        let calls = self
            .calls
            .iter()
            .map(|call| match call {
                Assembled::Complete(call) => call.clone(),
                Assembled::Invalid(invalid) => ToolCall {
                    id: invalid.id.clone(),
                    provider_id: invalid.provider_id.clone(),
                    name: invalid
                        .name
                        .clone()
                        .unwrap_or_else(|| Arc::from("unknown_tool")),
                    arguments: Value::Object(Map::new()),
                    provider_data: invalid.provider_data.clone(),
                },
            })
            .collect();
        Some(Message::Assistant {
            text: self.text.clone(),
            tool_calls: calls,
            provider_data: self.provider_data.clone(),
        })
    }
}

/// Fragments of one call, accumulated until [`ResponseAssembler::finish`].
#[derive(Debug, Default)]
struct CallFragments {
    provider_id: Option<Arc<str>>,
    name: Option<Arc<str>>,
    arguments: String,
    provider_data: Option<serde_json::Value>,
}

/// Accumulates the deltas of one LLM response. See the [module docs](self).
#[derive(Debug)]
pub struct ResponseAssembler {
    max_bytes: usize,
    max_calls: usize,
    used_bytes: usize,
    text: String,
    provider_data: Option<serde_json::Value>,
    calls: BTreeMap<u32, CallFragments>,
}

impl Default for ResponseAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseAssembler {
    /// An empty assembler with the default caps ([`DEFAULT_MAX_BYTES`], [`DEFAULT_MAX_CALLS`]).
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_BYTES, DEFAULT_MAX_CALLS)
    }

    /// An empty assembler that buffers at most `max_bytes` (and the default number of calls).
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self::with_limits(max_bytes, DEFAULT_MAX_CALLS)
    }

    /// An empty assembler that buffers at most `max_bytes` and opens at most `max_calls` calls.
    pub fn with_limits(max_bytes: usize, max_calls: usize) -> Self {
        Self {
            max_bytes,
            max_calls,
            used_bytes: 0,
            text: String::new(),
            provider_data: None,
            calls: BTreeMap::new(),
        }
    }

    /// The configured cap, in bytes.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The configured cap on tool calls per response.
    pub fn max_calls(&self) -> usize {
        self.max_calls
    }

    /// Bytes buffered so far (text, argument fragments, ids, names, provider data, and a slot
    /// charge per call).
    pub fn buffered_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Applies one delta. Only text, provider data and fragments are stored; nothing is parsed
    /// here.
    ///
    /// # Errors
    /// [`AssembleError::TooLarge`] when the delta would exceed the byte cap,
    /// [`AssembleError::TooManyCalls`] when it would open a call beyond the call cap, or a
    /// conflicting `id`, `name` or provider data. The delta is then not applied at all.
    pub fn push(&mut self, delta: Delta) -> Result<(), AssembleError> {
        match delta {
            Delta::Text(text) => {
                self.charge(text.len())?;
                self.text.push_str(&text);
                Ok(())
            }
            Delta::ProviderData(value) => {
                if value.is_null() {
                    return Ok(());
                }
                match &self.provider_data {
                    Some(first) if *first != value => {
                        return Err(AssembleError::ConflictingResponseProviderData);
                    }
                    Some(_) => return Ok(()),
                    None => {}
                }
                self.charge(json_len(&value))?;
                self.provider_data = Some(value);
                Ok(())
            }
            Delta::ToolCall {
                index,
                id,
                name,
                arguments,
                provider_data,
            } => {
                let id = non_empty(id);
                let name = non_empty(name);
                let provider_data = non_null(provider_data);
                let existing = self.calls.get(&index);
                if existing.is_none() && self.calls.len() >= self.max_calls {
                    return Err(AssembleError::TooManyCalls {
                        limit: self.max_calls,
                    });
                }
                let mut cost = arguments.len();
                if existing.is_none() {
                    cost = cost.saturating_add(CALL_SLOT_BYTES);
                }
                if let Some(later) = &id {
                    match existing.and_then(|call| call.provider_id.as_ref()) {
                        Some(first) if first != later => {
                            return Err(AssembleError::ConflictingId {
                                index,
                                first: first.clone(),
                                later: later.clone(),
                            });
                        }
                        Some(_) => {}
                        None => cost = cost.saturating_add(later.len()),
                    }
                }
                if let Some(later) = &name {
                    match existing.and_then(|call| call.name.as_ref()) {
                        Some(first) if first != later => {
                            return Err(AssembleError::ConflictingName {
                                index,
                                first: first.clone(),
                                later: later.clone(),
                            });
                        }
                        Some(_) => {}
                        None => cost = cost.saturating_add(later.len()),
                    }
                }
                if let Some(later) = &provider_data {
                    match existing.and_then(|call| call.provider_data.as_ref()) {
                        Some(first) if first != later => {
                            return Err(AssembleError::ConflictingCallProviderData { index });
                        }
                        Some(_) => {}
                        None => cost = cost.saturating_add(json_len(later)),
                    }
                }
                self.charge(cost)?;
                let call = self.calls.entry(index).or_default();
                if call.provider_id.is_none() {
                    call.provider_id = id;
                }
                if call.name.is_none() {
                    call.name = name;
                }
                if call.provider_data.is_none() {
                    call.provider_data = provider_data;
                }
                call.arguments.push_str(&arguments);
                Ok(())
            }
        }
    }

    /// Discards everything buffered. A cancelled response yields no text and no tool calls;
    /// this is the same as dropping the assembler.
    ///
    /// The guarantee is structural: `cancel` takes the assembler by value, so no [`Response`] can
    /// be produced from it afterwards. The doctest below must not compile.
    ///
    /// ```compile_fail
    /// use brisvox_agent::assemble::{Delta, ResponseAssembler};
    /// use brisvox_agent::Context;
    /// use brisvox_core::TurnEpoch;
    ///
    /// let mut assembler = ResponseAssembler::new();
    /// let _ = assembler.push(Delta::Text("half a sentence".into()));
    /// assembler.cancel();
    /// // Use after cancel: the assembler was moved, so `finish` cannot see its buffers.
    /// let _ = assembler.finish(TurnEpoch::new(1), &Context::new());
    /// ```
    pub fn cancel(self) {}

    /// Finishes the response: mints the internal ids for its calls with
    /// [`Context::next_tool_call_ids`] (in index order, continuing the calls of `epoch` already
    /// in `context`), then parses each call's arguments. Empty arguments mean `{}`.
    ///
    /// This is the only place where JSON is parsed. It does not fail: unusable calls come back
    /// as [`Assembled::Invalid`]. Push the response before finishing another one of the same
    /// epoch, and run large responses on the inference pool (see the [module docs](self)).
    pub fn finish(self, epoch: TurnEpoch, context: &Context) -> Response {
        let ids = context.next_tool_call_ids(epoch, self.calls.len());
        let calls = self
            .calls
            .into_iter()
            .zip(ids)
            .map(|((index, fragments), id)| settle(index, fragments, id))
            .collect();
        Response {
            text: Arc::from(self.text),
            calls,
            provider_data: self.provider_data,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), AssembleError> {
        let total = self.used_bytes.saturating_add(bytes);
        if total > self.max_bytes {
            return Err(AssembleError::TooLarge {
                limit: self.max_bytes,
            });
        }
        self.used_bytes = total;
        Ok(())
    }
}

/// An empty id or name counts as absent.
fn non_empty(value: Option<Arc<str>>) -> Option<Arc<str>> {
    value.filter(|text| !text.is_empty())
}

/// Provider data of `null` counts as absent.
fn non_null(value: Option<serde_json::Value>) -> Option<serde_json::Value> {
    value.filter(|value| !value.is_null())
}

/// A writer that only counts the bytes written to it, so the size of provider data is charged
/// without allocating its serialized form.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The serialized size of `value` in bytes. A value that cannot be measured counts as
/// `usize::MAX`, so it is rejected by the cap rather than admitted for free.
fn json_len(value: &serde_json::Value) -> usize {
    let mut counter = ByteCounter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => usize::MAX,
    }
}

/// Turns one call's fragments into a usable call or an invalid entry.
fn settle(index: u32, fragments: CallFragments, id: ToolCallId) -> Assembled {
    let CallFragments {
        provider_id,
        name,
        arguments,
        provider_data,
    } = fragments;
    let error = match (&name, parse_arguments(&arguments)) {
        (Some(name), Ok(arguments)) => {
            return Assembled::Complete(ToolCall {
                id,
                provider_id,
                name: name.clone(),
                arguments,
                provider_data,
            });
        }
        (None, _) => CallError::MissingName,
        (Some(_), Err(error)) => error,
    };
    Assembled::Invalid(InvalidCall {
        index,
        id,
        provider_id,
        name,
        raw_arguments: Arc::from(arguments),
        provider_data,
        error,
    })
}

/// Parses complete arguments: blank text is `{}`, otherwise a JSON object is required.
fn parse_arguments(raw: &str) -> Result<Value, CallError> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(CallError::NotAnObject),
        Err(error) => Err(CallError::InvalidJson(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextError, Message};
    use serde_json::json;

    const EPOCH: TurnEpoch = TurnEpoch::new(1);

    fn text(s: &str) -> Delta {
        Delta::Text(Arc::from(s))
    }

    fn frag(index: u32, id: Option<&str>, name: Option<&str>, arguments: &str) -> Delta {
        Delta::ToolCall {
            index,
            id: id.map(Arc::from),
            name: name.map(Arc::from),
            arguments: Arc::from(arguments),
            provider_data: None,
        }
    }

    fn frag_with_data(index: u32, arguments: &str, provider_data: Value) -> Delta {
        Delta::ToolCall {
            index,
            id: None,
            name: None,
            arguments: Arc::from(arguments),
            provider_data: Some(provider_data),
        }
    }

    fn assemble(deltas: Vec<Delta>) -> Response {
        let mut assembler = ResponseAssembler::new();
        for delta in deltas {
            assembler.push(delta).expect("delta fits");
        }
        assembler.finish(EPOCH, &Context::new())
    }

    fn complete(response: &Response, at: usize) -> &ToolCall {
        match response.calls().get(at) {
            Some(Assembled::Complete(call)) => call,
            other => panic!("expected a complete call at {at}, got {other:?}"),
        }
    }

    /// Answers every call of `response` (results for usable calls, error results for invalid
    /// ones) and checks the context stays valid.
    fn answer(context: &mut Context, response: &Response) {
        if let Some(message) = response.assistant_message() {
            context.push(message);
        }
        for call in response.calls() {
            match call {
                Assembled::Complete(call) => {
                    context.push(Message::tool_result(call.id.clone(), "{}"));
                }
                Assembled::Invalid(invalid) => context.push(invalid.error_result()),
            }
        }
        assert_eq!(context.validate(), Ok(()));
    }

    #[test]
    fn json_split_at_every_char_boundary_assembles_identically() {
        // Includes \u escapes (one BMP, one surrogate pair) next to literal multi-byte text, so a
        // split can fall inside an escape as well as between bytes of a character.
        let full = r#"{"city":"İzmir 🌦","note":"a\"b\\c ç","n":[1,2.5e3,null,true],"u":"ç😀 ç😀"}"#;
        let expected: Value = serde_json::from_str(full).expect("sample is JSON");
        for split in 0..=full.len() {
            if !full.is_char_boundary(split) {
                continue;
            }
            let (head, tail) = full.split_at(split);
            // The name and id ride on the first fragment, the rest arrives later.
            let response = assemble(vec![
                frag(0, Some("c1"), Some("weather"), head),
                frag(0, None, None, tail),
            ]);
            let call = complete(&response, 0);
            assert_eq!(call.arguments, expected, "split at byte {split}");
            assert_eq!(call.name.as_ref(), "weather");
        }
    }

    #[test]
    fn json_split_into_single_characters_assembles_identically() {
        let full = r#"{"q":"şA 👍🏽","k":"\\"}"#;
        let expected: Value = serde_json::from_str(full).expect("sample is JSON");
        let mut deltas = vec![frag(0, None, Some("search"), "")];
        deltas.extend(
            full.chars()
                .map(|c| frag(0, None, None, c.encode_utf8(&mut [0; 4]))),
        );
        let response = assemble(deltas);
        assert_eq!(complete(&response, 0).arguments, expected);
    }

    #[test]
    fn parallel_calls_with_interleaved_fragments_and_text() {
        let mut context = Context::new();
        context.push(Message::user("Ankara'da saat kaç ve hava nasıl?"));
        let mut assembler = ResponseAssembler::new();
        // Index 2 starts before index 0 and text is interleaved between fragments.
        for delta in [
            text("Bakıyorum "),
            frag(2, Some("call_time"), Some("time"), r#"{"tz":"Europe"#),
            frag(0, Some("call_w"), Some("weather"), r#"{"city":"#),
            text("ve "),
            frag(2, None, None, r#"/Istanbul"}"#),
            frag(0, None, None, r#""Ankara"}"#),
            text("🕒"),
        ] {
            assembler.push(delta).expect("fits");
        }
        let response = assembler.finish(EPOCH, &context);
        assert_eq!(response.text(), "Bakıyorum ve 🕒");
        assert_eq!(response.calls().len(), 2);
        let weather = complete(&response, 0);
        assert_eq!(weather.name.as_ref(), "weather");
        assert_eq!(weather.provider_id.as_deref(), Some("call_w"));
        assert_eq!(weather.arguments, json!({ "city": "Ankara" }));
        assert_eq!(weather.id, ToolCallId::new(EPOCH, 0));
        let time = complete(&response, 1);
        assert_eq!(time.name.as_ref(), "time");
        assert_eq!(time.arguments, json!({ "tz": "Europe/Istanbul" }));
        assert_eq!(time.id, ToolCallId::new(EPOCH, 1));
        answer(&mut context, &response);
    }

    #[test]
    fn id_and_name_arriving_late_and_repeated_values_are_fine() {
        let response = assemble(vec![
            frag(0, None, None, r#"{"a":"#),
            frag(0, Some("c1"), None, "1"),
            frag(0, None, Some("weather"), "}"),
            // The same values again, and empty values repeated by some providers.
            frag(0, Some("c1"), Some("weather"), ""),
            frag(0, Some(""), Some(""), ""),
        ]);
        let call = complete(&response, 0);
        assert_eq!(call.name.as_ref(), "weather");
        assert_eq!(call.provider_id.as_deref(), Some("c1"));
        assert_eq!(call.arguments, json!({ "a": 1 }));
    }

    #[test]
    fn conflicting_name_or_id_is_an_error_and_is_not_applied() {
        let mut assembler = ResponseAssembler::new();
        assembler
            .push(frag(0, Some("c1"), Some("weather"), "{"))
            .expect("first");
        let before = assembler.buffered_bytes();
        assert_eq!(
            assembler.push(frag(0, None, Some("time"), "}")),
            Err(AssembleError::ConflictingName {
                index: 0,
                first: Arc::from("weather"),
                later: Arc::from("time"),
            })
        );
        assert_eq!(
            assembler.push(frag(0, Some("c2"), None, "}")),
            Err(AssembleError::ConflictingId {
                index: 0,
                first: Arc::from("c1"),
                later: Arc::from("c2"),
            })
        );
        assert_eq!(
            assembler.buffered_bytes(),
            before,
            "rejected deltas are not applied"
        );
        // The arguments were not extended by the rejected fragments: "{" is still invalid JSON.
        let response = assembler.finish(EPOCH, &Context::new());
        assert!(matches!(
            response.calls(),
            [Assembled::Invalid(InvalidCall {
                error: CallError::InvalidJson(_),
                raw_arguments,
                ..
            })] if raw_arguments.as_ref() == "{"
        ));
    }

    #[test]
    fn invalid_and_non_object_calls_do_not_fail_their_valid_siblings() {
        let mut context = Context::new();
        context.push(Message::user("do three things"));
        let response = assemble(vec![
            frag(0, Some("a"), Some("ok"), r#"{"x":1}"#),
            // Stream cut off mid-object: partial JSON never executes.
            frag(7, Some("b"), Some("broken"), r#"{"x":"#),
            frag(3, None, Some("array"), "[1,2]"),
            // No name at all.
            frag(5, None, None, "{}"),
            frag(9, Some("c"), Some("ok2"), r#"{"y":"z"}"#),
        ]);
        // Gaps are fine: order is by index, ids are minted in that order.
        let ids: Vec<String> = response
            .calls()
            .iter()
            .map(|c| c.id().to_string())
            .collect();
        assert_eq!(ids, ["t1.0", "t1.1", "t1.2", "t1.3", "t1.4"]);
        assert!(
            matches!(response.calls().first(), Some(Assembled::Complete(c)) if c.name.as_ref() == "ok")
        );
        match response.calls().get(1) {
            Some(Assembled::Invalid(invalid)) => {
                assert_eq!(invalid.index, 3);
                assert_eq!(invalid.name.as_deref(), Some("array"));
                assert_eq!(invalid.error, CallError::NotAnObject);
                assert_eq!(invalid.raw_arguments.as_ref(), "[1,2]");
            }
            other => panic!("expected invalid array call, got {other:?}"),
        }
        match response.calls().get(2) {
            Some(Assembled::Invalid(invalid)) => {
                assert_eq!(invalid.index, 5);
                assert_eq!(invalid.name, None);
                assert_eq!(invalid.error, CallError::MissingName);
            }
            other => panic!("expected invalid nameless call, got {other:?}"),
        }
        match response.calls().get(3) {
            Some(Assembled::Invalid(invalid)) => {
                assert_eq!(invalid.index, 7);
                assert!(matches!(invalid.error, CallError::InvalidJson(_)));
                assert_eq!(invalid.raw_arguments.as_ref(), r#"{"x":"#);
            }
            other => panic!("expected invalid JSON call, got {other:?}"),
        }
        assert_eq!(complete(&response, 4).arguments, json!({ "y": "z" }));
        // The engine answers every call, the invalid ones with a structured error.
        answer(&mut context, &response);
        // The recorded assistant message names each call: a named invalid call keeps its name
        // (not the placeholder), a nameless one gets `unknown_tool`, and invalid arguments are
        // recorded as `{}`.
        let recorded: Vec<crate::context::ToolCall> = context
            .messages()
            .iter()
            .find_map(|m| match m {
                Message::Assistant { tool_calls, .. } => Some(tool_calls.clone()),
                _ => None,
            })
            .expect("the assistant message is recorded");
        let names: Vec<&str> = recorded.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, ["ok", "array", "unknown_tool", "broken", "ok2"]);
        assert_eq!(recorded[1].arguments, json!({}));
        assert_eq!(recorded[3].arguments, json!({}));
        assert_eq!(recorded[3].provider_id.as_deref(), Some("b"));
        // The result for the truncated call (index 7, the fourth call in index order: t1.3).
        let (content, is_error) = context
            .messages()
            .iter()
            .find_map(|m| match m {
                Message::Tool {
                    call_id,
                    content,
                    is_error,
                } if call_id.as_str() == "t1.3" => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("the invalid call is answered");
        assert!(is_error);
        let parsed: Value = serde_json::from_str(&content).expect("error result is JSON");
        assert_eq!(parsed["tool"], json!("broken"));
        assert!(
            parsed["detail"]
                .as_str()
                .is_some_and(|d| d.contains("JSON"))
        );
    }

    #[test]
    fn empty_or_blank_arguments_mean_an_empty_object() {
        let response = assemble(vec![
            frag(0, None, Some("ping"), ""),
            frag(1, None, Some("ping"), " \n\t"),
        ]);
        assert_eq!(complete(&response, 0).arguments, json!({}));
        assert_eq!(complete(&response, 1).arguments, json!({}));
    }

    #[test]
    fn leading_empty_id_and_name_do_not_conflict_with_the_real_values() {
        // Some providers send empty id and name on the first chunk, then the real values.
        let response = assemble(vec![
            frag(0, Some(""), Some(""), "{"),
            frag(0, Some("c1"), Some("weather"), "}"),
        ]);
        let call = complete(&response, 0);
        assert_eq!(call.provider_id.as_deref(), Some("c1"));
        assert_eq!(call.name.as_ref(), "weather");
        assert_eq!(call.arguments, json!({}));
    }

    #[test]
    fn provider_data_survives_finish_and_the_assistant_message() {
        let mut context = Context::new();
        context.push(Message::user("weather in two cities"));
        let response_data = json!({ "reasoning_signature": "sig-response" });
        let call_data = json!({ "signature": "sig-call-0" });
        let broken_data = json!({ "signature": "sig-call-1" });
        let mut assembler = ResponseAssembler::new();
        assembler
            .push(Delta::ProviderData(response_data.clone()))
            .expect("fits");
        // The data arrives with the first fragment and repeats (the same value) on a later one.
        assembler
            .push(Delta::ToolCall {
                index: 0,
                id: Some(Arc::from("c0")),
                name: Some(Arc::from("weather")),
                arguments: Arc::from(r#"{"city":"#),
                provider_data: Some(call_data.clone()),
            })
            .expect("fits");
        assembler
            .push(Delta::ToolCall {
                index: 0,
                id: None,
                name: None,
                arguments: Arc::from(r#""Ankara"}"#),
                provider_data: Some(call_data.clone()),
            })
            .expect("the same value again is fine");
        // An invalid call keeps its provider data too: the signature must come back unchanged.
        assembler
            .push(frag_with_data(4, "[1", broken_data.clone()))
            .expect("fits");
        let response = assembler.finish(EPOCH, &context);
        assert_eq!(response.provider_data(), Some(&response_data));
        assert_eq!(
            complete(&response, 0).provider_data,
            Some(call_data.clone())
        );
        match response.calls().get(1) {
            Some(Assembled::Invalid(invalid)) => {
                assert_eq!(invalid.provider_data, Some(broken_data.clone()));
            }
            other => panic!("expected an invalid call, got {other:?}"),
        }
        // The assistant message echoes all of it, for the next request.
        let message = response.assistant_message().expect("has calls");
        let Message::Assistant {
            provider_data,
            tool_calls,
            ..
        } = &message
        else {
            panic!("assistant message expected");
        };
        assert_eq!(provider_data.as_ref(), Some(&response_data));
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].provider_data.as_ref(), Some(&call_data));
        assert_eq!(tool_calls[1].provider_data.as_ref(), Some(&broken_data));
        assert_eq!(tool_calls[1].arguments, json!({}));
        answer(&mut context, &response);
    }

    #[test]
    fn conflicting_provider_data_is_an_error_and_is_not_applied() {
        let mut assembler = ResponseAssembler::new();
        assembler
            .push(frag_with_data(0, "{", json!("first")))
            .expect("first");
        let before = assembler.buffered_bytes();
        assert_eq!(
            assembler.push(frag_with_data(0, "}", json!("second"))),
            Err(AssembleError::ConflictingCallProviderData { index: 0 })
        );
        assert_eq!(assembler.buffered_bytes(), before);
        assembler
            .push(Delta::ProviderData(json!("response-a")))
            .expect("first response data");
        let before = assembler.buffered_bytes();
        assert_eq!(
            assembler.push(Delta::ProviderData(json!("response-b"))),
            Err(AssembleError::ConflictingResponseProviderData)
        );
        assert_eq!(assembler.buffered_bytes(), before);
        // A null value is absent: it neither conflicts nor overwrites.
        assembler
            .push(Delta::ProviderData(serde_json::Value::Null))
            .expect("null is absent");
        assembler
            .push(frag_with_data(0, "}", serde_json::Value::Null))
            .expect("null is absent");
        let response = assembler.finish(EPOCH, &Context::new());
        assert_eq!(response.provider_data(), Some(&json!("response-a")));
        assert_eq!(complete_or_invalid_data(&response, 0), Some(json!("first")));
    }

    fn complete_or_invalid_data(response: &Response, at: usize) -> Option<Value> {
        match response.calls().get(at)? {
            Assembled::Complete(call) => call.provider_data.clone(),
            Assembled::Invalid(call) => call.provider_data.clone(),
        }
    }

    #[test]
    fn exceeding_the_byte_cap_is_an_error_and_leaves_state_unchanged() {
        let mut assembler = ResponseAssembler::with_max_bytes(32);
        assert_eq!(assembler.max_bytes(), 32);
        assembler
            .push(text("12345678901234567890"))
            .expect("20 bytes fit");
        let before = assembler.buffered_bytes();
        assert_eq!(
            assembler.push(frag(0, None, Some("abcdefghijk"), "{}")),
            Err(AssembleError::TooLarge { limit: 32 })
        );
        assert_eq!(
            assembler.push(text("123456789012345678901234567890123")),
            Err(AssembleError::TooLarge { limit: 32 })
        );
        assert_eq!(assembler.buffered_bytes(), before);
        // Exactly at the cap is allowed.
        assembler
            .push(text("123456789012"))
            .expect("exactly 32 bytes");
        assert_eq!(assembler.buffered_bytes(), 32);
        // The rejected deltas left no trace in the response: only the accepted text remains.
        let response = assembler.finish(EPOCH, &Context::new());
        assert_eq!(response.text(), "12345678901234567890123456789012");
        assert!(response.calls().is_empty());
    }

    #[test]
    fn runaway_empty_slots_hit_the_byte_cap_when_the_call_limit_is_lifted() {
        // The call cap (DEFAULT_MAX_CALLS) would stop this first, so lift it to exercise the
        // byte cap alone: each new slot costs CALL_SLOT_BYTES, so empty slots are capped too.
        assert_eq!(DEFAULT_MAX_BYTES, 256 * 1024);
        let mut assembler = ResponseAssembler::with_limits(DEFAULT_MAX_BYTES, usize::MAX);
        assert_eq!(assembler.max_bytes(), DEFAULT_MAX_BYTES);
        let slots = DEFAULT_MAX_BYTES / CALL_SLOT_BYTES;
        for index in 0..slots {
            let index = u32::try_from(index).expect("small");
            assembler
                .push(frag(index, None, None, ""))
                .expect("within cap");
        }
        let index = u32::try_from(slots).expect("small");
        assert_eq!(
            assembler.push(frag(index, None, None, "")),
            Err(AssembleError::TooLarge {
                limit: DEFAULT_MAX_BYTES
            })
        );
    }

    #[test]
    fn runaway_call_count_hits_the_call_limit() {
        assert_eq!(DEFAULT_MAX_CALLS, 32);
        let mut assembler = ResponseAssembler::new();
        assert_eq!(assembler.max_calls(), DEFAULT_MAX_CALLS);
        for index in 0..DEFAULT_MAX_CALLS {
            let index = u32::try_from(index).expect("small");
            assembler
                .push(frag(index, None, Some("ping"), "{}"))
                .expect("within the call cap");
        }
        // Fragments for calls already opened are still accepted.
        assembler
            .push(frag(0, None, None, "{}"))
            .expect("an open call is not a new call");
        // A new call is refused, whether its index is the next one or a far-off gap.
        let next = u32::try_from(DEFAULT_MAX_CALLS).expect("small");
        for index in [next, u32::MAX] {
            let before = assembler.buffered_bytes();
            assert_eq!(
                assembler.push(frag(index, None, Some("ping"), "{}")),
                Err(AssembleError::TooManyCalls {
                    limit: DEFAULT_MAX_CALLS
                })
            );
            assert_eq!(assembler.buffered_bytes(), before);
        }
        let response = assembler.finish(EPOCH, &Context::new());
        assert_eq!(response.calls().len(), DEFAULT_MAX_CALLS);
    }

    #[test]
    fn a_lowered_call_limit_is_enforced() {
        let mut assembler = ResponseAssembler::with_limits(DEFAULT_MAX_BYTES, 2);
        assert_eq!(assembler.max_calls(), 2);
        assembler
            .push(frag(5, None, Some("a"), "{}"))
            .expect("first");
        assembler
            .push(frag(9, None, Some("b"), "{}"))
            .expect("second");
        assert_eq!(
            assembler.push(frag(10, None, Some("c"), "{}")),
            Err(AssembleError::TooManyCalls { limit: 2 })
        );
    }

    #[test]
    fn argument_and_id_bytes_count_against_the_cap() {
        // Slot (64) + name (1) = 65 bytes after the first fragment; 100 is the cap.
        let mut assembler = ResponseAssembler::with_max_bytes(100);
        assembler
            .push(frag(0, None, Some("n"), ""))
            .expect("slot and name fit");
        assert_eq!(assembler.buffered_bytes(), 65);
        // 40 argument bytes would reach 105: rejected, and nothing is buffered.
        let too_long = "x".repeat(40);
        assert_eq!(
            assembler.push(frag(0, None, None, &too_long)),
            Err(AssembleError::TooLarge { limit: 100 })
        );
        assert_eq!(assembler.buffered_bytes(), 65);
        // The id is charged too: a 36-byte id would reach 101 bytes.
        assert_eq!(
            assembler.push(frag(0, Some(&"i".repeat(36)), None, "")),
            Err(AssembleError::TooLarge { limit: 100 })
        );
        assert_eq!(assembler.buffered_bytes(), 65);
        // Exactly 35 argument bytes reach the cap, then no room is left for even an id byte.
        assembler
            .push(frag(0, None, None, &"x".repeat(35)))
            .expect("exactly 100 bytes");
        assert_eq!(assembler.buffered_bytes(), 100);
        assert_eq!(
            assembler.push(frag(0, Some("i"), None, "")),
            Err(AssembleError::TooLarge { limit: 100 })
        );
        assert_eq!(assembler.buffered_bytes(), 100);
        // Only the accepted 35 argument bytes and the name are in the call; the rejected id
        // and fragments were not applied.
        let response = assembler.finish(EPOCH, &Context::new());
        match response.calls() {
            [Assembled::Invalid(invalid)] => {
                assert_eq!(invalid.name.as_deref(), Some("n"));
                assert_eq!(invalid.provider_id, None);
                assert_eq!(invalid.raw_arguments.as_ref(), "x".repeat(35));
            }
            other => panic!("expected one call, got {other:?}"),
        }
    }

    #[test]
    fn provider_data_counts_against_the_cap() {
        // Slot (64) + name (1) = 65; a 34-x string serializes to 36 bytes and would reach 101.
        let mut assembler = ResponseAssembler::with_max_bytes(100);
        assembler
            .push(frag(0, None, Some("n"), ""))
            .expect("slot and name fit");
        assert_eq!(
            assembler.push(frag_with_data(0, "", json!("x".repeat(34)))),
            Err(AssembleError::TooLarge { limit: 100 })
        );
        assert_eq!(
            assembler.push(Delta::ProviderData(json!("x".repeat(34)))),
            Err(AssembleError::TooLarge { limit: 100 })
        );
        assert_eq!(assembler.buffered_bytes(), 65);
        // A 33-x string serializes to 35 bytes: exactly 100 bytes, allowed.
        assembler
            .push(frag_with_data(0, "", json!("x".repeat(33))))
            .expect("exactly at the cap");
        assert_eq!(assembler.buffered_bytes(), 100);
    }

    #[test]
    fn late_id_is_charged_and_its_conflict_is_detected() {
        let mut assembler = ResponseAssembler::new();
        assembler
            .push(frag(0, None, Some("weather"), "{"))
            .expect("first");
        assembler.push(frag(0, None, None, "}")).expect("no id yet");
        // The id first arrives on a later fragment, then a different one conflicts with it.
        assembler
            .push(frag(0, Some("c1"), None, ""))
            .expect("first id");
        let before = assembler.buffered_bytes();
        assert_eq!(
            assembler.push(frag(0, Some("c2"), None, "")),
            Err(AssembleError::ConflictingId {
                index: 0,
                first: Arc::from("c1"),
                later: Arc::from("c2"),
            })
        );
        assert_eq!(assembler.buffered_bytes(), before);
        let response = assembler.finish(EPOCH, &Context::new());
        assert_eq!(provider_id_at(&response, 0), Some("c1"));
    }

    fn provider_id_at(response: &Response, at: usize) -> Option<&str> {
        match response.calls().get(at)? {
            Assembled::Complete(call) => call.provider_id.as_deref(),
            Assembled::Invalid(call) => call.provider_id.as_deref(),
        }
    }

    #[test]
    fn deeply_nested_arguments_are_invalid_json_not_a_stack_overflow() {
        // serde_json stops at its recursion limit (128) with an error, so 10,000 levels cannot
        // overflow the stack of the thread that calls finish().
        for nested in [
            "[".repeat(10_000),
            r#"{"a":"#.repeat(10_000),
            format!("{}{}", "[".repeat(10_000), "]".repeat(10_000)),
        ] {
            let response = assemble(vec![frag(0, None, Some("deep"), &nested)]);
            assert!(
                matches!(
                    response.calls(),
                    [Assembled::Invalid(InvalidCall {
                        error: CallError::InvalidJson(_),
                        ..
                    })]
                ),
                "deep nesting must be an invalid call"
            );
        }
    }

    #[test]
    fn empty_response_has_no_assistant_message() {
        let response = assemble(Vec::new());
        assert!(response.is_empty());
        assert_eq!(response.assistant_message(), None);
        let (text, calls, provider_data) = response.into_parts();
        assert!(text.is_empty() && calls.is_empty() && provider_data.is_none());
    }

    #[test]
    fn ids_continue_across_two_tool_loop_rounds_in_one_epoch() {
        let epoch = TurnEpoch::new(3);
        let mut context = Context::with_system("You are a helpful agent.");
        context.push(Message::user("Ankara ve İzmir'de hava nasıl?"));

        let mut round1 = ResponseAssembler::new();
        round1
            .push(frag(0, Some("x0"), Some("weather"), r#"{"city":"Ankara"}"#))
            .expect("fits");
        round1
            .push(frag(1, Some("x1"), Some("weather"), r#"{"city":"İzmir"}"#))
            .expect("fits");
        let first = round1.finish(epoch, &context);
        let ids: Vec<String> = first.calls().iter().map(|c| c.id().to_string()).collect();
        assert_eq!(ids, ["t3.0", "t3.1"]);
        answer(&mut context, &first);

        // The model asks for a follow-up in the same turn: ids continue, they do not restart.
        let mut round2 = ResponseAssembler::new();
        round2
            .push(frag(0, Some("x2"), Some("forecast"), r#"{"days":2}"#))
            .expect("fits");
        let second = round2.finish(epoch, &context);
        assert_eq!(second.calls()[0].id().as_str(), "t3.2");
        answer(&mut context, &second);

        // Another turn starts again at zero, and the first turn's calls do not count.
        assert_eq!(
            context.next_tool_call_id(TurnEpoch::new(4)).as_str(),
            "t4.0"
        );
        assert_eq!(context.validate(), Ok(()));
    }

    #[test]
    fn two_responses_finished_before_either_is_pushed_collide_and_validate_rejects() {
        // The caveat in the module docs, pinned: ids come from the pushed calls only.
        let mut context = Context::new();
        context.push(Message::user("both please"));
        let mut speculative = ResponseAssembler::new();
        speculative
            .push(frag(0, Some("s"), Some("transfer"), "{}"))
            .expect("fits");
        let mut committed = ResponseAssembler::new();
        committed
            .push(frag(0, Some("c"), Some("transfer"), "{}"))
            .expect("fits");
        let first = speculative.finish(EPOCH, &context);
        let second = committed.finish(EPOCH, &context);
        assert_eq!(first.calls()[0].id(), second.calls()[0].id());
        // Pushing the first one (and answering it) and then the second one is rejected.
        context.push(first.assistant_message().expect("has a call"));
        context.push(Message::tool_result(ToolCallId::new(EPOCH, 0), "{}"));
        context.push(second.assistant_message().expect("has a call"));
        assert_eq!(
            context.validate(),
            Err(ContextError::DuplicateToolCallId {
                index: 3,
                call_id: ToolCallId::new(EPOCH, 0),
            })
        );
    }

    #[test]
    fn turkish_and_emoji_text_is_preserved_exactly() {
        let sample = "Merhaba! Şöyle: ğüşıöç İ I ı, 👋🏽 ve 👨‍👩‍👧‍👦 🇹🇷 — çok güzel 🌦️.";
        // One delta per character, and two characters per delta, must give the same text.
        let per_char: Vec<Delta> = sample.chars().map(|c| text(&c.to_string())).collect();
        assert_eq!(assemble(per_char).text(), sample);
        let chars: Vec<char> = sample.chars().collect();
        let pairs: Vec<Delta> = chars
            .chunks(2)
            .map(|pair| text(&pair.iter().collect::<String>()))
            .collect();
        let response = assemble(pairs);
        assert_eq!(response.text(), sample);
        assert_eq!(
            response.assistant_message(),
            Some(Message::assistant(sample))
        );
    }

    #[test]
    fn invalid_entries_are_recorded_under_the_placeholder_name_in_the_context() {
        let mut context = Context::new();
        context.push(Message::user("go"));
        let mut assembler = ResponseAssembler::new();
        assembler
            .push(frag(0, Some("p0"), None, "{}"))
            .expect("fits");
        let response = assembler.finish(EPOCH, &context);
        let message = response.assistant_message().expect("has a call");
        let Message::Assistant { tool_calls, .. } = &message else {
            panic!("assistant message expected");
        };
        assert_eq!(tool_calls[0].name.as_ref(), "unknown_tool");
        assert_eq!(tool_calls[0].provider_id.as_deref(), Some("p0"));
        assert_eq!(tool_calls[0].arguments, json!({}));
        context.push(message);
        if let Assembled::Invalid(invalid) = &response.calls()[0] {
            context.push(invalid.error_result());
        }
        assert_eq!(context.validate(), Ok(()));
    }
}
