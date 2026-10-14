//! Maps Messages API stream events (the JSON `data` of each SSE event) to [`LlmEvent`]s.
//!
//! Dispatch is on the JSON `type` field only (the `event:` line repeats it); unknown event, block
//! and delta types are ignored, as the versioning policy of the API requires ("Add new variants
//! to enum-like output values (for example, streaming event types)"). An event is parsed in two
//! steps: first only its `type`, then just the fields of that kind, so an event the mapper
//! ignores is never parsed beyond its `type`. Free-text error messages are never copied into
//! errors or logs: they can echo prompt content (and, for a 401, a fragment of the key). Only
//! the machine-readable error `type` is used.
//!
//! # What becomes what
//! * `text_delta` -> [`LlmEvent::TextDelta`].
//! * A `tool_use` block -> [`LlmEvent::ToolCallDelta`] with a dense `index` (the block `index`
//!   also counts text and thinking blocks), `id` and `name` and empty arguments at
//!   `content_block_start`, then one pure fragment per non-empty `input_json_delta`. A call
//!   whose stream carried no argument text at all gets one `{}` fragment at its
//!   `content_block_stop`, so the concatenation is always a JSON object.
//! * `message_stop` -> [`LlmEvent::Done`], using the `stop_reason` and the cumulative usage of the
//!   `message_delta` events.
//! * `thinking` and `redacted_thinking` blocks are assembled and kept for
//!   [`Message::provider_data`](crate::Message::provider_data); they are never spoken.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::PROVIDER;
use crate::error::Error;
use crate::llm::{FinishReason, LlmEvent, Usage};

/// Most tool calls one response may open; more is a protocol error (bounds memory).
const MAX_CALLS: usize = 128;

/// Most content blocks one response may start; more is a protocol error (bounds memory).
const MAX_BLOCKS: usize = 1024;

/// Most replayable entries (thinking blocks, call placeholders, the text marker) one response may
/// carry (bounds memory).
const MAX_REPLAY_ITEMS: usize = 256;

/// Most bytes of thinking text, signatures and redacted data one response may carry (16 MiB).
const MAX_REPLAY_BYTES: usize = 16 * 1024 * 1024;

/// Most bytes of arguments one tool call may stream (1 MiB): a caller keeps a copy of every
/// call's arguments, so without a cap a misbehaving endpoint could grow it for the whole
/// `max_stream_duration`.
pub(crate) const MAX_CALL_ARGS_BYTES: usize = 1024 * 1024;

/// Most bytes of arguments all tool calls of one response may stream together (4 MiB).
pub(crate) const MAX_TOTAL_ARGS_BYTES: usize = 4 * 1024 * 1024;

/// Longest `type` kept in an error detail.
const MAX_CODE: usize = 64;

/// The placeholder `provider_data` holds where the response's text sat among its thinking blocks
/// and tool calls. It is not a real block: the request builder puts the *played* text at that
/// position.
pub(crate) const TEXT_MARKER: &str = r#"{"type":"text"}"#;

/// What the pump does after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// The model made no progress: `ping`, `message_start`, an unknown event, a heartbeat. Does
    /// not re-arm the first-output or idle deadline.
    Idle,
    /// A content event (a block starting, a delta, a block stopping, `message_delta`). The
    /// response is alive: re-arms the idle deadline.
    Progress,
    /// `message_stop` was mapped (`Done` is in the output): stop reading.
    Finished,
}

/// `raw` as text, `None` when it is absent or not a JSON string.
fn text(raw: Option<&RawValue>) -> Option<Cow<'_, str>> {
    #[derive(Deserialize)]
    struct Text<'a>(#[serde(borrow)] Cow<'a, str>);
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

#[derive(Deserialize)]
struct MessageStart<'a> {
    #[serde(default, borrow)]
    message: Option<MessageParts<'a>>,
}

#[derive(Deserialize)]
struct MessageParts<'a> {
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
}

/// A `usage` object. Every field is "number or null"; null or absent means "not updated".
#[derive(Deserialize, Default)]
struct UsageObject {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// The cumulative usage of a response: `message_start` first, each `message_delta` overriding
/// the fields it carries.
#[derive(Debug, Default, Clone, Copy)]
struct Totals {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
    seen: bool,
}

impl Totals {
    fn merge(&mut self, update: &UsageObject) {
        self.seen = true;
        if let Some(n) = update.input_tokens {
            self.input = n;
        }
        if let Some(n) = update.cache_creation_input_tokens {
            self.cache_creation = n;
        }
        if let Some(n) = update.cache_read_input_tokens {
            self.cache_read = n;
        }
        if let Some(n) = update.output_tokens {
            self.output = n;
        }
    }

    /// Anthropic's `input_tokens` counts only the tokens after the last cache breakpoint: the
    /// total input is the sum of the three input fields.
    fn usage(&self) -> Usage {
        Usage {
            input_tokens: clamp(
                self.input
                    .saturating_add(self.cache_creation)
                    .saturating_add(self.cache_read),
            ),
            cached_input_tokens: clamp(self.cache_read),
            output_tokens: clamp(self.output),
        }
    }
}

#[derive(Deserialize)]
struct BlockStartEvent<'a> {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default, borrow)]
    content_block: Option<BlockStart<'a>>,
}

#[derive(Deserialize)]
struct BlockStart<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    thinking: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    signature: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    data: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct DeltaEvent<'a> {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default, borrow)]
    delta: Option<Delta<'a>>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    partial_json: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    thinking: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    signature: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    data: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct StopEvent {
    #[serde(default)]
    index: Option<u32>,
}

#[derive(Deserialize)]
struct MessageDeltaEvent<'a> {
    #[serde(default, borrow)]
    delta: Option<MessageDeltaBody<'a>>,
    #[serde(default, borrow)]
    usage: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct MessageDeltaBody<'a> {
    #[serde(default, borrow)]
    stop_reason: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    stop_details: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct StopDetails<'a> {
    #[serde(default, borrow)]
    category: Option<Cow<'a, str>>,
}

/// An `error` event (or any object with an `error` member).
#[derive(Deserialize)]
struct ErrorEvent<'a> {
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ErrorBody<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<&'a RawValue>,
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

/// Maps an Anthropic error `type` (from an `error` event, or the body of a refusal) to an
/// [`Error`]: the documented types become [`Error::Rejected`] with the status the same condition
/// has outside a stream (`overloaded_error` 529, `rate_limit_error` 429, `api_error` 500,
/// `invalid_request_error` 400, ...); anything else is a protocol error naming the (sanitised)
/// type, never the message.
pub(crate) fn error_for_type(kind: Option<&str>) -> Error {
    let status = match kind {
        Some("invalid_request_error") => Some(400),
        Some("authentication_error") => Some(401),
        Some("billing_error") => Some(402),
        Some("permission_error") => Some(403),
        Some("not_found_error") => Some(404),
        Some("request_too_large") => Some(413),
        Some("rate_limit_error") => Some(429),
        Some("api_error") => Some(500),
        Some("timeout_error") => Some(504),
        Some("overloaded_error") => Some(529),
        _ => None,
    };
    match (status, kind) {
        (Some(status), _) => Error::Rejected {
            provider: PROVIDER,
            status,
        },
        (None, Some(kind)) => protocol(format!(
            "the response failed with error type {}",
            sanitize(kind)
        )),
        (None, None) => protocol("the response failed without an error type"),
    }
}

/// A content block being streamed.
#[derive(Debug)]
enum Block {
    /// A text block (nothing to remember: the deltas are forwarded as they come).
    Text,
    /// A `tool_use` block.
    Tool {
        /// The dense call index of the emitted `ToolCallDelta`s.
        dense: u32,
        /// Bytes of arguments streamed so far.
        args_len: usize,
        /// Whether any argument fragment had content.
        any_content: bool,
    },
    /// A `thinking` block, assembled from its deltas.
    Thinking {
        thinking: String,
        signature: String,
        slot: usize,
    },
    /// A `redacted_thinking` block.
    Redacted { data: String, slot: usize },
    /// A block of a type this adapter does not know (ignored).
    Other,
}

/// A thinking block as the next request replays it.
#[derive(Serialize)]
struct ThinkingOut<'a> {
    r#type: &'static str,
    thinking: &'a str,
    signature: &'a str,
}

/// A redacted thinking block as the next request replays it.
#[derive(Serialize)]
struct RedactedOut<'a> {
    r#type: &'static str,
    data: &'a str,
}

/// Where a tool call sat among the thinking blocks (its arguments live in `Message::tool_calls`).
#[derive(Serialize)]
struct ToolPlaceholder<'a> {
    r#type: &'static str,
    id: &'a str,
    name: &'a str,
}

/// Per-response state of the mapping.
#[derive(Debug, Default)]
pub(crate) struct Mapper {
    /// Blocks that have started and not stopped, by their stream `index`.
    blocks: Vec<(u32, Block)>,
    /// Blocks started so far (bounded by [`MAX_BLOCKS`]).
    started: usize,
    /// Tool calls opened so far.
    tool_calls: u32,
    /// Tool blocks that are open right now.
    open_tools: usize,
    /// Bytes of arguments forwarded, all calls together.
    args_bytes: usize,
    /// The response's replayable entries in block order: thinking blocks (filled in when the
    /// block stops; `None` while open or when unreplayable), tool-call placeholders, and one
    /// [`TEXT_MARKER`] where the first text block sat.
    replay: Vec<Option<Box<RawValue>>>,
    /// Bytes held by thinking text, signatures and redacted data.
    replay_bytes: usize,
    /// Whether the marker is in `replay`.
    marked: bool,
    /// Whether a thinking or redacted block was kept: only then is there `provider_data`.
    kept_thinking: bool,
    usage: Totals,
    stop_reason: Option<String>,
    refusal_category: Option<String>,
    /// Whether any non-empty text was forwarded.
    text_seen: bool,
}

impl Mapper {
    /// A mapper for a new response.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether a `tool_use` block is open: the model may pause between argument fragments.
    pub(crate) fn tool_block_open(&self) -> bool {
        self.open_tools > 0
    }

    /// Whether a `thinking` or `redacted_thinking` block is open: with `display: omitted` the
    /// server streams nothing between the block start and its signature, however long the
    /// model thinks.
    pub(crate) fn thinking_block_open(&self) -> bool {
        self.blocks
            .iter()
            .any(|(_, block)| matches!(block, Block::Thinking { .. } | Block::Redacted { .. }))
    }

    /// Maps one event's `data` into `out` (appended; events of this call only).
    ///
    /// Empty (or whitespace-only) data and a `[DONE]` sentinel (which this API version no longer
    /// sends) are ignored.
    ///
    /// # Errors
    /// [`Error::Protocol`] for data that is not JSON or violates the protocol,
    /// [`Error::Rejected`] for an `error` event.
    pub(crate) fn handle(&mut self, data: &str, out: &mut Vec<LlmEvent>) -> Result<Flow, Error> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Flow::Idle);
        }
        let head: Head<'_> = serde_json::from_str(data)
            .map_err(|e| protocol(format!("an event is not valid JSON ({})", json_error(&e))))?;
        let kind = text(head.kind);
        let kind = match kind.as_deref() {
            Some(kind) => kind,
            // Any object with an `error` member is an error, even without a type.
            None if head.error.is_some() => "error",
            None => return Ok(Flow::Idle),
        };
        match kind {
            "ping" => return Ok(Flow::Idle),
            "message_start" => {
                let event: MessageStart<'_> = parse(data, kind)?;
                if let Some(usage) = event.message.and_then(|m| m.usage).and_then(usage_of) {
                    self.usage.merge(&usage);
                }
                return Ok(Flow::Idle);
            }
            "content_block_start" => self.block_start(&parse(data, kind)?, out)?,
            "content_block_delta" => self.block_delta(&parse(data, kind)?, out)?,
            "content_block_stop" => {
                let event: StopEvent = parse(data, kind)?;
                self.block_stop(event.index, out)?;
            }
            "message_delta" => self.message_delta(&parse(data, kind)?),
            "message_stop" => {
                self.finish(out)?;
                return Ok(Flow::Finished);
            }
            "error" => {
                // Lenient: a missing or odd `error` object is an error without a type.
                let event: Option<ErrorEvent<'_>> = serde_json::from_str(data).ok();
                let kind = event
                    .and_then(|e| e.error)
                    .and_then(|raw| serde_json::from_str::<ErrorBody<'_>>(raw.get()).ok())
                    .and_then(|body| text(body.kind).map(Cow::into_owned));
                return Err(error_for_type(kind.as_deref()));
            }
            other => {
                tracing::trace!(kind = other, "ignored anthropic stream event");
                return Ok(Flow::Idle);
            }
        }
        Ok(Flow::Progress)
    }

    /// Takes the open block `index` out of the open set.
    fn take(&mut self, index: u32) -> Option<Block> {
        let at = self.blocks.iter().position(|(i, _)| *i == index)?;
        let (_, block) = self.blocks.remove(at);
        if matches!(block, Block::Tool { .. }) {
            self.open_tools = self.open_tools.saturating_sub(1);
        }
        Some(block)
    }

    /// Accounts `add` bytes of replayable text.
    fn charge_replay(&mut self, add: usize) -> Result<(), Error> {
        self.replay_bytes = self.replay_bytes.saturating_add(add);
        if self.replay_bytes > MAX_REPLAY_BYTES {
            return Err(protocol(format!(
                "the response carries more than {MAX_REPLAY_BYTES} bytes of thinking"
            )));
        }
        Ok(())
    }

    /// Reserves a replay entry (to be filled in later) and returns its slot.
    fn reserve(&mut self, entry: Option<Box<RawValue>>) -> Result<usize, Error> {
        if self.replay.len() >= MAX_REPLAY_ITEMS {
            return Err(protocol(format!(
                "the response carries more than {MAX_REPLAY_ITEMS} replayable blocks"
            )));
        }
        self.replay.push(entry);
        Ok(self.replay.len() - 1)
    }

    fn block_start(
        &mut self,
        event: &BlockStartEvent<'_>,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let index = event
            .index
            .ok_or_else(|| protocol("a content_block_start has no index"))?;
        self.started += 1;
        if self.started > MAX_BLOCKS {
            return Err(protocol(format!(
                "the response starts more than {MAX_BLOCKS} content blocks"
            )));
        }
        // A restarted index replaces the old block.
        self.take(index);
        let Some(start) = &event.content_block else {
            self.blocks.push((index, Block::Other));
            return Ok(());
        };
        let block = match start.kind.as_deref() {
            Some("text") => {
                if !self.marked {
                    self.reserve(Some(raw(TEXT_MARKER)?))?;
                    self.marked = true;
                }
                if let Some(initial) = start.text.as_deref().filter(|t| !t.is_empty()) {
                    self.text_seen = true;
                    out.push(LlmEvent::TextDelta(Arc::from(initial)));
                }
                Block::Text
            }
            Some("tool_use") => {
                let (Some(id), Some(name)) = (start.id.as_deref(), start.name.as_deref()) else {
                    return Err(protocol("a tool_use block has no id or name"));
                };
                if usize::try_from(self.tool_calls).unwrap_or(usize::MAX) >= MAX_CALLS {
                    return Err(protocol(format!(
                        "the response opens more than {MAX_CALLS} tool calls"
                    )));
                }
                let placeholder = serde_json::value::to_raw_value(&ToolPlaceholder {
                    r#type: "tool_use",
                    id,
                    name,
                })
                .map_err(|e| protocol(format!("cannot build a call placeholder: {e}")))?;
                self.reserve(Some(placeholder))?;
                let dense = self.tool_calls;
                self.tool_calls += 1;
                self.open_tools += 1;
                out.push(LlmEvent::ToolCallDelta {
                    index: dense,
                    id: Some(Arc::from(id)),
                    name: Some(Arc::from(name)),
                    arguments: Arc::from(""),
                });
                Block::Tool {
                    dense,
                    args_len: 0,
                    any_content: false,
                }
            }
            Some("thinking") => {
                let thinking = start.thinking.as_deref().unwrap_or_default();
                let signature = start.signature.as_deref().unwrap_or_default();
                self.charge_replay(thinking.len() + signature.len())?;
                Block::Thinking {
                    thinking: thinking.to_owned(),
                    signature: signature.to_owned(),
                    slot: self.reserve(None)?,
                }
            }
            Some("redacted_thinking") => {
                let data = start.data.as_deref().unwrap_or_default();
                self.charge_replay(data.len())?;
                Block::Redacted {
                    data: data.to_owned(),
                    slot: self.reserve(None)?,
                }
            }
            other => {
                tracing::trace!(kind = other, "ignored anthropic content block");
                Block::Other
            }
        };
        self.blocks.push((index, block));
        Ok(())
    }

    fn block_delta(
        &mut self,
        event: &DeltaEvent<'_>,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let Some(delta) = &event.delta else {
            return Ok(());
        };
        let Some(kind) = delta.kind.as_deref() else {
            return Ok(());
        };
        match kind {
            "text_delta" => {
                if let Some(text) = delta.text.as_deref().filter(|t| !t.is_empty()) {
                    self.text_seen = true;
                    out.push(LlmEvent::TextDelta(Arc::from(text)));
                }
            }
            "input_json_delta" => {
                let Some(fragment) = delta.partial_json.as_deref().filter(|f| !f.is_empty()) else {
                    return Ok(());
                };
                self.push_arguments(event.index, fragment, out)?;
            }
            "thinking_delta" | "signature_delta" => {
                let (piece, is_signature) = if kind == "thinking_delta" {
                    (delta.thinking.as_deref(), false)
                } else {
                    (delta.signature.as_deref(), true)
                };
                let Some(piece) = piece.filter(|p| !p.is_empty()) else {
                    return Ok(());
                };
                self.charge_replay(piece.len())?;
                match self.block_mut(event.index)? {
                    Block::Thinking {
                        thinking,
                        signature,
                        ..
                    } => {
                        if is_signature {
                            signature.push_str(piece);
                        } else {
                            thinking.push_str(piece);
                        }
                    }
                    _ => {
                        tracing::debug!("anthropic thinking delta on a block that is not thinking")
                    }
                }
            }
            other => {
                // UNVERIFIED in docs/providers/anthropic.md: how a `redacted_thinking` block
                // streams. The usual shape carries all of `data` in `content_block_start`; if it
                // arrived empty and a delta of an unknown type carries a `data` string, take it.
                if let Some(data) = delta.data.as_deref().filter(|d| !d.is_empty()) {
                    self.charge_replay(data.len())?;
                    if let Ok(Block::Redacted { data: held, .. }) = self.block_mut(event.index)
                        && held.is_empty()
                    {
                        held.push_str(data);
                        return Ok(());
                    }
                }
                tracing::trace!(kind = other, "ignored anthropic content delta");
            }
        }
        Ok(())
    }

    /// The open block `index` (a delta for a block that never started is a protocol error).
    fn block_mut(&mut self, index: Option<u32>) -> Result<&mut Block, Error> {
        let index = index.ok_or_else(|| protocol("a content_block_delta has no index"))?;
        self.blocks
            .iter_mut()
            .find(|(i, _)| *i == index)
            .map(|(_, block)| block)
            .ok_or_else(|| protocol("a delta for a content block that was never started"))
    }

    /// Forwards a fragment of a tool call's arguments.
    ///
    /// # Errors
    /// [`Error::Protocol`] when no block `index` is open, or when the call, or all calls
    /// together, would exceed the argument caps ([`MAX_CALL_ARGS_BYTES`],
    /// [`MAX_TOTAL_ARGS_BYTES`]); nothing is forwarded then.
    fn push_arguments(
        &mut self,
        index: Option<u32>,
        fragment: &str,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), Error> {
        let total = self.args_bytes.saturating_add(fragment.len());
        let Block::Tool {
            dense,
            args_len,
            any_content,
        } = self.block_mut(index)?
        else {
            // The input of a block this adapter does not run (a server tool, say).
            return Ok(());
        };
        let held = args_len.saturating_add(fragment.len());
        if held > MAX_CALL_ARGS_BYTES || total > MAX_TOTAL_ARGS_BYTES {
            return Err(protocol(format!(
                "the response streams more than {MAX_CALL_ARGS_BYTES} bytes of arguments for \
                 one tool call or {MAX_TOTAL_ARGS_BYTES} bytes for all of them"
            )));
        }
        *args_len = held;
        *any_content |= !fragment.trim().is_empty();
        out.push(LlmEvent::ToolCallDelta {
            index: *dense,
            id: None,
            name: None,
            arguments: Arc::from(fragment),
        });
        self.args_bytes = total;
        Ok(())
    }

    fn block_stop(&mut self, index: Option<u32>, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        let Some(block) = index.and_then(|i| self.take(i)) else {
            return Ok(());
        };
        match block {
            Block::Tool {
                dense, any_content, ..
            } => {
                if !any_content {
                    // A tool without arguments: the input is the empty object.
                    out.push(LlmEvent::ToolCallDelta {
                        index: dense,
                        id: None,
                        name: None,
                        arguments: Arc::from("{}"),
                    });
                }
            }
            Block::Thinking {
                thinking,
                signature,
                slot,
            } => {
                // A block without a signature cannot be replayed (the API would reject it).
                if !signature.is_empty() {
                    let block = serde_json::value::to_raw_value(&ThinkingOut {
                        r#type: "thinking",
                        thinking: &thinking,
                        signature: &signature,
                    })
                    .map_err(|e| protocol(format!("cannot build a thinking block: {e}")))?;
                    self.fill(slot, block);
                }
            }
            Block::Redacted { data, slot } => {
                if !data.is_empty() {
                    let block = serde_json::value::to_raw_value(&RedactedOut {
                        r#type: "redacted_thinking",
                        data: &data,
                    })
                    .map_err(|e| protocol(format!("cannot build a redacted block: {e}")))?;
                    self.fill(slot, block);
                }
            }
            Block::Text | Block::Other => {}
        }
        Ok(())
    }

    fn fill(&mut self, slot: usize, block: Box<RawValue>) {
        if let Some(entry) = self.replay.get_mut(slot) {
            *entry = Some(block);
            self.kept_thinking = true;
        }
    }

    fn message_delta(&mut self, event: &MessageDeltaEvent<'_>) {
        if let Some(body) = &event.delta {
            if let Some(reason) = body.stop_reason.as_deref().filter(|r| !r.is_empty()) {
                self.stop_reason = Some(sanitize(reason));
            }
            self.refusal_category = body
                .stop_details
                .and_then(|raw| serde_json::from_str::<StopDetails<'_>>(raw.get()).ok())
                .and_then(|d| d.category.as_deref().map(sanitize))
                .or_else(|| self.refusal_category.take());
        }
        if let Some(usage) = event.usage.and_then(usage_of) {
            self.usage.merge(&usage);
        }
    }

    /// Maps `message_stop` to `Done`. Everything is read leniently: the answer has been delivered
    /// by now, so an unreadable bookkeeping field must not discard it.
    ///
    /// # Errors
    /// [`Error::Protocol`] if a `tool_use` block is still open: its arguments are cut off (or
    /// missing), and a `Done { ToolCalls }` would hand the caller a call it cannot run.
    fn finish(&mut self, out: &mut Vec<LlmEvent>) -> Result<(), Error> {
        if self.open_tools > 0 {
            return Err(protocol(
                "message_stop arrived while a tool_use block was still open (its arguments are \
                 incomplete)",
            ));
        }
        let had_calls = self.tool_calls > 0;
        let finish = match self.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence") => FinishReason::Stop,
            Some("tool_use") => FinishReason::ToolCalls,
            // Possibly cut in the middle of a call's arguments: the caller must not trust them.
            Some("max_tokens" | "model_context_window_exceeded") => FinishReason::Length,
            Some("refusal") => {
                // No brisvox variant: the answer simply ends (the refusal text, if any, was
                // streamed as text).
                tracing::warn!(
                    category = self.refusal_category.as_deref(),
                    "anthropic declined the request (stop_reason refusal)"
                );
                FinishReason::Stop
            }
            Some("pause_turn") => {
                // Server-tool loop limit: not expected with client tools.
                tracing::debug!("anthropic stop_reason pause_turn");
                FinishReason::Stop
            }
            other => {
                tracing::debug!(
                    stop_reason = other,
                    "anthropic unknown or missing stop_reason"
                );
                if had_calls {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                }
            }
        };
        if finish == FinishReason::Stop && !self.text_seen && !had_calls {
            // Documented: Claude sometimes ends a turn with 2-3 tokens and no content, typically
            // right after tool results. The caller hears nothing: log it, deliver `Done`.
            tracing::warn!("anthropic returned an empty response (dead air)");
        }
        out.push(LlmEvent::Done {
            finish,
            usage: self.usage.seen.then(|| self.usage.usage()),
            provider_data: self.provider_data(),
        });
        Ok(())
    }

    /// The JSON array of the replayable blocks (thinking blocks, call placeholders and the text
    /// marker, in stream order), `None` if no thinking block was kept: without thinking nothing
    /// needs replaying beyond the text and calls the engine records itself.
    fn provider_data(&self) -> Option<Arc<str>> {
        if !self.kept_thinking {
            return None;
        }
        let mut json = String::from("[");
        let mut first = true;
        for entry in self.replay.iter().flatten() {
            if !first {
                json.push(',');
            }
            first = false;
            json.push_str(entry.get());
        }
        json.push(']');
        Some(Arc::from(json))
    }
}

/// A `usage` object, `None` when it is unreadable.
fn usage_of(raw: &RawValue) -> Option<UsageObject> {
    serde_json::from_str(raw.get())
        .inspect_err(|_| tracing::debug!("anthropic usage object is unreadable"))
        .ok()
}

/// A constant JSON text as a raw value.
fn raw(json: &str) -> Result<Box<RawValue>, Error> {
    RawValue::from_string(json.to_owned())
        .map_err(|e| protocol(format!("cannot build a replay marker: {e}")))
}

/// What is safe to say about a JSON error: its category and position. The `Display` of a
/// `serde_json` type error embeds the offending value (`invalid type: string "abc", expected
/// u32`), and the value is server-supplied text.
fn json_error(error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "unexpected data type",
        serde_json::error::Category::Eof => "unexpected end",
    };
    format!(
        "{category} at line {} column {}",
        error.line(),
        error.column()
    )
}

/// Parses the fields of one event kind. The error names the kind and the position, never the
/// data.
fn parse<'a, T: Deserialize<'a>>(data: &'a str, kind: &str) -> Result<T, Error> {
    serde_json::from_str(data)
        .map_err(|e| protocol(format!("a {kind} event is malformed ({})", json_error(&e))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(events: &[&str]) -> (Vec<LlmEvent>, Result<Flow, Error>) {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let mut last = Ok(Flow::Idle);
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

    fn done(out: &[LlmEvent]) -> (FinishReason, Option<Usage>, Option<Arc<str>>) {
        match out.last() {
            Some(LlmEvent::Done {
                finish,
                usage,
                provider_data,
            }) => (*finish, *usage, provider_data.clone()),
            other => panic!("no Done: {other:?} in {out:?}"),
        }
    }

    const START: &str = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#;
    const TOOL_START: &str = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#;

    #[test]
    fn text_deltas_pings_and_a_finished_message_with_usage() {
        let (out, last) = run(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo \"you\"\n"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"some_future_event","x":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        assert_eq!(text(&out), "Hello \"you\"\n");
        let (finish, usage, data) = done(&out);
        assert_eq!(finish, FinishReason::Stop);
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 25,
                cached_input_tokens: 0,
                output_tokens: 15
            })
        );
        assert!(data.is_none());
    }

    #[test]
    fn usage_input_is_the_sum_of_the_three_input_fields_and_the_delta_overrides() {
        let (out, _) = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":30,"cache_creation_input_tokens":100,"cache_read_input_tokens":2000,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42,"input_tokens":null}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(
            done(&out).1,
            Some(Usage {
                input_tokens: 2130,
                cached_input_tokens: 2000,
                output_tokens: 42
            })
        );
        let (out, _) = run(&[
            START,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10682,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":510,"server_tool_use":{"web_search_requests":1}}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(done(&out).1.map(|u| u.input_tokens), Some(10682));
    }

    #[test]
    fn a_tool_call_streams_with_a_dense_index_over_lumpy_fragments() {
        let (out, last) = run(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            TOOL_START,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" CA\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}"#,
            r#"{"type":"message_stop"}"#,
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
                    Some("toolu_1".to_owned()),
                    Some("get_weather".to_owned()),
                    String::new()
                ),
                (0, None, None, "{\"location\":".to_owned()),
                (0, None, None, " \"San".to_owned()),
                (0, None, None, " CA\"}".to_owned()),
            ],
            "block index 1 -> dense index 0; the empty first fragment is skipped"
        );
        let (finish, _, data) = done(&out);
        assert_eq!(finish, FinishReason::ToolCalls);
        assert!(data.is_none(), "no thinking: nothing to replay");
    }

    #[test]
    fn a_call_without_any_argument_text_gets_an_empty_object() {
        let (out, _) = run(&[
            TOOL_START,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let args: String = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { arguments, .. } => Some(&**arguments),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{}");
    }

    #[test]
    fn thinking_and_redacted_blocks_are_assembled_in_order_for_replay() {
        let (out, last) = run(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG=="}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"RED=="}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_9","name":"t","input":{}}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        let (_, _, data) = done(&out);
        let blocks: serde_json::Value = serde_json::from_str(&data.unwrap()).unwrap();
        assert_eq!(
            blocks,
            serde_json::json!([
                {"type": "thinking", "thinking": "Let me think.", "signature": "SIG=="},
                {"type": "redacted_thinking", "data": "RED=="},
                {"type": "text"},
                {"type": "tool_use", "id": "toolu_9", "name": "t"},
            ])
        );
    }

    #[test]
    fn omitted_thinking_keeps_the_signature_and_an_unsigned_block_is_dropped() {
        let (out, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"S"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"x","signature":""}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks: serde_json::Value = serde_json::from_str(&done(&out).2.unwrap()).unwrap();
        assert_eq!(
            blocks,
            serde_json::json!([{"type": "thinking", "thinking": "", "signature": "S"}])
        );
    }

    #[test]
    fn a_redacted_block_whose_data_arrives_in_a_delta_is_still_kept() {
        // UNVERIFIED streaming shape: data empty at the start, carried by an unknown delta.
        let (out, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"redacted_thinking_delta","data":"LATE"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks: serde_json::Value = serde_json::from_str(&done(&out).2.unwrap()).unwrap();
        assert_eq!(
            blocks,
            serde_json::json!([{"type": "redacted_thinking", "data": "LATE"}])
        );
    }

    #[test]
    fn the_stop_reason_table() {
        for (reason, expected) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::Length),
            ("model_context_window_exceeded", FinishReason::Length),
            ("refusal", FinishReason::Stop),
            ("pause_turn", FinishReason::Stop),
            ("some_future_reason", FinishReason::Stop),
        ] {
            let delta = format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}","stop_details":{{"type":"refusal","category":"cyber","explanation":null}}}}}}"#
            );
            let (out, _) = run(&[START, &delta, r#"{"type":"message_stop"}"#]);
            assert_eq!(done(&out).0, expected, "{reason}");
        }
        // No stop_reason at all: calls decide.
        let (out, _) = run(&[
            TOOL_START,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(done(&out).0, FinishReason::ToolCalls);
        let (out, _) = run(&[r#"{"type":"message_stop"}"#]);
        assert_eq!(done(&out).0, FinishReason::Stop);
    }

    #[test]
    fn error_events_map_by_type_and_never_copy_the_message() {
        for (kind, status) in [
            ("overloaded_error", 529_u16),
            ("rate_limit_error", 429),
            ("api_error", 500),
            ("invalid_request_error", 400),
            ("authentication_error", 401),
            ("permission_error", 403),
        ] {
            let event = format!(
                r#"{{"type":"error","error":{{"type":"{kind}","message":"bad key sk-ant-SECRET"}}}}"#
            );
            let (_, last) = run(&[&event]);
            match last {
                Err(Error::Rejected {
                    provider: "anthropic",
                    status: got,
                }) => assert_eq!(got, status, "{kind}"),
                other => panic!("{kind}: {other:?}"),
            }
        }
        let (_, last) = run(&[
            r#"{"type":"error","error":{"type":"weird/type with spaces","message":"sk-ant-SECRET"}}"#,
        ]);
        let Err(Error::Protocol { detail, .. }) = last else {
            panic!("{last:?}");
        };
        assert!(detail.contains("weirdtypewithspaces"), "{detail}");
        assert!(!detail.contains("SECRET"));
        // No type at all, and an `error` member without a type field.
        assert!(matches!(
            run(&[r#"{"type":"error"}"#]).1,
            Err(Error::Protocol { .. })
        ));
        assert!(matches!(
            run(&[r#"{"error":{"type":"overloaded_error"}}"#]).1,
            Err(Error::Rejected { status: 529, .. })
        ));
    }

    #[test]
    fn heartbeats_done_sentinels_and_unknown_events_are_idle() {
        for data in [
            "",
            "   ",
            "[DONE]",
            r#"{"type":"ping"}"#,
            r#"{"type":"x"}"#,
            "{}",
        ] {
            let mut mapper = Mapper::new();
            let mut out = Vec::new();
            assert_eq!(
                mapper.handle(data, &mut out).unwrap(),
                Flow::Idle,
                "{data:?}"
            );
            assert!(out.is_empty());
        }
        let mut mapper = Mapper::new();
        assert!(mapper.handle("{this is not json", &mut Vec::new()).is_err());
    }

    #[test]
    fn violations_are_protocol_errors() {
        // A fragment for a call that never started.
        let (_, last) = run(&[
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        // A tool block without id or name.
        let (_, last) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","input":{}}}"#,
        ]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
        // A start without an index.
        let (_, last) = run(&[r#"{"type":"content_block_start","content_block":{"type":"text"}}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })));
    }

    #[test]
    fn a_message_stop_with_a_tool_block_still_open_is_a_protocol_error() {
        // Cut-off arguments.
        let (out, last) = run(&[
            TOOL_START,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(matches!(last, Err(Error::Protocol { .. })), "{last:?}");
        assert!(
            !out.iter().any(|e| matches!(e, LlmEvent::Done { .. })),
            "no Done with truncated arguments: {out:?}"
        );
        // No argument text at all and no content_block_stop.
        let (out, last) = run(&[TOOL_START, r#"{"type":"message_stop"}"#]);
        assert!(matches!(last, Err(Error::Protocol { .. })), "{last:?}");
        assert!(!out.iter().any(|e| matches!(e, LlmEvent::Done { .. })));
        // A text block left open is harmless.
        let (_, last) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
    }

    #[test]
    fn malformed_events_name_the_kind_and_position_never_the_data() {
        for event in [
            r#"{"type":"content_block_delta","index":"MARKER-abc","delta":{}}"#,
            r#"{"type":"content_block_start","index":"MARKER-abc"}"#,
            r#"{"type":"content_block_stop","index":"MARKER-abc"}"#,
            r#"{"type":"message_start","message":"MARKER-abc"}"#,
            r#"{"type":"message_delta","delta":"MARKER-abc"}"#,
            r#"{MARKER-abc"#,
        ] {
            let (_, last) = run(&[event]);
            let Err(Error::Protocol { detail, .. }) = last else {
                panic!("{event}: {last:?}");
            };
            assert!(!detail.contains("MARKER"), "{detail}");
            assert!(!detail.contains("abc"), "{detail}");
        }
    }

    #[test]
    fn thinking_block_open_tracks_thinking_and_redacted_blocks() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        assert!(!mapper.thinking_block_open());
        mapper
            .handle(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                &mut out,
            )
            .unwrap();
        assert!(mapper.thinking_block_open());
        mapper
            .handle(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"S"}}"#,
                &mut out,
            )
            .unwrap();
        mapper
            .handle(r#"{"type":"content_block_stop","index":0}"#, &mut out)
            .unwrap();
        assert!(!mapper.thinking_block_open());
        mapper
            .handle(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"R"}}"#,
                &mut out,
            )
            .unwrap();
        assert!(mapper.thinking_block_open());
    }

    /// A `content_block_delta` of `kind` carrying `value` in `field`.
    fn delta(index: u32, kind: &str, field: &str, value: &str) -> String {
        format!(
            r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"{kind}","{field}":"{value}"}}}}"#
        )
    }

    /// The start of a `tool_use` block `index` with id `toolu_{index}`.
    fn tool_at(index: u32) -> String {
        format!(
            r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"toolu_{index}","name":"t","input":{{}}}}}}"#
        )
    }

    /// The start of a content block `index` (`block` is the inside of the block object).
    fn start_at(index: u32, block: &str) -> String {
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{{block}}}}}"#)
    }

    /// Bytes of tool-call arguments in `out`.
    fn forwarded_args(out: &[LlmEvent]) -> usize {
        out.iter()
            .map(|e| match e {
                LlmEvent::ToolCallDelta { arguments, .. } => arguments.len(),
                _ => 0,
            })
            .sum()
    }

    /// The arguments of each dense call index of `out`, fragments joined.
    fn joined_args(out: &[LlmEvent]) -> Vec<(u32, String)> {
        let mut calls: Vec<(u32, String)> = Vec::new();
        for event in out {
            if let LlmEvent::ToolCallDelta {
                index, arguments, ..
            } = event
            {
                match calls.iter_mut().find(|c| c.0 == *index) {
                    Some(call) => call.1.push_str(arguments),
                    None => calls.push((*index, arguments.to_string())),
                }
            }
        }
        calls
    }

    #[test]
    fn argument_caps_are_enforced() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        mapper.handle(TOOL_START, &mut out).unwrap();
        let big = "x".repeat(MAX_CALL_ARGS_BYTES / 2 + 1);
        let event = delta(1, "input_json_delta", "partial_json", &big);
        mapper.handle(&event, &mut out).unwrap();
        assert!(matches!(
            mapper.handle(&event, &mut out),
            Err(Error::Protocol { .. })
        ));
        assert_eq!(
            forwarded_args(&out),
            big.len(),
            "nothing is forwarded once the cap trips"
        );
    }

    #[test]
    fn the_total_argument_cap_counts_all_calls_together() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        // Each call stays under its own 1 MiB cap; together they pass 4 MiB at the fifth.
        let fragment = "x".repeat(MAX_CALL_ARGS_BYTES - 1);
        for index in 0..4 {
            mapper.handle(&tool_at(index), &mut out).unwrap();
            mapper
                .handle(
                    &delta(index, "input_json_delta", "partial_json", &fragment),
                    &mut out,
                )
                .unwrap();
        }
        mapper.handle(&tool_at(4), &mut out).unwrap();
        let before = forwarded_args(&out);
        assert!(matches!(
            mapper.handle(
                &delta(4, "input_json_delta", "partial_json", &fragment),
                &mut out
            ),
            Err(Error::Protocol { .. })
        ));
        assert_eq!(
            forwarded_args(&out),
            before,
            "the fragment is not forwarded"
        );
        assert!(before <= MAX_TOTAL_ARGS_BYTES);
    }

    #[test]
    fn the_tool_call_count_is_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let max = u32::try_from(MAX_CALLS).unwrap();
        for index in 0..max {
            mapper.handle(&tool_at(index), &mut out).unwrap();
        }
        let opened = out.len();
        assert!(matches!(
            mapper.handle(&tool_at(max), &mut out),
            Err(Error::Protocol { .. })
        ));
        assert_eq!(out.len(), opened, "the extra call is not announced");
    }

    #[test]
    fn the_content_block_count_is_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        // Blocks of an unknown type: they count, and nothing else grows.
        let max = u32::try_from(MAX_BLOCKS).unwrap();
        for index in 0..max {
            mapper
                .handle(&start_at(index, r#""type":"server_tool_use""#), &mut out)
                .unwrap();
        }
        assert!(matches!(
            mapper.handle(
                &start_at(max, r#""type":"text","text":"never forwarded""#),
                &mut out
            ),
            Err(Error::Protocol { .. })
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn the_replayable_entry_count_is_capped() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        let thinking = r#""type":"thinking","thinking":"","signature":"S""#;
        let max = u32::try_from(MAX_REPLAY_ITEMS).unwrap();
        for index in 0..max {
            mapper.handle(&start_at(index, thinking), &mut out).unwrap();
        }
        assert!(matches!(
            mapper.handle(&start_at(max, thinking), &mut out),
            Err(Error::Protocol { .. })
        ));
        // A tool call needs an entry too.
        assert!(matches!(
            mapper.handle(&tool_at(max + 1), &mut out),
            Err(Error::Protocol { .. })
        ));
        assert!(out.is_empty(), "the extra call is not announced");
    }

    #[test]
    fn the_thinking_byte_cap_covers_starts_deltas_and_signatures() {
        let mut out = Vec::new();
        let open = start_at(0, r#""type":"thinking","thinking":"","signature":"""#);
        let chunk = "t".repeat(MAX_REPLAY_BYTES / 2);
        // Thinking text: exactly the cap is fine, one byte more is not.
        let mut mapper = Mapper::new();
        mapper.handle(&open, &mut out).unwrap();
        for _ in 0..2 {
            mapper
                .handle(&delta(0, "thinking_delta", "thinking", &chunk), &mut out)
                .unwrap();
        }
        assert!(matches!(
            mapper.handle(&delta(0, "thinking_delta", "thinking", "t"), &mut out),
            Err(Error::Protocol { .. })
        ));
        // A signature counts too.
        let mut mapper = Mapper::new();
        mapper.handle(&open, &mut out).unwrap();
        mapper
            .handle(&delta(0, "thinking_delta", "thinking", &chunk), &mut out)
            .unwrap();
        mapper
            .handle(&delta(0, "signature_delta", "signature", &chunk), &mut out)
            .unwrap();
        assert!(matches!(
            mapper.handle(&delta(0, "signature_delta", "signature", "s"), &mut out),
            Err(Error::Protocol { .. })
        ));
        // A block that arrives whole at its start is charged as well.
        let mut mapper = Mapper::new();
        let big = "r".repeat(MAX_REPLAY_BYTES + 1);
        assert!(matches!(
            mapper.handle(
                &start_at(0, &format!(r#""type":"redacted_thinking","data":"{big}""#)),
                &mut out
            ),
            Err(Error::Protocol { .. })
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn parallel_tool_calls_get_dense_indexes_and_their_own_arguments() {
        let (out, last) = run(&[
            START,
            &start_at(0, r#""type":"thinking","thinking":"","signature":"""#),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"S1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Two lookups."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            // The first call has arguments in lumps.
            &tool_at(2),
            &delta(2, "input_json_delta", "partial_json", ""),
            &delta(2, "input_json_delta", "partial_json", "{\\\"a\\\":"),
            &delta(2, "input_json_delta", "partial_json", "1}"),
            r#"{"type":"content_block_stop","index":2}"#,
            // The second has none at all: it still gets its own `{}`.
            &tool_at(3),
            r#"{"type":"content_block_stop","index":3}"#,
            // A redacted block between the calls, then a third call with arguments.
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"redacted_thinking","data":"RED"}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            &tool_at(5),
            &delta(5, "input_json_delta", "partial_json", "{}"),
            r#"{"type":"content_block_stop","index":5}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        assert_eq!(text(&out), "Two lookups.");
        let heads: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta {
                    index,
                    id: Some(id),
                    name: Some(name),
                    ..
                } => Some((*index, id.to_string(), name.to_string())),
                _ => None,
            })
            .collect();
        assert_eq!(
            heads,
            vec![
                (0, "toolu_2".to_owned(), "t".to_owned()),
                (1, "toolu_3".to_owned(), "t".to_owned()),
                (2, "toolu_5".to_owned(), "t".to_owned()),
            ]
        );
        assert_eq!(
            joined_args(&out),
            vec![
                (0, r#"{"a":1}"#.to_owned()),
                (1, "{}".to_owned()),
                (2, "{}".to_owned()),
            ]
        );
        let (_, _, data) = done(&out);
        let blocks: serde_json::Value = serde_json::from_str(&data.unwrap()).unwrap();
        assert_eq!(
            blocks,
            serde_json::json!([
                {"type": "thinking", "thinking": "", "signature": "S1"},
                {"type": "text"},
                {"type": "tool_use", "id": "toolu_2", "name": "t"},
                {"type": "tool_use", "id": "toolu_3", "name": "t"},
                {"type": "redacted_thinking", "data": "RED"},
                {"type": "tool_use", "id": "toolu_5", "name": "t"},
            ])
        );
    }

    #[test]
    fn interleaved_argument_fragments_are_routed_by_block_index() {
        let (out, last) = run(&[
            &tool_at(0),
            &tool_at(1),
            &delta(1, "input_json_delta", "partial_json", "{\\\"b\\\":"),
            &delta(0, "input_json_delta", "partial_json", "{\\\"a\\\":1}"),
            &delta(1, "input_json_delta", "partial_json", "2}"),
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(last.unwrap(), Flow::Finished);
        assert_eq!(
            joined_args(&out),
            vec![(0, r#"{"a":1}"#.to_owned()), (1, r#"{"b":2}"#.to_owned())]
        );
    }

    #[test]
    fn tool_block_open_tracks_the_open_tool_blocks() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        assert!(!mapper.tool_block_open());
        mapper.handle(TOOL_START, &mut out).unwrap();
        assert!(mapper.tool_block_open());
        mapper
            .handle(r#"{"type":"content_block_stop","index":1}"#, &mut out)
            .unwrap();
        assert!(!mapper.tool_block_open());
    }

    #[test]
    fn flow_distinguishes_progress_from_idle() {
        let mut mapper = Mapper::new();
        let mut out = Vec::new();
        assert_eq!(mapper.handle(START, &mut out).unwrap(), Flow::Idle);
        assert_eq!(
            mapper
                .handle(
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                    &mut out
                )
                .unwrap(),
            Flow::Progress,
            "thinking is progress even though it produces no event"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn sanitize_keeps_a_short_safe_token() {
        assert_eq!(sanitize("a b/c\n_d.e-f"), "abc_d.e-f");
        assert_eq!(sanitize(&"x".repeat(500)).len(), MAX_CODE);
    }
}
