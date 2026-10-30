//! Wire messages of the AssemblyAI streaming API v3 (see `docs/providers/assemblyai.md`).
//!
//! Client control messages are fixed JSON texts (audio goes as binary frames). Server messages
//! are JSON text frames discriminated by `type`; only the fields the adapter uses are parsed and
//! unknown fields and message types are ignored, so AssemblyAI can extend its protocol without
//! breaking sessions.

use serde::Deserialize;

/// `{"type":"ForceEndpoint"}`: ends the current turn now (brisvox `Finalize`).
pub(crate) const FORCE_ENDPOINT: &str = r#"{"type":"ForceEndpoint"}"#;
/// `{"type":"Terminate"}`: the server flushes, sends `Termination` and closes.
pub(crate) const TERMINATE: &str = r#"{"type":"Terminate"}"#;
/// `{"type":"KeepAlive"}`: resets the `inactivity_timeout` timer.
pub(crate) const KEEP_ALIVE: &str = r#"{"type":"KeepAlive"}"#;

/// One `Turn` message, reduced to what the adapter needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Turn {
    /// `turn_order`: increases monotonically within a connection.
    pub(crate) order: u64,
    /// `end_of_turn`: this is the final message of the turn.
    pub(crate) end_of_turn: bool,
    /// `turn_is_formatted`.
    pub(crate) formatted: bool,
    /// The whole turn so far, trimmed (each `Turn` replaces the previous one of its order).
    pub(crate) transcript: String,
    /// End of the last word, in milliseconds of audio on this connection; `None` when the
    /// message has no words.
    pub(crate) last_word_end_ms: Option<f64>,
    /// `language_code` and `language_confidence`: only present on a finished turn of a
    /// connection opened with `language_detection=true`.
    pub(crate) language: Option<crate::stt::SpokenLanguage>,
}

/// A parsed server message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    /// `Begin`: the session is open.
    Begin {
        /// `configuration.model`, when present (checked against the requested model).
        model: Option<String>,
    },
    /// `Turn`: a partial or final transcript of the current turn.
    Turn(Turn),
    /// `Heartbeat` (only with `session_heartbeat`): the connection is alive.
    Heartbeat,
    /// `Termination`: always the last message; the server closes after it.
    Termination,
    /// `Error`: text frame before the socket closes. Its field names are UNVERIFIED, so the
    /// description is whatever string field of the usual names is there.
    Error {
        /// Human-readable description (truncated), possibly empty.
        description: String,
    },
    /// `SpeechStarted`, `SpeakerRevision`, `LLMGatewayResponse` and message types the adapter
    /// does not know: nothing to do.
    Ignored,
}

/// Why a text frame was not a valid server message. Carries no message content: transcripts are
/// user data and must not end up in errors or logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParseError(pub(crate) &'static str);

#[derive(Deserialize)]
struct Peek {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawTurn {
    #[serde(default)]
    turn_order: u64,
    end_of_turn: bool,
    #[serde(default)]
    turn_is_formatted: bool,
    transcript: String,
    #[serde(default)]
    words: Option<Vec<RawWord>>,
    #[serde(default)]
    language_code: Option<String>,
    #[serde(default)]
    language_confidence: Option<f64>,
}

#[derive(Deserialize)]
struct RawWord {
    #[serde(default)]
    end: Option<f64>,
}

#[derive(Deserialize)]
struct RawBegin {
    #[serde(default)]
    configuration: Option<RawConfiguration>,
}

#[derive(Deserialize)]
struct RawConfiguration {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Longest `description` kept from an `Error` message.
const MAX_DESCRIPTION: usize = 200;

/// Parses one text frame.
///
/// A JSON object without `type` is ignored (forward compatibility); anything that is not a JSON
/// object, and a `Turn` without `transcript` or `end_of_turn`, is an error.
pub(crate) fn parse(text: &str) -> Result<Incoming, ParseError> {
    // serde would read a JSON array into the struct too.
    if !text.trim_start().starts_with('{') {
        return Err(ParseError("not a JSON object with a type"));
    }
    let peek: Peek =
        serde_json::from_str(text).map_err(|_| ParseError("not a JSON object with a type"))?;
    match peek.kind.as_deref() {
        Some("Turn") => {
            let raw: RawTurn =
                serde_json::from_str(text).map_err(|_| ParseError("malformed Turn message"))?;
            let last_word_end_ms = raw
                .words
                .iter()
                .flatten()
                .filter_map(|w| w.end)
                .filter(|e| e.is_finite() && *e >= 0.0)
                .reduce(f64::max);
            Ok(Incoming::Turn(Turn {
                order: raw.turn_order,
                end_of_turn: raw.end_of_turn,
                formatted: raw.turn_is_formatted,
                transcript: raw.transcript.trim().to_owned(),
                last_word_end_ms,
                language: raw
                    .language_code
                    .as_deref()
                    .filter(|code| !code.trim().is_empty())
                    .map(|code| {
                        let language = crate::stt::SpokenLanguage::new(code.trim());
                        match raw.language_confidence {
                            #[allow(clippy::cast_possible_truncation)] // a probability
                            Some(confidence) => language.with_confidence(confidence as f32),
                            None => language,
                        }
                    }),
            }))
        }
        Some("Begin") => {
            let raw: RawBegin =
                serde_json::from_str(text).map_err(|_| ParseError("malformed Begin message"))?;
            Ok(Incoming::Begin {
                model: raw.configuration.and_then(|c| c.model),
            })
        }
        Some("Heartbeat") => Ok(Incoming::Heartbeat),
        Some("Termination") => Ok(Incoming::Termination),
        Some("Error") => {
            let raw: RawError =
                serde_json::from_str(text).map_err(|_| ParseError("malformed Error message"))?;
            let mut description = raw
                .error
                .or(raw.message)
                .or(raw.error_message)
                .or(raw.reason)
                .unwrap_or_default();
            if let Some((cut, _)) = description.char_indices().nth(MAX_DESCRIPTION) {
                description.truncate(cut);
            }
            Ok(Incoming::Error { description })
        }
        Some(_) | None => Ok(Incoming::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str) -> Turn {
        match parse(text) {
            Ok(Incoming::Turn(t)) => t,
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn the_documented_final_turn_parses() {
        let t = turn(
            r#"{"type":"Turn","turn_order":0,"turn_is_formatted":true,"end_of_turn":true,
            "transcript":" Hello world. ","end_of_turn_confidence":1,
            "words":[{"text":"Hello","start":0,"end":500,"confidence":0.99,"word_is_final":true},
                     {"text":"world.","start":500,"end":1000,"confidence":0.98}]}"#,
        );
        assert_eq!(t.order, 0);
        assert!(t.end_of_turn && t.formatted);
        assert_eq!(t.transcript, "Hello world.");
        assert_eq!(t.last_word_end_ms, Some(1000.0));
    }

    #[test]
    fn the_documented_partial_turn_without_words_parses() {
        let t = turn(
            r#"{"type":"Turn","turn_order":3,"end_of_turn":false,"turn_is_formatted":false,
            "transcript":"my number is five five five","end_of_turn_confidence":0.0}"#,
        );
        assert_eq!(t.order, 3);
        assert!(!t.end_of_turn && !t.formatted);
        assert_eq!(t.last_word_end_ms, None);
    }

    #[test]
    fn extra_turn_fields_and_null_words_are_tolerated() {
        let t = turn(
            r#"{"type":"Turn","end_of_turn":true,"transcript":"x","utterance":"x",
            "language_code":"tr","language_confidence":0.9,"speaker_label":"A","words":null}"#,
        );
        assert_eq!(t.order, 0);
        assert_eq!(t.last_word_end_ms, None);
    }

    #[test]
    fn begin_heartbeat_termination_and_unknown_types_are_distinguished() {
        assert_eq!(
            parse(
                r#"{"type":"Begin","id":"x","expires_at":1772570132,
                "configuration":{"model":"universal-3-6-pro","mode":"balanced"}}"#
            ),
            Ok(Incoming::Begin {
                model: Some("universal-3-6-pro".to_owned())
            })
        );
        assert_eq!(
            parse(r#"{"type":"Begin","id":"x"}"#),
            Ok(Incoming::Begin { model: None })
        );
        assert_eq!(
            parse(r#"{"type":"Heartbeat","total_audio_received_ms":45000,"realtime_factor":1.0}"#),
            Ok(Incoming::Heartbeat)
        );
        assert_eq!(
            parse(
                r#"{"type":"Termination","audio_duration_seconds":45,"session_duration_seconds":47}"#
            ),
            Ok(Incoming::Termination)
        );
        for ignored in [
            r#"{"type":"SpeechStarted","timestamp":1216,"confidence":0.98}"#,
            r#"{"type":"SpeakerRevision","turn_order":1}"#,
            r#"{"type":"LLMGatewayResponse","turn_order":1,"transcript":"x","data":{}}"#,
            r#"{"type":"SomethingNew","a":1}"#,
            r#"{"no":"type"}"#,
        ] {
            assert_eq!(parse(ignored), Ok(Incoming::Ignored), "{ignored}");
        }
    }

    #[test]
    fn error_messages_read_whichever_description_field_is_there() {
        assert_eq!(
            parse(r#"{"type":"Error","error_code":3006,"error":"Invalid JSON"}"#),
            Ok(Incoming::Error {
                description: "Invalid JSON".to_owned()
            })
        );
        assert_eq!(
            parse(r#"{"type":"Error","message":"boom"}"#),
            Ok(Incoming::Error {
                description: "boom".to_owned()
            })
        );
        assert_eq!(
            parse(r#"{"type":"Error"}"#),
            Ok(Incoming::Error {
                description: String::new()
            })
        );
    }

    #[test]
    fn long_error_descriptions_are_truncated_on_a_char_boundary() {
        let long = "ü".repeat(500);
        let text = format!(r#"{{"type":"Error","error":"{long}"}}"#);
        match parse(&text) {
            Ok(Incoming::Error { description }) => {
                assert_eq!(description.chars().count(), MAX_DESCRIPTION);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn garbage_and_malformed_turns_are_errors_without_content() {
        for bad in [
            "not json",
            "[]",
            "",
            r#"{"type":"Turn"}"#,
            r#"{"type":"Turn","transcript":"x"}"#,
            r#"{"type":"Turn","end_of_turn":true}"#,
        ] {
            let err = parse(bad).unwrap_err();
            assert!(!err.0.contains("not json"));
        }
    }

    #[test]
    fn the_last_word_end_is_the_largest_finite_non_negative_one() {
        let t = turn(
            r#"{"type":"Turn","end_of_turn":true,"transcript":"x",
            "words":[{"end":300},{"end":900.5},{"end":-5},{"start":1}]}"#,
        );
        assert_eq!(t.last_word_end_ms, Some(900.5));
    }
}
