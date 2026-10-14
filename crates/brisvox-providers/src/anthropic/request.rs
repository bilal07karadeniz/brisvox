//! Builds the body of `POST /v1/messages` from an [`LlmRequest`].
//!
//! Stateless replay: the whole history is sent each turn, built from the engine's own messages,
//! so after an interruption the model sees only what was played. The layout follows
//! `docs/providers/anthropic.md`:
//!
//! * The leading `System` messages become the top-level `system` (one text block each).
//! * `User` messages are text blocks; consecutive `Tool` messages become the `tool_result`
//!   blocks of ONE user message, in front of any text that follows. Turns strictly alternate:
//!   consecutive messages of one role are merged into one turn.
//! * An `Assistant` message is its thinking blocks (from `provider_data`, unmodified, in their
//!   original positions), the *played* text, and its `tool_use` blocks (with the arguments from
//!   [`Message::tool_calls`]).
//! * A `System` message later in the history becomes a mid-conversation `role:"system"` message
//!   when the docs allow that position, and otherwise a text block of the next user turn.
//! * Tool results must pair with the calls before any I/O happens (the API answers a dangling
//!   `tool_use` with a 400 after a full round trip): a violation is [`Error::Protocol`].
//!
//! Prompt caching (see [`CacheConfig`]) puts `cache_control` on the last tool, the last system
//! block and the last block of the last stable history turn. All message content is sent as
//! arrays of blocks so the bytes of a turn do not change when a breakpoint moves onto or off it.

use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::PROVIDER;
use super::config::{AnthropicConfig, CacheConfig, CacheTtl, ThinkingMode, ToolChoice};
use super::events::sanitize;
use crate::error::{Error, Result};
use crate::llm::{LlmRequest, Message, Role, ToolCall, ToolSpec};

/// Longest tool name Anthropic accepts (`^[a-zA-Z0-9_-]{1,128}$`).
const MAX_TOOL_NAME: usize = 128;

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

/// `cache_control`: `{"type":"ephemeral"}`, plus `"ttl":"1h"` for the one-hour lifetime.
#[derive(Debug, Clone, Copy, Serialize)]
struct CacheControl {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

impl CacheControl {
    fn new(ttl: CacheTtl) -> Self {
        Self {
            r#type: "ephemeral",
            ttl: match ttl {
                CacheTtl::FiveMinutes => None,
                CacheTtl::OneHour => Some("1h"),
            },
        }
    }
}

/// The tool definitions of a request, rendered once and reused while the tools do not change.
#[derive(Debug)]
pub(crate) struct RenderedTools {
    specs: Vec<ToolSpec>,
    strict: bool,
    cache: Option<CacheTtl>,
    json: Box<RawValue>,
}

impl RenderedTools {
    /// Whether this rendering is still valid for `specs` under the same settings.
    pub(crate) fn matches(
        &self,
        specs: &[ToolSpec],
        strict: bool,
        cache: Option<CacheTtl>,
    ) -> bool {
        self.strict == strict && self.cache == cache && self.specs == specs
    }

    /// Whether the tool named `name` is among the rendered tools.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.specs.iter().any(|s| &*s.name == name)
    }

    /// Whether there are no tools.
    pub(crate) fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Renders `specs` as the Messages `tools` array. With `cache`, the last tool carries the
    /// breakpoint that caches all of them.
    pub(crate) fn render(
        specs: &[ToolSpec],
        strict: bool,
        cache: Option<CacheTtl>,
    ) -> Result<Arc<Self>> {
        #[derive(Serialize)]
        struct Tool<'a> {
            name: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            description: &'a str,
            input_schema: &'a RawValue,
            #[serde(skip_serializing_if = "Option::is_none")]
            strict: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cache_control: Option<CacheControl>,
        }
        let mut tools = Vec::with_capacity(specs.len());
        for (n, spec) in specs.iter().enumerate() {
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
            let schema: &RawValue = serde_json::from_str(&spec.parameters_schema).map_err(|e| {
                protocol(format!("tool {name}: parameters schema is not JSON: {e}"))
            })?;
            // Only the root `type` is read (nothing else is built): one scan over the schema.
            #[derive(Deserialize)]
            struct Root<'a> {
                #[serde(default, borrow)]
                r#type: Option<Cow<'a, str>>,
            }
            let is_object = serde_json::from_str::<Root<'_>>(schema.get())
                .ok()
                .is_some_and(|root| root.r#type.as_deref() == Some("object"));
            if !is_object {
                return Err(protocol(format!(
                    "tool {name}: the parameters schema must be a JSON Schema with \
                     \"type\": \"object\" at the root"
                )));
            }
            tools.push(Tool {
                name,
                description: &spec.description,
                input_schema: schema,
                strict: strict.then_some(true),
                cache_control: cache
                    .filter(|_| n + 1 == specs.len())
                    .map(CacheControl::new),
            });
        }
        let json = serde_json::value::to_raw_value(&tools)
            .map_err(|e| protocol(format!("cannot render tools: {e}")))?;
        Ok(Arc::new(Self {
            specs: specs.to_vec(),
            strict,
            cache,
            json,
        }))
    }
}

/// One content block of a request.
#[derive(Serialize)]
#[serde(untagged)]
enum Block<'a> {
    Text {
        r#type: &'static str,
        text: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        r#type: &'static str,
        id: &'a str,
        name: &'a str,
        input: Box<RawValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        r#type: &'static str,
        tool_use_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A `thinking` or `redacted_thinking` block of an earlier response, echoed exactly as
    /// received. Cannot carry a breakpoint (the API does not cache thinking blocks directly).
    Raw(&'a RawValue),
}

impl<'a> Block<'a> {
    fn text(text: impl Into<Cow<'a, str>>) -> Self {
        Self::Text {
            r#type: "text",
            text: text.into(),
            cache_control: None,
        }
    }

    /// Puts a breakpoint on this block; `false` when the block cannot carry one.
    fn mark(&mut self, control: CacheControl) -> bool {
        match self {
            Self::Text { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. } => {
                *cache_control = Some(control);
                true
            }
            Self::Raw(_) => false,
        }
    }

    fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }
}

/// The role of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnRole {
    User,
    Assistant,
    /// A mid-conversation `role:"system"` message.
    System,
}

/// One message of the request.
struct Turn<'a> {
    role: TurnRole,
    blocks: Vec<Block<'a>>,
    /// The text of a [`TurnRole::System`] turn (sent as a plain string).
    system_text: Option<&'a str>,
}

impl<'a> Turn<'a> {
    fn new(role: TurnRole) -> Self {
        Self {
            role,
            blocks: Vec::new(),
            system_text: None,
        }
    }
}

impl Serialize for Turn<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Turn", 2)?;
        match self.role {
            TurnRole::User => state.serialize_field("role", "user")?,
            TurnRole::Assistant => state.serialize_field("role", "assistant")?,
            TurnRole::System => state.serialize_field("role", "system")?,
        }
        match self.system_text {
            Some(text) => state.serialize_field("content", text)?,
            None => state.serialize_field("content", &self.blocks)?,
        }
        state.end()
    }
}

#[derive(Serialize)]
struct Thinking {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<&'static str>,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: &'static str,
}

#[derive(Serialize)]
struct ToolChoiceBody<'a> {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_parallel_tool_use: Option<bool>,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceBody<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a RawValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<Block<'a>>,
    messages: Vec<Turn<'a>>,
}

/// The fields of a replayed block the adapter needs to place it.
#[derive(Deserialize)]
struct BlockHead<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
}

/// One replayable entry of an earlier response.
enum ReplayItem<'a> {
    /// A `thinking` or `redacted_thinking` block, echoed unmodified.
    Thinking(&'a RawValue),
    /// A `tool_use` block, by call id (the arguments come from [`Message::tool_calls`]).
    Call(String),
    /// Where the played text sat (the adapter's own placeholder, see `events::TEXT_MARKER`).
    Text,
}

/// The replayable entries of an earlier response (`Message::provider_data`), in the order the
/// server produced them.
#[derive(Default)]
struct Replay<'a> {
    items: Vec<ReplayItem<'a>>,
}

impl<'a> Replay<'a> {
    /// Parses `provider_data`. Data that is not an array of Anthropic blocks (for example from
    /// another provider) yields nothing: adapters ignore data that is not theirs.
    fn parse(provider_data: &'a str) -> Self {
        let mut replay = Self::default();
        let Ok(items) = serde_json::from_str::<Vec<&'a RawValue>>(provider_data) else {
            return replay;
        };
        for raw in items {
            let Ok(head) = serde_json::from_str::<BlockHead<'_>>(raw.get()) else {
                continue;
            };
            match (head.kind.as_ref(), head.id) {
                ("thinking" | "redacted_thinking", _) => {
                    replay.items.push(ReplayItem::Thinking(raw));
                }
                ("tool_use", Some(id)) => replay.items.push(ReplayItem::Call(id.into_owned())),
                ("text", _) if !replay.items.iter().any(|i| matches!(i, ReplayItem::Text)) => {
                    replay.items.push(ReplayItem::Text);
                }
                _ => {}
            }
        }
        replay
    }

    fn has_thinking(&self) -> bool {
        self.items
            .iter()
            .any(|i| matches!(i, ReplayItem::Thinking(_)))
    }

    /// The index of the item the played text goes before, when the data has no text marker:
    /// before the thinking run that leads into the first call, so no thinking block is separated
    /// from the block it belongs to; after everything if there is no call.
    fn text_position(&self) -> usize {
        let Some(first_call) = self
            .items
            .iter()
            .position(|i| matches!(i, ReplayItem::Call(_)))
        else {
            return self.items.len();
        };
        let mut at = first_call;
        while at > 0 && matches!(self.items.get(at - 1), Some(ReplayItem::Thinking(_))) {
            at -= 1;
        }
        at
    }
}

/// A `tool_use` block for `call`, with its arguments as the `input` object.
fn tool_use(call: &ToolCall) -> Result<Block<'_>> {
    let arguments = call.arguments.trim();
    // A tool without arguments streams no text at all (or `{}`); both mean the empty object.
    let arguments = if arguments.is_empty() {
        "{}"
    } else {
        arguments
    };
    let input: Box<RawValue> = serde_json::from_str(arguments).map_err(|e| {
        protocol(format!(
            "arguments of tool call {} are not JSON: {e}",
            sanitize(&call.id)
        ))
    })?;
    if !input.get().starts_with('{') {
        return Err(protocol(format!(
            "arguments of tool call {} are not a JSON object",
            sanitize(&call.id)
        )));
    }
    Ok(Block::ToolUse {
        r#type: "tool_use",
        id: &call.id,
        name: &call.name,
        input,
        cache_control: None,
    })
}

/// Replays one assistant turn into `blocks`: the thinking blocks, the text that was played and
/// the tool calls, in the order the server produced them (a thinking block stays directly in
/// front of the block it belongs to). With `replay_thinking` off, or without provider data, the
/// turn is rebuilt from the engine's own record: the text, then one `tool_use` per call.
fn push_assistant<'a>(
    blocks: &mut Vec<Block<'a>>,
    message: &'a Message,
    replay: Option<&Replay<'a>>,
    config: &AnthropicConfig,
) -> Result<()> {
    let mut text = (!message.content.trim().is_empty()).then(|| Block::text(&*message.content));
    let mut sent_calls: Vec<&str> = Vec::new();
    if let Some(replay) = replay.filter(|r| config.replay_thinking && r.has_thinking()) {
        let marker = replay
            .items
            .iter()
            .position(|i| matches!(i, ReplayItem::Text));
        let text_at = marker.unwrap_or_else(|| replay.text_position());
        // Thinking waiting for the block that follows it. It goes out in front of the next kept
        // block (the text once placed, or a call); it is dropped only together with a call that is
        // not replayed, or when nothing kept follows it in the turn.
        let mut pending: Vec<&'a RawValue> = Vec::new();
        for at in 0..=replay.items.len() {
            if at == text_at
                && let Some(text) = text.take()
            {
                blocks.extend(pending.drain(..).map(Block::Raw));
                blocks.push(text);
            }
            match replay.items.get(at) {
                None => break,
                Some(ReplayItem::Thinking(raw)) => pending.push(raw),
                // The text marker: the text was placed above, or was not played. Thinking waiting
                // here stays pending for the next kept block (a call): only the unplayed text is
                // removed, so the thinking sits directly in front of the surviving `tool_use` and
                // the tool-use turn is still echoed with its thinking (the docs keep a thinking
                // block valid when the text after it is edited).
                Some(ReplayItem::Text) => {}
                Some(ReplayItem::Call(id)) => {
                    let call = message
                        .tool_calls
                        .iter()
                        .find(|c| *c.id == **id)
                        .filter(|c| !sent_calls.contains(&&*c.id));
                    match call {
                        Some(call) => {
                            blocks.extend(pending.drain(..).map(Block::Raw));
                            blocks.push(tool_use(call)?);
                            sent_calls.push(&call.id);
                        }
                        None => pending.clear(),
                    }
                }
            }
        }
    }
    blocks.extend(text);
    for call in &message.tool_calls {
        if !sent_calls.contains(&&*call.id) {
            blocks.push(tool_use(call)?);
        }
    }
    Ok(())
}

/// Splits the leading system messages (the top-level `system`) from the rest.
fn split_system(messages: &[Message]) -> (&[Message], &[Message]) {
    let leading = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    messages.split_at(leading)
}

/// The turns of `rest` (the history after the leading system messages).
fn build_turns<'a>(
    rest: &'a [Message],
    replays: &[Option<Replay<'a>>],
    config: &AnthropicConfig,
) -> Result<Vec<Turn<'a>>> {
    let mut turns: Vec<Turn<'a>> = Vec::with_capacity(rest.len());
    // Calls made by the history so far that no tool message has answered yet.
    let mut open: Vec<&str> = Vec::new();
    // Late system notes waiting for the next user turn.
    let mut notes: Vec<&str> = Vec::new();
    for (i, (message, replay)) in rest.iter().zip(replays).enumerate() {
        if !matches!(message.role, Role::Tool | Role::System) && !open.is_empty() {
            return Err(dangling(open.first().copied()));
        }
        match message.role {
            Role::System => {
                if message.content.trim().is_empty() {
                    continue;
                }
                // The docs allow a mid-conversation system message right after a user turn, when
                // an assistant turn (or the end of the history) follows; anywhere else it is a
                // 400, so the note rides in the next user turn instead. Only with
                // `mid_conversation_system` (model-dependent, wire shape unverified); a final
                // pass below re-checks the placement once dropped (empty) assistant turns are
                // known.
                let after_user = matches!(
                    turns.last().map(|t| t.role),
                    Some(TurnRole::User | TurnRole::System)
                );
                let before_assistant = rest
                    .get(i + 1..)
                    .unwrap_or_default()
                    .iter()
                    .find(|m| m.role != Role::System)
                    .is_none_or(|m| m.role == Role::Assistant);
                if config.mid_conversation_system
                    && after_user
                    && before_assistant
                    && notes.is_empty()
                    && open.is_empty()
                {
                    let mut turn = Turn::new(TurnRole::System);
                    turn.system_text = Some(&message.content);
                    turns.push(turn);
                } else {
                    notes.push(&message.content);
                }
            }
            Role::User => {
                if message.content.trim().is_empty() {
                    return Err(protocol(
                        "a user message is empty (the API rejects empty text blocks)",
                    ));
                }
                let turn = tail_turn(&mut turns, |last| last.role == TurnRole::User)?;
                for note in notes.drain(..) {
                    turn.blocks.push(Block::text(note));
                }
                turn.blocks.push(Block::text(&*message.content));
            }
            Role::Assistant => {
                flush_notes(&mut turns, &mut notes)?;
                let mut blocks = Vec::new();
                push_assistant(&mut blocks, message, replay.as_ref(), config)?;
                if blocks.is_empty() {
                    // Nothing was played and nothing was called: the turn leaves no trace.
                    continue;
                }
                // A turn with calls is never dropped, so each of its calls is in the request.
                open.extend(message.tool_calls.iter().map(|c| &*c.id));
                match turns.last_mut() {
                    Some(last) if last.role == TurnRole::Assistant => last.blocks.extend(blocks),
                    _ => {
                        let mut turn = Turn::new(TurnRole::Assistant);
                        turn.blocks = blocks;
                        turns.push(turn);
                    }
                }
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
                // Tool results open a user turn and are the only content of it until the text
                // that follows.
                let turn = tail_turn(&mut turns, |last| {
                    last.role == TurnRole::User && last.blocks.iter().all(Block::is_tool_result)
                })?;
                turn.blocks.push(Block::ToolResult {
                    r#type: "tool_result",
                    tool_use_id: call_id,
                    content: (!message.content.is_empty()).then_some(&*message.content),
                    cache_control: None,
                });
            }
        }
    }
    if let Some(call_id) = open.first() {
        return Err(dangling(Some(call_id)));
    }
    flush_notes(&mut turns, &mut notes)?;
    Ok(fold_stray_system(turns))
}

/// Folds every mid-conversation system turn that is not followed by an assistant turn (or the end
/// of the history) into the next user turn. The placement in the loop above looks at the input
/// history, but an assistant message that rendered to nothing (interrupted before any audio, no
/// calls) leaves no turn, which can put a system turn directly in front of a user turn: an HTTP
/// 400 that would only show after a full round trip. The folded notes ride in the user turn that
/// precedes the system turn, in front of the user text that follows.
fn fold_stray_system(turns: Vec<Turn<'_>>) -> Vec<Turn<'_>> {
    // For each turn: whether it stays a system turn. The first non-system turn after a run of
    // system turns decides for the whole run.
    let mut stays = vec![true; turns.len()];
    let mut following: Option<TurnRole> = None;
    for (turn, stay) in turns.iter().zip(stays.iter_mut()).rev() {
        if turn.role == TurnRole::System {
            *stay = following.is_none_or(|role| role == TurnRole::Assistant);
        } else {
            following = Some(turn.role);
        }
    }
    if stays.iter().all(|stay| *stay) {
        return turns;
    }
    let mut out: Vec<Turn<'_>> = Vec::with_capacity(turns.len());
    let mut folded: Vec<&str> = Vec::new();
    for (turn, stay) in turns.into_iter().zip(stays) {
        if turn.role == TurnRole::System && !stay {
            folded.extend(turn.system_text);
            continue;
        }
        if folded.is_empty() || turn.role != TurnRole::User {
            out.push(turn);
            continue;
        }
        let notes = folded.drain(..).map(Block::text);
        match out.last_mut() {
            // The user turn the system message followed: one user turn, notes first.
            Some(last) if last.role == TurnRole::User => {
                last.blocks.extend(notes);
                last.blocks.extend(turn.blocks);
            }
            _ => {
                let mut merged = Turn::new(TurnRole::User);
                merged.blocks.extend(notes);
                merged.blocks.extend(turn.blocks);
                out.push(merged);
            }
        }
    }
    out
}

fn dangling(call_id: Option<&str>) -> Error {
    protocol(format!(
        "tool call {} has no tool result in the history (cancelled tool?): the caller must \
         answer or drop it before the next request",
        sanitize(call_id.unwrap_or_default())
    ))
}

/// The last turn when `joins` accepts it, else a new user turn (which becomes the last).
fn tail_turn<'a, 'b>(
    turns: &'b mut Vec<Turn<'a>>,
    joins: impl FnOnce(&Turn<'a>) -> bool,
) -> Result<&'b mut Turn<'a>> {
    if !turns.last().is_some_and(joins) {
        turns.push(Turn::new(TurnRole::User));
    }
    turns
        .last_mut()
        .ok_or_else(|| protocol("internal error: no turn to extend"))
}

/// Moves pending late system notes into the last turn, which must be a user turn (the notes go
/// after its tool results).
fn flush_notes<'a>(turns: &mut [Turn<'a>], notes: &mut Vec<&'a str>) -> Result<()> {
    if notes.is_empty() {
        return Ok(());
    }
    match turns.last_mut() {
        Some(last) if last.role == TurnRole::User => {
            last.blocks.extend(notes.drain(..).map(Block::text));
            Ok(())
        }
        _ => Err(protocol(
            "a system message in the history has no user message to ride in \
             (the API allows a mid-conversation system message only right after a user turn)",
        )),
    }
}

/// Puts the breakpoint of `cache.history` on the last cacheable block of the last stable turn:
/// the last user or assistant turn before the newest one.
fn mark_history(turns: &mut [Turn<'_>], control: CacheControl) {
    let mut content: Vec<&mut Turn<'_>> = turns
        .iter_mut()
        .filter(|t| t.role != TurnRole::System)
        .collect();
    // Everything but the newest turn is what the previous request already sent.
    content.pop();
    for turn in content.into_iter().rev() {
        if turn.blocks.iter_mut().rev().any(|b| b.mark(control)) {
            return;
        }
    }
}

/// The JSON body for `request`.
pub(crate) fn build_body(
    config: &AnthropicConfig,
    request: &LlmRequest,
    tools: Option<&RenderedTools>,
) -> Result<Bytes> {
    let (system_messages, rest) = split_system(&request.messages);
    // Parsed first so the borrows outlive the blocks that point into them.
    let replays: Vec<Option<Replay<'_>>> = rest
        .iter()
        .map(|m| {
            (m.role == Role::Assistant)
                .then_some(m.provider_data.as_deref())
                .flatten()
                .map(Replay::parse)
        })
        .collect();
    let mut turns = build_turns(rest, &replays, config)?;
    match turns.iter().rev().find(|t| t.role != TurnRole::System) {
        None => {
            return Err(protocol(
                "the request has no user message (only system prompts or nothing)",
            ));
        }
        Some(last) if last.role != TurnRole::User => {
            return Err(protocol(
                "the conversation must end with a user turn: the API rejects an assistant \
                 prefill on the Claude 5.x models",
            ));
        }
        Some(_) => {}
    }

    let CacheConfig {
        system: cache_system,
        history: cache_history,
        ttl,
        ..
    } = config.cache;
    let control = CacheControl::new(ttl);
    let mut system: Vec<Block<'_>> = system_messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .map(|m| Block::text(&*m.content))
        .collect();
    if cache_system && let Some(last) = system.last_mut() {
        last.mark(control);
    }
    if cache_history {
        mark_history(&mut turns, control);
    }

    let tools = tools.filter(|t| !t.is_empty());
    let tool_choice = match tools {
        Some(tools) => tool_choice_body(config, tools)?,
        None => None,
    };
    let body = Body {
        model: &config.model,
        max_tokens: config.max_tokens,
        stream: true,
        thinking: thinking_body(config),
        output_config: config.effort.map(|e| OutputConfig { effort: e.as_str() }),
        tool_choice,
        tools: tools.map(|t| &*t.json),
        system,
        messages: turns,
    };
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|e| protocol(format!("cannot serialise the request: {e}")))
}

fn thinking_body(config: &AnthropicConfig) -> Option<Thinking> {
    let (kind, shows) = match config.thinking {
        ThinkingMode::Disabled => ("disabled", false),
        ThinkingMode::Adaptive => ("adaptive", true),
        ThinkingMode::BetweenTools => ("between_tools", true),
        ThinkingMode::ModelDefault => return None,
    };
    Some(Thinking {
        r#type: kind,
        display: config
            .thinking_display
            .filter(|_| shows)
            .map(|d| d.as_str()),
    })
}

fn tool_choice_body<'a>(
    config: &'a AnthropicConfig,
    tools: &RenderedTools,
) -> Result<Option<ToolChoiceBody<'a>>> {
    let disable = config.disable_parallel_tool_use.then_some(true);
    Ok(match &config.tool_choice {
        ToolChoice::Auto => disable.map(|d| ToolChoiceBody {
            r#type: "auto",
            name: None,
            disable_parallel_tool_use: Some(d),
        }),
        ToolChoice::Any => Some(ToolChoiceBody {
            r#type: "any",
            name: None,
            disable_parallel_tool_use: disable,
        }),
        ToolChoice::Tool(name) => {
            if !tools.contains(name) {
                return Err(protocol(format!(
                    "tool_choice names tool {}, which is not among the request's tools",
                    sanitize(name)
                )));
            }
            Some(ToolChoiceBody {
                r#type: "tool",
                name: Some(name),
                disable_parallel_tool_use: disable,
            })
        }
        ToolChoice::None => Some(ToolChoiceBody {
            r#type: "none",
            name: None,
            disable_parallel_tool_use: None,
        }),
    })
}

#[cfg(test)]
mod tests {
    use brisvox_core::Secret;
    use serde_json::{Value, json};

    use super::*;
    use crate::anthropic::config::{Effort, ThinkingDisplay};

    fn config() -> AnthropicConfig {
        AnthropicConfig::new(Secret::new("sk-ant-test".to_owned()))
    }

    fn body_with(config: &AnthropicConfig, request: &LlmRequest) -> Result<Value> {
        let cache = (config.cache.tools).then_some(config.cache.ttl);
        let tools = RenderedTools::render(&request.tools, config.strict_tools, cache)?;
        let bytes = build_body(config, request, Some(&tools))?;
        serde_json::from_slice(&bytes).map_err(|e| protocol(e.to_string()))
    }

    fn body(config: &AnthropicConfig, request: &LlmRequest) -> Value {
        body_with(config, request).unwrap()
    }

    fn spec(name: &str, schema: &str) -> ToolSpec {
        ToolSpec {
            name: Arc::from(name),
            description: Arc::from("does a thing"),
            parameters_schema: Arc::from(schema),
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: Arc::from(id),
            name: Arc::from(name),
            arguments: Arc::from(arguments),
        }
    }

    fn req(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            messages,
            tools: Vec::new(),
        }
    }

    fn no_cache() -> AnthropicConfig {
        let mut c = config();
        c.cache = CacheConfig::OFF;
        c
    }

    const OBJ: &str = r#"{"type":"object","properties":{"city":{"type":"string"}}}"#;

    #[test]
    fn a_plain_conversation_is_sent_stateless_with_leading_system_as_blocks() {
        let request = req(vec![
            Message::new(Role::System, "Be brief."),
            Message::new(Role::System, "Speak Turkish."),
            Message::new(Role::User, "Merhaba"),
            Message::new(Role::Assistant, "Selam!"),
            Message::new(Role::User, "Nasılsın?"),
        ]);
        assert_eq!(
            body(&no_cache(), &request),
            json!({
                "model": "claude-haiku-5-5",
                "max_tokens": 1024,
                "stream": true,
                "thinking": {"type": "disabled"},
                "system": [
                    {"type": "text", "text": "Be brief."},
                    {"type": "text", "text": "Speak Turkish."},
                ],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "Merhaba"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Selam!"}]},
                    {"role": "user", "content": [{"type": "text", "text": "Nasılsın?"}]},
                ],
            })
        );
    }

    #[test]
    fn breakpoints_go_on_the_last_tool_the_last_system_block_and_the_last_stable_turn() {
        let mut request = req(vec![
            Message::new(Role::System, "Persona."),
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ]);
        request.tools = vec![spec("a", OBJ), spec("b", OBJ)];
        let b = body(&config(), &request);
        let ephemeral = json!({"type": "ephemeral"});
        assert!(b["tools"][0].get("cache_control").is_none());
        assert_eq!(b["tools"][1]["cache_control"], ephemeral);
        assert_eq!(b["system"][0]["cache_control"], ephemeral);
        assert!(
            b["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(b["messages"][1]["content"][0]["cache_control"], ephemeral);
        assert!(
            b["messages"][2]["content"][0]
                .get("cache_control")
                .is_none(),
            "the newest turn is volatile: never cached"
        );
        // Three of the four allowed breakpoints.
        assert_eq!(b.to_string().matches("cache_control").count(), 3);
    }

    #[test]
    fn the_one_hour_ttl_and_switching_the_breakpoints_off() {
        let mut c = config();
        c.cache.ttl = CacheTtl::OneHour;
        let request = req(vec![
            Message::new(Role::System, "Persona."),
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ]);
        let b = body(&c, &request);
        assert_eq!(
            b["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(
            b["messages"][1]["content"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert!(
            !body(&no_cache(), &request)
                .to_string()
                .contains("cache_control")
        );
        // A single user turn has no stable history: only the system prompt is marked.
        let single = req(vec![
            Message::new(Role::System, "Persona."),
            Message::new(Role::User, "one"),
        ]);
        assert_eq!(
            body(&config(), &single)
                .to_string()
                .matches("cache_control")
                .count(),
            1
        );
    }

    #[test]
    fn the_history_breakpoint_skips_thinking_blocks_and_lands_on_a_tool_use() {
        let thinking = r#"[{"type":"thinking","thinking":"hm","signature":"S"},{"type":"tool_use","id":"toolu_1","name":"t"}]"#;
        let request = req(vec![
            Message::new(Role::User, "weather?"),
            Message::assistant(
                "",
                vec![call("toolu_1", "t", r#"{"a":1}"#)],
                Some(Arc::from(thinking)),
            ),
            Message::tool_result("toolu_1", "sunny"),
        ]);
        let b = body(&config(), &request);
        let assistant = &b["messages"][1]["content"];
        assert_eq!(assistant[0]["type"], "thinking");
        assert!(assistant[0].get("cache_control").is_none());
        assert_eq!(assistant[1]["type"], "tool_use");
        assert_eq!(assistant[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn tool_calls_results_and_following_text_form_alternating_turns() {
        let request = req(vec![
            Message::new(Role::User, "weather in two cities"),
            Message::assistant(
                "Checking.",
                vec![
                    call("toolu_1", "get_weather", r#"{"city":"Paris"}"#),
                    call("toolu_2", "get_weather", ""),
                ],
                None,
            ),
            Message::tool_result("toolu_2", "rainy"),
            Message::tool_result("toolu_1", ""),
            Message::new(Role::User, "and then?"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "weather in two cities"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "get_weather", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "rainy"},
                    {"type": "tool_result", "tool_use_id": "toolu_1"},
                    {"type": "text", "text": "and then?"},
                ]},
            ])
        );
    }

    #[test]
    fn thinking_blocks_are_echoed_unmodified_in_their_original_positions() {
        let data = r#"[{"type":"thinking","thinking":"first","signature":"S1"},{"type":"text"},{"type":"redacted_thinking","data":"RED"},{"type":"tool_use","id":"toolu_1","name":"t"}]"#;
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant(
                "Sure.",
                vec![call("toolu_1", "t", "{}")],
                Some(Arc::from(data)),
            ),
            Message::tool_result("toolu_1", "ok"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "first", "signature": "S1"},
                {"type": "text", "text": "Sure."},
                {"type": "redacted_thinking", "data": "RED"},
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ])
        );
        // Turned off: the turn is rebuilt from the engine's record.
        let mut c = no_cache();
        c.replay_thinking = false;
        assert_eq!(
            body(&c, &request)["messages"][1]["content"],
            json!([
                {"type": "text", "text": "Sure."},
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ])
        );
    }

    #[test]
    fn an_unplayed_preamble_keeps_the_thinking_in_front_of_the_surviving_call() {
        // Up-front thinking, a preamble that was not played, then the call that is kept.
        let data = r#"[{"type":"thinking","thinking":"a","signature":"S1"},{"type":"text"},{"type":"tool_use","id":"toolu_1","name":"t"}]"#;
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant("", vec![call("toolu_1", "t", "")], Some(Arc::from(data))),
            Message::tool_result("toolu_1", "ok"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "a", "signature": "S1"},
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ])
        );
        // With the preamble played, the order is thinking, text, call.
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant(
                "Let me check",
                vec![call("toolu_1", "t", "")],
                Some(Arc::from(data)),
            ),
            Message::tool_result("toolu_1", "ok"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "a", "signature": "S1"},
                {"type": "text", "text": "Let me check"},
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {}},
            ])
        );
    }

    #[test]
    fn an_interrupted_turn_drops_thinking_that_led_into_what_was_not_played() {
        let data = r#"[{"type":"thinking","thinking":"a","signature":"S1"},{"type":"text"},{"type":"thinking","thinking":"b","signature":"S2"},{"type":"tool_use","id":"toolu_1","name":"t"}]"#;
        // Nothing played, and the call was dropped: the whole turn leaves no trace.
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant("", Vec::new(), Some(Arc::from(data))),
            Message::new(Role::User, "never mind"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "go"},
                {"type": "text", "text": "never mind"},
            ]}])
        );
        // Text played but the call dropped: the thinking in front of the call goes with it.
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant("Part of it", Vec::new(), Some(Arc::from(data))),
            Message::new(Role::User, "stop"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "a", "signature": "S1"},
                {"type": "text", "text": "Part of it"},
            ])
        );
    }

    #[test]
    fn data_of_another_provider_is_ignored() {
        let openai = r#"[{"type":"reasoning","encrypted_content":"X"},{"type":"message"}]"#;
        let request = req(vec![
            Message::new(Role::User, "go"),
            Message::assistant("hi", Vec::new(), Some(Arc::from(openai))),
            Message::new(Role::User, "again"),
        ]);
        assert_eq!(
            body(&no_cache(), &request)["messages"][1]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
    }

    #[test]
    fn late_system_messages_use_the_documented_positions_or_ride_in_the_next_user_turn() {
        let mut mid = no_cache();
        mid.mid_conversation_system = true;
        // Right after a user turn, before an assistant turn: a system message.
        let request = req(vec![
            Message::new(Role::User, "one"),
            Message::new(Role::System, "note A"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ]);
        assert_eq!(
            body(&mid, &request)["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "one"}]},
                {"role": "system", "content": "note A"},
                {"role": "assistant", "content": [{"type": "text", "text": "two"}]},
                {"role": "user", "content": [{"type": "text", "text": "three"}]},
            ])
        );
        // The safe default (model-dependent, wire shape unverified): the note rides in the user
        // turn it followed, whatever the model.
        assert_eq!(
            body(&no_cache(), &request)["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "note A"},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "two"}]},
                {"role": "user", "content": [{"type": "text", "text": "three"}]},
            ])
        );
        // After an assistant turn (a 400 as a system message): a text block of the next user turn.
        let request = req(vec![
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::System, "note B"),
            Message::new(Role::User, "three"),
        ]);
        for config in [&mid, &no_cache()] {
            assert_eq!(
                body(config, &request)["messages"][2],
                json!({"role": "user", "content": [
                    {"type": "text", "text": "note B"},
                    {"type": "text", "text": "three"},
                ]})
            );
        }
        // Between a tool call and its result: after the tool results.
        let request = req(vec![
            Message::new(Role::User, "one"),
            Message::assistant("", vec![call("toolu_1", "t", "{}")], None),
            Message::new(Role::System, "note C"),
            Message::tool_result("toolu_1", "ok"),
        ]);
        assert_eq!(
            body(&mid, &request)["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
                {"type": "text", "text": "note C"},
            ]})
        );
        // Last in the history, after a user turn: allowed ("be last") when enabled.
        let request = req(vec![
            Message::new(Role::User, "one"),
            Message::new(Role::System, "note D"),
        ]);
        assert_eq!(
            body(&mid, &request)["messages"][1],
            json!({"role": "system", "content": "note D"})
        );
        assert_eq!(
            body(&no_cache(), &request)["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "one"},
                {"type": "text", "text": "note D"},
            ]}])
        );
        // After an assistant turn with nothing to ride in: refused before any I/O.
        let request = req(vec![
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::System, "note E"),
        ]);
        assert!(body_with(&mid, &request).is_err());
        assert!(body_with(&no_cache(), &request).is_err());
    }

    #[test]
    fn a_late_system_message_before_a_dropped_assistant_turn_never_precedes_a_user_turn() {
        // The assistant turn was interrupted before any audio and has no calls: it leaves no
        // turn, so the system message would sit directly in front of the next user turn (a 400:
        // it must be last or followed by an assistant turn).
        let mut mid = no_cache();
        mid.mid_conversation_system = true;
        let request = req(vec![
            Message::new(Role::User, "A"),
            Message::new(Role::System, "note"),
            Message::new(Role::Assistant, ""),
            Message::new(Role::User, "B"),
        ]);
        for config in [&mid, &no_cache()] {
            assert_eq!(
                body(config, &request)["messages"],
                json!([{"role": "user", "content": [
                    {"type": "text", "text": "A"},
                    {"type": "text", "text": "note"},
                    {"type": "text", "text": "B"},
                ]}])
            );
        }
        // Several notes, and a thinking-only assistant turn that is dropped as well.
        let data = r#"[{"type":"thinking","thinking":"a","signature":"S1"},{"type":"text"}]"#;
        let request = req(vec![
            Message::new(Role::User, "A"),
            Message::new(Role::System, "n1"),
            Message::new(Role::System, "n2"),
            Message::assistant("", Vec::new(), Some(Arc::from(data))),
            Message::new(Role::User, "B"),
            Message::new(Role::Assistant, "ok"),
            Message::new(Role::User, "C"),
        ]);
        assert_eq!(
            body(&mid, &request)["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "A"},
                    {"type": "text", "text": "n1"},
                    {"type": "text", "text": "n2"},
                    {"type": "text", "text": "B"},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": "C"}]},
            ])
        );
        // The dropped turn at the very end: the note is last, which the docs allow.
        let request = req(vec![
            Message::new(Role::User, "A"),
            Message::new(Role::System, "note"),
            Message::new(Role::Assistant, ""),
        ]);
        assert_eq!(
            body(&mid, &request)["messages"][1],
            json!({"role": "system", "content": "note"})
        );
        // No system turn before a user turn in any of the shapes.
        let value = body(
            &mid,
            &req(vec![
                Message::new(Role::User, "A"),
                Message::new(Role::System, "note"),
                Message::new(Role::Assistant, ""),
                Message::new(Role::Assistant, ""),
                Message::new(Role::User, "B"),
            ]),
        );
        let roles: Vec<&str> = value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user"]);
    }

    #[test]
    fn invalid_histories_are_refused_before_any_io() {
        let cases: Vec<(&str, LlmRequest)> = vec![
            ("nothing", req(vec![])),
            ("only system", req(vec![Message::new(Role::System, "x")])),
            ("empty user", req(vec![Message::new(Role::User, "  \n")])),
            (
                "ends with assistant",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::new(Role::Assistant, "b"),
                ]),
            ),
            (
                "dangling call",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("", vec![call("toolu_1", "t", "{}")], None),
                    Message::new(Role::User, "b"),
                ]),
            ),
            (
                "dangling call at the end",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("", vec![call("toolu_1", "t", "{}")], None),
                ]),
            ),
            (
                "orphan result",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::tool_result("toolu_9", "x"),
                ]),
            ),
            (
                "answered twice",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("", vec![call("toolu_1", "t", "{}")], None),
                    Message::tool_result("toolu_1", "x"),
                    Message::tool_result("toolu_1", "y"),
                ]),
            ),
            (
                "arguments are not an object",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("", vec![call("toolu_1", "t", "[1]")], None),
                    Message::tool_result("toolu_1", "x"),
                ]),
            ),
            (
                "arguments are cut off",
                req(vec![
                    Message::new(Role::User, "a"),
                    Message::assistant("", vec![call("toolu_1", "t", "{\"a\":")], None),
                    Message::tool_result("toolu_1", "x"),
                ]),
            ),
        ];
        for (name, request) in cases {
            assert!(
                matches!(
                    body_with(&no_cache(), &request),
                    Err(Error::Protocol { .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn tools_are_validated_and_rendered_with_the_documented_fields() {
        let mut c = no_cache();
        c.strict_tools = true;
        let mut request = req(vec![Message::new(Role::User, "hi")]);
        request.tools = vec![ToolSpec {
            name: Arc::from("get_weather"),
            description: Arc::from(""),
            parameters_schema: Arc::from(OBJ),
        }];
        assert_eq!(
            body(&c, &request)["tools"],
            json!([{
                "name": "get_weather",
                "input_schema": serde_json::from_str::<Value>(OBJ).unwrap(),
                "strict": true,
            }]),
            "an empty description is left out"
        );
        for (name, schema) in [
            ("", OBJ),
            ("has space", OBJ),
            ("ok", "not json"),
            ("ok", r#"{"type":"string"}"#),
            ("ok", r#"{"properties":{}}"#),
        ] {
            let result = RenderedTools::render(&[spec(name, schema)], false, None);
            assert!(
                matches!(result, Err(Error::Protocol { .. })),
                "{name} {schema}"
            );
        }
        assert!(RenderedTools::render(&[spec(&"n".repeat(128), OBJ)], false, None).is_ok());
        assert!(RenderedTools::render(&[spec(&"n".repeat(129), OBJ)], false, None).is_err());
    }

    #[test]
    fn thinking_effort_and_tool_choice_settings() {
        let mut request = req(vec![Message::new(Role::User, "hi")]);
        request.tools = vec![spec("a", OBJ)];
        let mut c = no_cache();
        // Defaults: thinking disabled, nothing else.
        let b = body(&c, &request);
        assert_eq!(b["thinking"], json!({"type": "disabled"}));
        assert!(b.get("output_config").is_none());
        assert!(b.get("tool_choice").is_none());

        c.thinking = ThinkingMode::Adaptive;
        c.thinking_display = Some(ThinkingDisplay::Omitted);
        c.effort = Some(Effort::Medium);
        let b = body(&c, &request);
        assert_eq!(
            b["thinking"],
            json!({"type": "adaptive", "display": "omitted"})
        );
        assert_eq!(b["output_config"], json!({"effort": "medium"}));

        c.thinking = ThinkingMode::BetweenTools;
        assert_eq!(
            body(&c, &request)["thinking"],
            json!({"type": "between_tools", "display": "omitted"})
        );
        c.thinking = ThinkingMode::Disabled;
        assert_eq!(
            body(&c, &request)["thinking"],
            json!({"type": "disabled"}),
            "display is not sent with thinking off"
        );
        c.thinking = ThinkingMode::ModelDefault;
        assert!(body(&c, &request).get("thinking").is_none());

        c.disable_parallel_tool_use = true;
        assert_eq!(
            body(&c, &request)["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        c.tool_choice = ToolChoice::Any;
        assert_eq!(
            body(&c, &request)["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        c.tool_choice = ToolChoice::Tool("a".to_owned());
        c.disable_parallel_tool_use = false;
        assert_eq!(
            body(&c, &request)["tool_choice"],
            json!({"type": "tool", "name": "a"})
        );
        c.tool_choice = ToolChoice::Tool("missing".to_owned());
        assert!(body_with(&c, &request).is_err());
        c.tool_choice = ToolChoice::None;
        assert_eq!(body(&c, &request)["tool_choice"], json!({"type": "none"}));

        // Without tools no tool_choice is sent, whatever the setting.
        let plain = req(vec![Message::new(Role::User, "hi")]);
        c.tool_choice = ToolChoice::Any;
        assert!(body(&c, &plain).get("tool_choice").is_none());
        assert!(body(&c, &plain).get("tools").is_none());
    }

    #[test]
    fn the_rendered_tools_are_reused_only_while_nothing_changes() {
        let specs = vec![spec("a", OBJ)];
        let rendered = RenderedTools::render(&specs, false, Some(CacheTtl::FiveMinutes)).unwrap();
        assert!(rendered.matches(&specs, false, Some(CacheTtl::FiveMinutes)));
        assert!(!rendered.matches(&specs, true, Some(CacheTtl::FiveMinutes)));
        assert!(!rendered.matches(&specs, false, None));
        assert!(!rendered.matches(&specs, false, Some(CacheTtl::OneHour)));
        assert!(!rendered.matches(&[spec("b", OBJ)], false, Some(CacheTtl::FiveMinutes)));
    }
}
