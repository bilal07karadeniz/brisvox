//! ElevenLabs multi-context TTS WebSocket wire format: client frames, server message decoding,
//! error mapping and the base64 audio decoder. Pure functions and types, no I/O
//! (docs/providers/elevenlabs.md).
//!
//! Naming (docs): client frames use snake_case, server messages camelCase; the snake_case
//! spellings of the server's fields are accepted too, because the docs contradict themselves
//! about `isFinal` / `is_final`.

use std::borrow::Cow;
use std::fmt;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use bytes::{Bytes, BytesMut};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use super::{PROVIDER, VoiceSettings};
use crate::error::Error;

/// Standard base64 alphabet; padding is accepted but not required.
const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Size of the blocks audio chunks are carved from (one allocation per block, not per chunk).
const AUDIO_BLOCK: usize = 64 * 1024;

// ----- client frames --------------------------------------------------------------------------

#[derive(Serialize)]
struct GenerationConfig<'a> {
    chunk_length_schedule: &'a [u16],
}

/// Creates (or re-initialises) a context: `text` is a single space (docs: the first message
/// of a context), voice settings and generation config go on this message only.
#[derive(Serialize)]
struct InitFrame<'a> {
    text: &'static str,
    context_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice_settings: Option<&'a VoiceSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig<'a>>,
}

/// Text for a context, optionally forcing generation of the buffer.
#[derive(Serialize)]
struct TextFrame<'a> {
    text: &'a str,
    context_id: &'a str,
    flush: bool,
}

/// `{"context_id": .., "flush": true}`: generate what is buffered, keep the context open.
#[derive(Serialize)]
struct FlushFrame<'a> {
    context_id: &'a str,
    flush: bool,
}

/// `{"context_id": .., "close_context": true}`.
#[derive(Serialize)]
struct CloseFrame<'a> {
    context_id: &'a str,
    close_context: bool,
}

/// `{"context_id": .., "text": ""}`: ignored by the server, resets the context's inactivity
/// timer (docs: `KeepContextAlive`).
#[derive(Serialize)]
struct KeepAliveFrame<'a> {
    context_id: &'a str,
    text: &'static str,
}

fn encode<T: Serialize>(frame: &T) -> Result<String, Error> {
    serde_json::to_string(frame).map_err(|e| Error::Protocol {
        provider: PROVIDER,
        detail: format!("cannot encode request: {e}"),
    })
}

/// The per-session settings the client frames carry.
#[derive(Debug, Clone, Default)]
pub(super) struct FrameSettings {
    pub(super) voice_settings: Option<VoiceSettings>,
    pub(super) chunk_length_schedule: Option<Vec<u16>>,
}

impl FrameSettings {
    /// The first message of a server context.
    pub(super) fn init(&self, context_id: &str) -> Result<String, Error> {
        encode(&InitFrame {
            text: " ",
            context_id,
            voice_settings: self.voice_settings.as_ref(),
            generation_config: self
                .chunk_length_schedule
                .as_deref()
                .map(|chunk_length_schedule| GenerationConfig {
                    chunk_length_schedule,
                }),
        })
    }
}

/// A text frame with `flush: true`. The docs ask every chunk to end with a single space; one
/// is appended unless the text already ends in whitespace.
pub(super) fn text_frame(context_id: &str, text: &str) -> Result<String, Error> {
    let owned;
    let text = if text.ends_with(char::is_whitespace) {
        text
    } else {
        owned = format!("{text} ");
        owned.as_str()
    };
    encode(&TextFrame {
        text,
        context_id,
        flush: true,
    })
}

/// Forces generation of whatever the context buffered.
pub(super) fn flush_frame(context_id: &str) -> Result<String, Error> {
    encode(&FlushFrame {
        context_id,
        flush: true,
    })
}

/// Closes a context (also the barge-in cancel: the docs' only cancel).
pub(super) fn close_frame(context_id: &str) -> Result<String, Error> {
    encode(&CloseFrame {
        context_id,
        close_context: true,
    })
}

/// Resets the inactivity timer of a context.
pub(super) fn keepalive_frame(context_id: &str) -> Result<String, Error> {
    encode(&KeepAliveFrame {
        context_id,
        text: "",
    })
}

// ----- server messages ------------------------------------------------------------------------

/// Visits a JSON string as `Cow<str>`: borrowed from the input when the string has no escapes,
/// copied when it has (`\/` in base64, `\"` as an alignment character), so neither shape fails
/// to parse.
struct CowVisitor;

impl<'de> Visitor<'de> for CowVisitor {
    type Value = Cow<'de, str>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string")
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v))
    }
}

/// One array element, deserialized through [`CowVisitor`].
struct CowStr<'de>(Cow<'de, str>);

impl<'de> Deserialize<'de> for CowStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(CowVisitor).map(Self)
    }
}

/// `Option<Cow<str>>` that borrows from the input when the JSON string has no escapes.
fn opt_cow<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Outer;

    impl<'de> Visitor<'de> for Outer {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_str(CowVisitor).map(Some)
        }
    }

    deserializer.deserialize_option(Outer)
}

/// `Option<Vec<Cow<str>>>`: one allocation for the list and none per element that has no
/// escapes (an alignment has one element per character, 10 to 50 per message).
fn opt_cow_list<'de, D>(deserializer: D) -> Result<Option<Vec<Cow<'de, str>>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Outer;
    struct List;

    impl<'de> Visitor<'de> for List {
        type Value = Vec<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an array of strings")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut list = Vec::with_capacity(seq.size_hint().unwrap_or(64).min(4096));
            while let Some(CowStr(item)) = seq.next_element()? {
                list.push(item);
            }
            Ok(list)
        }
    }

    impl<'de> Visitor<'de> for Outer {
        type Value = Option<Vec<Cow<'de, str>>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an array of strings or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_seq(List).map(Some)
        }
    }

    deserializer.deserialize_option(Outer)
}

/// Like [`opt_cow`], but a value that is not a string is `None` instead of an error: used for
/// the routing field, where a malformed value must not fail the whole message.
fn lenient_cow<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Lenient;

    impl<'de> Visitor<'de> for Lenient {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("anything")
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(v)))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(v.to_owned())))
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(v)))
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map
                .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(Lenient)
}

/// The routing field of a server message, read leniently: a message whose `contextId` has the
/// wrong JSON type is still routed (and dropped when its context is not live) instead of
/// failing the session. Only used when the full message does not parse, to decide whether the
/// failure matters (its context is live) or the message is dropped (it is not).
#[derive(Debug, Deserialize)]
pub(super) struct Envelope<'a> {
    #[serde(
        rename = "contextId",
        alias = "context_id",
        borrow,
        default,
        deserialize_with = "lenient_cow"
    )]
    context_id: Option<Cow<'a, str>>,
}

impl<'a> Envelope<'a> {
    /// Parses the routing field of one text frame.
    pub(super) fn parse(text: &'a str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::Protocol {
            provider: PROVIDER,
            // serde_json's message names the problem and its position, never the input text.
            detail: format!("malformed server message: {e}"),
        })
    }

    /// The server context the message belongs to.
    pub(super) fn context_id(&self) -> Option<&str> {
        self.context_id.as_deref()
    }
}

/// An alignment object: three parallel arrays, one entry per character. Times are relative to
/// the message's own audio (docs: "relative to the returned chunk").
#[derive(Debug, Default, Deserialize)]
pub(super) struct Alignment<'a> {
    #[serde(borrow, default, deserialize_with = "opt_cow_list")]
    pub(super) chars: Option<Vec<Cow<'a, str>>>,
    #[serde(default, rename = "charStartTimesMs", alias = "char_start_times_ms")]
    pub(super) starts: Option<Vec<f64>>,
}

impl Alignment<'_> {
    /// Whether the object carries any character.
    pub(super) fn is_empty(&self) -> bool {
        self.chars.as_ref().is_none_or(Vec::is_empty)
    }
}

/// A server message. Only the fields brisvox reads; everything else is ignored.
#[derive(Debug, Deserialize)]
pub(super) struct Incoming<'a> {
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub(super) audio: Option<Cow<'a, str>>,
    #[serde(default, rename = "isFinal", alias = "is_final")]
    pub(super) is_final: Option<bool>,
    #[serde(borrow, default)]
    pub(super) alignment: Option<Alignment<'a>>,
    #[serde(
        borrow,
        default,
        rename = "normalizedAlignment",
        alias = "normalized_alignment"
    )]
    pub(super) normalized: Option<Alignment<'a>>,
    /// The routing field, read leniently like [`Envelope`] does.
    #[serde(
        rename = "contextId",
        alias = "context_id",
        borrow,
        default,
        deserialize_with = "lenient_cow"
    )]
    context_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    error: Option<&'a RawValue>,
    #[serde(borrow, default)]
    detail: Option<&'a RawValue>,
    #[serde(borrow, default)]
    message: Option<&'a RawValue>,
}

impl<'a> Incoming<'a> {
    /// The server context the message belongs to (`None`: absent or of the wrong JSON type).
    pub(super) fn context_id(&self) -> Option<&str> {
        self.context_id.as_deref()
    }

    /// Parses one text frame.
    pub(super) fn parse(text: &'a str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::Protocol {
            provider: PROVIDER,
            // serde_json's message names the problem and its position, never the input text.
            detail: format!("malformed server message: {e}"),
        })
    }

    /// Whether this is an error report. The docs name no WebSocket error message (UNVERIFIED),
    /// so any message with an `error` key, or a `detail` / `message` key and none of the
    /// documented output fields, counts.
    pub(super) fn is_error(&self) -> bool {
        let output = self.audio.is_some() || self.is_final.is_some() || self.alignment.is_some();
        self.error.is_some() || (!output && (self.detail.is_some() || self.message.is_some()))
    }
}

/// Searches `value`, then its `detail` and `error` objects, for the first of `keys` whose value
/// satisfies `want`.
fn find<'v>(value: &'v Value, keys: &[&str], want: fn(&Value) -> bool) -> Option<&'v Value> {
    [Some(value), value.get("detail"), value.get("error")]
        .into_iter()
        .flatten()
        .filter(|v| v.is_object())
        .find_map(|v| keys.iter().find_map(|k| v.get(*k).filter(|x| want(x))))
}

/// At most 200 characters of server-provided text.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

/// Maps a server error message to the crate's error type.
///
/// Refusals that retrying or waiting can fix (auth, rate, concurrency, credits, 5xx) become
/// `Rejected { status }`; everything else is `Protocol` with the server's code and text (clipped;
/// the server never echoes the key). The shape of WebSocket errors is UNVERIFIED, so the
/// message is searched for a numeric status, then for the documented HTTP error codes
/// (`invalid_api_key`, `rate_limit_exceeded`, `concurrent_limit_exceeded`, `system_busy`,
/// `insufficient_credits`, `internal_error`).
pub(super) fn map_server_error(text: &str) -> Error {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Error::Protocol {
            provider: PROVIDER,
            detail: "server error message that is not valid JSON".to_owned(),
        };
    };
    let status = find(&value, &["status", "status_code", "code"], Value::is_u64)
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok());
    let code = find(&value, &["code", "type", "status"], Value::is_string).and_then(Value::as_str);
    let message = find(&value, &["message"], Value::is_string)
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str));
    let rejected = |status| Error::Rejected {
        provider: PROVIDER,
        status,
    };
    if let Some(s) = status
        && (matches!(s, 401 | 402 | 403 | 408 | 429) || s >= 500)
    {
        return rejected(s);
    }
    match code {
        Some("invalid_api_key" | "missing_api_key" | "authentication_error") => rejected(401),
        Some(
            "rate_limit_exceeded"
            | "concurrent_limit_exceeded"
            | "system_busy"
            | "too_many_concurrent_requests"
            | "rate_limit_error",
        ) => rejected(429),
        Some("insufficient_credits" | "payment_required") => rejected(402),
        Some("internal_error") => rejected(500),
        _ => Error::Protocol {
            provider: PROVIDER,
            detail: format!(
                "server error {} {}: {}",
                status.map_or_else(|| "?".to_owned(), |s| s.to_string()),
                clip(code.unwrap_or("-")),
                clip(message.unwrap_or("-")),
            ),
        },
    }
}

/// Decodes base64 audio into `Bytes` carved from large reusable blocks.
#[derive(Debug, Default)]
pub(super) struct AudioDecoder {
    scratch: BytesMut,
}

impl AudioDecoder {
    /// Decodes `encoded`; the returned `Bytes` shares a block with earlier chunks.
    pub(super) fn decode(&mut self, encoded: &str) -> Result<Bytes, Error> {
        let estimate = base64::decoded_len_estimate(encoded.len());
        if self.scratch.capacity() < estimate {
            self.scratch.reserve(estimate.max(AUDIO_BLOCK));
        }
        self.scratch.resize(estimate, 0);
        match ENGINE.decode_slice(encoded.as_bytes(), &mut self.scratch) {
            Ok(written) => {
                self.scratch.truncate(written);
                Ok(self.scratch.split().freeze())
            }
            Err(e) => {
                self.scratch.clear();
                Err(Error::Protocol {
                    provider: PROVIDER,
                    detail: format!("audio chunk is not valid base64: {e}"),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn init_frame_carries_settings_only_when_configured() {
        let plain = FrameSettings::default().init("c1").unwrap();
        assert_eq!(
            json(&plain),
            serde_json::json!({"text": " ", "context_id": "c1"})
        );
        let settings = FrameSettings {
            voice_settings: Some(VoiceSettings {
                stability: Some(0.5),
                speed: Some(1.0),
                ..VoiceSettings::default()
            }),
            chunk_length_schedule: Some(vec![50, 120]),
        };
        assert_eq!(
            json(&settings.init("c1").unwrap()),
            serde_json::json!({
                "text": " ", "context_id": "c1",
                "voice_settings": {"stability": 0.5, "speed": 1.0},
                "generation_config": {"chunk_length_schedule": [50, 120]}
            })
        );
    }

    #[test]
    fn text_frames_end_with_a_space_and_flush() {
        assert_eq!(
            json(&text_frame("c", "Hello").unwrap()),
            serde_json::json!({"text": "Hello ", "context_id": "c", "flush": true})
        );
        // Already ends in whitespace: not doubled.
        assert_eq!(json(&text_frame("c", "Hello ").unwrap())["text"], "Hello ");
        assert_eq!(json(&text_frame("c", "Hi\n").unwrap())["text"], "Hi\n");
        // Quotes and non-ASCII survive.
        assert_eq!(
            json(&text_frame("c", "\"Merhaba\" dünya").unwrap())["text"],
            "\"Merhaba\" dünya "
        );
    }

    #[test]
    fn control_frames_are_exactly_the_documented_ones() {
        assert_eq!(
            json(&flush_frame("c").unwrap()),
            serde_json::json!({"context_id": "c", "flush": true})
        );
        assert_eq!(
            json(&close_frame("c").unwrap()),
            serde_json::json!({"context_id": "c", "close_context": true})
        );
        assert_eq!(
            json(&keepalive_frame("c").unwrap()),
            serde_json::json!({"context_id": "c", "text": ""})
        );
    }

    #[test]
    fn parses_every_documented_server_message() {
        let text = r#"{"audio":"AAE=","alignment":{"chars":["H","i"," "],"charStartTimesMs":[0,60,120],"charDurationsMs":[60,60,40]},"normalizedAlignment":{"chars":["H"],"charStartTimesMs":[0],"charDurationsMs":[60]},"contextId":"a"}"#;
        assert_eq!(Envelope::parse(text).unwrap().context_id(), Some("a"));
        let m = Incoming::parse(text).unwrap();
        assert_eq!(m.audio.as_deref(), Some("AAE="));
        assert!(!m.is_error());
        let a = m.alignment.unwrap();
        assert_eq!(a.chars.unwrap(), ["H", "i", " "]);
        assert_eq!(a.starts.unwrap(), [0.0, 60.0, 120.0]);
        assert_eq!(m.normalized.unwrap().chars.unwrap(), ["H"]);
        assert_eq!(m.is_final, None);

        let text = r#"{"isFinal":true,"contextId":"a"}"#;
        let m = Incoming::parse(text).unwrap();
        assert_eq!(m.is_final, Some(true));
        assert!(!m.is_error());
        assert_eq!(Envelope::parse(text).unwrap().context_id(), Some("a"));
    }

    #[test]
    fn tolerates_the_documented_ambiguities() {
        // snake_case spellings.
        let text = r#"{"is_final":true,"context_id":"a"}"#;
        assert_eq!(Incoming::parse(text).unwrap().is_final, Some(true));
        assert_eq!(Envelope::parse(text).unwrap().context_id(), Some("a"));
        // Null audio and null alignment on an alignment-only / final message.
        let m = Incoming::parse(
            r#"{"audio":null,"alignment":null,"normalizedAlignment":null,"isFinal":null,"contextId":"a"}"#,
        )
        .unwrap();
        assert!(m.audio.is_none() && m.alignment.is_none() && m.is_final.is_none());
        assert!(!m.is_error());
        // Null arrays inside an alignment, empty audio.
        let m =
            Incoming::parse(r#"{"audio":"","alignment":{"chars":null,"charStartTimesMs":null}}"#)
                .unwrap();
        assert!(m.alignment.unwrap().is_empty());
        // Escaped base64 and context id.
        let text = r#"{"audio":"AA\/B","contextId":"a\"b"}"#;
        assert_eq!(
            Incoming::parse(text).unwrap().audio.as_deref(),
            Some("AA/B")
        );
        assert_eq!(Envelope::parse(text).unwrap().context_id(), Some("a\"b"));
        // A wrongly typed routing field is not an error.
        assert_eq!(
            Envelope::parse(r#"{"contextId":{"x":[1]},"audio":5}"#)
                .unwrap()
                .context_id(),
            None
        );
        assert!(Envelope::parse("not json").is_err());
    }

    #[test]
    fn alignment_characters_borrow_from_the_frame_unless_escaped() {
        // The hot path: no String per character.
        let text =
            r#"{"alignment":{"chars":["a","b"," "],"charStartTimesMs":[0,1,2]},"contextId":"c"}"#;
        let m = Incoming::parse(text).unwrap();
        assert_eq!(m.context_id(), Some("c"));
        let chars = m.alignment.unwrap().chars.unwrap();
        assert_eq!(chars.len(), 3);
        assert!(chars.iter().all(|c| matches!(c, Cow::Borrowed(_))));
        // An escaped character is copied, and still parses.
        let text = r#"{"alignment":{"chars":["\"","\\","\u00fc"]}}"#;
        let chars = Incoming::parse(text)
            .unwrap()
            .alignment
            .unwrap()
            .chars
            .unwrap();
        assert_eq!(chars, ["\"", "\\", "\u{fc}"]);
        assert!(matches!(chars[0], Cow::Owned(_)));
        // A non-string element is a parse error; a null list is empty.
        assert!(Incoming::parse(r#"{"alignment":{"chars":[1]}}"#).is_err());
        let m = Incoming::parse(r#"{"alignment":{"chars":null}}"#).unwrap();
        assert!(m.alignment.unwrap().is_empty());
    }

    #[test]
    fn error_messages_are_recognised_in_several_shapes() {
        for text in [
            r#"{"error":"boom"}"#,
            r#"{"detail":{"type":"x","code":"y","message":"z"}}"#,
            r#"{"message":"Maximum simultaneous contexts reached","contextId":"a"}"#,
            r#"{"error":{"code":"x"},"audio":"AA=="}"#,
        ] {
            assert!(Incoming::parse(text).unwrap().is_error(), "{text}");
        }
        for text in [
            r#"{"audio":"AA==","message":"hi"}"#,
            r#"{"isFinal":true,"detail":"x"}"#,
            r#"{}"#,
        ] {
            assert!(!Incoming::parse(text).unwrap().is_error(), "{text}");
        }
    }

    #[test]
    fn server_errors_map_to_rejected_or_protocol() {
        let rejected = |text: &str, expected: u16| {
            assert!(
                matches!(map_server_error(text), Error::Rejected { status, .. } if status == expected),
                "{text}"
            );
        };
        rejected(
            r#"{"detail":{"type":"authentication_error","code":"invalid_api_key","message":"m","status":"invalid_api_key"}}"#,
            401,
        );
        rejected(r#"{"detail":{"code":"missing_api_key"}}"#, 401);
        rejected(r#"{"error":"x","code":429}"#, 429);
        rejected(
            r#"{"detail":{"type":"rate_limit_error","code":"concurrent_limit_exceeded"}}"#,
            429,
        );
        rejected(r#"{"detail":{"code":"system_busy"}}"#, 429);
        rejected(r#"{"detail":{"code":"insufficient_credits"}}"#, 402);
        rejected(r#"{"error":{"code":"internal_error"}}"#, 500);
        rejected(r#"{"error":"x","status":503}"#, 503);
        let Error::Protocol { detail, .. } =
            map_server_error(r#"{"detail":{"code":"voice_not_found","message":"No such voice"}}"#)
        else {
            panic!("expected Protocol");
        };
        assert!(detail.contains("voice_not_found") && detail.contains("No such voice"));
        assert!(matches!(
            map_server_error("not json"),
            Error::Protocol { .. }
        ));
        assert!(matches!(
            map_server_error(r#"{"error":"plain text"}"#),
            Error::Protocol { .. }
        ));
    }

    #[test]
    fn garbage_is_a_protocol_error_without_the_input() {
        let err = Incoming::parse("secret-looking garbage").unwrap_err();
        let Error::Protocol { detail, .. } = err else {
            panic!("expected Protocol");
        };
        assert!(!detail.contains("secret-looking"));
    }

    #[test]
    fn audio_decoder_shares_blocks_and_rejects_garbage() {
        let mut decoder = AudioDecoder::default();
        let a = decoder.decode("AAEC").unwrap();
        let b = decoder.decode("AwQ=").unwrap();
        assert_eq!(&a[..], &[0, 1, 2]);
        assert_eq!(&b[..], &[3, 4]);
        assert_eq!(&decoder.decode("AwQ").unwrap()[..], &[3, 4]);
        assert!(decoder.decode("!!!!").is_err());
        assert_eq!(&decoder.decode("AAEC").unwrap()[..], &[0, 1, 2]);
        assert!(decoder.decode("").unwrap().is_empty());
    }
}
