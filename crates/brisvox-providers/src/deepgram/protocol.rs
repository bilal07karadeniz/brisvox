//! Wire messages of the Deepgram live API (see `docs/providers/deepgram.md`).
//!
//! Client control messages are fixed JSON texts. Server messages are JSON text frames
//! discriminated by `type`; only the fields the adapter uses are parsed and unknown fields and
//! message types are ignored, so Deepgram can extend its protocol without breaking sessions.

use serde::Deserialize;

/// `{"type":"KeepAlive"}` (text frame; no server response).
pub(crate) const KEEP_ALIVE: &str = r#"{"type":"KeepAlive"}"#;
/// `{"type":"Finalize"}` (text frame; flushes unprocessed audio as final results).
pub(crate) const FINALIZE: &str = r#"{"type":"Finalize"}"#;
/// `{"type":"CloseStream"}` (text frame; the server flushes, sends `Metadata` and closes).
pub(crate) const CLOSE_STREAM: &str = r#"{"type":"CloseStream"}"#;

/// One `Results` message, reduced to what the adapter needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Results {
    /// Transcript of the first alternative (empty when there is none or it is blank).
    pub(crate) transcript: String,
    /// The audio segment is fixed.
    pub(crate) is_final: bool,
    /// Endpointing saw a pause.
    pub(crate) speech_final: bool,
    /// Produced by a `Finalize`.
    pub(crate) from_finalize: bool,
    /// Start of the segment in seconds of audio sent on this connection.
    pub(crate) start: f64,
    /// Duration of the segment in seconds.
    pub(crate) duration: f64,
    /// The language Deepgram reports for the segment with `language=multi`: the one most of the
    /// first alternative's words are in (`words[].language`), else the first of its `languages`
    /// (sorted by descending word count). Lowercase primary code. `None` when it reports none
    /// (a monolingual `language`).
    pub(crate) language: Option<std::sync::Arc<str>>,
}

/// A parsed server message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    /// Transcript data.
    Results(Results),
    /// `UtteranceEnd`: a gap of `utterance_end_ms` followed the last finalized word.
    UtteranceEnd {
        /// `-1` means the result was already finalized before the gap: ignore.
        last_word_end: f64,
    },
    /// `Error`: a rejected or malformed client message.
    Error {
        /// `variant` (e.g. `SchemaError`).
        variant: String,
        /// Human-readable description (truncated).
        description: String,
    },
    /// `Metadata` (summary after `CloseStream`), `SpeechStarted` and message types the adapter
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
struct RawResults {
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    speech_final: bool,
    #[serde(default)]
    from_finalize: bool,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    duration: f64,
    channel: RawChannel,
}

#[derive(Deserialize)]
struct RawChannel {
    #[serde(default)]
    alternatives: Vec<RawAlternative>,
}

#[derive(Deserialize)]
struct RawAlternative {
    #[serde(default)]
    transcript: String,
}

/// The language fields of a `language=multi` result, borrowed from the message text (a word
/// that has none is skipped; nothing here allocates per word).
#[derive(Deserialize)]
struct RawLanguageChannel<'a> {
    #[serde(borrow, default)]
    alternatives: Vec<RawLanguageAlternative<'a>>,
}

#[derive(Deserialize)]
struct RawLanguageAlternative<'a> {
    #[serde(borrow, default)]
    words: Vec<RawWord<'a>>,
    #[serde(borrow, default)]
    languages: Vec<std::borrow::Cow<'a, str>>,
}

#[derive(Deserialize)]
struct RawWord<'a> {
    #[serde(borrow, default)]
    language: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Deserialize)]
struct RawMultiResults<'a> {
    #[serde(borrow)]
    channel: RawLanguageChannel<'a>,
}

#[derive(Deserialize)]
struct RawUtteranceEnd {
    #[serde(default)]
    last_word_end: Option<f64>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    variant: String,
    #[serde(default)]
    description: String,
}

/// Longest `description` kept from an `Error` message.
const MAX_DESCRIPTION: usize = 200;

/// Parses one text frame.
///
/// A missing `type` is treated as `Results`: the documented interim example omits it.
#[cfg(test)]
pub(crate) fn parse(text: &str) -> Result<Incoming, ParseError> {
    parse_with(text, true)
}

/// [`parse`], reading the language fields of the results only when `multi` (a `language=multi`
/// session): a monolingual session parses and allocates nothing for them.
pub(crate) fn parse_with(text: &str, multi: bool) -> Result<Incoming, ParseError> {
    let peek: Peek =
        serde_json::from_str(text).map_err(|_| ParseError("not a JSON object with a type"))?;
    match peek.kind.as_deref() {
        None | Some("Results") => {
            let raw: RawResults =
                serde_json::from_str(text).map_err(|_| ParseError("malformed Results message"))?;
            let transcript = raw.alternatives_first().trim().to_owned();
            Ok(Incoming::Results(Results {
                transcript,
                is_final: raw.is_final,
                speech_final: raw.speech_final,
                from_finalize: raw.from_finalize,
                start: finite_non_negative(raw.start),
                duration: finite_non_negative(raw.duration),
                language: if multi { language_of(text) } else { None },
            }))
        }
        Some("UtteranceEnd") => {
            let raw: RawUtteranceEnd = serde_json::from_str(text)
                .map_err(|_| ParseError("malformed UtteranceEnd message"))?;
            Ok(Incoming::UtteranceEnd {
                last_word_end: raw.last_word_end.unwrap_or(-1.0),
            })
        }
        Some("Error") => {
            let raw: RawError =
                serde_json::from_str(text).map_err(|_| ParseError("malformed Error message"))?;
            let mut description = raw.description;
            if let Some((cut, _)) = description.char_indices().nth(MAX_DESCRIPTION) {
                description.truncate(cut);
            }
            Ok(Incoming::Error {
                variant: raw.variant,
                description,
            })
        }
        Some(_) => Ok(Incoming::Ignored),
    }
}

impl RawResults {
    fn alternatives_first(&self) -> &str {
        self.channel
            .alternatives
            .first()
            .map_or("", |a| a.transcript.as_str())
    }
}

/// The primary subtag of a language code (`en-GB` is `en`), borrowed.
fn primary_code(code: &str) -> &str {
    code.split(['-', '_']).next().unwrap_or("").trim()
}

/// The most distinct languages counted for one segment; a segment mixes two in practice.
const MAX_SEGMENT_LANGUAGES: usize = 4;

/// The language a `language=multi` result reports for its segment (see [`Results::language`]):
/// the one most words of the first alternative are in, else the first of its `languages`.
/// Reads the message a second time, borrowing its strings: the only allocation is the answer.
fn language_of(text: &str) -> Option<std::sync::Arc<str>> {
    let raw: RawMultiResults<'_> = serde_json::from_str(text).ok()?;
    let alternative = raw.channel.alternatives.first()?;
    let mut seen: [(&str, usize); MAX_SEGMENT_LANGUAGES] = [("", 0); MAX_SEGMENT_LANGUAGES];
    let mut used = 0;
    for code in alternative
        .words
        .iter()
        .filter_map(|word| word.language.as_deref())
    {
        let code = primary_code(code);
        if code.is_empty() {
            continue;
        }
        if let Some((_, count)) = seen
            .iter_mut()
            .take(used)
            .find(|(known, _)| known.eq_ignore_ascii_case(code))
        {
            *count += 1;
        } else if let Some(slot) = seen.get_mut(used) {
            *slot = (code, 1);
            used += 1;
        }
    }
    let mut best: Option<&(&str, usize)> = None;
    for entry in seen.iter().take(used) {
        if best.is_none_or(|b| entry.1 > b.1) {
            best = Some(entry);
        }
    }
    let code = match best {
        Some((code, _)) => *code,
        None => primary_code(alternative.languages.first()?),
    };
    (!code.is_empty()).then(|| std::sync::Arc::from(code.to_ascii_lowercase().as_str()))
}

fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(text: &str) -> Results {
        match parse(text) {
            Ok(Incoming::Results(r)) => r,
            other => panic!("expected Results, got {other:?}"),
        }
    }

    #[test]
    fn the_language_of_a_multi_result_is_the_majority_of_its_words() {
        // `language=multi`: every word carries its language (Deepgram's multilingual page).
        let r = results(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":
            "bana order status göster","languages":["tr","en"],"words":[
            {"word":"bana","language":"tr"},{"word":"order","language":"en"},
            {"word":"status","language":"en-GB"},{"word":"göster","language":"tr"},
            {"word":"lütfen","language":"tr"}]}]}}"#,
        );
        assert_eq!(r.language.as_deref(), Some("tr"));
        let r = results(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":
            "where is my order","words":[{"word":"where","language":"EN"},
            {"word":"is","language":"en"},{"word":"my"},{"word":"order","language":"tr"}]}]}}"#,
        );
        assert_eq!(r.language.as_deref(), Some("en"));
    }

    #[test]
    fn without_word_languages_the_segment_languages_list_decides() {
        let r = results(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":
            "merhaba","languages":["tr","en"],"languages_hinted":["en","tr"],"words":[]}]}}"#,
        );
        assert_eq!(r.language.as_deref(), Some("tr"));
    }

    #[test]
    fn a_monolingual_session_does_not_read_languages_at_all() {
        let text = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":
            "hello","words":[{"word":"hello","language":"en"}]}]}}"#;
        let Ok(Incoming::Results(r)) = parse_with(text, false) else {
            panic!("Results expected");
        };
        assert_eq!(r.language, None);
        let Ok(Incoming::Results(r)) = parse_with(text, true) else {
            panic!("Results expected");
        };
        assert_eq!(r.language.as_deref(), Some("en"));
    }

    #[test]
    fn a_monolingual_result_reports_no_language() {
        let r = results(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"hello",
            "words":[{"word":"hello","confidence":0.9}]}]}}"#,
        );
        assert_eq!(r.language, None);
    }

    #[test]
    fn the_documented_interim_example_without_a_type_parses_as_results() {
        let r = results(
            r#"{"channel_index":[0,1],"duration":1.039875,"start":0,"is_final":false,
            "channel":{"alternatives":[{"transcript":"another big","confidence":0.96,
            "words":[{"word":"another","start":0.29,"end":0.79,"confidence":0.95}]}]}}"#,
        );
        assert_eq!(r.transcript, "another big");
        assert!(!r.is_final && !r.speech_final && !r.from_finalize);
        assert!((r.duration - 1.039_875).abs() < 1e-9);
    }

    #[test]
    fn results_flags_and_timing_are_read() {
        let r = results(
            r#"{"type":"Results","is_final":true,"speech_final":true,"from_finalize":true,
            "start":1.5,"duration":2.0,"channel_index":[0,1],"metadata":{"request_id":"x"},
            "channel":{"alternatives":[{"transcript":"  merhaba dünya "}]}}"#,
        );
        assert_eq!(r.transcript, "merhaba dünya");
        assert!(r.is_final && r.speech_final && r.from_finalize);
        assert!((r.start - 1.5).abs() < 1e-9 && (r.duration - 2.0).abs() < 1e-9);
    }

    #[test]
    fn an_empty_alternatives_list_is_an_empty_transcript() {
        let r = results(r#"{"type":"Results","is_final":true,"channel":{"alternatives":[]}}"#);
        assert_eq!(r.transcript, "");
    }

    #[test]
    fn utterance_end_error_and_unknown_types_are_distinguished() {
        assert_eq!(
            parse(r#"{"channel":[0,1],"last_word_end":2.395,"type":"UtteranceEnd"}"#),
            Ok(Incoming::UtteranceEnd {
                last_word_end: 2.395
            })
        );
        assert_eq!(
            parse(r#"{"type":"UtteranceEnd","last_word_end":-1}"#),
            Ok(Incoming::UtteranceEnd {
                last_word_end: -1.0
            })
        );
        assert_eq!(
            parse(r#"{"type":"Error","variant":"SchemaError","description":"bad","message":"x"}"#),
            Ok(Incoming::Error {
                variant: "SchemaError".to_owned(),
                description: "bad".to_owned()
            })
        );
        for ignored in [
            r#"{"type":"SpeechStarted","channel":[0,1],"timestamp":9.54}"#,
            r#"{"type":"Metadata","request_id":"r","duration":0.0,"channels":0}"#,
            r#"{"type":"SomethingNew","a":1}"#,
        ] {
            assert_eq!(parse(ignored), Ok(Incoming::Ignored), "{ignored}");
        }
    }

    #[test]
    fn garbage_and_malformed_results_are_errors_without_content() {
        for bad in [
            "not json",
            "[]",
            "",
            r#"{"type":"Results"}"#,
            r#"{"is_final":true}"#,
        ] {
            let err = parse(bad).unwrap_err();
            assert!(!err.0.contains("not json"));
        }
    }

    #[test]
    fn long_error_descriptions_are_truncated_on_a_char_boundary() {
        let long = "ü".repeat(500);
        let text = format!(r#"{{"type":"Error","variant":"V","description":"{long}"}}"#);
        match parse(&text) {
            Ok(Incoming::Error { description, .. }) => {
                assert_eq!(description.chars().count(), MAX_DESCRIPTION);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn negative_or_missing_timestamps_become_zero() {
        let r = results(
            r#"{"type":"Results","start":-3,"is_final":true,"channel":{"alternatives":[]}}"#,
        );
        assert!(r.start == 0.0 && r.duration == 0.0);
    }
}
