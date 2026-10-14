//! The JSON messages of the Gemini Live WebSocket protocol (`docs/providers/gemini-live.md`):
//! the client messages we build and the [`Mapper`] that turns server messages into neutral
//! [`S2sEvent`]s.
//!
//! Gemini has no response or item ids and no explicit "response started" message, so the mapper
//! mints them: a response begins with the first model audio, transcript or tool call after the
//! caller's turn and ends with `turnComplete`, `interrupted` or a tool call (the model then waits
//! for the tool result, and the answer is a new response).

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use brisvox_core::frame::SampleRate;
use bytes::{BufMut, Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{Value, json};

use super::PROVIDER;
use super::config::GeminiLiveConfig;
use crate::error::{Error, Result};
use crate::s2s::{
    S2S_MAX_HISTORY_ITEMS, S2sConfig, S2sEvent, S2sHistoryItem, S2sResponseStatus, S2sTurnDetection,
};

/// The one rate the adapter accepts for the model's audio (`audio/pcm;rate=24000`).
pub(super) const OUTPUT_RATE: SampleRate = SampleRate::Hz24000;

/// Longest caller transcript and assistant transcript the mapper accumulates for one turn.
const MAX_TEXT: usize = 16 * 1024;

/// Tool calls remembered for de-duplication.
const MAX_SEEN_CALLS: usize = 256;

/// Tool calls awaiting an answer that are tracked at once.
const MAX_OUTSTANDING: usize = 64;

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Client messages
// ---------------------------------------------------------------------------------------------

/// `{"realtimeInput":{"activityStart":{}}}`: the caller starts speaking (manual turn detection).
pub(super) const ACTIVITY_START: &str = r#"{"realtimeInput":{"activityStart":{}}}"#;

/// `{"realtimeInput":{"activityEnd":{}}}`: the caller stopped speaking (manual turn detection).
pub(super) const ACTIVITY_END: &str = r#"{"realtimeInput":{"activityEnd":{}}}"#;

/// An empty `clientContent` that completes the turn: asks the model to answer now. UNVERIFIED
/// as a way to request a response after a note (see `docs/providers/gemini-live.md`).
pub(super) const TURN_COMPLETE: &str = r#"{"clientContent":{"turns":[],"turnComplete":true}}"#;

/// The `setup` message that configures a new session.
///
/// # Errors
/// [`Error::Protocol`] for an output rate other than 24 kHz, a tool schema that is not JSON, or a
/// history that is too long.
pub(super) fn setup(cfg: &GeminiLiveConfig, s2s: &S2sConfig) -> Result<String> {
    if s2s.output_rate != OUTPUT_RATE {
        return Err(protocol(format!(
            "the model answers at 24 kHz only (asked for {} out)",
            s2s.output_rate
        )));
    }
    if s2s.history.len() > S2S_MAX_HISTORY_ITEMS {
        return Err(protocol(format!(
            "the history has {} items (limit {S2S_MAX_HISTORY_ITEMS})",
            s2s.history.len()
        )));
    }
    let voice = s2s.voice.as_deref().unwrap_or(&cfg.voice);
    let mut detection = serde_json::Map::new();
    match s2s.turn_detection {
        S2sTurnDetection::Manual => {
            detection.insert("disabled".to_owned(), json!(true));
        }
        S2sTurnDetection::Provider => {
            if let Some(s) = cfg.start_sensitivity {
                detection.insert("startOfSpeechSensitivity".to_owned(), json!(s.start_wire()));
            }
            if let Some(s) = cfg.end_sensitivity {
                detection.insert("endOfSpeechSensitivity".to_owned(), json!(s.end_wire()));
            }
            if let Some(ms) = cfg.prefix_padding_ms {
                detection.insert("prefixPaddingMs".to_owned(), json!(ms));
            }
            if let Some(ms) = cfg.silence_duration_ms {
                detection.insert("silenceDurationMs".to_owned(), json!(ms));
            }
        }
    }
    let mut setup = json!({
        "model": format!("models/{}", cfg.model),
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {
                "voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice}},
            },
        },
        "systemInstruction": {"parts": [{"text": &*s2s.instructions}]},
        "realtimeInputConfig": {
            "automaticActivityDetection": detection,
            "activityHandling": "START_OF_ACTIVITY_INTERRUPTS",
        },
        // The model's own words feed the local mirror (what was played).
        "outputAudioTranscription": {},
    });
    if let Some(object) = setup.as_object_mut() {
        if s2s.input_transcription {
            object.insert("inputAudioTranscription".to_owned(), json!({}));
        }
        if !s2s.history.is_empty() {
            object.insert(
                "historyConfig".to_owned(),
                json!({"initialHistoryInClientContent": true}),
            );
        }
        if !s2s.tools.is_empty() {
            let mut declarations = Vec::with_capacity(s2s.tools.len());
            for tool in &s2s.tools {
                let parameters: Value =
                    serde_json::from_str(&tool.parameters_schema).map_err(|_| {
                        protocol(format!("tool {}: the schema is not valid JSON", tool.name))
                    })?;
                // `parametersJsonSchema` takes the JSON Schema as is; `parameters` only takes
                // Gemini's OpenAPI subset and refuses the whole setup on keywords such as
                // `additionalProperties` (live 2026-10-14: every connect failed).
                declarations.push(json!({
                    "name": &*tool.name,
                    "description": &*tool.description,
                    "parametersJsonSchema": parameters,
                }));
            }
            object.insert(
                "tools".to_owned(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
    }
    Ok(json!({"setup": setup}).to_string())
}

/// The `clientContent` that seeds a new session with `history` (oldest first), or `None` when
/// the history is empty. Consecutive items of the same side share a turn. Tool calls and results
/// are written as text: the structured shape of past calls is not documented, and text cannot be
/// refused by the server.
///
/// # Errors
/// [`Error::Protocol`] when the history is longer than [`S2S_MAX_HISTORY_ITEMS`].
pub(super) fn history_message(history: &[S2sHistoryItem]) -> Result<Option<String>> {
    if history.is_empty() {
        return Ok(None);
    }
    if history.len() > S2S_MAX_HISTORY_ITEMS {
        return Err(protocol(format!(
            "the history has {} items (limit {S2S_MAX_HISTORY_ITEMS})",
            history.len()
        )));
    }
    let mut names: Vec<(&str, &str)> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, text: String| {
        let part = json!({"text": text});
        match turns.last_mut() {
            Some((last, parts)) if *last == role => parts.push(part),
            _ => turns.push((role, vec![part])),
        }
    };
    for item in history {
        match item {
            S2sHistoryItem::User(text) => push("user", text.to_string()),
            S2sHistoryItem::Assistant(text) => push("model", text.to_string()),
            S2sHistoryItem::Note(text) => push("user", format!("[note] {text}")),
            S2sHistoryItem::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                names.push((call_id, name));
                push("model", format!("[tool call] {name}({arguments})"));
            }
            S2sHistoryItem::ToolResult { call_id, output } => {
                let name = names
                    .iter()
                    .find(|(id, _)| *id == &**call_id)
                    .map_or("tool", |(_, name)| *name);
                push("user", format!("[tool result] {name}: {output}"));
            }
        }
    }
    let turns: Vec<Value> = turns
        .into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect();
    Ok(Some(
        json!({"clientContent": {"turns": turns, "turnComplete": false}}).to_string(),
    ))
}

/// A system note as a `clientContent` user turn that does not start a response. UNVERIFIED
/// mid-session (see the provider doc).
pub(super) fn note_message(text: &str) -> String {
    json!({"clientContent": {
        "turns": [{"role": "user", "parts": [{"text": format!("[note] {text}")}]}],
        "turnComplete": false,
    }})
    .to_string()
}

/// One answer to a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolAnswer {
    /// The id the server gave the call, if any.
    pub(super) wire_id: Option<String>,
    /// The tool name.
    pub(super) name: String,
    /// The result text.
    pub(super) output: String,
}

/// The `toolResponse` for `answers`. The result goes in as `{"result": <JSON or string>}`.
pub(super) fn tool_response(answers: &[ToolAnswer]) -> String {
    let responses: Vec<Value> = answers
        .iter()
        .map(|answer| {
            let result = serde_json::from_str::<Value>(&answer.output)
                .unwrap_or_else(|_| Value::String(answer.output.clone()));
            let mut response = json!({"name": answer.name, "response": {"result": result}});
            if let (Some(id), Some(object)) = (&answer.wire_id, response.as_object_mut()) {
                object.insert("id".to_owned(), json!(id));
            }
            response
        })
        .collect();
    json!({"toolResponse": {"functionResponses": responses}}).to_string()
}

/// The fixed start of every audio message for audio at `rate`.
pub(super) fn audio_head(rate: SampleRate) -> Bytes {
    Bytes::from(format!(
        r#"{{"realtimeInput":{{"audio":{{"mimeType":"audio/pcm;rate={}","data":""#,
        rate.hz()
    ))
}

/// A `realtimeInput.audio` message for `pcm`, built into the reused buffer `out` and handed out
/// as a frozen slice of it (the buffer's allocation is reclaimed once the socket dropped the
/// slice, so the steady state does not allocate per frame).
///
/// # Errors
/// [`Error::Protocol`] when the buffer could not be written (cannot happen for a `BytesMut`).
pub(super) fn append_audio(head: &[u8], pcm: &[u8], out: &mut BytesMut) -> Result<Bytes> {
    const TAIL: &[u8] = br#""}}}"#;
    let encoded = base64::encoded_len(pcm.len(), true).unwrap_or(0);
    out.reserve(head.len() + encoded + TAIL.len());
    let written = {
        let mut writer = out.writer();
        writer
            .write_all(head)
            .and_then(|()| write!(writer, "{}", Base64Display::new(pcm, &STANDARD)))
            .and_then(|()| writer.write_all(TAIL))
    };
    written.map_err(|_| protocol("an audio frame could not be encoded"))?;
    Ok(out.split().freeze())
}

// ---------------------------------------------------------------------------------------------
// Server messages
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Incoming<'a> {
    #[serde(default)]
    setup_complete: Option<Value>,
    #[serde(borrow, default)]
    server_content: Option<ServerContent<'a>>,
    #[serde(borrow, default)]
    tool_call: Option<ToolCallMessage<'a>>,
    #[serde(borrow, default)]
    tool_call_cancellation: Option<Cancellation<'a>>,
    #[serde(default)]
    go_away: Option<GoAway>,
    #[serde(borrow, default)]
    voice_activity: Option<VoiceActivity<'a>>,
    #[serde(borrow, default)]
    error: Option<ErrorBody<'a>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerContent<'a> {
    #[serde(borrow, default)]
    model_turn: Option<ModelTurn<'a>>,
    #[serde(default)]
    generation_complete: Option<bool>,
    #[serde(default)]
    turn_complete: Option<bool>,
    #[serde(default)]
    interrupted: Option<bool>,
    #[serde(borrow, default)]
    input_transcription: Option<Transcription<'a>>,
    #[serde(borrow, default)]
    output_transcription: Option<Transcription<'a>>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelTurn<'a> {
    #[serde(borrow, default)]
    parts: Vec<Part<'a>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part<'a> {
    #[serde(borrow, default)]
    inline_data: Option<Blob<'a>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Blob<'a> {
    #[serde(borrow, default)]
    mime_type: Cow<'a, str>,
    #[serde(borrow, default)]
    data: Cow<'a, str>,
}

#[derive(Debug, Default, Deserialize)]
struct Transcription<'a> {
    #[serde(borrow, default)]
    text: Cow<'a, str>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallMessage<'a> {
    #[serde(borrow, default, rename = "functionCalls")]
    function_calls: Vec<FunctionCall<'a>>,
}

#[derive(Debug, Default, Deserialize)]
struct FunctionCall<'a> {
    #[serde(borrow, default)]
    id: Cow<'a, str>,
    #[serde(borrow, default)]
    name: Cow<'a, str>,
    #[serde(default)]
    args: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct Cancellation<'a> {
    #[serde(borrow, default)]
    ids: Vec<Cow<'a, str>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoAway {
    #[serde(default)]
    time_left: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct VoiceActivity<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Cow<'a, str>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody<'a> {
    #[serde(default)]
    code: Option<Value>,
    #[serde(borrow, default)]
    status: Cow<'a, str>,
    #[serde(borrow, default)]
    message: Cow<'a, str>,
}

/// A protobuf JSON `Duration` (`"3.5s"`), or a plain number of seconds.
fn parse_duration(value: Option<&Value>) -> Duration {
    let seconds = match value {
        Some(Value::String(text)) => text.trim_end_matches('s').parse::<f64>().ok(),
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    seconds
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .unwrap_or(Duration::ZERO)
}

/// What one server message was.
#[derive(Debug)]
pub(super) enum Decoded {
    /// `setupComplete`.
    SetupComplete,
    /// Zero or more events for the engine.
    Events(Vec<S2sEvent>),
    /// An error body.
    Error {
        /// The code or status.
        code: Arc<str>,
        /// The text.
        message: Arc<str>,
    },
}

/// A call the model made and nobody answered yet.
#[derive(Debug, Clone)]
struct Outstanding {
    /// The id the engine sees.
    id: Arc<str>,
    /// The id the server gave, if any.
    wire_id: Option<String>,
    name: String,
}

/// The response being generated.
#[derive(Debug)]
struct Open {
    response: Arc<str>,
    item: Arc<str>,
    audio: bool,
    audio_done: bool,
    transcript: String,
    transcript_sent: bool,
}

/// Turns Gemini Live server messages into neutral events; owns everything that has to be
/// remembered between messages (the response in progress, who is speaking, open tool calls).
#[derive(Debug, Default)]
pub(super) struct Mapper {
    next_response: u64,
    next_call: u64,
    open: Option<Open>,
    /// The last response ended and no audio or tool call has started a new one yet: transcript
    /// chunks that arrive now are late pieces of the finished turn (Gemini sends transcripts as
    /// independent messages), and must not open a response of their own.
    settled: bool,
    /// The caller is speaking (the provider's activity detection, or an interruption).
    speaking: bool,
    /// The engine cancelled the response: the rest of it is dropped until the turn ends.
    discard: bool,
    /// Caller transcript chunks of the turn so far.
    user_text: String,
    seen_calls: VecDeque<Arc<str>>,
    outstanding: Vec<Outstanding>,
}

fn push_capped(target: &mut String, text: &str) {
    let room = MAX_TEXT.saturating_sub(target.len());
    if room == 0 {
        return;
    }
    if text.len() <= room {
        target.push_str(text);
        return;
    }
    let mut end = room;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(text.get(..end).unwrap_or_default());
}

impl Mapper {
    /// A response of the model is in progress.
    #[cfg(test)]
    pub(super) fn response_open(&self) -> bool {
        self.open.is_some()
    }

    /// The engine cancelled the running response (Gemini has no cancel message: the caller's next
    /// `activityStart` or speech interrupts the server; locally the rest of the response is
    /// dropped). Returns the events that end it.
    pub(super) fn cancel_local(&mut self) -> Vec<S2sEvent> {
        let mut out = Vec::new();
        if self.open.is_some() {
            self.close(S2sResponseStatus::Cancelled, &mut out);
            self.discard = true;
        }
        out
    }

    /// Takes the answer slot of the call the engine knows as `id`; `None` when the call is not
    /// open (cancelled, interrupted, never made).
    pub(super) fn answer(&mut self, id: &str, output: &str) -> Option<ToolAnswer> {
        let at = self.outstanding.iter().position(|c| &*c.id == id)?;
        let call = self.outstanding.remove(at);
        Some(ToolAnswer {
            wire_id: call.wire_id,
            name: call.name,
            output: output.to_owned(),
        })
    }

    /// Calls still waiting for an answer.
    pub(super) fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    /// Decodes one server message.
    ///
    /// # Errors
    /// [`Error::Protocol`] for JSON that is not an object of the protocol, or audio at a rate
    /// other than 24 kHz.
    pub(super) fn decode(&mut self, bytes: &[u8]) -> Result<Decoded> {
        let message: Incoming<'_> = serde_json::from_slice(bytes)
            .map_err(|_| protocol("a server message is not valid JSON"))?;
        if message.setup_complete.is_some() {
            return Ok(Decoded::SetupComplete);
        }
        if let Some(error) = message.error {
            let code = match error.code {
                Some(Value::Number(n)) => n.to_string(),
                _ => error.status.to_string(),
            };
            return Ok(Decoded::Error {
                code: Arc::from(code),
                message: Arc::from(&*error.message),
            });
        }
        let mut out = Vec::new();
        if let Some(activity) = message.voice_activity {
            match &*activity.kind {
                "ACTIVITY_START" if !self.speaking => {
                    self.speaking = true;
                    out.push(S2sEvent::SpeechStarted);
                }
                "ACTIVITY_END" if self.speaking => {
                    self.speaking = false;
                    out.push(S2sEvent::SpeechStopped);
                }
                _ => {}
            }
        }
        if let Some(go_away) = message.go_away {
            out.push(S2sEvent::SessionEnding {
                time_left: parse_duration(go_away.time_left.as_ref()),
            });
        }
        if let Some(cancel) = message.tool_call_cancellation {
            self.cancel_calls(&cancel, &mut out);
        }
        if let Some(content) = message.server_content {
            self.server_content(content, &mut out)?;
        }
        if let Some(calls) = message.tool_call {
            self.tool_calls(calls, &mut out);
        }
        Ok(Decoded::Events(out))
    }

    fn cancel_calls(&mut self, cancel: &Cancellation<'_>, out: &mut Vec<S2sEvent>) {
        self.outstanding
            .retain(|call| !cancel.ids.iter().any(|id| **id == *call.id));
        out.push(S2sEvent::Warning {
            code: Arc::from("tool_call_cancelled"),
            message: Arc::from(format!(
                "the server cancelled {} tool call(s)",
                cancel.ids.len()
            )),
        });
    }

    /// Makes sure a response is open (announcing it, and ending the caller's speech first);
    /// `false` when the rest of the response is being discarded.
    fn ensure_open(&mut self, out: &mut Vec<S2sEvent>) -> bool {
        if self.discard {
            return false;
        }
        if self.open.is_none() {
            if self.speaking {
                self.speaking = false;
                out.push(S2sEvent::SpeechStopped);
            }
            self.flush_user(out);
            self.next_response = self.next_response.saturating_add(1);
            let response: Arc<str> = Arc::from(format!("gr{}", self.next_response));
            out.push(S2sEvent::ResponseStarted {
                response: Arc::clone(&response),
            });
            self.settled = false;
            self.open = Some(Open {
                item: Arc::from(format!("{response}-audio")),
                response,
                audio: false,
                audio_done: false,
                transcript: String::new(),
                transcript_sent: false,
            });
        }
        true
    }

    fn flush_user(&mut self, out: &mut Vec<S2sEvent>) {
        let text = self.user_text.trim();
        if !text.is_empty() {
            out.push(S2sEvent::UserTranscript {
                text: Arc::from(text),
            });
        }
        self.user_text.clear();
    }

    /// Ends the audio of the open response (AudioDone, then the whole transcript).
    fn finish_audio(&mut self, out: &mut Vec<S2sEvent>) {
        let Some(open) = &mut self.open else { return };
        if open.audio && !open.audio_done {
            open.audio_done = true;
            out.push(S2sEvent::AudioDone {
                response: Arc::clone(&open.response),
                item: Arc::clone(&open.item),
            });
        }
        if !open.transcript.is_empty() && !open.transcript_sent {
            open.transcript_sent = true;
            out.push(S2sEvent::AssistantTranscript {
                response: Arc::clone(&open.response),
                item: Arc::clone(&open.item),
                text: Arc::from(open.transcript.as_str()),
                is_final: true,
            });
        }
    }

    fn close(&mut self, status: S2sResponseStatus, out: &mut Vec<S2sEvent>) {
        self.finish_audio(out);
        self.settled = true;
        if let Some(open) = self.open.take() {
            out.push(S2sEvent::ResponseDone {
                response: open.response,
                status,
            });
        }
    }

    fn server_content(
        &mut self,
        content: ServerContent<'_>,
        out: &mut Vec<S2sEvent>,
    ) -> Result<()> {
        if let Some(t) = &content.input_transcription {
            push_capped(&mut self.user_text, &t.text);
        }
        if content.interrupted == Some(true) {
            if !self.speaking {
                // No voice-activity message told us: the interruption is the signal.
                self.speaking = true;
                out.push(S2sEvent::SpeechStarted);
            }
            self.close(S2sResponseStatus::Cancelled, out);
            self.discard = false;
            // The server discards the pending calls of an interrupted turn.
            self.outstanding.clear();
        }
        if let Some(turn) = &content.model_turn {
            for part in &turn.parts {
                let Some(blob) = &part.inline_data else {
                    continue;
                };
                if !blob.mime_type.is_empty() && !blob.mime_type.starts_with("audio/") {
                    continue;
                }
                if let Some(rate) = blob.mime_type.split("rate=").nth(1)
                    && rate.trim() != "24000"
                {
                    return Err(protocol(format!(
                        "audio at an unsupported rate ({})",
                        rate.trim()
                    )));
                }
                if !self.ensure_open(out) {
                    continue;
                }
                let pcm = STANDARD
                    .decode(blob.data.as_bytes())
                    .map_err(|_| protocol("model audio is not valid base64"))?;
                if let Some(open) = &mut self.open {
                    open.audio = true;
                    out.push(S2sEvent::Audio {
                        response: Arc::clone(&open.response),
                        item: Arc::clone(&open.item),
                        pcm: Bytes::from(pcm),
                    });
                }
            }
        }
        if let Some(t) = &content.output_transcription
            && !t.text.is_empty()
            && (self.open.is_some() || !self.settled)
            && self.ensure_open(out)
            && let Some(open) = &mut self.open
        {
            push_capped(&mut open.transcript, &t.text);
            out.push(S2sEvent::AssistantTranscript {
                response: Arc::clone(&open.response),
                item: Arc::clone(&open.item),
                text: Arc::from(&*t.text),
                is_final: false,
            });
        }
        if content.generation_complete == Some(true) {
            self.finish_audio(out);
        }
        if content.turn_complete == Some(true) {
            if self.discard {
                self.discard = false;
            } else {
                self.close(S2sResponseStatus::Completed, out);
            }
            self.flush_user(out);
        }
        Ok(())
    }

    fn tool_calls(&mut self, message: ToolCallMessage<'_>, out: &mut Vec<S2sEvent>) {
        // Calls already delivered (a repeat) are skipped before anything is announced.
        let mut fresh = Vec::new();
        for call in message.function_calls {
            let (id, wire_id) = if call.id.is_empty() {
                self.next_call = self.next_call.saturating_add(1);
                (Arc::<str>::from(format!("gc{}", self.next_call)), None)
            } else {
                (Arc::<str>::from(&*call.id), Some(call.id.to_string()))
            };
            if self.seen_calls.iter().any(|seen| *seen == id) {
                continue;
            }
            if self.seen_calls.len() >= MAX_SEEN_CALLS {
                self.seen_calls.pop_front();
            }
            self.seen_calls.push_back(Arc::clone(&id));
            fresh.push((id, wire_id, call));
        }
        if fresh.is_empty() || !self.ensure_open(out) {
            return;
        }
        let Some(response) = self.open.as_ref().map(|o| Arc::clone(&o.response)) else {
            return;
        };
        for (id, wire_id, call) in fresh {
            if self.outstanding.len() >= MAX_OUTSTANDING {
                self.outstanding.remove(0);
            }
            self.outstanding.push(Outstanding {
                id: Arc::clone(&id),
                wire_id,
                name: call.name.to_string(),
            });
            let arguments = call
                .args
                .filter(Value::is_object)
                .map_or_else(|| "{}".to_owned(), |v| v.to_string());
            out.push(S2sEvent::ToolCall {
                response: Arc::clone(&response),
                call_id: id,
                name: Arc::from(&*call.name),
                arguments: Arc::from(arguments),
            });
        }
        // The model waits for the answers: this response is over, the answer starts the next.
        self.close(S2sResponseStatus::Completed, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn events(mapper: &mut Mapper, message: &str) -> Vec<S2sEvent> {
        match mapper.decode(message.as_bytes()).unwrap() {
            Decoded::Events(events) => events,
            other => panic!("not events: {other:?}"),
        }
    }

    fn audio_message(pcm: &[u8]) -> String {
        json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {
            "mimeType": "audio/pcm;rate=24000",
            "data": STANDARD.encode(pcm),
        }}]}}})
        .to_string()
    }

    fn kinds(events: &[S2sEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                S2sEvent::SpeechStarted => "speech_started",
                S2sEvent::SpeechStopped => "speech_stopped",
                S2sEvent::ResponseStarted { .. } => "response_started",
                S2sEvent::Audio { .. } => "audio",
                S2sEvent::AudioDone { .. } => "audio_done",
                S2sEvent::AssistantTranscript { .. } => "assistant_transcript",
                S2sEvent::UserTranscript { .. } => "user_transcript",
                S2sEvent::ToolCall { .. } => "tool_call",
                S2sEvent::ResponseDone { .. } => "response_done",
                S2sEvent::Warning { .. } => "warning",
                S2sEvent::SessionEnding { .. } => "session_ending",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn setup_carries_instructions_voice_tools_and_detection() {
        let cfg = GeminiLiveConfig::new(brisvox_core::Secret::new("k".to_owned()));
        let mut s2s = S2sConfig::new("Be brief.");
        s2s.tools.push(ToolSpec {
            name: Arc::from("lookup"),
            description: Arc::from("find"),
            parameters_schema: Arc::from(r#"{"type":"object"}"#),
        });
        s2s.input_transcription = true;
        let value: Value = serde_json::from_str(&setup(&cfg, &s2s).unwrap()).unwrap();
        let setup = &value["setup"];
        assert_eq!(setup["model"], "models/gemini-3.8-live");
        assert_eq!(setup["generationConfig"]["responseModalities"][0], "AUDIO");
        assert_eq!(
            setup["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Kore"
        );
        assert_eq!(setup["systemInstruction"]["parts"][0]["text"], "Be brief.");
        assert_eq!(
            setup["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert_eq!(setup["inputAudioTranscription"], json!({}));
        assert!(setup.get("historyConfig").is_none());
        assert_eq!(
            setup["realtimeInputConfig"]["activityHandling"],
            "START_OF_ACTIVITY_INTERRUPTS"
        );
        s2s.turn_detection = S2sTurnDetection::Manual;
        let value: Value = serde_json::from_str(&super::setup(&cfg, &s2s).unwrap()).unwrap();
        assert_eq!(
            value["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
            true
        );
        s2s.output_rate = SampleRate::Hz16000;
        assert!(super::setup(&cfg, &s2s).is_err());
    }

    #[test]
    fn history_groups_turns_and_renders_tools_as_text() {
        let history = vec![
            S2sHistoryItem::User(Arc::from("hi")),
            S2sHistoryItem::Assistant(Arc::from("hello")),
            S2sHistoryItem::User(Arc::from("weather?")),
            S2sHistoryItem::ToolCall {
                call_id: Arc::from("c1"),
                name: Arc::from("weather"),
                arguments: Arc::from(r#"{"city":"Ankara"}"#),
            },
            S2sHistoryItem::ToolResult {
                call_id: Arc::from("c1"),
                output: Arc::from("sunny"),
            },
        ];
        let text = history_message(&history).unwrap().unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let turns = value["clientContent"]["turns"].as_array().unwrap();
        let roles: Vec<&str> = turns.iter().map(|t| t["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "model", "user", "model", "user"]);
        assert_eq!(value["clientContent"]["turnComplete"], false);
        assert!(
            turns[3]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("weather(")
        );
        assert!(
            turns[4]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("weather: sunny")
        );
        assert!(history_message(&[]).unwrap().is_none());
        let long = vec![S2sHistoryItem::User(Arc::from("x")); S2S_MAX_HISTORY_ITEMS + 1];
        assert!(history_message(&long).is_err());
    }

    #[test]
    fn audio_messages_are_base64_with_the_rate() {
        let head = audio_head(SampleRate::Hz24000);
        let mut scratch = BytesMut::new();
        let bytes = append_audio(&head, &[1, 2, 3, 4], &mut scratch).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let audio = &value["realtimeInput"]["audio"];
        assert_eq!(audio["mimeType"], "audio/pcm;rate=24000");
        assert_eq!(
            STANDARD.decode(audio["data"].as_str().unwrap()).unwrap(),
            [1, 2, 3, 4]
        );
    }

    #[test]
    fn a_turn_maps_to_one_response() {
        let mut m = Mapper::default();
        let out = events(
            &mut m,
            r#"{"serverContent":{"inputTranscription":{"text":"merhaba "}}}"#,
        );
        assert!(out.is_empty());
        let out = events(&mut m, r#"{"voiceActivity":{"type":"ACTIVITY_START"}}"#);
        assert_eq!(kinds(&out), ["speech_started"]);
        let out = events(&mut m, r#"{"voiceActivity":{"type":"ACTIVITY_END"}}"#);
        assert_eq!(kinds(&out), ["speech_stopped"]);
        let out = events(&mut m, &audio_message(&[0, 1]));
        assert_eq!(
            kinds(&out),
            ["user_transcript", "response_started", "audio"]
        );
        let out = events(
            &mut m,
            r#"{"serverContent":{"outputTranscription":{"text":"selam"}}}"#,
        );
        assert_eq!(kinds(&out), ["assistant_transcript"]);
        let out = events(&mut m, r#"{"serverContent":{"generationComplete":true}}"#);
        assert_eq!(kinds(&out), ["audio_done", "assistant_transcript"]);
        let out = events(&mut m, r#"{"serverContent":{"turnComplete":true}}"#);
        assert_eq!(kinds(&out), ["response_done"]);
        assert!(!m.response_open());
        // The next turn gets a new response id.
        let out = events(&mut m, &audio_message(&[0, 1]));
        match &out[0] {
            S2sEvent::ResponseStarted { response } => assert_eq!(&**response, "gr2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_late_transcript_chunk_does_not_open_a_phantom_response() {
        let mut m = Mapper::default();
        events(&mut m, &audio_message(&[0, 1]));
        let out = events(&mut m, r#"{"serverContent":{"turnComplete":true}}"#);
        assert_eq!(kinds(&out), ["audio_done", "response_done"]);
        // Transcripts are independent messages: one after turnComplete is dropped.
        let out = events(
            &mut m,
            r#"{"serverContent":{"outputTranscription":{"text":"geç"}}}"#,
        );
        assert!(out.is_empty(), "{out:?}");
        assert!(!m.response_open());
        // The next turn still opens normally, with the next id.
        let out = events(&mut m, &audio_message(&[0, 1]));
        assert_eq!(kinds(&out), ["response_started", "audio"]);
        assert!(m.response_open());
        // A transcript of a turn that has not started yet (nothing settled) opens it.
        let mut fresh = Mapper::default();
        let out = events(
            &mut fresh,
            r#"{"serverContent":{"outputTranscription":{"text":"hi"}}}"#,
        );
        assert_eq!(kinds(&out), ["response_started", "assistant_transcript"]);
    }

    #[test]
    fn an_interruption_cancels_the_response_and_signals_speech() {
        let mut m = Mapper::default();
        events(&mut m, &audio_message(&[0, 1]));
        let out = events(&mut m, r#"{"serverContent":{"interrupted":true}}"#);
        assert_eq!(
            kinds(&out),
            ["speech_started", "audio_done", "response_done"]
        );
        assert!(matches!(
            out.last(),
            Some(S2sEvent::ResponseDone {
                status: S2sResponseStatus::Cancelled,
                ..
            })
        ));
        // The speech ends when the next response begins.
        let out = events(&mut m, &audio_message(&[0, 1]));
        assert_eq!(kinds(&out), ["speech_stopped", "response_started", "audio"]);
    }

    #[test]
    fn a_local_cancel_drops_the_rest_of_the_response() {
        let mut m = Mapper::default();
        events(&mut m, &audio_message(&[0, 1]));
        let out = m.cancel_local();
        assert_eq!(kinds(&out), ["audio_done", "response_done"]);
        // Late audio of the cancelled response is dropped; the turn end re-arms the mapper.
        assert!(events(&mut m, &audio_message(&[0, 1])).is_empty());
        assert!(events(&mut m, r#"{"serverContent":{"turnComplete":true}}"#).is_empty());
        assert_eq!(
            kinds(&events(&mut m, &audio_message(&[0, 1]))),
            ["response_started", "audio"]
        );
        // Nothing to cancel when no response runs.
        let mut idle = Mapper::default();
        assert!(idle.cancel_local().is_empty());
    }

    #[test]
    fn a_tool_call_is_delivered_once_and_ends_the_response() {
        let mut m = Mapper::default();
        let message =
            r#"{"toolCall":{"functionCalls":[{"id":"fc1","name":"lookup","args":{"q":"x"}}]}}"#;
        let out = events(&mut m, message);
        assert_eq!(
            kinds(&out),
            ["response_started", "tool_call", "response_done"]
        );
        match &out[1] {
            S2sEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(
                    (&**call_id, &**name, &**arguments),
                    ("fc1", "lookup", r#"{"q":"x"}"#)
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(m.outstanding(), 1);
        // A repeat of the same call and a turnComplete after it change nothing.
        assert!(events(&mut m, message).is_empty());
        assert!(events(&mut m, r#"{"serverContent":{"turnComplete":true}}"#).is_empty());
        let answer = m.answer("fc1", r#"{"ok":true}"#).unwrap();
        assert_eq!(answer.wire_id.as_deref(), Some("fc1"));
        assert!(m.answer("fc1", "again").is_none());
        let sent: Value = serde_json::from_str(&tool_response(&[answer])).unwrap();
        let response = &sent["toolResponse"]["functionResponses"][0];
        assert_eq!(response["id"], "fc1");
        assert_eq!(response["name"], "lookup");
        assert_eq!(response["response"]["result"]["ok"], true);
    }

    #[test]
    fn a_call_without_an_id_gets_a_minted_one_and_an_interruption_drops_pending_calls() {
        let mut m = Mapper::default();
        let out = events(
            &mut m,
            r#"{"toolCall":{"functionCalls":[{"name":"lookup"}]}}"#,
        );
        match &out[1] {
            S2sEvent::ToolCall {
                call_id, arguments, ..
            } => {
                assert_eq!((&**call_id, &**arguments), ("gc1", "{}"));
            }
            other => panic!("{other:?}"),
        }
        let answer = m.answer("gc1", "plain text").unwrap();
        assert!(answer.wire_id.is_none());
        let sent: Value = serde_json::from_str(&tool_response(&[answer])).unwrap();
        assert!(
            sent["toolResponse"]["functionResponses"][0]
                .get("id")
                .is_none()
        );
        assert_eq!(
            sent["toolResponse"]["functionResponses"][0]["response"]["result"],
            "plain text"
        );
        events(
            &mut m,
            r#"{"toolCall":{"functionCalls":[{"id":"a","name":"x"}]}}"#,
        );
        events(&mut m, r#"{"serverContent":{"interrupted":true}}"#);
        assert_eq!(m.outstanding(), 0);
        events(
            &mut m,
            r#"{"toolCall":{"functionCalls":[{"id":"b","name":"x"}]}}"#,
        );
        let out = events(&mut m, r#"{"toolCallCancellation":{"ids":["b"]}}"#);
        assert_eq!(kinds(&out), ["warning"]);
        assert_eq!(m.outstanding(), 0);
    }

    #[test]
    fn go_away_and_errors_and_bad_input() {
        let mut m = Mapper::default();
        let out = events(&mut m, r#"{"goAway":{"timeLeft":"50.5s"}}"#);
        match &out[0] {
            S2sEvent::SessionEnding { time_left } => {
                assert_eq!(*time_left, Duration::from_millis(50_500));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            m.decode(br#"{"setupComplete":{}}"#),
            Ok(Decoded::SetupComplete)
        ));
        assert!(matches!(
            m.decode(br#"{"error":{"code":429,"message":"slow down"}}"#),
            Ok(Decoded::Error { .. })
        ));
        assert!(m.decode(b"not json").is_err());
        let wrong_rate = json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {
            "mimeType": "audio/pcm;rate=16000", "data": "AAA=",
        }}]}}});
        assert!(m.decode(wrong_rate.to_string().as_bytes()).is_err());
        let bad_data = json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {
            "mimeType": "audio/pcm;rate=24000", "data": "***",
        }}]}}});
        assert!(m.decode(bad_data.to_string().as_bytes()).is_err());
    }
}
