//! Wire messages of the Gladia live API (see `docs/providers/gladia.md`).
//!
//! The only client control message is a fixed JSON text. Server messages are JSON text frames
//! discriminated by `type`; only the fields the adapter uses are parsed and unknown fields and
//! message types are ignored, so Gladia can extend its protocol without breaking sessions.

use serde::Deserialize;

/// `{"type":"stop_recording"}` (text frame; no more audio is accepted, post-processing starts).
pub(crate) const STOP_RECORDING: &str = r#"{"type":"stop_recording"}"#;

/// One `transcript` message, reduced to what the adapter needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Transcript {
    /// Utterance id (`data.id`): partials and the final of one utterance share it. Opaque.
    pub(crate) id: String,
    /// `data.is_final`. A missing flag is read as `false` (never silently as a final).
    pub(crate) is_final: bool,
    /// `data.utterance.text`, trimmed (may be empty).
    pub(crate) text: String,
    /// End of the utterance in seconds from the session start.
    pub(crate) end: f64,
    /// `data.utterance.language`: the language Gladia detected for the utterance (lowercase
    /// primary code); `None` when absent or blank.
    pub(crate) language: Option<String>,
}

/// What an acknowledgment acknowledges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckKind {
    /// An `audio_chunk` message (the server received and buffered audio).
    Audio,
    /// The `stop_recording` message.
    Stop,
}

/// A parsed server message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    /// A partial or final transcript.
    Transcript(Transcript),
    /// `speech_start`: speech was detected.
    SpeechStart,
    /// `speech_end`: the speech stopped (the final follows once endpointing closes it).
    SpeechEnd,
    /// An acknowledgment (`type` reuses the client message name; `acknowledged` tells them apart
    /// from client messages).
    Ack {
        /// What was acknowledged.
        kind: AckKind,
        /// `acknowledged`.
        acknowledged: bool,
        /// Short description of the `error` object, if any (status and exception name only).
        error: Option<String>,
    },
    /// `end_session` or `end_recording`: the server stopped receiving audio.
    SessionEnded,
    /// An error message (UNVERIFIED shape: the reference defines the error object but no
    /// standalone message type; both `type: "error"` carrying `error` and carrying `data` are
    /// accepted).
    Error {
        /// Status code, when the message has one.
        status: Option<u16>,
        /// Short description: exception name and status, never the free-form message.
        detail: String,
    },
    /// Lifecycle (`start_session`, `start_recording`), post-processing and add-on messages and
    /// types the adapter does not know: nothing to do.
    Ignored,
}

/// Why a text frame was not a valid server message. Carries no message content: transcripts are
/// user data and must not end up in errors or logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParseError(pub(crate) &'static str);

/// Reads a field leniently: a value of the wrong type (a number where text is expected, `null`)
/// counts as absent. Only the shapes the adapter relies on are read, and a provider that sends
/// an unexpected shape in a field must not end a live call (the exact partial shape is
/// UNVERIFIED in the docs).
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).ok())
}

#[derive(Deserialize)]
struct Peek {
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawTranscript {
    #[serde(default, deserialize_with = "lenient")]
    data: Option<RawTranscriptData>,
}

#[derive(Deserialize)]
struct RawTranscriptData {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<NumOrText>,
    #[serde(default, deserialize_with = "lenient")]
    is_final: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    utterance: Option<RawUtterance>,
}

#[derive(Deserialize)]
struct RawUtterance {
    #[serde(default, deserialize_with = "lenient")]
    text: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    end: Option<f64>,
    #[serde(default, deserialize_with = "lenient")]
    language: Option<String>,
}

#[derive(Deserialize)]
struct RawAck {
    #[serde(default, deserialize_with = "lenient")]
    acknowledged: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawErrorMessage {
    #[serde(default, deserialize_with = "lenient")]
    error: Option<RawError>,
    #[serde(default, deserialize_with = "lenient")]
    data: Option<RawError>,
}

/// The error object used inside messages (`status_code` may be a number or a string).
#[derive(Deserialize)]
struct RawError {
    #[serde(default, deserialize_with = "lenient")]
    status_code: Option<NumOrText>,
    #[serde(default, deserialize_with = "lenient")]
    exception: Option<String>,
}

/// A number or a string (ids and status codes are seen as both).
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrText {
    Num(i64),
    Text(String),
}

impl NumOrText {
    fn status(&self) -> Option<u16> {
        match self {
            Self::Num(n) => u16::try_from(*n).ok(),
            Self::Text(t) => t.trim().parse().ok(),
        }
    }

    fn into_text(self) -> String {
        match self {
            Self::Num(n) => n.to_string(),
            Self::Text(t) => t,
        }
    }
}

/// Longest exception name kept.
const MAX_NAME: usize = 64;

impl RawError {
    /// `(status, "exception (status)")`, sanitised: only the exception name's alphanumerics.
    fn describe(&self) -> (Option<u16>, String) {
        let status = self.status_code.as_ref().and_then(NumOrText::status);
        let name: String = self
            .exception
            .as_deref()
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            .take(MAX_NAME)
            .collect();
        let name = if name.is_empty() { "unknown" } else { &name };
        let detail = match status {
            Some(s) => format!("{name} ({s})"),
            None => name.to_owned(),
        };
        (status, detail)
    }
}

/// Parses one text frame.
///
/// Only a frame that is not a JSON object is an error. A message of an unexpected shape (a
/// `null` or numeric text or id, a `type` that is not a string, a transcript without a usable
/// body) is read as far as it makes sense and otherwise ignored: the adapter relies on a few
/// fields of a documented protocol it has no way to fully verify, and one odd message must not
/// end a call.
///
/// # Errors
/// [`ParseError`] if the frame is not a JSON object.
pub(crate) fn parse(text: &str) -> Result<Incoming, ParseError> {
    const NOT_OBJECT: ParseError = ParseError("not a JSON object");
    if !text.trim_start().starts_with('{') {
        return Err(NOT_OBJECT);
    }
    let peek: Peek = serde_json::from_str(text).map_err(|_| NOT_OBJECT)?;
    match peek.kind.as_deref() {
        Some("transcript") => {
            let Ok(raw) = serde_json::from_str::<RawTranscript>(text) else {
                return Ok(Incoming::Ignored);
            };
            let Some(data) = raw.data else {
                return Ok(Incoming::Ignored);
            };
            let (text, end, language) = match data.utterance {
                Some(u) => (u.text.unwrap_or_default(), u.end, u.language),
                None => (String::new(), None, None),
            };
            Ok(Incoming::Transcript(Transcript {
                id: data.id.map(NumOrText::into_text).unwrap_or_default(),
                is_final: data.is_final.unwrap_or(false),
                text: text.trim().to_owned(),
                end: end.filter(|e| e.is_finite()).unwrap_or(0.0),
                language: language
                    .as_deref()
                    .and_then(|l| l.split(['-', '_']).next())
                    .map(|l| l.trim().to_ascii_lowercase())
                    .filter(|l| !l.is_empty()),
            }))
        }
        Some("speech_start") => Ok(Incoming::SpeechStart),
        Some("speech_end") => Ok(Incoming::SpeechEnd),
        Some(name @ ("audio_chunk" | "stop_recording")) => {
            let Ok(raw) = serde_json::from_str::<RawAck>(text) else {
                return Ok(Incoming::Ignored);
            };
            Ok(match raw.acknowledged {
                Some(acknowledged) => Incoming::Ack {
                    kind: if name == "audio_chunk" {
                        AckKind::Audio
                    } else {
                        AckKind::Stop
                    },
                    acknowledged,
                    error: raw.error.map(|e| e.describe().1),
                },
                None => Incoming::Ignored,
            })
        }
        Some("end_session" | "end_recording") => Ok(Incoming::SessionEnded),
        Some("error") => {
            let raw = serde_json::from_str::<RawErrorMessage>(text).unwrap_or(RawErrorMessage {
                error: None,
                data: None,
            });
            let (status, detail) = raw
                .error
                .or(raw.data)
                .map_or((None, "unknown".to_owned()), |e| e.describe());
            Ok(Incoming::Error { status, detail })
        }
        _ => Ok(Incoming::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &str, id: &str, is_final: bool, end: f64) -> String {
        serde_json::json!({
            "session_id": "s", "created_at": "2025-09-19T12:34:10Z", "type": "transcript",
            "data": {"id": id, "is_final": is_final, "utterance": {
                "start": 0.0, "end": end, "confidence": 0.9, "channel": 0,
                "words": [], "text": text, "language": "tr"}}
        })
        .to_string()
    }

    #[test]
    fn partial_and_final_transcripts() {
        assert_eq!(
            parse(&transcript(" merhaba ", "00-00000011", false, 0.5)),
            Ok(Incoming::Transcript(Transcript {
                id: "00-00000011".into(),
                is_final: false,
                text: "merhaba".into(),
                end: 0.5,
                language: Some("tr".into()),
            }))
        );
        assert!(matches!(
            parse(&transcript("Merhaba.", "00-00000011", true, 0.9)),
            Ok(Incoming::Transcript(Transcript { is_final: true, .. }))
        ));
    }

    #[test]
    fn the_documented_quickstart_transcript_without_is_final_is_a_partial() {
        let text = r#"{"type":"transcript","session_id":"d","created_at":"2025-04-09T08:44:16.471Z",
            "data":{"id":"00_00000000","utterance":{"text":"Hello, I'm Sami.","start":0.188,
            "end":2.852,"language":"en","channel":0}}}"#;
        assert!(matches!(
            parse(text),
            Ok(Incoming::Transcript(Transcript {
                is_final: false,
                ..
            }))
        ));
    }

    #[test]
    fn a_transcript_without_a_body_is_ignored_and_a_sparse_one_parses() {
        assert_eq!(parse(r#"{"type":"transcript"}"#), Ok(Incoming::Ignored));
        assert_eq!(
            parse(r#"{"type":"transcript","data":"x"}"#),
            Ok(Incoming::Ignored)
        );
        assert!(matches!(
            parse(r#"{"type":"transcript","data":{"id":"x"}}"#),
            Ok(Incoming::Transcript(Transcript { ref text, .. })) if text.is_empty()
        ));
    }

    #[test]
    fn odd_field_shapes_never_end_the_call() {
        // text: null, a numeric text and a strange utterance read as blank, not as an error.
        for body in [
            r#"{"type":"transcript","data":{"id":"a","is_final":false,"utterance":{"text":null,"end":1.0}}}"#,
            r#"{"type":"transcript","data":{"id":"a","utterance":{"text":3}}}"#,
            r#"{"type":"transcript","data":{"id":"a","utterance":null}}"#,
            r#"{"type":"transcript","data":{"id":"a","utterance":[1,2]}}"#,
        ] {
            assert!(
                matches!(
                    parse(body),
                    Ok(Incoming::Transcript(Transcript { ref text, .. })) if text.is_empty()
                ),
                "{body}"
            );
        }
        // A numeric id is kept as its decimal text, a strange one is absent.
        assert!(matches!(
            parse(r#"{"type":"transcript","data":{"id":123,"is_final":true,"utterance":{"text":"x"}}}"#),
            Ok(Incoming::Transcript(Transcript { ref id, is_final: true, .. })) if id == "123"
        ));
        assert!(matches!(
            parse(r#"{"type":"transcript","data":{"id":{"a":1},"is_final":"yes","utterance":{"text":"x","end":"soon"}}}"#),
            Ok(Incoming::Transcript(Transcript { ref id, is_final: false, end, .. }))
                if id.is_empty() && end == 0.0
        ));
        // A type that is not a string is just a message the adapter does not know.
        for body in [
            r#"{"type":null}"#,
            r#"{"type":7,"data":{}}"#,
            r#"{"type":["transcript"]}"#,
        ] {
            assert_eq!(parse(body), Ok(Incoming::Ignored), "{body}");
        }
        // Odd acknowledgment and error objects.
        assert_eq!(
            parse(r#"{"type":"audio_chunk","acknowledged":"yes","error":5}"#),
            Ok(Incoming::Ignored)
        );
        assert_eq!(
            parse(r#"{"type":"error","error":"boom","data":7}"#),
            Ok(Incoming::Error {
                status: None,
                detail: "unknown".into()
            })
        );
    }

    #[test]
    fn speech_events() {
        let start = r#"{"type":"speech_start","data":{"time":1.24,"channel":0}}"#;
        let end = r#"{"type":"speech_end","data":{"time":3.1,"channel":0}}"#;
        assert_eq!(parse(start), Ok(Incoming::SpeechStart));
        assert_eq!(parse(end), Ok(Incoming::SpeechEnd));
    }

    #[test]
    fn acknowledgments_reuse_the_client_type_names() {
        let audio = r#"{"type":"audio_chunk","acknowledged":true,"error":null,
            "data":{"byte_range":[0,4095],"time_range":[0,0.256]}}"#;
        assert_eq!(
            parse(audio),
            Ok(Incoming::Ack {
                kind: AckKind::Audio,
                acknowledged: true,
                error: None
            })
        );
        let rejected = r#"{"type":"stop_recording","acknowledged":false,"error":
            {"status_code":409,"exception":"ConflictError","message":"Recording already stopped"}}"#;
        assert_eq!(
            parse(rejected),
            Ok(Incoming::Ack {
                kind: AckKind::Stop,
                acknowledged: false,
                error: Some("ConflictError (409)".into())
            })
        );
        // A client-shaped audio_chunk (no acknowledged flag) is not an acknowledgment.
        assert_eq!(
            parse(r#"{"type":"audio_chunk","data":{"chunk":"AAAA"}}"#),
            Ok(Incoming::Ignored)
        );
    }

    #[test]
    fn lifecycle_post_processing_and_unknown_messages() {
        for kind in ["start_session", "start_recording"] {
            assert_eq!(
                parse(&format!(r#"{{"type":"{kind}"}}"#)),
                Ok(Incoming::Ignored)
            );
        }
        for kind in ["end_session", "end_recording"] {
            assert_eq!(
                parse(&format!(
                    r#"{{"type":"{kind}","data":{{"reason":"user_request"}}}}"#
                )),
                Ok(Incoming::SessionEnded)
            );
        }
        for body in [
            r#"{"type":"post_transcript","data":{"full_transcript":"x"}}"#,
            r#"{"type":"post_final_transcript","data":{"metadata":{"audio_duration":1.0}}}"#,
            r#"{"type":"post_summarization","data":{"results":"x"}}"#,
            r#"{"type":"translation","data":{}}"#,
            r#"{"type":"brand_new","x":1}"#,
            r#"{"no_type":true}"#,
        ] {
            assert_eq!(parse(body), Ok(Incoming::Ignored), "{body}");
        }
    }

    #[test]
    fn error_messages_never_echo_free_text() {
        let a = r#"{"type":"error","error":{"status_code":"503","exception":"Unavailable",
            "message":"secret text"}}"#;
        let b = r#"{"type":"error","data":{"status_code":400,"exception":"Invalid Argument!",
            "message":"Unsupported sample rate"}}"#;
        assert_eq!(
            parse(a),
            Ok(Incoming::Error {
                status: Some(503),
                detail: "Unavailable (503)".into()
            })
        );
        assert_eq!(
            parse(b),
            Ok(Incoming::Error {
                status: Some(400),
                detail: "InvalidArgument (400)".into()
            })
        );
        assert_eq!(
            parse(r#"{"type":"error"}"#),
            Ok(Incoming::Error {
                status: None,
                detail: "unknown".into()
            })
        );
    }

    #[test]
    fn garbage_is_an_error_without_content() {
        for text in ["<<not json>>", "", "[1,2]", "42", "\"transcript\"", "null"] {
            let err = parse(text).unwrap_err();
            assert!(!err.0.contains("not json"));
        }
    }
}
