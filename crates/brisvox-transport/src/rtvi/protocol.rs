//! The RTVI messages (protocol 2.1.0, `docs/providers/rtvi.md`): the JSON envelope, the typed
//! events the server can send and the client messages it understands.
//!
//! ```json
//! { "label": "rtvi-ai", "type": "<kebab-case>", "id": "<string>", "data": { } }
//! ```
//!
//! Client messages must carry an `id` (Pipecat's `Message` model requires it); replies
//! (`bot-ready`, `server-response`, `error-response`) echo it; server-initiated events carry no
//! `id`, and payload-less events carry no `data`. Names and fields follow `models.py` of Pipecat
//! v1.12.0 as recorded in the doc; nothing here is invented.

use brisvox_core::frame::DtmfDigit;
use serde::Serialize;
use serde_json::{Map, Value};

/// The `label` of every RTVI message. Messages with another label are not RTVI and are dropped.
pub const LABEL: &str = "rtvi-ai";

/// The protocol version this server speaks (`PROTOCOL_VERSION` in Pipecat's `models.py`).
pub const PROTOCOL_VERSION: &str = "2.1.0";

/// The server's own major version.
const SERVER_MAJOR: u64 = 2;

/// The older major version Pipecat accepts as legacy.
const LEGACY_MAJOR: u64 = 1;

/// Longest client-supplied string echoed back in an error text.
const MAX_ECHO: usize = 64;

/// A metric value of [`Metrics`]: `{ "processor": ..., "value": ..., "model": ... }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricEntry {
    /// The processor (pipeline stage or provider service) the metric belongs to.
    pub processor: String,
    /// The value: seconds for `ttfb` and `processing`, characters for `characters`.
    pub value: f64,
    /// The model the metric was measured for, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The `data` of a `metrics` event. Lists that are empty are omitted.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Metrics {
    /// Processing times.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub processing: Vec<MetricEntry>,
    /// Time to first byte of each service.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ttfb: Vec<MetricEntry>,
    /// Characters processed (TTS usage).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub characters: Vec<MetricEntry>,
}

/// The `data` of events that carry one text: `bot-llm-text`, `bot-tts-text`, `user-llm-text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextData {
    /// The text.
    pub text: String,
}

impl TextData {
    /// Text data from anything string-like.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// The `data` of `user-transcription`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserTranscription {
    /// The transcript.
    pub text: String,
    /// Speaker id (empty when unknown).
    pub user_id: String,
    /// A timestamp string; the format is unspecified by the protocol (ISO-8601 in practice, see
    /// [`RtviSender::user_transcription`](super::RtviSender::user_transcription)).
    pub timestamp: String,
    /// `false` for an interim result, `true` for the final one.
    #[serde(rename = "final")]
    pub is_final: bool,
}

/// How far a `bot-output` segment has been spoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpokenStatus {
    /// Not spoken yet.
    New,
    /// Being spoken.
    InProgress,
    /// Fully spoken.
    Completed,
}

/// The `data` of `bot-output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotOutput {
    /// The text of the segment.
    pub text: String,
    /// What aggregated the segment: `sentence`, `word`, `code`, `url`, ...
    pub aggregated_by: String,
    /// Whether the text will be spoken at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub will_be_spoken: Option<bool>,
    /// Playback state of the segment (2.0+ clients).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoken_status: Option<SpokenStatus>,
}

/// The `data` of `llm-function-call-started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionCallStarted {
    /// The function being called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
}

/// The `data` of `llm-function-call-in-progress`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionCallInProgress {
    /// Identifies the call; the same id ends it in `llm-function-call-stopped`.
    pub tool_call_id: String,
    /// The function being called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    /// The arguments (only with a report level that allows them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// The `data` of `llm-function-call-stopped`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionCallStopped {
    /// Identifies the call.
    pub tool_call_id: String,
    /// Whether the call was cancelled instead of completed.
    pub cancelled: bool,
    /// The function that was called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    /// The result (only with a report level that allows it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// The `data` of `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorData {
    /// The message shown to the application.
    pub error: String,
    /// `true` makes the JS client disconnect.
    pub fatal: bool,
}

/// An RTVI event the server sends to the client without being asked.
///
/// [`RtviSender::send`](super::RtviSender::send) puts it on the wire as a protobuf `MessageFrame`
/// holding the RTVI envelope. The JS client (`@pipecat-ai/client-js` 1.13.1) acts on every event
/// below except `bot-interrupted`, which it only logs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RtviEvent {
    /// `user-started-speaking`.
    UserStartedSpeaking,
    /// `user-stopped-speaking`.
    UserStoppedSpeaking,
    /// `bot-started-speaking`.
    BotStartedSpeaking,
    /// `bot-stopped-speaking`.
    BotStoppedSpeaking,
    /// `bot-interrupted`: the bot was cut off (sent for every barge-in; client-js 1.13.1 has no
    /// handler for it).
    BotInterrupted,
    /// `user-mute-started`.
    UserMuteStarted,
    /// `user-mute-stopped`.
    UserMuteStopped,
    /// `user-transcription`, interim or final.
    UserTranscription(UserTranscription),
    /// `user-llm-text`: the text handed to the LLM.
    UserLlmText(TextData),
    /// `bot-output`: aggregated bot text (sentences), the replacement of `bot-transcription`.
    BotOutput(BotOutput),
    /// `bot-llm-started`.
    BotLlmStarted,
    /// `bot-llm-text`: a streamed LLM chunk.
    BotLlmText(TextData),
    /// `bot-llm-stopped`.
    BotLlmStopped,
    /// `bot-tts-started`.
    BotTtsStarted,
    /// `bot-tts-text`: the text sent to the TTS.
    BotTtsText(TextData),
    /// `bot-tts-stopped`.
    BotTtsStopped,
    /// `metrics`.
    Metrics(Metrics),
    /// `llm-function-call-started`.
    #[serde(rename = "llm-function-call-started")]
    FunctionCallStarted(FunctionCallStarted),
    /// `llm-function-call-in-progress`.
    #[serde(rename = "llm-function-call-in-progress")]
    FunctionCallInProgress(FunctionCallInProgress),
    /// `llm-function-call-stopped`.
    #[serde(rename = "llm-function-call-stopped")]
    FunctionCallStopped(FunctionCallStopped),
    /// `server-message`: any JSON value for the application.
    ServerMessage(Value),
    /// `error`: a problem on the server; `fatal: true` makes the JS client disconnect.
    Error(ErrorData),
}

/// A reply to a client request (it echoes the request's `id`) or the handshake answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub(crate) enum Reply {
    BotReady {
        version: String,
        about: Value,
    },
    ServerResponse {
        t: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        d: Option<Value>,
    },
    ErrorResponse {
        error: String,
    },
}

/// Renders an envelope: `value` is the serialized `{type, data?}` object.
fn envelope(value: Value, id: Option<&str>) -> String {
    let mut object = match value {
        Value::Object(object) => object,
        other => {
            let mut object = Map::new();
            object.insert("data".to_owned(), other);
            object
        }
    };
    object.insert("label".to_owned(), Value::String(LABEL.to_owned()));
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    // A `Map` of strings and JSON values always serializes.
    serde_json::to_string(&Value::Object(object)).unwrap_or_default()
}

/// The RTVI JSON of a server event (no `id`).
pub fn render_event(event: &RtviEvent) -> String {
    envelope(serde_json::to_value(event).unwrap_or(Value::Null), None)
}

/// The RTVI JSON of a reply, echoing `id`.
pub(crate) fn render_reply(reply: &Reply, id: &str) -> String {
    envelope(serde_json::to_value(reply).unwrap_or(Value::Null), Some(id))
}

/// The `bot-ready` answer: `data.version` and `data.about` only (no config or actions in 2.x).
pub(crate) fn bot_ready(version: &str) -> Reply {
    Reply::BotReady {
        version: version.to_owned(),
        about: serde_json::json!({
            "library": "brisvox",
            "library_version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

/// A client request the server understands.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// `client-ready`: the handshake. `version` is the client's RTVI version string if it sent
    /// one, `about` its `about` object.
    ClientReady {
        /// `data.version`.
        version: Option<String>,
        /// `data.about` (library, library_version, platform, ...).
        about: Option<Value>,
    },
    /// `disconnect-bot`: end the call.
    DisconnectBot,
    /// `client-message`: an application message `{ t, d }`. Answer it with
    /// [`RtviSender::respond`](super::RtviSender::respond) or
    /// [`RtviSender::respond_error`](super::RtviSender::respond_error), or the client's
    /// `sendClientRequest()` never resolves.
    ClientMessage {
        /// The message name.
        t: String,
        /// The payload, if any.
        d: Option<Value>,
    },
    /// `send-text`: text typed by the user.
    SendText {
        /// The text.
        content: String,
        /// `options.run_immediately` (default `true`): interrupt the bot and answer now.
        run_immediately: bool,
        /// `options.audio_response` (default `true`): `false` asks for a text-only answer.
        audio_response: bool,
    },
    /// `dtmf`: key presses, in order.
    Dtmf {
        /// The keys.
        buttons: Vec<DtmfDigit>,
    },
    /// `llm-function-call-result`: the client's answer to a function call it handles.
    FunctionCallResult {
        /// The function name.
        function_name: String,
        /// Identifies the call.
        tool_call_id: String,
        /// The arguments the call was made with.
        arguments: Value,
        /// The result (a JSON value or a string).
        result: Value,
    },
    /// A UI-worker message (`ui-snapshot`, `ui-event`, `ui-cancel-job-group`): optional, ignored.
    Ignored(String),
    /// A type the server does not support (`append-to-context` is deprecated and unsupported in
    /// Pipecat 1.12 too, `raw-audio` is not used by the JS client): answered with an
    /// `error-response`.
    Unsupported(String),
}

/// A parsed client request: the id to echo and the message.
#[derive(Debug, Clone, PartialEq)]
pub struct Inbound {
    /// The request id (`id`), echoed in replies.
    pub id: String,
    /// The message.
    pub message: ClientMessage,
}

/// What a text from the client turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A valid RTVI message.
    Message(Inbound),
    /// A JSON object whose `label` is not `rtvi-ai`: not RTVI, silently dropped (Pipecat logs
    /// `Ignoring not RTVI message`).
    Foreign,
    /// Not JSON, not an object, or no valid `type` / `id`: Pipecat answers `error` "Invalid RTVI
    /// transport message".
    Invalid,
    /// A known `type` whose `data` is malformed; the request id and a description for an
    /// `error-response`.
    BadData {
        /// The request id.
        id: String,
        /// What is wrong.
        error: String,
    },
}

/// Parses the JSON of one client `MessageFrame`.
pub fn parse(json: &str) -> Parsed {
    let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(json) else {
        return Parsed::Invalid;
    };
    match object.get("label") {
        Some(Value::String(label)) if label == LABEL => {}
        Some(_) => return Parsed::Foreign,
        None => return Parsed::Invalid,
    }
    let (Some(Value::String(kind)), Some(Value::String(id))) =
        (object.remove("type"), object.remove("id"))
    else {
        return Parsed::Invalid;
    };
    let data = object.remove("data").unwrap_or(Value::Null);
    match message(&kind, data) {
        Ok(message) => Parsed::Message(Inbound { id, message }),
        Err(error) => Parsed::BadData { id, error },
    }
}

fn message(kind: &str, data: Value) -> Result<ClientMessage, String> {
    let bad = |what: &str| format!("Invalid data for {kind}: {what}");
    let mut fields = match data {
        Value::Object(fields) => fields,
        Value::Null => Map::new(),
        _ => {
            // `disconnect-bot` and friends ignore their payload; the rest need an object.
            if matches!(kind, "disconnect-bot") {
                Map::new()
            } else {
                return Err(bad("data must be an object"));
            }
        }
    };
    let text = |fields: &mut Map<String, Value>, key: &str| match fields.remove(key) {
        Some(Value::String(text)) => Some(text),
        _ => None,
    };
    Ok(match kind {
        "client-ready" => ClientMessage::ClientReady {
            version: text(&mut fields, "version"),
            about: fields.remove("about"),
        },
        "disconnect-bot" => ClientMessage::DisconnectBot,
        "client-message" => ClientMessage::ClientMessage {
            t: text(&mut fields, "t").ok_or_else(|| bad("t must be a string"))?,
            d: fields.remove("d"),
        },
        "send-text" => {
            let content = text(&mut fields, "content").ok_or_else(|| bad("content is missing"))?;
            let options = match fields.remove("options") {
                Some(Value::Object(options)) => options,
                _ => Map::new(),
            };
            let flag = |key: &str| options.get(key).and_then(Value::as_bool).unwrap_or(true);
            ClientMessage::SendText {
                content,
                run_immediately: flag("run_immediately"),
                audio_response: flag("audio_response"),
            }
        }
        "dtmf" => {
            let Some(Value::Array(buttons)) = fields.remove("buttons") else {
                return Err(bad("buttons must be a list"));
            };
            if buttons.is_empty() {
                return Err(bad("buttons must not be empty"));
            }
            let mut digits = Vec::with_capacity(buttons.len());
            for button in &buttons {
                let digit = button.as_str().and_then(dtmf_digit);
                digits.push(digit.ok_or_else(|| bad("buttons are 0-9, * or #"))?);
            }
            ClientMessage::Dtmf { buttons: digits }
        }
        "llm-function-call-result" => ClientMessage::FunctionCallResult {
            function_name: text(&mut fields, "function_name")
                .ok_or_else(|| bad("function_name is missing"))?,
            tool_call_id: text(&mut fields, "tool_call_id")
                .ok_or_else(|| bad("tool_call_id is missing"))?,
            arguments: fields.remove("arguments").unwrap_or(Value::Null),
            result: fields.remove("result").unwrap_or(Value::Null),
        },
        "ui-snapshot" | "ui-event" | "ui-cancel-job-group" => {
            ClientMessage::Ignored(kind.to_owned())
        }
        other => ClientMessage::Unsupported(other.to_owned()),
    })
}

/// A key of the `dtmf` message: one of `0-9`, `*`, `#`.
fn dtmf_digit(button: &str) -> Option<DtmfDigit> {
    Some(match button {
        "0" => DtmfDigit::D0,
        "1" => DtmfDigit::D1,
        "2" => DtmfDigit::D2,
        "3" => DtmfDigit::D3,
        "4" => DtmfDigit::D4,
        "5" => DtmfDigit::D5,
        "6" => DtmfDigit::D6,
        "7" => DtmfDigit::D7,
        "8" => DtmfDigit::D8,
        "9" => DtmfDigit::D9,
        "*" => DtmfDigit::Star,
        "#" => DtmfDigit::Pound,
        _ => return None,
    })
}

/// How the server answers the version in a `client-ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionCheck {
    /// The `version` for `bot-ready`: the server's, or a 1.x client's own.
    pub(crate) echo: String,
    /// The `error-response` text if the version is unusable; the call continues regardless.
    pub(crate) warning: Option<String>,
}

/// Cuts `text` to at most [`MAX_ECHO`] bytes on a char boundary.
fn clip(text: &str) -> &str {
    let mut end = text.len().min(MAX_ECHO);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// Pipecat's version check: exactly three integers, major 2 (the server's) or 1 (legacy). A
/// failure only produces a warning; the connection continues.
pub(crate) fn check_version(client: Option<&str>) -> VersionCheck {
    const SUFFIX: &str = " Compatibility issues may occur.";
    let server = || PROTOCOL_VERSION.to_owned();
    let Some(version) = client else {
        return VersionCheck {
            echo: server(),
            warning: Some(format!("Client version unknown.{SUFFIX}")),
        };
    };
    let parts: Option<Vec<u64>> = version
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect();
    match parts.as_deref() {
        Some([major, _, _]) if *major == SERVER_MAJOR => VersionCheck {
            echo: server(),
            warning: None,
        },
        Some([major, _, _]) if *major == LEGACY_MAJOR => VersionCheck {
            echo: version.to_owned(),
            warning: None,
        },
        Some([_, _, _]) => VersionCheck {
            echo: server(),
            warning: Some(format!(
                "RTVI version {} is not compatible with server protocol {PROTOCOL_VERSION}.{SUFFIX}",
                clip(version)
            )),
        },
        _ => VersionCheck {
            echo: server(),
            warning: Some(format!(
                "Invalid client version format ({}).{SUFFIX}",
                clip(version)
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event_json(event: &RtviEvent) -> Value {
        serde_json::from_str(&render_event(event)).unwrap()
    }

    #[test]
    fn payload_less_events_have_no_data_and_no_id() {
        let cases = [
            (RtviEvent::UserStartedSpeaking, "user-started-speaking"),
            (RtviEvent::UserStoppedSpeaking, "user-stopped-speaking"),
            (RtviEvent::BotStartedSpeaking, "bot-started-speaking"),
            (RtviEvent::BotStoppedSpeaking, "bot-stopped-speaking"),
            (RtviEvent::BotInterrupted, "bot-interrupted"),
            (RtviEvent::UserMuteStarted, "user-mute-started"),
            (RtviEvent::UserMuteStopped, "user-mute-stopped"),
            (RtviEvent::BotLlmStarted, "bot-llm-started"),
            (RtviEvent::BotLlmStopped, "bot-llm-stopped"),
            (RtviEvent::BotTtsStarted, "bot-tts-started"),
            (RtviEvent::BotTtsStopped, "bot-tts-stopped"),
        ];
        for (event, kind) in cases {
            assert_eq!(
                event_json(&event),
                json!({"label": "rtvi-ai", "type": kind}),
                "{kind}"
            );
        }
    }

    #[test]
    fn events_with_data_match_the_doc_examples() {
        assert_eq!(
            event_json(&RtviEvent::UserTranscription(UserTranscription {
                text: "merhaba".to_owned(),
                user_id: String::new(),
                timestamp: "2026-10-08T10:00:00.000+00:00".to_owned(),
                is_final: true,
            })),
            json!({"label":"rtvi-ai","type":"user-transcription","data":{
                "text":"merhaba","user_id":"","timestamp":"2026-10-08T10:00:00.000+00:00","final":true}})
        );
        assert_eq!(
            event_json(&RtviEvent::BotLlmText(TextData::new("Merhaba"))),
            json!({"label":"rtvi-ai","type":"bot-llm-text","data":{"text":"Merhaba"}})
        );
        assert_eq!(
            event_json(&RtviEvent::BotTtsText(TextData::new(
                "Merhaba, nasilsiniz?"
            ))),
            json!({"label":"rtvi-ai","type":"bot-tts-text","data":{"text":"Merhaba, nasilsiniz?"}})
        );
        assert_eq!(
            event_json(&RtviEvent::UserLlmText(TextData::new("x"))),
            json!({"label":"rtvi-ai","type":"user-llm-text","data":{"text":"x"}})
        );
        assert_eq!(
            event_json(&RtviEvent::BotOutput(BotOutput {
                text: "Merhaba, nasilsiniz?".to_owned(),
                aggregated_by: "sentence".to_owned(),
                will_be_spoken: Some(true),
                spoken_status: Some(SpokenStatus::Completed),
            })),
            json!({"label":"rtvi-ai","type":"bot-output","data":{
                "text":"Merhaba, nasilsiniz?","aggregated_by":"sentence",
                "spoken_status":"completed","will_be_spoken":true}})
        );
        assert_eq!(
            event_json(&RtviEvent::Error(ErrorData {
                error: "stt connection lost".to_owned(),
                fatal: false
            })),
            json!({"label":"rtvi-ai","type":"error","data":{"error":"stt connection lost","fatal":false}})
        );
        assert_eq!(
            event_json(&RtviEvent::ServerMessage(json!({"any": "json"}))),
            json!({"label":"rtvi-ai","type":"server-message","data":{"any":"json"}})
        );
        assert_eq!(
            event_json(&RtviEvent::Metrics(Metrics {
                ttfb: vec![MetricEntry {
                    processor: "CartesiaTTSService#0".to_owned(),
                    value: 0.21,
                    model: Some("sonic".to_owned()),
                }],
                ..Metrics::default()
            })),
            json!({"label":"rtvi-ai","type":"metrics","data":{"ttfb":[
                {"processor":"CartesiaTTSService#0","value":0.21,"model":"sonic"}]}})
        );
    }

    #[test]
    fn function_call_events_use_the_llm_prefix() {
        assert_eq!(
            event_json(&RtviEvent::FunctionCallStarted(FunctionCallStarted {
                function_name: Some("get_weather".to_owned())
            })),
            json!({"label":"rtvi-ai","type":"llm-function-call-started",
                   "data":{"function_name":"get_weather"}})
        );
        assert_eq!(
            event_json(&RtviEvent::FunctionCallInProgress(FunctionCallInProgress {
                tool_call_id: "c1".to_owned(),
                function_name: None,
                arguments: Some(json!({"city": "Ankara"})),
            })),
            json!({"label":"rtvi-ai","type":"llm-function-call-in-progress",
                   "data":{"tool_call_id":"c1","arguments":{"city":"Ankara"}}})
        );
        assert_eq!(
            event_json(&RtviEvent::FunctionCallStopped(FunctionCallStopped {
                tool_call_id: "c1".to_owned(),
                cancelled: false,
                function_name: None,
                result: None,
            })),
            json!({"label":"rtvi-ai","type":"llm-function-call-stopped",
                   "data":{"tool_call_id":"c1","cancelled":false}})
        );
    }

    #[test]
    fn replies_echo_the_request_id() {
        let ready: Value =
            serde_json::from_str(&render_reply(&bot_ready(PROTOCOL_VERSION), "a1b2c3d4")).unwrap();
        assert_eq!(ready["label"], "rtvi-ai");
        assert_eq!(ready["type"], "bot-ready");
        assert_eq!(ready["id"], "a1b2c3d4");
        assert_eq!(ready["data"]["version"], "2.1.0");
        assert_eq!(ready["data"]["about"]["library"], "brisvox");
        let response = Reply::ServerResponse {
            t: "get-weather".to_owned(),
            d: Some(json!({"temp": 21})),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&render_reply(&response, "e5f6a7b8")).unwrap(),
            json!({"label":"rtvi-ai","type":"server-response","id":"e5f6a7b8",
                   "data":{"t":"get-weather","d":{"temp":21}}})
        );
        let error = Reply::ErrorResponse {
            error: "Unsupported type foo".to_owned(),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&render_reply(&error, "e5f6a7b8")).unwrap(),
            json!({"label":"rtvi-ai","type":"error-response","id":"e5f6a7b8",
                   "data":{"error":"Unsupported type foo"}})
        );
    }

    fn inbound(text: &str) -> ClientMessage {
        match parse(text) {
            Parsed::Message(inbound) => inbound.message,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_doc_client_messages_parse() {
        let ready = r#"{"label":"rtvi-ai","type":"client-ready","id":"a1b2c3d4","data":{"version":"2.1.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.13.1"}}}"#;
        let Parsed::Message(parsed) = parse(ready) else {
            panic!()
        };
        assert_eq!(parsed.id, "a1b2c3d4");
        assert_eq!(
            parsed.message,
            ClientMessage::ClientReady {
                version: Some("2.1.0".to_owned()),
                about: Some(json!({"library":"@pipecat-ai/client-js","library_version":"1.13.1"})),
            }
        );
        assert_eq!(
            inbound(
                r#"{"label":"rtvi-ai","type":"client-message","id":"e5f6a7b8","data":{"t":"get-weather","d":{"city":"Ankara"}}}"#
            ),
            ClientMessage::ClientMessage {
                t: "get-weather".to_owned(),
                d: Some(json!({"city": "Ankara"})),
            }
        );
        assert_eq!(
            inbound(
                r#"{"label":"rtvi-ai","type":"send-text","id":"c9d0e1f2","data":{"content":"hello","options":{"run_immediately":false}}}"#
            ),
            ClientMessage::SendText {
                content: "hello".to_owned(),
                run_immediately: false,
                audio_response: true,
            }
        );
        assert_eq!(
            inbound(r#"{"label":"rtvi-ai","type":"disconnect-bot","id":"x","data":{}}"#),
            ClientMessage::DisconnectBot
        );
        assert_eq!(
            inbound(r##"{"label":"rtvi-ai","type":"dtmf","id":"x","data":{"buttons":["1","#"]}}"##),
            ClientMessage::Dtmf {
                buttons: vec![DtmfDigit::D1, DtmfDigit::Pound]
            }
        );
        assert_eq!(
            inbound(
                r#"{"label":"rtvi-ai","type":"llm-function-call-result","id":"x","data":{"function_name":"f","tool_call_id":"c","arguments":{"a":1},"result":"ok"}}"#
            ),
            ClientMessage::FunctionCallResult {
                function_name: "f".to_owned(),
                tool_call_id: "c".to_owned(),
                arguments: json!({"a": 1}),
                result: json!("ok"),
            }
        );
    }

    #[test]
    fn unsupported_and_ui_types_are_classified() {
        for kind in ["append-to-context", "raw-audio", "nonsense"] {
            let text = format!(r#"{{"label":"rtvi-ai","type":"{kind}","id":"x","data":{{}}}}"#);
            assert_eq!(inbound(&text), ClientMessage::Unsupported(kind.to_owned()));
        }
        let ui = r#"{"label":"rtvi-ai","type":"ui-event","id":"x","data":{}}"#;
        assert_eq!(inbound(ui), ClientMessage::Ignored("ui-event".to_owned()));
    }

    #[test]
    fn invalid_foreign_and_bad_data_are_told_apart() {
        assert_eq!(parse("not json"), Parsed::Invalid);
        assert_eq!(parse("[1]"), Parsed::Invalid);
        // No id: Pipecat's Message model rejects it.
        assert_eq!(
            parse(r#"{"label":"rtvi-ai","type":"client-ready"}"#),
            Parsed::Invalid
        );
        assert_eq!(
            parse(r#"{"label":"other","type":"client-ready","id":"x"}"#),
            Parsed::Foreign
        );
        assert_eq!(
            parse(r#"{"type":"client-ready","id":"x"}"#),
            Parsed::Invalid
        );
        let Parsed::BadData { id, error } =
            parse(r#"{"label":"rtvi-ai","type":"send-text","id":"q","data":{}}"#)
        else {
            panic!()
        };
        assert_eq!(id, "q");
        assert!(error.contains("send-text"), "{error}");
        assert!(matches!(
            parse(r#"{"label":"rtvi-ai","type":"dtmf","id":"q","data":{"buttons":["x"]}}"#),
            Parsed::BadData { .. }
        ));
        assert!(matches!(
            parse(r#"{"label":"rtvi-ai","type":"dtmf","id":"q","data":{"buttons":[]}}"#),
            Parsed::BadData { .. }
        ));
    }

    #[test]
    fn the_version_check_follows_pipecat() {
        let ok = check_version(Some("2.1.0"));
        assert_eq!((ok.echo.as_str(), ok.warning), ("2.1.0", None));
        let other_minor = check_version(Some("2.0.7"));
        assert_eq!(other_minor.echo, "2.1.0");
        assert_eq!(other_minor.warning, None);
        // A 1.x client gets its own version back.
        let legacy = check_version(Some("1.2.0"));
        assert_eq!((legacy.echo.as_str(), legacy.warning), ("1.2.0", None));
        let unknown = check_version(None);
        assert!(
            unknown
                .warning
                .unwrap()
                .starts_with("Client version unknown.")
        );
        let bad = check_version(Some("2.1"));
        assert!(
            bad.warning
                .unwrap()
                .starts_with("Invalid client version format (2.1).")
        );
        let future = check_version(Some("3.0.0"));
        let warning = future.warning.unwrap();
        assert!(warning.contains("not compatible with server protocol 2.1.0."));
        assert!(warning.ends_with(" Compatibility issues may occur."));
        assert_eq!(future.echo, "2.1.0");
    }
}
