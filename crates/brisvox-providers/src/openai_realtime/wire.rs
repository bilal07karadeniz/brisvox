//! The JSON messages of the Realtime WebSocket protocol (`docs/providers/openai-realtime.md`):
//! the client events we build and the server events we decode.

use std::borrow::Cow;
use std::io::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use brisvox_core::frame::SampleRate;
use bytes::{BufMut, Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{Value, json};

use super::PROVIDER;
use super::config::{OpenAiRealtimeConfig, ServerVad};
use crate::error::{Error, Result};
use crate::s2s::{
    S2S_MAX_HISTORY_ITEMS, S2sConfig, S2sControl, S2sEvent, S2sHistoryItem, S2sResponseStatus,
    S2sTurnDetection,
};

/// The one audio rate the adapter asks the server for (`audio/pcm` at 24 kHz).
pub(super) const PCM_RATE: SampleRate = SampleRate::Hz24000;

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Client events
// ---------------------------------------------------------------------------------------------

/// The `session.update` that configures a new session.
pub(super) fn session_update(cfg: &OpenAiRealtimeConfig, s2s: &S2sConfig) -> Result<String> {
    if s2s.input_rate != PCM_RATE || s2s.output_rate != PCM_RATE {
        return Err(protocol(format!(
            "only 24 kHz PCM is supported (asked for {} in, {} out)",
            s2s.input_rate, s2s.output_rate
        )));
    }
    let mut tools = Vec::with_capacity(s2s.tools.len());
    for tool in &s2s.tools {
        let parameters: Value = serde_json::from_str(&tool.parameters_schema)
            .map_err(|_| protocol(format!("tool {}: the schema is not valid JSON", tool.name)))?;
        tools.push(json!({
            "type": "function",
            "name": &*tool.name,
            "description": &*tool.description,
            "parameters": parameters,
        }));
    }
    let turn_detection = match s2s.turn_detection {
        S2sTurnDetection::Manual => Value::Null,
        S2sTurnDetection::Provider => match cfg.vad {
            ServerVad::Silence {
                threshold,
                prefix_padding_ms,
                silence_duration_ms,
            } => {
                let mut vad = json!({
                    "type": "server_vad",
                    "create_response": true,
                    "interrupt_response": true,
                });
                if let Some(object) = vad.as_object_mut() {
                    if let Some(t) = threshold {
                        object.insert("threshold".to_owned(), json!(t));
                    }
                    if let Some(ms) = prefix_padding_ms {
                        object.insert("prefix_padding_ms".to_owned(), json!(ms));
                    }
                    if let Some(ms) = silence_duration_ms {
                        object.insert("silence_duration_ms".to_owned(), json!(ms));
                    }
                }
                vad
            }
            ServerVad::Semantic { eagerness } => {
                let mut vad = json!({
                    "type": "semantic_vad",
                    "create_response": true,
                    "interrupt_response": true,
                });
                if let (Some(object), Some(e)) = (vad.as_object_mut(), eagerness) {
                    object.insert("eagerness".to_owned(), json!(e.wire()));
                }
                vad
            }
        },
    };
    let format = json!({"type": "audio/pcm", "rate": PCM_RATE.hz()});
    let mut input = json!({"format": format.clone(), "turn_detection": turn_detection});
    if s2s.input_transcription
        && let (Some(model), Some(object)) = (&cfg.transcription_model, input.as_object_mut())
    {
        let mut transcription = json!({"model": model});
        if let (Some(language), Some(t)) = (&s2s.language, transcription.as_object_mut()) {
            t.insert("language".to_owned(), json!(&**language));
        }
        object.insert("transcription".to_owned(), transcription);
    }
    let voice = s2s.voice.as_deref().unwrap_or(&cfg.voice);
    let event = json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "instructions": &*s2s.instructions,
            "output_modalities": ["audio"],
            "tools": tools,
            "tool_choice": "auto",
            "audio": {
                "input": input,
                "output": {"format": format, "voice": voice},
            },
        },
    });
    Ok(event.to_string())
}

/// The `conversation.item.create` events that seed a new session with `history` (oldest first).
///
/// # Errors
/// [`Error::Protocol`] when the history is longer than [`S2S_MAX_HISTORY_ITEMS`].
pub(super) fn history_events(
    cfg: &OpenAiRealtimeConfig,
    history: &[S2sHistoryItem],
) -> Result<Vec<String>> {
    if history.len() > S2S_MAX_HISTORY_ITEMS {
        return Err(protocol(format!(
            "the history has {} items (limit {S2S_MAX_HISTORY_ITEMS})",
            history.len()
        )));
    }
    let message = |role: &str, kind: &str, text: &str| {
        json!({
            "type": "conversation.item.create",
            "item": {
                "type": "message",
                "role": role,
                "content": [{"type": kind, "text": text}],
            },
        })
        .to_string()
    };
    let mut out = Vec::with_capacity(history.len());
    for item in history {
        out.push(match item {
            S2sHistoryItem::User(text) => message("user", "input_text", text),
            S2sHistoryItem::Assistant(text) if cfg.seed_assistant_as_system => message(
                "system",
                "input_text",
                &format!("The assistant said earlier: {text}"),
            ),
            S2sHistoryItem::Assistant(text) => message("assistant", "output_text", text),
            S2sHistoryItem::Note(text) => message("system", "input_text", text),
            S2sHistoryItem::ToolCall {
                call_id,
                name,
                arguments,
            } => json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call",
                    "call_id": &**call_id,
                    "name": &**name,
                    "arguments": &**arguments,
                },
            })
            .to_string(),
            S2sHistoryItem::ToolResult { call_id, output } => json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": &**call_id,
                    "output": &**output,
                },
            })
            .to_string(),
        });
    }
    Ok(out)
}

/// What a client event is, for the error classifier: errors answering these are expected races.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sent {
    /// `response.cancel`, `conversation.item.truncate`: the response or item may be gone already.
    Racy,
    /// Any other event.
    Plain,
}

/// A control command as a client event, with its `event_id`.
pub(super) fn control_event(control: &S2sControl, event_id: &str) -> (String, Sent) {
    let (value, sent) = match control {
        S2sControl::Commit => (json!({"type": "input_audio_buffer.commit"}), Sent::Plain),
        S2sControl::ClearInput => (json!({"type": "input_audio_buffer.clear"}), Sent::Plain),
        S2sControl::CreateResponse => (json!({"type": "response.create"}), Sent::Plain),
        S2sControl::CancelResponse => (json!({"type": "response.cancel"}), Sent::Racy),
        S2sControl::Truncate { item, audio_end } => (
            json!({
                "type": "conversation.item.truncate",
                "item_id": &**item,
                "content_index": 0,
                "audio_end_ms": u64::try_from(audio_end.as_millis()).unwrap_or(u64::MAX),
            }),
            Sent::Racy,
        ),
        S2sControl::ToolResult { call_id, output } => (
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": &**call_id,
                    "output": &**output,
                },
            }),
            Sent::Plain,
        ),
        S2sControl::Note { text } => (
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": &**text}],
                },
            }),
            Sent::Plain,
        ),
    };
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert("event_id".to_owned(), json!(event_id));
    }
    (value.to_string(), sent)
}

/// An `input_audio_buffer.append` for `pcm`, built into the reused buffer `out` and handed out
/// as a frozen slice of it. The slice goes to the socket without a copy and the buffer's
/// allocation is reclaimed once the socket dropped the slice, so the steady state does not
/// allocate per frame.
///
/// # Errors
/// [`Error::Protocol`] when the buffer could not be written (cannot happen for a `BytesMut`).
pub(super) fn append_audio(pcm: &[u8], out: &mut BytesMut) -> Result<Bytes> {
    const HEAD: &[u8] = br#"{"type":"input_audio_buffer.append","audio":""#;
    const TAIL: &[u8] = br#""}"#;
    let encoded = base64::encoded_len(pcm.len(), true).unwrap_or(0);
    out.reserve(HEAD.len() + encoded + TAIL.len());
    let written = {
        let mut writer = out.writer();
        writer
            .write_all(HEAD)
            .and_then(|()| write!(writer, "{}", Base64Display::new(pcm, &STANDARD)))
            .and_then(|()| writer.write_all(TAIL))
    };
    written.map_err(|_| protocol("an audio frame could not be encoded"))?;
    Ok(out.split().freeze())
}

// ---------------------------------------------------------------------------------------------
// Server events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct Incoming<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    response_id: Cow<'a, str>,
    #[serde(borrow, default)]
    item_id: Cow<'a, str>,
    #[serde(borrow, default)]
    call_id: Cow<'a, str>,
    #[serde(borrow, default)]
    name: Cow<'a, str>,
    #[serde(borrow, default)]
    arguments: Cow<'a, str>,
    #[serde(borrow, default)]
    delta: Cow<'a, str>,
    #[serde(borrow, default)]
    transcript: Cow<'a, str>,
    #[serde(borrow, default)]
    response: Option<ResponseObject<'a>>,
    #[serde(borrow, default)]
    error: Option<ErrorObject<'a>>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponseObject<'a> {
    #[serde(borrow, default)]
    id: Cow<'a, str>,
    #[serde(borrow, default)]
    status: Cow<'a, str>,
    #[serde(borrow, default)]
    status_details: Option<StatusDetails<'a>>,
    #[serde(borrow, default)]
    output: Vec<OutputItem<'a>>,
}

#[derive(Debug, Default, Deserialize)]
struct StatusDetails<'a> {
    #[serde(borrow, default)]
    reason: Cow<'a, str>,
    #[serde(borrow, default)]
    error: Option<ErrorObject<'a>>,
}

#[derive(Debug, Default, Deserialize)]
struct OutputItem<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    call_id: Cow<'a, str>,
    #[serde(borrow, default)]
    name: Cow<'a, str>,
    #[serde(borrow, default)]
    arguments: Cow<'a, str>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorObject<'a> {
    #[serde(borrow, default)]
    code: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    message: Cow<'a, str>,
    #[serde(borrow, default)]
    event_id: Option<Cow<'a, str>>,
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
}

/// What one server message means to the session loop.
#[derive(Debug)]
pub(super) enum Decoded {
    /// `session.created`.
    SessionCreated,
    /// `session.updated`.
    SessionUpdated,
    /// Exactly one event for the engine (the common case, no list allocated).
    One(S2sEvent),
    /// Events for the engine, in order (zero when the message is ignored).
    Events(Vec<S2sEvent>),
    /// An `error` event. `event_id` is the client event that failed, when the server names it.
    Error {
        event_id: Option<String>,
        code: Arc<str>,
        message: Arc<str>,
    },
}

/// Per-session state of the decoder: the call ids already delivered (a call arrives both as
/// `response.function_call_arguments.done` and inside `response.done`) and cached ids, so a
/// stream of audio deltas does not allocate an `Arc<str>` per chunk.
#[derive(Debug, Default)]
pub(super) struct Decoder {
    delivered: std::collections::VecDeque<String>,
    response: Option<Arc<str>>,
    item: Option<Arc<str>>,
    /// Audio deltas that could not be decoded (dropped).
    pub(super) bad_audio: u64,
    /// Reused buffer the audio deltas are decoded into.
    pcm: BytesMut,
}

/// Calls remembered for de-duplication.
const DELIVERED_CAP: usize = 128;

impl Decoder {
    fn cached(slot: &mut Option<Arc<str>>, id: &str) -> Arc<str> {
        match slot {
            Some(cached) if &**cached == id => Arc::clone(cached),
            _ => {
                let fresh: Arc<str> = Arc::from(id);
                *slot = Some(Arc::clone(&fresh));
                fresh
            }
        }
    }

    /// Records `call_id` as delivered; `false` when it already was.
    fn first_delivery(&mut self, call_id: &str) -> bool {
        if self.delivered.iter().any(|seen| seen == call_id) {
            return false;
        }
        if self.delivered.len() >= DELIVERED_CAP {
            self.delivered.pop_front();
        }
        self.delivered.push_back(call_id.to_owned());
        true
    }

    /// Decodes base64 audio into the reused buffer; `None` when it is not valid base64.
    fn decode_pcm(&mut self, delta: &[u8]) -> Option<Bytes> {
        let max = base64::decoded_len_estimate(delta.len());
        self.pcm.clear();
        self.pcm.resize(max, 0);
        let written = STANDARD.decode_slice(delta, &mut self.pcm).ok()?;
        self.pcm.truncate(written);
        Some(self.pcm.split().freeze())
    }

    /// Decodes one text message.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the text is not a JSON object with a `type`.
    pub(super) fn decode(&mut self, text: &str) -> Result<Decoded> {
        let msg: Incoming<'_> = serde_json::from_str(text)
            .map_err(|_| protocol("a server message is not a JSON event"))?;
        let one = |event| Ok(Decoded::One(event));
        match &*msg.kind {
            "session.created" => Ok(Decoded::SessionCreated),
            "session.updated" => Ok(Decoded::SessionUpdated),
            "input_audio_buffer.speech_started" => one(S2sEvent::SpeechStarted),
            "input_audio_buffer.speech_stopped" => one(S2sEvent::SpeechStopped),
            "response.created" => {
                let id = msg.response.as_ref().map_or("", |r| &r.id);
                one(S2sEvent::ResponseStarted {
                    response: Self::cached(&mut self.response, id),
                })
            }
            "response.output_audio.delta" => {
                let Some(pcm) = self.decode_pcm(msg.delta.as_bytes()) else {
                    self.bad_audio = self.bad_audio.saturating_add(1);
                    return Ok(Decoded::Events(Vec::new()));
                };
                one(S2sEvent::Audio {
                    response: Self::cached(&mut self.response, &msg.response_id),
                    item: Self::cached(&mut self.item, &msg.item_id),
                    pcm,
                })
            }
            "response.output_audio.done" => one(S2sEvent::AudioDone {
                response: Self::cached(&mut self.response, &msg.response_id),
                item: Self::cached(&mut self.item, &msg.item_id),
            }),
            "response.output_audio_transcript.delta" => one(S2sEvent::AssistantTranscript {
                response: Self::cached(&mut self.response, &msg.response_id),
                item: Self::cached(&mut self.item, &msg.item_id),
                text: Arc::from(&*msg.delta),
                is_final: false,
            }),
            "response.output_audio_transcript.done" => one(S2sEvent::AssistantTranscript {
                response: Self::cached(&mut self.response, &msg.response_id),
                item: Self::cached(&mut self.item, &msg.item_id),
                text: Arc::from(&*msg.transcript),
                is_final: true,
            }),
            "conversation.item.input_audio_transcription.completed" => {
                if msg.transcript.is_empty() {
                    Ok(Decoded::Events(Vec::new()))
                } else {
                    one(S2sEvent::UserTranscript {
                        text: Arc::from(&*msg.transcript),
                    })
                }
            }
            "response.function_call_arguments.done" => {
                if msg.call_id.is_empty() || !self.first_delivery(&msg.call_id) {
                    return Ok(Decoded::Events(Vec::new()));
                }
                one(S2sEvent::ToolCall {
                    response: Self::cached(&mut self.response, &msg.response_id),
                    call_id: Arc::from(&*msg.call_id),
                    name: Arc::from(&*msg.name),
                    arguments: Arc::from(&*msg.arguments),
                })
            }
            "response.done" => {
                let Some(response) = msg.response else {
                    return Err(protocol("response.done without a response"));
                };
                let id = Self::cached(&mut self.response, &response.id);
                let mut events = Vec::new();
                for item in &response.output {
                    if item.kind == "function_call"
                        && !item.call_id.is_empty()
                        && self.first_delivery(&item.call_id)
                    {
                        events.push(S2sEvent::ToolCall {
                            response: Arc::clone(&id),
                            call_id: Arc::from(&*item.call_id),
                            name: Arc::from(&*item.name),
                            arguments: Arc::from(&*item.arguments),
                        });
                    }
                }
                let status = match &*response.status {
                    "completed" => S2sResponseStatus::Completed,
                    "cancelled" => S2sResponseStatus::Cancelled,
                    "incomplete" => S2sResponseStatus::Incomplete,
                    other => {
                        let detail = response.status_details.as_ref().map_or_else(
                            || other.to_owned(),
                            |d| {
                                d.error
                                    .as_ref()
                                    .map(|e| e.message.to_string())
                                    .filter(|m| !m.is_empty())
                                    .unwrap_or_else(|| {
                                        if d.reason.is_empty() {
                                            other.to_owned()
                                        } else {
                                            d.reason.to_string()
                                        }
                                    })
                            },
                        );
                        S2sResponseStatus::Failed(Arc::from(detail))
                    }
                };
                events.push(S2sEvent::ResponseDone {
                    response: id,
                    status,
                });
                Ok(Decoded::Events(events))
            }
            "error" => {
                let error = msg.error.unwrap_or_default();
                let code: Arc<str> = Arc::from(error.code.as_deref().unwrap_or(&error.kind));
                Ok(Decoded::Error {
                    event_id: error.event_id.map(|id| id.into_owned()),
                    code,
                    message: Arc::from(&*error.message),
                })
            }
            _ => Ok(Decoded::Events(Vec::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use brisvox_core::Secret;

    use super::*;
    use crate::llm::ToolSpec;

    fn cfg() -> OpenAiRealtimeConfig {
        OpenAiRealtimeConfig::new(Secret::new("k".to_owned()))
    }

    fn s2s() -> S2sConfig {
        let mut c = S2sConfig::new("be brief");
        c.tools.push(ToolSpec {
            name: Arc::from("lookup"),
            description: Arc::from("look a thing up"),
            parameters_schema: Arc::from(r#"{"type":"object","properties":{}}"#),
        });
        c
    }

    fn events(decoded: Decoded) -> Vec<S2sEvent> {
        match decoded {
            Decoded::Events(events) => events,
            Decoded::One(event) => vec![event],
            other => panic!("not events: {other:?}"),
        }
    }

    #[test]
    fn session_update_carries_tools_formats_and_server_vad() {
        let text = session_update(&cfg(), &s2s()).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["type"], "realtime");
        assert_eq!(v["session"]["instructions"], "be brief");
        assert_eq!(v["session"]["tools"][0]["type"], "function");
        assert_eq!(v["session"]["tools"][0]["name"], "lookup");
        assert_eq!(v["session"]["tools"][0]["parameters"]["type"], "object");
        assert_eq!(v["session"]["tool_choice"], "auto");
        let input = &v["session"]["audio"]["input"];
        assert_eq!(input["format"]["type"], "audio/pcm");
        assert_eq!(input["format"]["rate"], 24000);
        assert_eq!(input["turn_detection"]["type"], "server_vad");
        assert_eq!(input["turn_detection"]["interrupt_response"], true);
        assert_eq!(v["session"]["audio"]["output"]["voice"], "marin");
    }

    #[test]
    fn manual_turn_detection_is_null_and_other_rates_are_refused() {
        let mut manual = s2s();
        manual.turn_detection = S2sTurnDetection::Manual;
        let v: Value = serde_json::from_str(&session_update(&cfg(), &manual).unwrap()).unwrap();
        assert!(v["session"]["audio"]["input"]["turn_detection"].is_null());
        let mut odd = s2s();
        odd.input_rate = SampleRate::Hz16000;
        assert!(session_update(&cfg(), &odd).is_err());
    }

    #[test]
    fn control_events_have_the_documented_shapes() {
        let (text, sent) = control_event(
            &S2sControl::Truncate {
                item: Arc::from("item_1"),
                audio_end: Duration::from_millis(1500),
            },
            "c1",
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "conversation.item.truncate");
        assert_eq!(v["item_id"], "item_1");
        assert_eq!(v["content_index"], 0);
        assert_eq!(v["audio_end_ms"], 1500);
        assert_eq!(v["event_id"], "c1");
        assert_eq!(sent, Sent::Racy);
        let (text, _) = control_event(
            &S2sControl::ToolResult {
                call_id: Arc::from("call_9"),
                output: Arc::from("{\"ok\":true}"),
            },
            "c2",
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["item"]["type"], "function_call_output");
        assert_eq!(v["item"]["call_id"], "call_9");
        assert_eq!(v["item"]["output"], "{\"ok\":true}");
    }

    #[test]
    fn audio_append_is_base64_json() {
        let mut out = BytesMut::new();
        let first = append_audio(&[1, 2, 3, 4], &mut out).unwrap();
        let v: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(v["type"], "input_audio_buffer.append");
        assert_eq!(v["audio"], "AQIDBA==");
        // The buffer is reusable: a second frame does not contain the first.
        let second = append_audio(&[9], &mut out).unwrap();
        let v: Value = serde_json::from_slice(&second).unwrap();
        assert_eq!(v["audio"], "CQ==");
        assert!(out.is_empty());
    }

    #[test]
    fn audio_deltas_decode_and_reuse_their_ids() {
        let mut d = Decoder::default();
        let msg = r#"{"type":"response.output_audio.delta","response_id":"r1","item_id":"i1","output_index":0,"content_index":0,"delta":"AQID"}"#;
        let first = events(d.decode(msg).unwrap());
        let second = events(d.decode(msg).unwrap());
        match (&first[0], &second[0]) {
            (
                S2sEvent::Audio {
                    response,
                    item,
                    pcm,
                },
                S2sEvent::Audio {
                    response: r2,
                    item: i2,
                    ..
                },
            ) => {
                assert_eq!(&**response, "r1");
                assert_eq!(&**item, "i1");
                assert_eq!(&pcm[..], &[1, 2, 3]);
                assert!(Arc::ptr_eq(response, r2) && Arc::ptr_eq(item, i2));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_tool_call_is_delivered_once_from_either_event() {
        let mut d = Decoder::default();
        let args = r#"{"type":"response.function_call_arguments.done","response_id":"r1","item_id":"i2","output_index":1,"call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}"#;
        let done = r#"{"type":"response.done","response":{"id":"r1","status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},{"type":"function_call","call_id":"call_2","name":"lookup","arguments":"{}"}]}}"#;
        assert_eq!(events(d.decode(args).unwrap()).len(), 1);
        let after = events(d.decode(done).unwrap());
        // call_2 (new) then the response end; call_1 is not delivered again.
        assert_eq!(after.len(), 2);
        assert!(matches!(&after[0], S2sEvent::ToolCall { call_id, .. } if &**call_id == "call_2"));
        assert!(matches!(
            &after[1],
            S2sEvent::ResponseDone {
                status: S2sResponseStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn response_status_maps() {
        let mut d = Decoder::default();
        let cancelled =
            r#"{"type":"response.done","response":{"id":"r1","status":"cancelled","output":[]}}"#;
        assert!(matches!(
            &events(d.decode(cancelled).unwrap())[0],
            S2sEvent::ResponseDone {
                status: S2sResponseStatus::Cancelled,
                ..
            }
        ));
        let failed = r#"{"type":"response.done","response":{"id":"r2","status":"failed","status_details":{"type":"failed","error":{"code":"x","message":"boom"}},"output":[]}}"#;
        assert!(matches!(
            &events(d.decode(failed).unwrap())[0],
            S2sEvent::ResponseDone { status: S2sResponseStatus::Failed(m), .. } if &**m == "boom"
        ));
    }

    #[test]
    fn errors_and_junk() {
        let mut d = Decoder::default();
        let err = r#"{"type":"error","error":{"type":"invalid_request_error","code":"bad","message":"nope","event_id":"c7"}}"#;
        match d.decode(err).unwrap() {
            Decoded::Error {
                code,
                message,
                event_id,
            } => {
                assert_eq!(event_id.as_deref(), Some("c7"));
                assert_eq!(&*code, "bad");
                assert_eq!(&*message, "nope");
            }
            other => panic!("{other:?}"),
        }
        assert!(d.decode("not json").is_err());
        assert!(events(d.decode(r#"{"type":"rate_limits.updated"}"#).unwrap()).is_empty());
    }
}
