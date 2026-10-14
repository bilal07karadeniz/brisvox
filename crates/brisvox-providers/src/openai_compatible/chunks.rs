//! Maps `chat.completion.chunk` JSON objects (the `data` of each SSE event) to [`LlmEvent`]s.
//!
//! One tolerant reader for every vendor (docs/providers/openai-compatible.md, "Known
//! deviations"):
//!
//! - tool calls arrive as fragments (first with id and name, then arguments) or whole (id, name
//!   and the full arguments in one delta); `index` may be missing or `null`; `id` may be
//!   missing, empty or repeated; `arguments` may be a JSON string or (defensively) an object;
//! - `finish_reason` may be `stop`, `length`, `tool_calls`, `function_call` (deprecated), `eos`
//!   (Together), `content_filter` or anything else; whatever it says, a response that called
//!   tools ends in [`FinishReason::ToolCalls`] unless it was cut by `length`;
//! - usage may sit at the top level, in `x_groq.usage` (Groq) or in the choice; the last one
//!   seen is reported; usage never delays `Done` beyond the grace period of the adapter;
//! - `delta.reasoning`, `reasoning_content`, `refusal`, `role` and every unknown field are
//!   ignored: reasoning must never reach TTS;
//! - an `error` member (object with `message`/`type`/`code`, or a bare string; also
//!   `x_groq.error`) ends the response with an error event;
//! - `choices` may be empty or `null` (usage chunks, keep-alives); only the first choice is read
//!   (`n` is 1);
//! - the chunk `id` is never used (Ollama's is a random number below 999).
//!
//! Free-text error messages are never copied into errors or logs: they can echo prompt content
//! or a fragment of the key. Only machine-readable `type` / `code` values are used, sanitised.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde_json::value::RawValue;

use super::config::{Quirks, UsagePlacement};
use crate::error::Error;
use crate::http_llm::{protocol, sanitize};
use crate::llm::{FinishReason, LlmEvent, Usage};

/// Most tool calls one response may open; more is a protocol error (bounds memory).
pub(crate) const MAX_CALLS: usize = 128;

/// Most bytes of arguments one tool call may stream (1 MiB).
pub(crate) const MAX_CALL_ARGS_BYTES: usize = 1024 * 1024;

/// Most bytes of arguments all tool calls of one response may stream together (4 MiB).
pub(crate) const MAX_TOTAL_ARGS_BYTES: usize = 4 * 1024 * 1024;

/// What the pump does after a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep reading.
    Continue,
    /// The chunk with `finish_reason` was seen without usage, and the vendor may still send a
    /// trailing usage chunk: keep reading, but only for the usage grace period.
    AwaitUsage,
    /// `Done` is in the output: stop reading.
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

/// `raw` as a non-empty string.
fn non_empty(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    text(raw).filter(|t| !t.is_empty())
}

/// `raw` as an integer, `None` for absent, `null` or anything else.
fn integer(raw: Option<&RawValue>) -> Option<i64> {
    serde_json::from_str::<i64>(raw?.get()).ok()
}

/// A chunk, read leniently: members with a surprising type are kept raw and interpreted
/// individually, so one odd field does not turn a good answer into an error.
#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    choices: Option<FirstChoice<'a>>,
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
    #[serde(default, borrow)]
    x_groq: Option<XGroq<'a>>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
    /// The upstream provider a gateway such as OpenRouter routed the request to.
    #[serde(default, borrow)]
    provider: Option<&'a RawValue>,
    /// The model the server resolved the request to.
    #[serde(default, borrow)]
    model: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct XGroq<'a> {
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

/// The first element of `choices`, read without allocating a vector for the rest.
struct FirstChoice<'a>(Option<Choice<'a>>);

impl<'de: 'a, 'a> Deserialize<'de> for FirstChoice<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = FirstChoice<'a>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array of choices")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let first = seq.next_element::<Choice<'a>>()?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FirstChoice(first))
            }
        }
        deserializer.deserialize_seq(V(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(default, borrow)]
    delta: Option<Delta<'a>>,
    #[serde(default, borrow)]
    finish_reason: Option<&'a RawValue>,
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(default, borrow)]
    content: Option<&'a RawValue>,
    #[serde(default, borrow)]
    tool_calls: Option<Vec<WireToolCall<'a>>>,
}

#[derive(Deserialize)]
struct WireToolCall<'a> {
    #[serde(default, borrow)]
    index: Option<&'a RawValue>,
    #[serde(default, borrow)]
    id: Option<&'a RawValue>,
    #[serde(default, borrow)]
    function: Option<WireFunction<'a>>,
}

#[derive(Deserialize)]
struct WireFunction<'a> {
    #[serde(default, borrow)]
    name: Option<&'a RawValue>,
    #[serde(default, borrow)]
    arguments: Option<&'a RawValue>,
}

/// The `arguments` of a tool-call fragment as text: a JSON string is decoded, a JSON object or
/// array (a vendor that does not stringify) is kept as its own JSON text, anything else is empty.
fn arguments(raw: Option<&RawValue>) -> Cow<'_, str> {
    let Some(raw) = raw else {
        return Cow::Borrowed("");
    };
    match raw.get().as_bytes().first() {
        Some(b'"') => text(Some(raw)).unwrap_or(Cow::Borrowed("")),
        Some(b'{' | b'[') => Cow::Borrowed(raw.get()),
        _ => Cow::Borrowed(""),
    }
}

/// The token counts of a `usage` object.
#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `raw` as usage; an unreadable or empty `usage` is ignored (it must not fail a good answer).
fn usage_of(raw: Option<&RawValue>) -> Option<Usage> {
    let parsed: WireUsage = serde_json::from_str(raw?.get()).ok()?;
    if parsed.prompt_tokens.is_none() && parsed.completion_tokens.is_none() {
        return None;
    }
    let input = parsed.prompt_tokens.unwrap_or(0);
    let cached = parsed
        .prompt_tokens_details
        .and_then(|d| d.cached_tokens)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: clamp(input),
        cached_input_tokens: clamp(cached.min(input)),
        output_tokens: clamp(parsed.completion_tokens.unwrap_or(0)),
    })
}

/// How the adapter reads an `error` member: the machine-readable parts only.
#[derive(Deserialize)]
struct WireError<'a> {
    #[serde(default, borrow)]
    r#type: Option<&'a RawValue>,
    #[serde(default, borrow)]
    code: Option<&'a RawValue>,
    #[serde(default, borrow)]
    status: Option<&'a RawValue>,
}

/// A value that is a string or a number, as text.
fn scalar(raw: Option<&RawValue>) -> Option<String> {
    let raw = raw?;
    match raw.get().as_bytes().first()? {
        b'"' => text(Some(raw)).map(|t| t.into_owned()),
        b'0'..=b'9' | b'-' => Some(raw.get().to_owned()),
        _ => None,
    }
}

/// The sanitised `type` and `code` of an `error` member, for logs: `error` may be an object, or a
/// bare string (Ollama native, whose text is never kept).
pub(crate) fn error_parts(raw: Option<&RawValue>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None);
    };
    let Ok(parsed) = serde_json::from_str::<WireError<'_>>(raw.get()) else {
        return (None, None);
    };
    (
        scalar(parsed.r#type).map(|s| sanitize(&s)),
        scalar(parsed.code.or(parsed.status)).map(|s| sanitize(&s)),
    )
}

/// Maps the `error` member of a stream chunk to an [`Error`]: a numeric `code` or `status` of 400
/// to 599 is that HTTP status; known `type` / `code` words map to 429, 500, 503, 400 or 401;
/// anything else is a protocol error naming the (sanitised) type and code.
fn stream_error(provider: &'static str, raw: &RawValue) -> Error {
    let (kind, code) = error_parts(Some(raw));
    let numeric = code
        .as_deref()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|s| (400..600).contains(s));
    let by_word = |word: &str| match word {
        "rate_limit_exceeded" | "rate_limit_error" | "insufficient_quota" | "too_many_requests" => {
            Some(429)
        }
        "server_error" | "api_error" | "internal_error" | "internal_server_error" => Some(500),
        "server_is_overloaded" | "service_unavailable" | "overloaded" | "overloaded_error" => {
            Some(503)
        }
        "invalid_request_error" | "context_length_exceeded" | "bad_request" => Some(400),
        "authentication_error" | "invalid_api_key" | "unauthorized" => Some(401),
        _ => None,
    };
    let status = numeric
        .or_else(|| code.as_deref().and_then(by_word))
        .or_else(|| kind.as_deref().and_then(by_word));
    match status {
        Some(status) => Error::Rejected { provider, status },
        None => protocol(
            provider,
            format!(
                "the stream carried an error (type {}, code {})",
                kind.as_deref().unwrap_or("none"),
                code.as_deref().unwrap_or("none")
            ),
        ),
    }
}

/// The finish reason a vendor's `finish_reason` word stands for.
fn finish_of(word: &str) -> FinishReason {
    match word {
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        // `stop`, `eos` (Together), `content_filter` (no variant of its own) and anything new.
        _ => FinishReason::Stop,
    }
}

/// One tool call being assembled.
#[derive(Debug)]
struct Call {
    /// The call's `index` upstream, if the vendor sent one.
    upstream: Option<i64>,
    /// The id handed to the consumer (real, or synthetic `call_<n>`).
    id: Arc<str>,
    /// `id` is the vendor's, not synthetic.
    real_id: bool,
    /// The name emitted for this call, once a fragment carrying it was seen.
    name: Option<Arc<str>>,
    args_bytes: usize,
}

/// Maps chunks to events and tracks the end of the response.
#[derive(Debug)]
pub(crate) struct Mapper {
    provider: &'static str,
    quirks: Quirks,
    calls: Vec<Call>,
    total_args: usize,
    /// `finish_reason` seen on a chunk.
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    synthetic: u32,
    done: bool,
    /// The `provider` and `model` of the first chunk that named either (a gateway such as
    /// OpenRouter says which upstream served the request), until [`Self::take_upstream`].
    upstream: Option<Upstream>,
    upstream_seen: bool,
}

/// Who served a request, as the response says: the gateway's upstream `provider` and the
/// resolved `model` (either may be missing). Informational: nothing branches on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Upstream {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
}

/// A JSON string member as text (surprising types and control characters are ignored).
fn short_text(raw: Option<&RawValue>) -> Option<String> {
    let text: String = serde_json::from_str(raw?.get()).ok()?;
    (!text.is_empty() && text.chars().count() <= 128 && !text.chars().any(char::is_control))
        .then_some(text)
}

impl Mapper {
    /// A mapper for a vendor with `quirks`; errors name `provider`.
    pub(crate) fn new(provider: &'static str, quirks: Quirks) -> Self {
        Self {
            provider,
            quirks,
            calls: Vec::new(),
            total_args: 0,
            finish: None,
            usage: None,
            synthetic: 0,
            done: false,
            upstream: None,
            upstream_seen: false,
        }
    }

    /// The upstream provider and model the first chunk that named one reported; returned once.
    pub(crate) fn take_upstream(&mut self) -> Option<Upstream> {
        self.upstream.take()
    }

    /// Maps the data of one SSE event, appending events to `out`.
    ///
    /// # Errors
    /// [`Error::Protocol`] for an unreadable chunk or a limit violation, [`Error::Rejected`] for
    /// an error chunk that names a status.
    pub(crate) fn handle(&mut self, data: &str, out: &mut Vec<LlmEvent>) -> Result<Flow, Error> {
        let data = data.trim();
        if data == "[DONE]" {
            if self.finish.is_none() && !self.done && !self.calls.is_empty() {
                // A gateway that aborted the generation mid tool call still writes `[DONE]`:
                // the arguments may be truncated JSON, so this is a cut, exactly like an end of
                // body without a `finish_reason`, and never a tool-call turn to run.
                return Err(Error::Disconnected {
                    provider: self.provider,
                });
            }
            return Ok(self.finish_now(out));
        }
        if data.is_empty() {
            return Ok(Flow::Continue);
        }
        if self.done {
            return Ok(Flow::Finished);
        }
        let chunk: Chunk<'_> = serde_json::from_str(data)
            .map_err(|_| protocol(self.provider, "a stream chunk is not a valid JSON object"))?;
        if let Some(error) = chunk
            .error
            .or_else(|| chunk.x_groq.as_ref().and_then(|g| g.error))
        {
            return Err(stream_error(self.provider, error));
        }
        if !self.upstream_seen {
            let provider = short_text(chunk.provider);
            let model = short_text(chunk.model);
            if provider.is_some() || model.is_some() {
                self.upstream_seen = true;
                self.upstream = Some(Upstream { provider, model });
            }
        }
        let choice = chunk.choices.and_then(|c| c.0);
        let usage_now = usage_of(chunk.usage)
            .or_else(|| usage_of(chunk.x_groq.as_ref().and_then(|g| g.usage)))
            .or_else(|| usage_of(choice.as_ref().and_then(|c| c.usage)));
        let usage_in_chunk = usage_now.is_some();
        let had_finish = self.finish.is_some();
        if let Some(choice) = choice {
            if let Some(delta) = choice.delta {
                if let Some(content) = non_empty(delta.content) {
                    out.push(LlmEvent::TextDelta(Arc::from(&*content)));
                }
                if let Some(calls) = delta.tool_calls {
                    for call in &calls {
                        self.tool_call(call, out)?;
                    }
                }
            }
            if let Some(word) = non_empty(choice.finish_reason) {
                self.finish = Some(finish_of(&word));
            }
        }
        if let Some(usage) = usage_now {
            // The last usage seen wins: a vendor with running totals on every chunk ends with
            // the final one.
            self.usage = Some(usage);
            if had_finish {
                return Ok(self.finish_now(out));
            }
        }
        if self.finish.is_some() && !had_finish {
            // Only usage on the finish chunk itself is final: earlier usage may be a running
            // total (vLLM `continuous_usage_stats`) and the real one is still to come.
            let waits = self.quirks.usage == UsagePlacement::TrailingChunk && !usage_in_chunk;
            return Ok(if waits {
                Flow::AwaitUsage
            } else {
                self.finish_now(out)
            });
        }
        Ok(Flow::Continue)
    }

    /// Ends the response with the information at hand (a missing usage is not waited for).
    pub(crate) fn finish_now(&mut self, out: &mut Vec<LlmEvent>) -> Flow {
        if !self.done {
            self.done = true;
            let finish = match self.finish.unwrap_or(FinishReason::Stop) {
                FinishReason::Stop if !self.calls.is_empty() => FinishReason::ToolCalls,
                other => other,
            };
            out.push(LlmEvent::Done {
                finish,
                usage: self.usage,
                provider_data: None,
            });
        }
        Flow::Finished
    }

    /// The body ended: a response that already reached its `finish_reason` is complete (its usage
    /// chunk, if any, did not come); anything else was cut off. Returns whether `Done` was
    /// produced.
    pub(crate) fn finish_at_eof(&mut self, out: &mut Vec<LlmEvent>) -> bool {
        if self.finish.is_none() || self.done {
            return false;
        }
        self.finish_now(out);
        true
    }

    /// Which call a fragment belongs to, opening a new one when it starts one.
    fn resolve(
        &mut self,
        index: Option<i64>,
        id: Option<&str>,
        name: Option<&str>,
    ) -> Result<(usize, bool), Error> {
        // A fragment that names a function other than the one the call already has can never
        // continue it: a vendor may number every whole call 0 and send no id (Ollama ids are
        // `omitempty`), so the name tells the calls apart.
        let same_name = |c: &Call| match (name, c.name.as_deref()) {
            (Some(new), Some(sent)) => new == sent,
            _ => true,
        };
        let existing = match (index, id) {
            // A different id on a known index is a new call (a vendor that numbers every call 0).
            (Some(ix), id) => self.calls.iter().rposition(|c| {
                c.upstream == Some(ix)
                    && id.is_none_or(|id| !c.real_id || &*c.id == id)
                    && same_name(c)
            }),
            (None, Some(id)) => self.calls.iter().rposition(|c| &*c.id == id),
            // No index and no id: a fragment that names a function opens a call, one that only
            // carries arguments continues the newest.
            (None, None) if name.is_some() => None,
            (None, None) => self.calls.len().checked_sub(1),
        };
        if let Some(slot) = existing {
            return Ok((slot, false));
        }
        if self.calls.len() >= MAX_CALLS {
            return Err(protocol(
                self.provider,
                format!("more than {MAX_CALLS} tool calls in one response"),
            ));
        }
        if id.is_none() && !self.quirks.synthesize_missing_ids {
            return Err(protocol(self.provider, "a tool call has no id"));
        }
        let (id, real_id) = match id {
            Some(id) => (Arc::from(id), true),
            None => {
                self.synthetic += 1;
                (Arc::from(format!("call_{}", self.synthetic)), false)
            }
        };
        self.calls.push(Call {
            upstream: index,
            id,
            real_id,
            name: None,
            args_bytes: 0,
        });
        Ok((self.calls.len() - 1, true))
    }

    fn tool_call(&mut self, call: &WireToolCall<'_>, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let id = non_empty(call.id);
        let (name, args) = match &call.function {
            Some(f) => (non_empty(f.name), arguments(f.arguments)),
            None => (None, Cow::Borrowed("")),
        };
        let (slot, is_new) = self.resolve(integer(call.index), id.as_deref(), name.as_deref())?;
        let provider = self.provider;
        let Some(state) = self.calls.get_mut(slot) else {
            return Err(protocol(provider, "tool call bookkeeping is inconsistent"));
        };
        state.args_bytes = state.args_bytes.saturating_add(args.len());
        self.total_args = self.total_args.saturating_add(args.len());
        if state.args_bytes > MAX_CALL_ARGS_BYTES || self.total_args > MAX_TOTAL_ARGS_BYTES {
            return Err(protocol(
                provider,
                "tool call arguments exceed the size limit",
            ));
        }
        let send_name = name.is_some() && state.name.is_none();
        if !is_new && !send_name && args.is_empty() {
            return Ok(());
        }
        if send_name {
            state.name = name.as_deref().map(Arc::from);
        }
        out.push(LlmEvent::ToolCallDelta {
            index: u32::try_from(slot).unwrap_or(u32::MAX),
            id: is_new.then(|| Arc::clone(&state.id)),
            name: name.filter(|_| send_name).map(|n| Arc::from(&*n)),
            arguments: Arc::from(&*args),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quirks() -> Quirks {
        Quirks::default()
    }

    fn run(chunks: &[&str]) -> (Vec<LlmEvent>, Vec<Result<Flow, String>>) {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        let mut flows = Vec::new();
        for chunk in chunks {
            flows.push(mapper.handle(chunk, &mut out).map_err(|e| e.to_string()));
        }
        (out, flows)
    }

    fn calls(events: &[LlmEvent]) -> Vec<(u32, Option<String>, Option<String>, String)> {
        events
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
            .collect()
    }

    #[test]
    fn the_upstream_provider_of_a_gateway_is_reported_once() {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        let first = r#"{"id":"gen-1","provider":"OpenAI","model":"openai/gpt-6-luna-20260922","choices":[{"delta":{"content":"Hi"}}]}"#;
        mapper.handle(first, &mut out).unwrap();
        assert_eq!(
            mapper.take_upstream(),
            Some(Upstream {
                provider: Some("OpenAI".into()),
                model: Some("openai/gpt-6-luna-20260922".into()),
            })
        );
        let second = r#"{"provider":"Azure","choices":[{"delta":{"content":"!"}}]}"#;
        mapper.handle(second, &mut out).unwrap();
        assert_eq!(mapper.take_upstream(), None, "only the first chunk counts");
        // A surprising type is ignored, not an error.
        let mut mapper = Mapper::new("test", quirks());
        let odd = r#"{"provider":{"name":"x"},"model":7,"choices":[{"delta":{"content":"Hi"}}]}"#;
        mapper.handle(odd, &mut out).unwrap();
        assert_eq!(mapper.take_upstream(), None);
    }

    #[test]
    fn fragments_and_whole_calls_use_one_path() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}"#,
        ]);
        assert_eq!(
            calls(&events),
            vec![
                (0, Some("a".into()), Some("f".into()), String::new()),
                (0, None, None, "{\"x\":".into()),
                (0, None, None, "1}".into()),
                (1, Some("b".into()), Some("g".into()), "{}".into()),
            ]
        );
    }

    #[test]
    fn a_missing_index_and_id_is_told_apart_by_the_name() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"f","arguments":"{\"a\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":null,"function":{"name":"g","arguments":"{}"}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, 0);
        assert_eq!(got[0].1.as_deref(), Some("call_1"));
        assert_eq!(got[1], (0, None, None, "1}".into()));
        assert_eq!(got[2].0, 1);
        assert_eq!(got[2].1.as_deref(), Some("call_2"));
    }

    #[test]
    fn a_new_id_on_a_used_index_opens_a_new_call() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"x"}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(got[1].0, 1);
        // The argument-only fragment continues the newest call that has this index.
        assert_eq!(got[2], (1, None, None, "x".into()));
    }

    #[test]
    fn whole_calls_sharing_one_index_and_no_id_are_told_apart_by_the_name() {
        // Ollama ids are `omitempty` and the position of `index` across parallel whole calls is
        // UNVERIFIED: two whole calls may both say index 0 and carry no id.
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{\"a\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"x"}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(
            got,
            vec![
                (
                    0,
                    Some("call_1".into()),
                    Some("f".into()),
                    "{\"a\":1}".into()
                ),
                (1, Some("call_2".into()), Some("g".into()), "{}".into()),
                // An argument-only fragment continues the newest call of that index.
                (1, None, None, "x".into()),
            ]
        );
    }

    #[test]
    fn a_repeated_name_on_the_same_index_is_still_a_continuation() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"}"}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], (0, None, None, "}".into()));
    }

    #[test]
    fn empty_ids_and_names_count_as_absent_and_object_arguments_are_kept_as_json() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","function":{"name":"","arguments":{"k":1}}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.as_deref(), Some("call_1"));
        assert_eq!(got[0].2, None);
        assert_eq!(got[0].3, r#"{"k":1}"#);
    }

    #[test]
    fn a_name_that_arrives_late_is_emitted_once() {
        let (events, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"}"}}]}}]}"#,
        ]);
        let got = calls(&events);
        assert_eq!(got[0].2, None);
        assert_eq!(got[1].2.as_deref(), Some("f"));
        assert_eq!(got[2].2, None);
    }

    #[test]
    fn a_missing_id_is_an_error_when_synthesis_is_off() {
        let mut mapper = Mapper::new(
            "test",
            Quirks {
                synthesize_missing_ids: false,
                ..quirks()
            },
        );
        let mut out = Vec::new();
        let r = mapper.handle(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f"}}]}}]}"#,
            &mut out,
        );
        assert!(matches!(r, Err(Error::Protocol { .. })));
    }

    #[test]
    fn finish_words_map_and_tool_calls_win_over_stop_but_not_over_length() {
        for (word, calls_seen, want) in [
            ("stop", false, FinishReason::Stop),
            ("eos", false, FinishReason::Stop),
            ("content_filter", false, FinishReason::Stop),
            ("length", false, FinishReason::Length),
            ("tool_calls", false, FinishReason::ToolCalls),
            ("function_call", false, FinishReason::ToolCalls),
            ("stop", true, FinishReason::ToolCalls),
            ("length", true, FinishReason::Length),
        ] {
            let mut mapper = Mapper::new("test", quirks());
            let mut out = Vec::new();
            if calls_seen {
                mapper
                    .handle(
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}"#,
                        &mut out,
                    )
                    .unwrap();
            }
            let chunk = format!(r#"{{"choices":[{{"delta":{{}},"finish_reason":"{word}"}}]}}"#);
            assert_eq!(mapper.handle(&chunk, &mut out).unwrap(), Flow::AwaitUsage);
            mapper.handle("[DONE]", &mut out).unwrap();
            assert!(matches!(
                out.last(),
                Some(LlmEvent::Done { finish, .. }) if *finish == want
            ));
        }
    }

    #[test]
    fn usage_is_read_from_every_documented_place() {
        for chunk in [
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6}}}"#,
            r#"{"choices":[],"x_groq":{"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6}}}}"#,
            r#"{"choices":[{"delta":{},"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6}}}]}"#,
        ] {
            let mut mapper = Mapper::new("test", quirks());
            let mut out = Vec::new();
            mapper
                .handle(
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                    &mut out,
                )
                .unwrap();
            assert_eq!(mapper.handle(chunk, &mut out).unwrap(), Flow::Finished);
            let Some(LlmEvent::Done { usage, .. }) = out.pop() else {
                panic!("no Done for {chunk}");
            };
            assert_eq!(
                usage,
                Some(Usage {
                    input_tokens: 10,
                    cached_input_tokens: 6,
                    output_tokens: 4
                })
            );
        }
    }

    #[test]
    fn usage_on_the_finish_chunk_ends_the_response_at_once() {
        let (events, flows) = run(&[
            r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        ]);
        assert_eq!(flows, vec![Ok(Flow::Finished)]);
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Done { usage: Some(u), .. }) if u.input_tokens == 3
        ));
    }

    #[test]
    fn running_usage_before_the_finish_chunk_is_not_final() {
        // A vendor with running totals on content chunks (vLLM `continuous_usage_stats`): the
        // finish chunk has none, so the response waits for the real one and reports it.
        let (events, flows) = run(&[
            r#"{"choices":[{"delta":{"content":"a"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
            r#"{"choices":[{"delta":{"content":"b"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":9}}"#,
        ]);
        assert_eq!(
            flows,
            vec![
                Ok(Flow::Continue),
                Ok(Flow::Continue),
                Ok(Flow::AwaitUsage),
                Ok(Flow::Finished)
            ]
        );
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Done { usage: Some(u), .. }) if u.output_tokens == 9
        ));
        // And when the grace expires first, the last running total is what is reported.
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        for chunk in [
            r#"{"choices":[{"delta":{"content":"a"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ] {
            mapper.handle(chunk, &mut out).unwrap();
        }
        mapper.finish_now(&mut out);
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done { usage: Some(u), .. }) if u.output_tokens == 2
        ));
    }

    #[test]
    fn quirks_decide_whether_the_end_waits_for_usage() {
        for (placement, flow) in [
            (UsagePlacement::TrailingChunk, Flow::AwaitUsage),
            (UsagePlacement::FinishChunk, Flow::Finished),
            (UsagePlacement::Absent, Flow::Finished),
        ] {
            let mut mapper = Mapper::new(
                "test",
                Quirks {
                    usage: placement,
                    ..quirks()
                },
            );
            let mut out = Vec::new();
            let got = mapper
                .handle(
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                    &mut out,
                )
                .unwrap();
            assert_eq!(got, flow);
        }
    }

    #[test]
    fn eof_after_a_finish_reason_completes_and_eof_without_one_does_not() {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        mapper
            .handle(r#"{"choices":[{"delta":{"content":"x"}}]}"#, &mut out)
            .unwrap();
        assert!(!mapper.finish_at_eof(&mut out));
        mapper
            .handle(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                &mut out,
            )
            .unwrap();
        assert!(mapper.finish_at_eof(&mut out));
        assert!(matches!(
            out.last(),
            Some(LlmEvent::Done { usage: None, .. })
        ));
    }

    #[test]
    fn text_reasoning_and_odd_chunks() {
        let (events, flows) = run(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"hmm","reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel","refusal":null,"tool_calls":null}}]}"#,
            r#"{"choices":null}"#,
            r#"{"choices":[]}"#,
            r#"{"id":"chatcmpl-7","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"lo"},"logprobs":null,"finish_reason":null}],"usage":null,"warnings":[1]}"#,
            "",
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta(t) => Some(t.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(flows.iter().all(|f| *f == Ok(Flow::Continue)));
    }

    #[test]
    fn done_with_open_tool_calls_and_no_finish_reason_is_a_cut() {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        mapper
            .handle(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{\"city\":\"Par"}}]}}]}"#,
                &mut out,
            )
            .unwrap();
        let result = mapper.handle("[DONE]", &mut out);
        assert!(
            matches!(result, Err(Error::Disconnected { .. })),
            "{result:?}"
        );
        assert!(
            !out.iter().any(|e| matches!(e, LlmEvent::Done { .. })),
            "{out:?}"
        );
    }

    #[test]
    fn done_without_a_finish_reason_still_completes() {
        let (events, flows) = run(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#, "[DONE]"]);
        assert_eq!(flows.last(), Some(&Ok(Flow::Finished)));
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Done {
                finish: FinishReason::Stop,
                ..
            })
        ));
    }

    #[test]
    fn error_chunks_map_by_status_or_word_and_never_echo_the_message() {
        for (chunk, status) in [
            (
                r#"{"error":{"message":"sk-secret","type":"rate_limit_exceeded","code":null}}"#,
                Some(429),
            ),
            (r#"{"error":{"message":"x","code":503}}"#, Some(503)),
            (r#"{"error":{"message":"x","code":"500"}}"#, Some(500)),
            (
                r#"{"error":{"message":"x","type":"server_error"}}"#,
                Some(500),
            ),
            (
                r#"{"error":{"message":"x","type":"invalid_request_error"}}"#,
                Some(400),
            ),
            (
                r#"{"x_groq":{"error":{"message":"x","code":429}}}"#,
                Some(429),
            ),
            (
                r#"{"error":"the model failed to generate a response"}"#,
                None,
            ),
            (r#"{"error":{"message":"x","type":"weird thing!"}}"#, None),
        ] {
            let mut mapper = Mapper::new("test", quirks());
            let mut out = Vec::new();
            let err = mapper.handle(chunk, &mut out).unwrap_err();
            let shown = err.to_string();
            assert!(!shown.contains("sk-secret"), "{shown}");
            assert!(!shown.contains("failed to generate"), "{shown}");
            match (status, err) {
                (Some(want), Error::Rejected { status, .. }) => assert_eq!(status, want),
                (None, Error::Protocol { .. }) => {}
                (want, other) => panic!("{chunk}: {want:?} vs {other:?}"),
            }
        }
    }

    #[test]
    fn a_malformed_chunk_is_a_protocol_error_that_does_not_echo_it() {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        let err = mapper
            .handle("{this is not json sk-secret", &mut out)
            .unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }));
        assert!(!err.to_string().contains("sk-secret"));
        assert!(mapper.handle("[1,2]", &mut out).is_err());
    }

    #[test]
    fn size_limits_are_enforced() {
        let mut mapper = Mapper::new("test", quirks());
        let mut out = Vec::new();
        let big = "a".repeat(MAX_CALL_ARGS_BYTES + 1);
        let chunk = format!(
            r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"a","function":{{"name":"f","arguments":"{big}"}}}}]}}}}]}}"#
        );
        assert!(mapper.handle(&chunk, &mut out).is_err());
        let mut mapper = Mapper::new("test", quirks());
        for i in 0..MAX_CALLS {
            let c = format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":{i},"id":"c{i}","function":{{"name":"f"}}}}]}}}}]}}"#
            );
            mapper.handle(&c, &mut out).unwrap();
        }
        let c = r#"{"choices":[{"delta":{"tool_calls":[{"index":999,"id":"z","function":{"name":"f"}}]}}]}"#;
        assert!(mapper.handle(c, &mut out).is_err());
    }
}
