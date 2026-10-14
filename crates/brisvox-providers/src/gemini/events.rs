//! Maps `GenerateContentResponse` chunks (the JSON `data` of each SSE event) to [`LlmEvent`]s.
//!
//! Every chunk is one complete `GenerateContentResponse`; there is no event type to dispatch
//! on. Only `candidates[0]` is read (`candidateCount` is 1). All `parts` of a chunk are walked,
//! in order (a chunk may hold text and a function call, and a call need not be the last part).
//! Free-text error messages are never copied into errors or logs: they can echo prompt content.
//! Only machine-readable codes (`status`, `finishReason`, `blockReason`) are used, sanitised.
//!
//! # Parts
//! - `text` without `thought: true` is a [`LlmEvent::TextDelta`]; empty text is skipped (it may
//!   carry only a `thoughtSignature`, which is recorded). `thought: true` parts are thought
//!   summaries and are never spoken.
//! - `functionCall` arrives whole (`args` is a JSON object, not a string): one
//!   [`LlmEvent::ToolCallDelta`] with `index` = position among the response's calls, the call's
//!   `id` (made up as `brisvox_call_<index>` when the API sent none; such an id is never sent
//!   back) and the whole `args` JSON text (`{}` when absent).
//!
//! # Ending
//! `finishReason` stays empty while the model is generating. The turn ends when it is seen and
//! `usageMetadata` has been seen (usually the same chunk), or at the clean end of the body or a
//! `[DONE]` line, whichever is first: the last chunk may not carry the usage (UNVERIFIED).
//! `STOP` is [`FinishReason::ToolCalls`] when a call was seen (there is no dedicated value),
//! `MAX_TOKENS` is [`FinishReason::Length`]. Filtered or failed turns: see [`finish_error`].
//!
//! # Replay data
//! [`LlmEvent::Done::provider_data`] is the JSON array of the response's parts as received:
//! text parts (adjacent unsigned ones coalesced, a signed one never merged with another),
//! `functionCall` parts with their `thoughtSignature`. It is `None` when there is no call and
//! no signature to replay.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::PROVIDER;
use super::request::RenderedTools;
use crate::error::Error;
use crate::llm::{FinishReason, LlmEvent, Usage};

/// Prefix of the ids the adapter makes up for function calls the API gave none.
pub(crate) const SYNTHETIC_ID_PREFIX: &str = "brisvox_call_";

/// The id made up for the `ordinal`-th function call of a response.
pub(crate) fn synthetic_id(ordinal: usize) -> String {
    format!("{SYNTHETIC_ID_PREFIX}{ordinal}")
}

/// Most function calls one response may carry; more is a protocol error (bounds memory).
const MAX_CALLS: usize = 128;

/// Most bytes of arguments one call may carry (1 MiB).
pub(crate) const MAX_CALL_ARGS_BYTES: usize = 1024 * 1024;

/// Most bytes of arguments all calls of one response may carry together (4 MiB).
pub(crate) const MAX_TOTAL_ARGS_BYTES: usize = 4 * 1024 * 1024;

/// Most bytes of replayable parts one response may carry (16 MiB): the text, the call names,
/// ids and arguments, and the `thoughtSignature` strings, plus a fixed overhead per part.
const MAX_REPLAY_BYTES: usize = 16 * 1024 * 1024;

/// Longest code kept in an error detail.
const MAX_CODE: usize = 64;

/// What the pump does after a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep reading.
    Continue,
    /// `Done` is in the output: stop reading.
    Finished,
}

#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    candidates: Option<Vec<Candidate<'a>>>,
    #[serde(default, rename = "promptFeedback", borrow)]
    prompt_feedback: Option<&'a RawValue>,
    #[serde(default, rename = "usageMetadata", borrow)]
    usage: Option<&'a RawValue>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Candidate<'a> {
    #[serde(default, borrow)]
    content: Option<CandidateContent<'a>>,
    #[serde(default, rename = "finishReason", borrow)]
    finish_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct CandidateContent<'a> {
    #[serde(default, borrow)]
    parts: Option<Vec<ResponsePart<'a>>>,
}

#[derive(Deserialize)]
struct ResponsePart<'a> {
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall", alias = "function_call", borrow)]
    function_call: Option<FunctionCall<'a>>,
    #[serde(
        default,
        rename = "thoughtSignature",
        alias = "thought_signature",
        borrow
    )]
    signature: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct FunctionCall<'a> {
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    args: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct PromptFeedback<'a> {
    #[serde(default, rename = "blockReason", borrow)]
    block_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt: u64,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates: u64,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts: u64,
}

#[derive(Deserialize)]
struct ErrorObject<'a> {
    #[serde(default)]
    code: Option<u64>,
    #[serde(default, borrow)]
    status: Option<Cow<'a, str>>,
}

/// A part as the next request must replay it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Recorded {
    Text {
        text: String,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Call {
        #[serde(rename = "functionCall")]
        call: RecordedCall,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct RecordedCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    args: Box<RawValue>,
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

/// `s` reduced to a short, log-safe token (`[A-Za-z0-9_.-]`, at most [`MAX_CODE`] characters).
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(MAX_CODE)
        .collect()
}

/// Maps the `code` / gRPC `status` of an in-band error object to an [`Error`]: an HTTP-like code
/// (400-599) is that status, otherwise the status name decides; anything else is a protocol error
/// naming the (sanitised) status.
pub(crate) fn error_for(code: Option<u64>, status: Option<&str>) -> Error {
    let from_code = code
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..600).contains(c));
    let from_status = status.and_then(|s| match s {
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "OUT_OF_RANGE" => Some(400),
        "UNAUTHENTICATED" => Some(401),
        "PERMISSION_DENIED" => Some(403),
        "NOT_FOUND" => Some(404),
        "RESOURCE_EXHAUSTED" => Some(429),
        "CANCELLED" => Some(499),
        "INTERNAL" | "UNKNOWN" | "DATA_LOSS" => Some(500),
        "UNAVAILABLE" => Some(503),
        "DEADLINE_EXCEEDED" => Some(504),
        _ => None,
    });
    match from_code.or(from_status) {
        Some(status) => Error::Rejected {
            provider: PROVIDER,
            status,
        },
        None => protocol(format!(
            "the response carried an error with status {}",
            status.map_or_else(|| "(none)".to_owned(), sanitize)
        )),
    }
}

/// What a `finishReason` means for the turn.
enum Ending {
    /// A normal end (`STOP`, `MAX_TOKENS`).
    Normal(FinishReason),
    /// The model produced something unusable: the call or the thought signature is invalid.
    Failed,
    /// The turn was filtered or cut for a policy reason.
    Filtered,
    /// A reason this adapter does not know: treated like `STOP` only after spoken text.
    Unknown,
}

fn ending_of(reason: &str, calls: usize) -> Ending {
    match reason {
        "STOP" => Ending::Normal(if calls > 0 {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }),
        "MAX_TOKENS" => Ending::Normal(FinishReason::Length),
        "MALFORMED_FUNCTION_CALL"
        | "MISSING_THOUGHT_SIGNATURE"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "MALFORMED_RESPONSE"
        | "LANGUAGE" => Ending::Failed,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "NO_IMAGE"
        | "OTHER" => Ending::Filtered,
        _ => Ending::Unknown,
    }
}

/// The error a failed or filtered finish becomes, `None` when the turn may end normally.
///
/// A failed turn (`MALFORMED_FUNCTION_CALL`, `MISSING_THOUGHT_SIGNATURE`, `UNEXPECTED_TOOL_CALL`,
/// `TOO_MANY_TOOL_CALLS`, `MALFORMED_RESPONSE`, `LANGUAGE`) is [`Error::Protocol`] naming the
/// reason: no call may be executed.
///
/// A filtered turn (`SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII`, `OTHER`,
/// image reasons) is `Rejected { 400 }` (the provider refused the content) when it produced
/// nothing **or when it already emitted a function call**: a response the provider cut for
/// policy must not get its side-effect tool executed. One that only spoke ends as `Stop` (the
/// caller heard the part that came through) and is logged.
///
/// A reason this adapter does not know (`ESCALATION`, `PUP_LIMITED_DISABLED`, future values) is
/// treated like a normal stop only when text was already spoken and no call is pending; with no
/// output at all, or after a call, it is [`Error::Protocol`] naming the (sanitised) reason: a
/// silent empty "success" would leave the caller mute, and an unvouched call must not run.
fn finish_error(reason: &str, ending: &Ending, emitted: bool, calls: usize) -> Option<Error> {
    match ending {
        Ending::Failed => Some(protocol(format!(
            "the model ended the turn with finishReason {}",
            sanitize(reason)
        ))),
        Ending::Filtered if !emitted || calls > 0 => Some(Error::Rejected {
            provider: PROVIDER,
            status: 400,
        }),
        Ending::Unknown if !emitted || calls > 0 => Some(protocol(format!(
            "the model ended the turn with the unhandled finishReason {} and no usable output",
            sanitize(reason)
        ))),
        Ending::Filtered | Ending::Unknown => {
            tracing::warn!(
                finish_reason = %sanitize(reason),
                "gemini ended the turn early; treated as a normal stop"
            );
            None
        }
        Ending::Normal(_) => None,
    }
}

/// Per-response state of the mapping.
#[derive(Debug, Default)]
pub(crate) struct Mapper {
    /// Function calls seen (the next call's `index`).
    calls: usize,
    /// Bytes of arguments forwarded, all calls together.
    args_bytes: usize,
    /// Parts to replay, in arrival order.
    replay: Vec<Recorded>,
    /// Bytes held by `replay`.
    replay_bytes: usize,
    /// Whether any recorded part carries a signature.
    signed: bool,
    /// Whether any text or call was emitted.
    emitted: bool,
    /// The finish of the turn, once `finishReason` was seen.
    finish: Option<FinishReason>,
    /// The usage of the last chunk that had one.
    usage: Option<Usage>,
    /// The tools of the request, to restore free-form objects the model sent as strings.
    tools: Option<Arc<RenderedTools>>,
}

impl Mapper {
    /// A mapper for a new response to a request without tools.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A mapper for a new response to a request that carried `tools`: the arguments of a call to
    /// a tool whose schema was degraded (a nested free-form object declared as a string) get
    /// those strings turned back into objects before the call is forwarded. The replay data
    /// keeps the arguments as the model sent them.
    pub(crate) fn with_tools(tools: Option<Arc<RenderedTools>>) -> Self {
        Self {
            tools,
            ..Self::default()
        }
    }

    /// Maps one event's `data` into `out` (appended; events of this call only).
    ///
    /// Empty (or whitespace-only) data is a heartbeat and is ignored.
    ///
    /// # Errors
    /// [`Error::Protocol`] for data that is not JSON or violates the protocol,
    /// [`Error::Rejected`] for an in-band error or a blocked prompt, [`Error::Disconnected`] for
    /// a `[DONE]` sentinel before `finishReason`.
    pub(crate) fn handle(&mut self, data: &str, out: &mut Vec<LlmEvent>) -> Result<Flow, Error> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Flow::Continue);
        }
        if data == "[DONE]" {
            return self.end_of_stream(out).map(|()| Flow::Finished);
        }
        let chunk: Chunk<'_> = serde_json::from_str(data)
            .map_err(|e| protocol(format!("a chunk is not a GenerateContentResponse: {e}")))?;
        if let Some(error) = chunk.error {
            return Err(match serde_json::from_str::<ErrorObject<'_>>(error.get()) {
                Ok(e) => error_for(e.code, e.status.as_deref()),
                Err(_) => error_for(None, None),
            });
        }
        if let Some(raw) = chunk.usage {
            // The last chunk that has usage wins (never summed: it may be cumulative).
            if let Ok(u) = serde_json::from_str::<UsageMetadata>(raw.get()) {
                self.usage = Some(Usage {
                    input_tokens: clamp(u.prompt),
                    cached_input_tokens: clamp(u.cached),
                    output_tokens: clamp(u.candidates.saturating_add(u.thoughts)),
                });
            }
        }
        let candidate = chunk.candidates.and_then(|c| c.into_iter().next());
        let Some(candidate) = candidate else {
            // No candidate: a blocked prompt, or a chunk that carries only usage.
            if let Some(feedback) = chunk.prompt_feedback
                && let Ok(feedback) = serde_json::from_str::<PromptFeedback<'_>>(feedback.get())
                && let Some(reason) = feedback.block_reason.as_deref()
            {
                tracing::warn!(block_reason = %sanitize(reason), "gemini blocked the prompt");
                return Err(Error::Rejected {
                    provider: PROVIDER,
                    status: 400,
                });
            }
            return self.settle(out);
        };
        for part in candidate
            .content
            .and_then(|c| c.parts)
            .into_iter()
            .flatten()
        {
            self.part(part, out)?;
        }
        if let Some(reason) = candidate
            .finish_reason
            .as_deref()
            .filter(|r| !r.is_empty() && *r != "FINISH_REASON_UNSPECIFIED")
        {
            let ending = ending_of(reason, self.calls);
            if let Some(error) = finish_error(reason, &ending, self.emitted, self.calls) {
                return Err(error);
            }
            self.finish = Some(match ending {
                Ending::Normal(finish) => finish,
                // Filtered or unknown after spoken text only (see `finish_error`).
                _ => FinishReason::Stop,
            });
        }
        self.settle(out)
    }

    /// Whether `finishReason` was seen: the content of the turn is complete, only the usage may
    /// still be missing.
    pub(crate) fn has_finish(&self) -> bool {
        self.finish.is_some()
    }

    /// Emits `Done` once the finish and the usage are both known.
    fn settle(&mut self, out: &mut Vec<LlmEvent>) -> Result<Flow, Error> {
        if self.finish.is_some() && self.usage.is_some() {
            self.done(out)?;
            return Ok(Flow::Finished);
        }
        Ok(Flow::Continue)
    }

    /// The body ended cleanly (or `[DONE]` arrived): the turn is complete when `finishReason`
    /// was seen.
    ///
    /// # Errors
    /// [`Error::Disconnected`] when no `finishReason` was seen: the response was cut off.
    pub(crate) fn end_of_stream(&mut self, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        if self.finish.is_none() {
            return Err(Error::Disconnected { provider: PROVIDER });
        }
        self.done(out)
    }

    fn done(&mut self, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let finish = self.finish.unwrap_or(FinishReason::Stop);
        let provider_data = if self.signed || self.calls > 0 {
            let json = serde_json::to_string(&self.replay)
                .map_err(|e| protocol(format!("cannot record the response parts: {e}")))?;
            Some(Arc::from(json))
        } else {
            None
        };
        out.push(LlmEvent::Done {
            finish,
            usage: self.usage,
            provider_data,
        });
        Ok(())
    }

    fn part(&mut self, part: ResponsePart<'_>, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let signature = part
            .signature
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(call) = part.function_call {
            return self.call(call, signature, out);
        }
        if part.thought == Some(true) {
            return Ok(());
        }
        let Some(text) = part.text.as_deref() else {
            return Ok(());
        };
        if text.is_empty() && signature.is_none() {
            return Ok(());
        }
        self.charge(
            text.len()
                .saturating_add(signature.as_ref().map_or(0, String::len)),
        )?;
        self.signed |= signature.is_some();
        match (self.replay.last_mut(), &signature) {
            // Adjacent unsigned text parts coalesce; a signed part never merges with another.
            (
                Some(Recorded::Text {
                    text: last,
                    signature: None,
                }),
                None,
            ) => last.push_str(text),
            _ => self.replay.push(Recorded::Text {
                text: text.to_owned(),
                signature,
            }),
        }
        if !text.is_empty() {
            self.emitted = true;
            out.push(LlmEvent::TextDelta(Arc::from(text)));
        }
        Ok(())
    }

    fn call(
        &mut self,
        call: FunctionCall<'_>,
        signature: Option<String>,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let Some(name) = call.name.as_deref().filter(|n| !n.is_empty()) else {
            return Err(protocol("a functionCall has no name"));
        };
        if self.calls >= MAX_CALLS {
            return Err(protocol(format!(
                "the response has more than {MAX_CALLS} function calls"
            )));
        }
        let args: &str = match call.args.map(|a| a.get()) {
            None | Some("null") => "{}",
            Some(raw) if raw.trim_start().starts_with('{') => raw,
            Some(_) => return Err(protocol("functionCall args is not a JSON object")),
        };
        if args.len() > MAX_CALL_ARGS_BYTES {
            return Err(protocol(format!(
                "a function call has more than {MAX_CALL_ARGS_BYTES} bytes of arguments"
            )));
        }
        self.args_bytes = self.args_bytes.saturating_add(args.len());
        if self.args_bytes > MAX_TOTAL_ARGS_BYTES {
            return Err(protocol(format!(
                "the function calls have more than {MAX_TOTAL_ARGS_BYTES} bytes of arguments"
            )));
        }
        let index = self.calls;
        let given = call.id.as_deref().filter(|id| !id.is_empty());
        self.charge(
            args.len()
                .saturating_add(name.len())
                .saturating_add(given.map_or(0, str::len))
                .saturating_add(signature.as_ref().map_or(0, String::len)),
        )?;
        let id: Arc<str> = given.map_or_else(|| Arc::from(synthetic_id(index)), Arc::from);
        let recorded_args = RawValue::from_string(args.to_owned())
            .map_err(|e| protocol(format!("functionCall args is not valid JSON: {e}")))?;
        self.signed |= signature.is_some();
        self.replay.push(Recorded::Call {
            call: RecordedCall {
                id: given.map(str::to_owned),
                name: name.to_owned(),
                args: recorded_args,
            },
            signature,
        });
        self.calls += 1;
        self.emitted = true;
        let arguments: Arc<str> = self
            .tools
            .as_ref()
            .and_then(|tools| tools.restore_args(name, args))
            .map_or_else(|| Arc::from(args), Arc::from);
        out.push(LlmEvent::ToolCallDelta {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            id: Some(id),
            name: Some(Arc::from(name)),
            arguments,
        });
        Ok(())
    }

    /// Accounts `bytes` of replayable data.
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.replay_bytes = self.replay_bytes.saturating_add(bytes).saturating_add(32);
        if self.replay_bytes > MAX_REPLAY_BYTES {
            return Err(protocol(format!(
                "the response holds more than {MAX_REPLAY_BYTES} bytes of parts"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn chunk(parts: Value, finish: Option<&str>, usage: Option<Value>) -> String {
        let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
        if let Some(finish) = finish {
            candidate["finishReason"] = json!(finish);
        }
        let mut chunk = json!({"candidates": [candidate], "modelVersion": "m", "responseId": "r"});
        if let Some(usage) = usage {
            chunk["usageMetadata"] = usage;
        }
        chunk.to_string()
    }

    fn feed(mapper: &mut Mapper, data: &str) -> (Vec<LlmEvent>, Result<Flow, Error>) {
        let mut out = Vec::new();
        let flow = mapper.handle(data, &mut out);
        (out, flow)
    }

    fn done_of(events: &[LlmEvent]) -> (FinishReason, Option<Usage>, Option<Value>) {
        match events.last() {
            Some(LlmEvent::Done {
                finish,
                usage,
                provider_data,
            }) => (
                *finish,
                *usage,
                provider_data
                    .as_deref()
                    .map(|d| serde_json::from_str(d).unwrap()),
            ),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    fn usage() -> Value {
        json!({"promptTokenCount": 100, "cachedContentTokenCount": 60,
               "candidatesTokenCount": 7, "thoughtsTokenCount": 3, "totalTokenCount": 110})
    }

    #[test]
    fn text_chunks_become_deltas_and_the_last_chunk_ends_the_turn() {
        let mut mapper = Mapper::new();
        let (out, flow) = feed(
            &mut mapper,
            &chunk(json!([{"text": "Merhaba, "}]), None, None),
        );
        assert_eq!(flow.unwrap(), Flow::Continue);
        assert!(matches!(&out[..], [LlmEvent::TextDelta(t)] if &**t == "Merhaba, "));
        let (out, flow) = feed(
            &mut mapper,
            &chunk(json!([{"text": "dunya"}]), Some("STOP"), Some(usage())),
        );
        assert_eq!(flow.unwrap(), Flow::Finished);
        assert!(matches!(&out[0], LlmEvent::TextDelta(t) if &**t == "dunya"));
        let (finish, usage, data) = done_of(&out);
        assert_eq!(finish, FinishReason::Stop);
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 100,
                cached_input_tokens: 60,
                output_tokens: 10
            })
        );
        assert_eq!(data, None, "unsigned plain text needs no replay data");
    }

    #[test]
    fn a_function_call_arrives_whole_and_is_replayed_with_its_signature() {
        let mut mapper = Mapper::new();
        let (out, flow) = feed(
            &mut mapper,
            &chunk(
                json!([
                    {"text": "One moment. "},
                    {"functionCall": {"id": "abc", "name": "get_weather", "args": {"city": "Paris", "n": 1}},
                     "thoughtSignature": "SIG"},
                    {"functionCall": {"id": "def", "name": "get_time", "args": {}}}
                ]),
                Some("STOP"),
                Some(usage()),
            ),
        );
        assert_eq!(flow.unwrap(), Flow::Finished);
        match &out[1] {
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("abc"));
                assert_eq!(name.as_deref(), Some("get_weather"));
                // The text of `args` is forwarded as received.
                assert_eq!(&**arguments, r#"{"city":"Paris","n":1}"#);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&out[2], LlmEvent::ToolCallDelta { index: 1, .. }));
        let (finish, _, data) = done_of(&out);
        assert_eq!(finish, FinishReason::ToolCalls);
        assert_eq!(
            data.unwrap(),
            json!([
                {"text": "One moment. "},
                {"functionCall": {"id": "abc", "name": "get_weather", "args": {"city": "Paris", "n": 1}},
                 "thoughtSignature": "SIG"},
                {"functionCall": {"id": "def", "name": "get_time", "args": {}}}
            ])
        );
    }

    #[test]
    fn the_text_of_args_is_forwarded_as_received() {
        let raw = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{ "a" : [1, 2] , "b":"x"}}}]}}]}"#;
        let (out, _) = feed(&mut Mapper::new(), raw);
        match &out[0] {
            LlmEvent::ToolCallDelta { arguments, .. } => {
                assert_eq!(&**arguments, r#"{ "a" : [1, 2] , "b":"x"}"#);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_call_without_an_id_gets_a_made_up_one_that_is_not_recorded() {
        let mut mapper = Mapper::new();
        let (out, _) = feed(
            &mut mapper,
            &chunk(
                json!([
                    {"functionCall": {"name": "a", "args": {"x": 1}}},
                    {"functionCall": {"id": "real", "name": "b"}},
                    {"functionCall": {"name": "c", "args": null}}
                ]),
                Some("STOP"),
                Some(usage()),
            ),
        );
        let ids: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { id, arguments, .. } => {
                    Some((id.as_deref().unwrap().to_owned(), arguments.to_string()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                ("brisvox_call_0".to_owned(), r#"{"x":1}"#.to_owned()),
                ("real".to_owned(), "{}".to_owned()),
                ("brisvox_call_2".to_owned(), "{}".to_owned()),
            ]
        );
        let (_, _, data) = done_of(&out);
        let data = data.unwrap();
        assert!(data[0]["functionCall"].get("id").is_none());
        assert_eq!(data[1]["functionCall"]["id"], json!("real"));
    }

    #[test]
    fn thoughts_and_empty_text_are_not_spoken_and_signed_empty_text_is_recorded() {
        let mut mapper = Mapper::new();
        let (out, _) = feed(
            &mut mapper,
            &chunk(
                json!([
                    {"text": "I should check the weather", "thought": true},
                    {"text": ""},
                    {"text": "Hi", "thoughtSignature": "S1"},
                    {"text": " there"},
                    {"text": "", "thoughtSignature": "S2"},
                    {"inlineData": {"mimeType": "x", "data": "y"}}
                ]),
                None,
                None,
            ),
        );
        let spoken: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta(t) => Some(t.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(spoken, vec!["Hi", " there"]);
        let (out, _) = feed(&mut mapper, &chunk(json!([]), Some("STOP"), Some(usage())));
        let (_, _, data) = done_of(&out);
        // The signed part is not merged with the unsigned text after it.
        assert_eq!(
            data.unwrap(),
            json!([
                {"text": "Hi", "thoughtSignature": "S1"},
                {"text": " there"},
                {"text": "", "thoughtSignature": "S2"}
            ])
        );
    }

    #[test]
    fn adjacent_unsigned_text_parts_are_coalesced_in_the_replay_data() {
        let mut mapper = Mapper::new();
        for piece in ["a", "b", "c"] {
            feed(&mut mapper, &chunk(json!([{"text": piece}]), None, None))
                .1
                .unwrap();
        }
        let (out, _) = feed(
            &mut mapper,
            &chunk(
                json!([{"functionCall": {"id": "x", "name": "f", "args": {}}}]),
                Some("STOP"),
                Some(usage()),
            ),
        );
        let (_, _, data) = done_of(&out);
        assert_eq!(data.unwrap()[0], json!({"text": "abc"}));
    }

    #[test]
    fn finish_reasons_map_as_documented() {
        let end = |reason: &str, parts: Value| {
            let mut mapper = Mapper::new();
            let (out, flow) = feed(&mut mapper, &chunk(parts, Some(reason), Some(usage())));
            (out, flow)
        };
        let text = json!([{"text": "x"}]);
        let call = json!([{"functionCall": {"id": "i", "name": "f", "args": {}}}]);
        let (out, _) = end("STOP", text.clone());
        assert_eq!(done_of(&out).0, FinishReason::Stop);
        let (out, _) = end("STOP", call.clone());
        assert_eq!(done_of(&out).0, FinishReason::ToolCalls);
        let (out, _) = end("MAX_TOKENS", text.clone());
        assert_eq!(done_of(&out).0, FinishReason::Length);
        // Early ends after output: a normal stop (logged).
        for reason in [
            "SAFETY",
            "RECITATION",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "OTHER",
            "SPII",
        ] {
            let (out, flow) = end(reason, text.clone());
            assert_eq!(flow.unwrap(), Flow::Finished, "{reason}");
            assert_eq!(done_of(&out).0, FinishReason::Stop, "{reason}");
        }
        // Reasons of the doc's list this adapter has no mapping for, and future values: a normal
        // stop only after spoken text.
        for reason in ["ESCALATION", "PUP_LIMITED_DISABLED", "SOMETHING_NEW"] {
            let (out, flow) = end(reason, text.clone());
            assert_eq!(flow.unwrap(), Flow::Finished, "{reason}");
            assert_eq!(done_of(&out).0, FinishReason::Stop, "{reason}");
            // With nothing emitted it is not a silent success.
            let (out, flow) = end(reason, json!([]));
            match flow {
                Err(Error::Protocol { detail, .. }) => assert!(detail.contains(reason), "{detail}"),
                other => panic!("{reason}: {other:?}"),
            }
            assert!(out.is_empty(), "{reason}: no Done");
            // A call is not vouched for either.
            let (_, flow) = end(reason, call.clone());
            assert!(matches!(flow, Err(Error::Protocol { .. })), "{reason}");
        }
        // A policy cut after a function call must not execute the call: no Done{ToolCalls}.
        for reason in ["SAFETY", "RECITATION", "PROHIBITED_CONTENT", "OTHER"] {
            let (out, flow) = end(reason, call.clone());
            assert!(
                matches!(flow, Err(Error::Rejected { status: 400, .. })),
                "{reason}"
            );
            assert!(
                !out.iter().any(|e| matches!(e, LlmEvent::Done { .. })),
                "{reason}"
            );
        }
        // Early end with no output: the content was refused.
        for reason in ["SAFETY", "RECITATION", "OTHER", "PROHIBITED_CONTENT"] {
            let (_, flow) = end(reason, json!([]));
            assert!(
                matches!(flow, Err(Error::Rejected { status: 400, .. })),
                "{reason}"
            );
        }
        // A failed turn is an error even after output.
        for reason in [
            "MALFORMED_FUNCTION_CALL",
            "MISSING_THOUGHT_SIGNATURE",
            "UNEXPECTED_TOOL_CALL",
            "TOO_MANY_TOOL_CALLS",
            "MALFORMED_RESPONSE",
            "LANGUAGE",
        ] {
            let (_, flow) = end(reason, text.clone());
            match flow {
                Err(Error::Protocol { detail, .. }) => assert!(detail.contains(reason)),
                other => panic!("{reason}: {other:?}"),
            }
        }
    }

    #[test]
    fn done_waits_for_the_usage_chunk_or_the_end_of_the_body() {
        let mut mapper = Mapper::new();
        let (out, flow) = feed(
            &mut mapper,
            &chunk(json!([{"text": "x"}]), Some("STOP"), None),
        );
        assert_eq!(flow.unwrap(), Flow::Continue);
        assert_eq!(out.len(), 1, "no Done yet");
        // A usage-only chunk (no candidates) completes the turn.
        let (out, flow) = feed(&mut mapper, &json!({"usageMetadata": usage()}).to_string());
        assert_eq!(flow.unwrap(), Flow::Finished);
        assert_eq!(done_of(&out).1.unwrap().input_tokens, 100);

        // Without any usage the clean end of the body completes it, with `usage: None`.
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            &chunk(json!([{"text": "x"}]), Some("STOP"), None),
        )
        .1
        .unwrap();
        let mut out = Vec::new();
        mapper.end_of_stream(&mut out).unwrap();
        assert_eq!(done_of(&out), (FinishReason::Stop, None, None));
    }

    #[test]
    fn usage_is_taken_from_the_last_chunk_not_summed() {
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            &chunk(
                json!([{"text": "a"}]),
                None,
                Some(json!({"promptTokenCount": 5})),
            ),
        )
        .1
        .unwrap();
        let (out, _) = feed(
            &mut mapper,
            &chunk(
                json!([{"text": "b"}]),
                Some("STOP"),
                Some(json!({"promptTokenCount": 5, "candidatesTokenCount": 2})),
            ),
        );
        assert_eq!(
            done_of(&out).1,
            Some(Usage {
                input_tokens: 5,
                cached_input_tokens: 0,
                output_tokens: 2
            })
        );
    }

    #[test]
    fn a_body_that_ends_without_a_finish_reason_is_a_disconnect() {
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            &chunk(json!([{"text": "x"}]), None, Some(usage())),
        )
        .1
        .unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            mapper.end_of_stream(&mut out),
            Err(Error::Disconnected { .. })
        ));
        assert!(out.is_empty());
        let (_, flow) = feed(&mut mapper, "[DONE]");
        assert!(matches!(flow, Err(Error::Disconnected { .. })));
    }

    #[test]
    fn done_sentinel_after_the_finish_completes_the_turn() {
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            &chunk(json!([{"text": "x"}]), Some("STOP"), None),
        )
        .1
        .unwrap();
        let (out, flow) = feed(&mut mapper, "[DONE]");
        assert_eq!(flow.unwrap(), Flow::Finished);
        assert_eq!(done_of(&out).0, FinishReason::Stop);
    }

    #[test]
    fn blocked_prompts_and_in_band_errors_are_rejections() {
        let mut mapper = Mapper::new();
        let (_, flow) = feed(
            &mut mapper,
            r#"{"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":3}}"#,
        );
        assert!(matches!(flow, Err(Error::Rejected { status: 400, .. })));

        for (data, want) in [
            (
                r#"{"error":{"code":429,"message":"quota key=AIza123","status":"RESOURCE_EXHAUSTED"}}"#,
                429,
            ),
            (r#"{"error":{"code":503,"status":"UNAVAILABLE"}}"#, 503),
            (r#"{"error":{"status":"INTERNAL"}}"#, 500),
            (r#"{"error":{"code":400,"status":"INVALID_ARGUMENT"}}"#, 400),
            (r#"{"error":{"status":"DEADLINE_EXCEEDED"}}"#, 504),
        ] {
            let (_, flow) = feed(&mut Mapper::new(), data);
            match flow {
                Err(Error::Rejected { status, .. }) => assert_eq!(status, want, "{data}"),
                other => panic!("{data}: {other:?}"),
            }
        }
        let (_, flow) = feed(
            &mut Mapper::new(),
            r#"{"error":{"status":"WEIRD STATUS!"}}"#,
        );
        match flow {
            Err(Error::Protocol { detail, .. }) => assert!(detail.contains("WEIRDSTATUS")),
            other => panic!("{other:?}"),
        }
        let (_, flow) = feed(&mut Mapper::new(), r#"{"error":"boom"}"#);
        assert!(matches!(flow, Err(Error::Protocol { .. })));
    }

    #[test]
    fn heartbeats_and_unknown_fields_are_ignored_and_garbage_is_a_protocol_error() {
        let mut mapper = Mapper::new();
        assert_eq!(feed(&mut mapper, "  ").1.unwrap(), Flow::Continue);
        assert_eq!(
            feed(
                &mut mapper,
                r#"{"modelStatus":{"modelStage":"X"},"extra":[1,2]}"#
            )
            .1
            .unwrap(),
            Flow::Continue
        );
        let (_, flow) = feed(&mut mapper, "{this is not json");
        assert!(matches!(flow, Err(Error::Protocol { .. })));
        let (_, flow) = feed(&mut mapper, "[1,2]");
        assert!(matches!(flow, Err(Error::Protocol { .. })));
    }

    #[test]
    fn malformed_calls_are_protocol_errors() {
        for part in [
            json!({"functionCall": {"id": "i", "args": {}}}),
            json!({"functionCall": {"id": "i", "name": "", "args": {}}}),
            json!({"functionCall": {"id": "i", "name": "f", "args": "text"}}),
            json!({"functionCall": {"id": "i", "name": "f", "args": [1]}}),
        ] {
            let (_, flow) = feed(&mut Mapper::new(), &chunk(json!([part]), None, None));
            assert!(matches!(flow, Err(Error::Protocol { .. })), "{part}");
        }
    }

    #[test]
    fn calls_and_arguments_are_capped() {
        let mut mapper = Mapper::new();
        let mut last = Ok(Flow::Continue);
        for n in 0..=MAX_CALLS {
            last = feed(
                &mut mapper,
                &chunk(
                    json!([{"functionCall": {"id": format!("c{n}"), "name": "f", "args": {}}}]),
                    None,
                    None,
                ),
            )
            .1;
        }
        assert!(matches!(last, Err(Error::Protocol { .. })));
        let big = "x".repeat(MAX_CALL_ARGS_BYTES);
        let (_, flow) = feed(
            &mut Mapper::new(),
            &chunk(
                json!([{"functionCall": {"id": "c", "name": "f", "args": {"a": big}}}]),
                None,
                None,
            ),
        );
        assert!(matches!(flow, Err(Error::Protocol { .. })));
    }

    #[test]
    fn snake_case_signature_and_call_keys_are_accepted() {
        let mut mapper = Mapper::new();
        let (out, _) = feed(
            &mut mapper,
            &chunk(
                json!([{"function_call": {"id": "i", "name": "f", "args": {}}, "thought_signature": "S"}]),
                Some("STOP"),
                Some(usage()),
            ),
        );
        let (_, _, data) = done_of(&out);
        // Recorded in the documented camelCase spelling.
        assert_eq!(data.unwrap()[0]["thoughtSignature"], json!("S"));
    }

    #[test]
    fn signatures_and_ids_are_charged_to_the_replay_budget() {
        // Parts with no text and a ~1 MiB signature each: far under the per-event cap, and
        // nothing else in them would ever add up to MAX_REPLAY_BYTES.
        let signature = "S".repeat(1024 * 1024);
        let mut mapper = Mapper::new();
        let mut failed_at = None;
        for n in 0..=(MAX_REPLAY_BYTES / signature.len() + 1) {
            let (_, flow) = feed(
                &mut mapper,
                &chunk(
                    json!([{"text": "", "thoughtSignature": signature}]),
                    None,
                    None,
                ),
            );
            if let Err(error) = flow {
                assert!(matches!(error, Error::Protocol { .. }), "{error}");
                failed_at = Some(n);
                break;
            }
        }
        let at = failed_at.expect("the replay cap must trip");
        assert!(at <= MAX_REPLAY_BYTES / signature.len(), "{at}");
        // The same for calls: a long id and a long signature on tiny calls.
        let long = "I".repeat(1024 * 1024);
        let mut mapper = Mapper::new();
        let mut failed = false;
        for _ in 0..=(MAX_REPLAY_BYTES / long.len()) {
            let (_, flow) = feed(
                &mut mapper,
                &chunk(
                    json!([{"functionCall": {"id": long, "name": "f", "args": {}}}]),
                    None,
                    None,
                ),
            );
            if flow.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "call ids count towards the replay cap");
    }

    #[test]
    fn free_form_objects_sent_as_strings_are_restored_in_the_forwarded_arguments() {
        use crate::llm::ToolSpec;
        let tools = RenderedTools::render(
            &[
                ToolSpec {
                    name: Arc::from("save"),
                    description: Arc::from("saves"),
                    parameters_schema: Arc::from(
                        r#"{"type":"object","properties":{"data":{"type":"object","additionalProperties":true},"n":{"type":"integer"}}}"#,
                    ),
                },
                ToolSpec {
                    name: Arc::from("plain"),
                    description: Arc::from("plain"),
                    parameters_schema: Arc::from(
                        r#"{"type":"object","properties":{"s":{"type":"string"}}}"#,
                    ),
                },
            ],
            super::super::SchemaMode::OpenApiSubset,
        )
        .unwrap();
        let mut mapper = Mapper::with_tools(Some(tools));
        let (out, flow) = feed(
            &mut mapper,
            &chunk(
                json!([
                    {"functionCall": {"id": "a", "name": "save",
                                      "args": {"data": "{\"k\": [1, 2]}", "n": 3}},
                     "thoughtSignature": "SIG"},
                    {"functionCall": {"id": "b", "name": "plain", "args": {"s": "{\"k\": 1}"}}}
                ]),
                Some("STOP"),
                Some(usage()),
            ),
        );
        assert_eq!(flow.unwrap(), Flow::Finished);
        let args: Vec<Value> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { arguments, .. } => {
                    Some(serde_json::from_str(arguments).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(args[0], json!({"data": {"k": [1, 2]}, "n": 3}));
        assert_eq!(
            args[1],
            json!({"s": "{\"k\": 1}"}),
            "other tools are untouched"
        );
        // The replay data keeps what the model sent.
        let (_, _, data) = done_of(&out);
        assert_eq!(
            data.unwrap()[0]["functionCall"]["args"],
            json!({"data": "{\"k\": [1, 2]}", "n": 3})
        );
    }
}
