//! Deepgram Aura (`/v1/speak`) wire format: client messages, server messages, the query string,
//! voice validation and text segmentation. Pure functions and types, no I/O
//! (docs/providers/deepgram-aura.md).

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{OutputEncoding, PROVIDER};
use crate::error::Error;

/// Languages with an Aura-2 voice (docs/providers/deepgram-aura.md, fetched 2026-10-08).
/// Turkish is NOT among them; neither is any other language not listed here.
pub const SUPPORTED_LANGUAGES: [&str; 7] = ["en", "es", "de", "fr", "nl", "it", "ja"];

/// Whether Deepgram Aura has a voice for the language tag (`"en"`, `"es-419"`, `"tr-TR"`;
/// matched case-insensitively on the primary subtag). Turkish is not supported as of
/// 2026-10-08.
pub fn supports_language(tag: &str) -> bool {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    SUPPORTED_LANGUAGES.contains(&primary.as_str())
}

/// The `{"type":"Flush"}` client message.
pub(super) const FLUSH: &str = r#"{"type":"Flush"}"#;
/// The `{"type":"Clear"}` client message.
pub(super) const CLEAR: &str = r#"{"type":"Clear"}"#;
/// The `{"type":"Close"}` client message.
pub(super) const CLOSE: &str = r#"{"type":"Close"}"#;

#[derive(Serialize)]
struct Speak<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

/// Encodes a `Speak` message. The text is sent verbatim (spaces included): whether the server
/// puts whitespace between two `Speak`s is UNVERIFIED for Aura.
pub(super) fn encode_speak(text: &str) -> Result<String, Error> {
    serde_json::to_string(&Speak {
        kind: "Speak",
        text,
    })
    .map_err(|e| Error::Protocol {
        provider: PROVIDER,
        detail: format!("cannot encode Speak: {e}"),
    })
}

/// The fixed settings of one connection: Aura cannot change voice or output format after the
/// upgrade, so these identify a socket.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SessionParams {
    /// Model string (`aura-2-thalia-en`): voice and language together.
    pub(super) model: String,
    pub(super) encoding: OutputEncoding,
    pub(super) sample_rate: u32,
    pub(super) speed: Option<f32>,
}

impl SessionParams {
    /// The query string (without the leading `?`). Only `model`, `encoding`, `sample_rate` and
    /// `speed` are sent: `container` is not a documented parameter of the streaming endpoint.
    pub(super) fn query(&self) -> String {
        let mut query = format!(
            "model={}&encoding={}&sample_rate={}",
            self.model,
            self.encoding.wire_name(),
            self.sample_rate
        );
        if let Some(speed) = self.speed {
            let _ = write!(query, "&speed={speed}");
        }
        query
    }
}

/// Checks that a model string can go into the query unescaped and is not a Turkish (or other
/// unsupported-language) voice.
///
/// # Errors
/// [`Error::Protocol`] describing what is wrong; never contains credentials.
pub(super) fn validate_voice(voice: &str) -> Result<(), Error> {
    let invalid = |detail: String| Error::Protocol {
        provider: PROVIDER,
        detail: format!("invalid session configuration: {detail}"),
    };
    if voice.is_empty() {
        return Err(invalid("empty voice (model) id".to_owned()));
    }
    if voice.len() > 64
        || !voice
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(invalid(
            "the voice (model) id may only contain letters, digits, '-', '_' and '.'".to_owned(),
        ));
    }
    // Aura model strings end in the language: `aura-2-thalia-en`.
    // The suffix is compared case-insensitively: `aura-2-ozge-TR` must not slip through.
    // A locale-style `tr_TR` is covered too (the last `-` or `_` separated part).
    let suffix = voice
        .rsplit(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let suffix = suffix.as_str();
    let looks_like_language = suffix.len() == 2 && suffix.chars().all(|c| c.is_ascii_lowercase());
    if voice.contains(['-', '_']) && looks_like_language && !supports_language(suffix) {
        let detail = if suffix == "tr" {
            "Deepgram Aura has no Turkish voice (not supported as of 2026-10-08); use another \
             TTS provider for Turkish calls"
                .to_owned()
        } else {
            format!(
                "Deepgram Aura has no voice for language {suffix:?} (supported: {})",
                SUPPORTED_LANGUAGES.join(", ")
            )
        };
        return Err(invalid(detail));
    }
    Ok(())
}

/// One piece of text for a `Speak` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Segment {
    pub(super) text: String,
    /// Send a `Flush` after this segment (the last one of an input, under
    /// `FlushPolicy::PerText`).
    pub(super) flush_after: bool,
}

/// Splits `text` into pieces of at most `max_chars` characters, preferably after a sentence end
/// followed by white space, else after any white space, else mid-word. The pieces concatenate
/// to exactly `text` (spaces stay where they were).
pub(super) fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    let max = max_chars.max(1);
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let Some((limit, _)) = rest.char_indices().nth(max) else {
            pieces.push(rest.to_owned());
            break;
        };
        let window = rest.get(..limit).unwrap_or(rest);
        let cut = best_cut(window).unwrap_or(limit);
        let Some((head, tail)) = rest.split_at_checked(cut) else {
            pieces.push(rest.to_owned());
            break;
        };
        pieces.push(head.to_owned());
        rest = tail;
    }
    pieces
}

/// A byte offset in `window` (after which a piece may end), or `None`. Sentence ends win over
/// plain white space, unless they would leave a tiny piece.
fn best_cut(window: &str) -> Option<usize> {
    let mut sentence = None;
    let mut space = None;
    let mut previous: Option<(usize, char)> = None;
    for (index, c) in window.char_indices() {
        if c.is_whitespace() {
            let end = index.saturating_add(c.len_utf8());
            space = Some(end);
            if previous.is_some_and(|(_, p)| matches!(p, '.' | '!' | '?' | '\u{2026}' | '\u{3002}'))
                || c == '\n'
            {
                sentence = Some(end);
            }
        }
        previous = Some((index, c));
    }
    let floor = window.len() / 4;
    sentence
        .filter(|cut| *cut > floor)
        .or(space)
        .filter(|cut| *cut > 0)
}

/// What the server sent as JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Incoming {
    /// `Metadata` (sent on open per the migration page; timing UNVERIFIED).
    Metadata { request_id: String },
    /// `Flushed`: everything sent before the matching `Flush` has been synthesized.
    Flushed,
    /// `Cleared`: the reply to `Clear`, sent after the remaining audio.
    Cleared,
    /// `Warning`: not an error, the session goes on.
    Warning { code: String, description: String },
    /// An `Error` message. Not in the v1 schema; handled defensively.
    Error { code: String, description: String },
    /// Any other (unknown) message type.
    Other(String),
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    code: Option<Value>,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    request_id: Option<Value>,
}

/// A bounded printable form of a JSON value for logs and errors (never credentials: the
/// server does not echo them).
fn show(value: Option<Value>) -> String {
    let text = match value {
        Some(Value::String(s)) => s,
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

impl Incoming {
    /// Parses a text frame.
    ///
    /// # Errors
    /// [`Error::Protocol`] if the frame is not a JSON object.
    pub(super) fn parse(text: &str) -> Result<Self, Error> {
        let envelope: Envelope = serde_json::from_str(text).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "unparsable message from the server".to_owned(),
        })?;
        let kind = envelope.kind.unwrap_or_default();
        Ok(match kind.as_str() {
            "Metadata" => Self::Metadata {
                request_id: show(envelope.request_id),
            },
            "Flushed" => Self::Flushed,
            "Cleared" => Self::Cleared,
            "Warning" => Self::Warning {
                code: show(envelope.code),
                description: show(envelope.description),
            },
            "Error" => Self::Error {
                code: show(envelope.code),
                description: show(envelope.description),
            },
            _ => Self::Other(kind.chars().take(32).collect()),
        })
    }
}

/// Maps a WebSocket close frame. `1003`, `1008` and `1009` are protocol violations of ours
/// (unsupported message, unknown command or too many characters, too large); everything else is
/// a disconnect. The reason is the server's error code string (`DATA-0001`), shown bounded.
pub(super) fn close_error(code: u16, reason: &str) -> Error {
    match code {
        1003 | 1008 | 1009 => {
            let reason: String = reason
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .take(64)
                .collect();
            Error::Protocol {
                provider: PROVIDER,
                detail: format!("server closed the connection with {code} {reason}")
                    .trim_end()
                    .to_owned(),
            }
        }
        _ => Error::Disconnected { provider: PROVIDER },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speak_is_encoded_with_the_text_verbatim() {
        let json = encode_speak(" héllo \"world\"\n").unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "Speak");
        assert_eq!(value["text"], " héllo \"world\"\n");
    }

    #[test]
    fn the_query_has_only_the_documented_parameters() {
        let params = SessionParams {
            model: "aura-2-thalia-en".to_owned(),
            encoding: OutputEncoding::Pcm16,
            sample_rate: 24_000,
            speed: None,
        };
        assert_eq!(
            params.query(),
            "model=aura-2-thalia-en&encoding=linear16&sample_rate=24000"
        );
        let params = SessionParams {
            speed: Some(1.2),
            encoding: OutputEncoding::Mulaw,
            sample_rate: 8000,
            ..params
        };
        assert_eq!(
            params.query(),
            "model=aura-2-thalia-en&encoding=mulaw&sample_rate=8000&speed=1.2"
        );
    }

    #[test]
    fn turkish_and_unlisted_languages_are_refused_while_listed_ones_pass() {
        assert!(validate_voice("aura-2-thalia-en").is_ok());
        assert!(validate_voice("aura-2-javier-es").is_ok());
        assert!(validate_voice("aura-asteria-en").is_ok());
        assert!(validate_voice("contract").is_ok());
        let tr = validate_voice("aura-2-ozge-tr").unwrap_err().to_string();
        assert!(tr.contains("Turkish"), "{tr}");
        let pt = validate_voice("aura-2-x-pt").unwrap_err().to_string();
        assert!(pt.contains("\"pt\""), "{pt}");
        // The language suffix is matched case-insensitively and in locale form.
        for voice in [
            "aura-2-ozge-TR",
            "aura-2-ozge-Tr",
            "aura-2-ozge-tR",
            "tr_TR",
        ] {
            let e = validate_voice(voice).unwrap_err().to_string();
            assert!(e.contains("Turkish"), "{voice}: {e}");
        }
        assert!(validate_voice("aura-2-x-PT").is_err());
        // Every listed language is accepted, in either case.
        for lang in SUPPORTED_LANGUAGES {
            assert!(
                validate_voice(&format!("aura-2-x-{lang}")).is_ok(),
                "{lang}"
            );
            let upper = lang.to_ascii_uppercase();
            assert!(
                validate_voice(&format!("aura-2-x-{upper}")).is_ok(),
                "{upper}"
            );
        }
        assert!(validate_voice("aura-2-agathe-fr").is_ok());
        assert!(validate_voice("aura-2-lars-nl").is_ok());
        assert!(validate_voice("aura-2-cesare-it").is_ok());
        assert!(validate_voice("").is_err());
        assert!(validate_voice("a&b=c").is_err());
        assert!(validate_voice("a b").is_err());
        assert!(supports_language("EN"));
        assert!(supports_language("es-419"));
        assert!(!supports_language("tr"));
        assert!(!supports_language("tr-TR"));
    }

    #[test]
    fn text_is_split_at_sentences_then_spaces_and_concatenates_back() {
        let text = "One two three. Four five six! Seven eight nine ten eleven twelve";
        for max in [5, 10, 15, 20, 30, 1000] {
            let pieces = split_text(text, max);
            assert_eq!(pieces.concat(), text, "max {max}");
            assert!(
                pieces.iter().all(|p| p.chars().count() <= max),
                "max {max}: {pieces:?}"
            );
        }
        let pieces = split_text(text, 20);
        assert_eq!(pieces.first().map(String::as_str), Some("One two three. "));
        // A single over-long word is cut mid-word, multi-byte characters stay whole.
        let pieces = split_text("ééééééééé", 4);
        assert_eq!(pieces, ["éééé", "éééé", "é"]);
        assert!(split_text("", 10).is_empty());
        assert_eq!(split_text(" a", 10), [" a"]);
    }

    #[test]
    fn server_messages_are_parsed_leniently() {
        assert_eq!(
            Incoming::parse(r#"{"type":"Flushed","sequence_id":0}"#).unwrap(),
            Incoming::Flushed
        );
        assert_eq!(
            Incoming::parse(r#"{"type":"Cleared","sequence_id":7}"#).unwrap(),
            Incoming::Cleared
        );
        assert_eq!(
            Incoming::parse(r#"{"type":"Metadata","request_id":"abc","model_name":"x"}"#).unwrap(),
            Incoming::Metadata {
                request_id: "abc".to_owned()
            }
        );
        assert_eq!(
            Incoming::parse(r#"{"type":"Warning","description":"slow down","code":42}"#).unwrap(),
            Incoming::Warning {
                code: "42".to_owned(),
                description: "slow down".to_owned()
            }
        );
        assert_eq!(
            Incoming::parse(r#"{"type":"Brand New","x":1}"#).unwrap(),
            Incoming::Other("Brand New".to_owned())
        );
        assert_eq!(
            Incoming::parse("{}").unwrap(),
            Incoming::Other(String::new())
        );
        assert!(Incoming::parse("{not json").is_err());
        assert!(Incoming::parse("[1]").is_err());
    }

    #[test]
    fn close_codes_map_to_protocol_or_disconnect() {
        let e = close_error(1008, "DATA-0001");
        assert!(
            matches!(&e, Error::Protocol { detail, .. } if detail.contains("1008 DATA-0001")),
            "{e}"
        );
        assert!(matches!(close_error(1009, ""), Error::Protocol { .. }));
        assert!(matches!(
            close_error(1003, "MESSAGE-0000"),
            Error::Protocol { .. }
        ));
        assert!(matches!(
            close_error(1011, "NET-0000"),
            Error::Disconnected { .. }
        ));
        assert!(matches!(close_error(1000, ""), Error::Disconnected { .. }));
    }
}
