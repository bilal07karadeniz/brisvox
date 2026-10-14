//! Cartesia TTS WebSocket wire format: request encoding, server message decoding and the
//! base64 audio decoder. Pure functions and types, no I/O (docs/providers/cartesia.md).

use std::borrow::Cow;
use std::fmt;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use bytes::{Bytes, BytesMut};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

use super::{OutputEncoding, PROVIDER};
use crate::error::Error;

/// Standard base64 alphabet; padding is accepted but not required.
const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Size of the blocks audio chunks are carved from (one allocation per block, not per chunk).
const AUDIO_BLOCK: usize = 64 * 1024;

/// What a generation request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestKind {
    /// A text chunk of a context that continues (`continue: true`).
    Text,
    /// An empty-transcript manual flush (`continue: true`, `flush: true`): the server answers
    /// `flush_done` once everything before it has been produced.
    Marker,
    /// Closes the context (`continue: false`, empty transcript).
    Finish,
}

#[derive(Serialize)]
struct OutputFormat {
    container: &'static str,
    encoding: &'static str,
    sample_rate: u32,
}

#[derive(Serialize)]
struct Request<'a> {
    model_id: &'a str,
    transcript: &'a str,
    voice: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    context_id: &'a str,
    output_format: OutputFormat,
    #[serde(rename = "continue")]
    continue_: bool,
    max_buffer_delay_ms: u16,
    add_timestamps: bool,
    use_normalized_timestamps: bool,
    #[serde(skip_serializing_if = "is_false")]
    flush: bool,
}

// serde's `skip_serializing_if` hands the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
struct CancelRequest<'a> {
    context_id: &'a str,
    cancel: bool,
}

/// The per-session fields every request repeats unchanged (the contexts page: all fields but
/// `transcript` and `continue` must stay identical within a context).
#[derive(Debug, Clone)]
pub(super) struct RequestTemplate {
    pub(super) model_id: String,
    pub(super) voice: String,
    pub(super) language: Option<String>,
    pub(super) encoding: OutputEncoding,
    pub(super) sample_rate: u32,
    pub(super) max_buffer_delay_ms: u16,
    pub(super) use_normalized_timestamps: bool,
}

impl RequestTemplate {
    /// Encodes one generation request as a JSON text frame.
    pub(super) fn encode(
        &self,
        context_id: &str,
        transcript: &str,
        kind: RequestKind,
    ) -> Result<String, Error> {
        let request = Request {
            model_id: &self.model_id,
            transcript,
            voice: &self.voice,
            language: self.language.as_deref(),
            context_id,
            output_format: OutputFormat {
                container: "raw",
                encoding: self.encoding.wire_name(),
                sample_rate: self.sample_rate,
            },
            continue_: kind != RequestKind::Finish,
            max_buffer_delay_ms: self.max_buffer_delay_ms,
            add_timestamps: true,
            use_normalized_timestamps: self.use_normalized_timestamps,
            flush: kind == RequestKind::Marker,
        };
        serde_json::to_string(&request).map_err(|e| Error::Protocol {
            provider: PROVIDER,
            detail: format!("cannot encode request: {e}"),
        })
    }
}

/// Encodes the cancel message for `context_id`.
pub(super) fn encode_cancel(context_id: &str) -> Result<String, Error> {
    serde_json::to_string(&CancelRequest {
        context_id,
        cancel: true,
    })
    .map_err(|e| Error::Protocol {
        provider: PROVIDER,
        detail: format!("cannot encode cancel: {e}"),
    })
}

/// `Option<Cow<str>>` that borrows from the input when the JSON string has no escapes and
/// copies when it has (`\/` in base64, `\"` in a word), so neither shape fails to parse.
fn opt_cow<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Outer;
    struct Inner;

    impl<'de> Visitor<'de> for Inner {
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
            d.deserialize_str(Inner).map(Some)
        }
    }

    deserializer.deserialize_option(Outer)
}

/// Like [`opt_cow`], but a value that is not a string (a number, a list, an object) is `None`
/// instead of an error: used where a malformed field must not fail the whole message.
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

/// The routing fields of a server message, read leniently: a message whose `type` or
/// `context_id` has the wrong JSON type is still routed (and dropped when its context is not
/// live) instead of failing the session.
#[derive(Debug, Deserialize)]
pub(super) struct Envelope<'a> {
    #[serde(rename = "type", borrow, default, deserialize_with = "lenient_cow")]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "lenient_cow")]
    pub(super) context_id: Option<Cow<'a, str>>,
}

impl<'a> Envelope<'a> {
    /// Parses the routing fields of one text frame.
    pub(super) fn parse(text: &'a str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::Protocol {
            provider: PROVIDER,
            // serde_json's message names the problem and its position, never the input text.
            detail: format!("malformed server message: {e}"),
        })
    }

    /// The message type, or an empty string.
    pub(super) fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or("")
    }
}

/// `word_timestamps` of a `timestamps` message: parallel arrays, seconds.
#[derive(Debug, Default, Deserialize)]
pub(super) struct WordTimestamps {
    #[serde(default)]
    pub(super) words: Vec<String>,
    #[serde(default)]
    pub(super) start: Vec<f64>,
}

/// A server message. Only the fields brisvox reads; everything else is ignored.
#[derive(Debug, Deserialize)]
pub(super) struct Incoming<'a> {
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub(super) data: Option<Cow<'a, str>>,
    #[serde(default)]
    pub(super) done: Option<bool>,
    #[serde(default)]
    pub(super) status_code: Option<u32>,
    #[serde(default)]
    pub(super) word_timestamps: Option<WordTimestamps>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub(super) error_code: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub(super) title: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub(super) message: Option<Cow<'a, str>>,
}

impl<'a> Incoming<'a> {
    /// Parses one text frame.
    pub(super) fn parse(text: &'a str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::Protocol {
            provider: PROVIDER,
            // serde_json's message names the problem and its position, never the input text.
            detail: format!("malformed server message: {e}"),
        })
    }
}

/// Maps a server `error` message to the crate's error type.
///
/// Refusals that retrying or waiting can fix (auth, rate, concurrency, quota, 5xx) become
/// `Rejected { status }`; request problems (bad model, voice, language) become `Protocol` with
/// the server's code and text, which are safe to log (they never echo the API key).
pub(super) fn map_server_error(incoming: &Incoming<'_>) -> Error {
    let status = incoming.status_code.and_then(|s| u16::try_from(s).ok());
    let code = incoming.error_code.as_deref();
    match (code, status) {
        (Some("concurrency_limited"), s) => Error::Rejected {
            provider: PROVIDER,
            status: s.unwrap_or(429),
        },
        (Some("quota_exceeded"), s) => Error::Rejected {
            provider: PROVIDER,
            status: s.unwrap_or(402),
        },
        (Some("plan_upgrade_required"), s) => Error::Rejected {
            provider: PROVIDER,
            status: s.unwrap_or(403),
        },
        (_, Some(s)) if matches!(s, 401 | 403 | 408 | 429) || s >= 500 => Error::Rejected {
            provider: PROVIDER,
            status: s,
        },
        _ => Error::Protocol {
            provider: PROVIDER,
            detail: format!(
                "server error {} {}: {}: {}",
                status.map_or_else(|| "?".to_owned(), |s| s.to_string()),
                clip(code.unwrap_or("-")),
                clip(incoming.title.as_deref().unwrap_or("-")),
                clip(incoming.message.as_deref().unwrap_or("-")),
            ),
        },
    }
}

/// The server refused a context because the plan's concurrency is used up (a cancelled
/// context that is still generating may still count). Expected to pass within seconds.
pub(super) fn is_concurrency_limited(incoming: &Incoming<'_>) -> bool {
    incoming.error_code.as_deref() == Some("concurrency_limited")
        || (incoming.error_code.is_none() && incoming.status_code == Some(429))
}

/// At most 200 characters of server-provided text.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
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

    fn template() -> RequestTemplate {
        RequestTemplate {
            model_id: "sonic-3.6".to_owned(),
            voice: "voice-1".to_owned(),
            language: Some("tr".to_owned()),
            encoding: OutputEncoding::Pcm16,
            sample_rate: 24_000,
            max_buffer_delay_ms: 0,
            use_normalized_timestamps: false,
        }
    }

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn text_request_has_the_documented_shape() {
        let text = template()
            .encode("ctx-1", "Hello, ", RequestKind::Text)
            .unwrap();
        assert_eq!(
            json(&text),
            serde_json::json!({
                "model_id": "sonic-3.6",
                "transcript": "Hello, ",
                "voice": "voice-1",
                "language": "tr",
                "context_id": "ctx-1",
                "output_format": {"container": "raw", "encoding": "pcm_s16le", "sample_rate": 24000},
                "continue": true,
                "max_buffer_delay_ms": 0,
                "add_timestamps": true,
                "use_normalized_timestamps": false
            })
        );
    }

    #[test]
    fn marker_flushes_and_finish_closes_the_context() {
        let marker = json(&template().encode("c", "", RequestKind::Marker).unwrap());
        assert_eq!(marker["flush"], true);
        assert_eq!(marker["continue"], true);
        assert_eq!(marker["transcript"], "");
        let finish = json(&template().encode("c", "", RequestKind::Finish).unwrap());
        assert_eq!(finish["continue"], false);
        assert!(finish.get("flush").is_none());
    }

    #[test]
    fn language_is_omitted_when_unset_and_mulaw_is_named() {
        let mut t = template();
        t.language = None;
        t.encoding = OutputEncoding::Mulaw;
        t.sample_rate = 8000;
        let v = json(&t.encode("c", "x", RequestKind::Text).unwrap());
        assert!(v.get("language").is_none());
        assert_eq!(v["output_format"]["encoding"], "pcm_mulaw");
    }

    #[test]
    fn cancel_message_is_exactly_the_documented_one() {
        assert_eq!(
            json(&encode_cancel("happy-monkeys-fly").unwrap()),
            serde_json::json!({"context_id": "happy-monkeys-fly", "cancel": true})
        );
    }

    #[test]
    fn parses_every_documented_server_message() {
        let text = r#"{"type":"chunk","data":"AAE=","done":false,"status_code":206,"step_time":123,"context_id":"a"}"#;
        let chunk = Incoming::parse(text).unwrap();
        let envelope = Envelope::parse(text).unwrap();
        assert_eq!(envelope.kind(), "chunk");
        assert_eq!(envelope.context_id.as_deref(), Some("a"));
        assert_eq!(chunk.data.as_deref(), Some("AAE="));
        let text = r#"{"type":"flush_done","done":false,"flush_done":true,"flush_id":1,"status_code":206,"context_id":"a"}"#;
        Incoming::parse(text).unwrap();
        assert_eq!(Envelope::parse(text).unwrap().kind(), "flush_done");
        let done =
            Incoming::parse(r#"{"type":"done","done":true,"status_code":206,"context_id":"a"}"#)
                .unwrap();
        assert_eq!(done.done, Some(true));
        let ts = Incoming::parse(
            r#"{"type":"timestamps","done":false,"status_code":206,"context_id":"a","word_timestamps":{"words":["Example","text"],"start":[0,0.5],"end":[0.4,0.9]}}"#,
        )
        .unwrap();
        let words = ts.word_timestamps.unwrap();
        assert_eq!(words.words, ["Example", "text"]);
        assert_eq!(words.start, [0.0, 0.5]);
        let text = r#"{"type":"phoneme_timestamps","done":false,"status_code":206,"context_id":"a","phoneme_timestamps":{"phonemes":["h"],"start":[0.09],"end":[0.17]}}"#;
        let phonemes = Incoming::parse(text).unwrap();
        assert_eq!(Envelope::parse(text).unwrap().kind(), "phoneme_timestamps");
        assert!(phonemes.word_timestamps.is_none());
        let error = Incoming::parse(
            r#"{"type":"error","done":true,"error_code":"model_not_found","status_code":400,"title":"Invalid model","message":"The model is not valid, make sure it is a valid model ID.","doc_url":"https://x","request_id":"r","context_id":"a"}"#,
        )
        .unwrap();
        assert_eq!(error.error_code.as_deref(), Some("model_not_found"));
    }

    #[test]
    fn escaped_strings_parse_too() {
        let text = r#"{"type":"chunk","data":"AA\/B","context_id":"a\"b"}"#;
        let m = Incoming::parse(text).unwrap();
        assert_eq!(m.data.as_deref(), Some("AA/B"));
        assert_eq!(
            Envelope::parse(text).unwrap().context_id.as_deref(),
            Some("a\"b")
        );
    }

    #[test]
    fn the_envelope_tolerates_wrongly_typed_fields() {
        let ok = Envelope::parse(r#"{"type":"chunk","context_id":"a","data":5}"#).unwrap();
        assert_eq!((ok.kind(), ok.context_id.as_deref()), ("chunk", Some("a")));
        let odd = Envelope::parse(r#"{"type":7,"context_id":{"x":[1,2]},"done":"yes"}"#).unwrap();
        assert_eq!((odd.kind(), odd.context_id.as_deref()), ("", None));
        let nulls = Envelope::parse(r#"{"type":null,"context_id":null}"#).unwrap();
        assert_eq!(nulls.context_id.as_deref(), None);
        let escaped = Envelope::parse(r#"{"context_id":"a\"b"}"#).unwrap();
        assert_eq!(escaped.context_id.as_deref(), Some("a\"b"));
        // The same message is invalid for the typed parse, which is only run for live contexts.
        assert!(Incoming::parse(r#"{"type":"chunk","context_id":"a","status_code":"x"}"#).is_err());
        assert!(Envelope::parse("not json").is_err());
    }

    #[test]
    fn concurrency_refusals_are_recognised() {
        let parse = |t: &str| {
            let text = t.to_owned();
            is_concurrency_limited(&Incoming::parse(&text).unwrap())
        };
        assert!(parse(
            r#"{"type":"error","error_code":"concurrency_limited","status_code":429}"#
        ));
        assert!(parse(r#"{"type":"error","status_code":429}"#));
        assert!(!parse(
            r#"{"type":"error","error_code":"quota_exceeded","status_code":429}"#
        ));
        assert!(!parse(r#"{"type":"error","status_code":400}"#));
    }

    #[test]
    fn garbage_is_a_protocol_error_without_the_input() {
        let err = Incoming::parse("secret-looking garbage").unwrap_err();
        let Error::Protocol { detail, .. } = err else {
            panic!("expected Protocol");
        };
        assert!(!detail.contains("secret-looking"));
    }

    fn error_message(code: Option<&str>, status: Option<u32>) -> Error {
        let text = serde_json::json!({
            "type": "error", "error_code": code, "status_code": status,
            "title": "T", "message": "M", "context_id": "a"
        })
        .to_string();
        map_server_error(&Incoming::parse(&text).unwrap())
    }

    #[test]
    fn server_errors_map_to_rejected_or_protocol() {
        assert!(matches!(
            error_message(Some("concurrency_limited"), Some(429)),
            Error::Rejected { status: 429, .. }
        ));
        assert!(matches!(
            error_message(Some("concurrency_limited"), None),
            Error::Rejected { status: 429, .. }
        ));
        assert!(matches!(
            error_message(Some("quota_exceeded"), None),
            Error::Rejected { status: 402, .. }
        ));
        assert!(matches!(
            error_message(None, Some(401)),
            Error::Rejected { status: 401, .. }
        ));
        assert!(matches!(
            error_message(None, Some(503)),
            Error::Rejected { status: 503, .. }
        ));
        let Error::Protocol { detail, .. } = error_message(Some("model_not_found"), Some(400))
        else {
            panic!("expected Protocol");
        };
        assert!(detail.contains("model_not_found") && detail.contains("400"));
    }

    #[test]
    fn audio_decoder_shares_blocks_and_rejects_garbage() {
        let mut decoder = AudioDecoder::default();
        let a = decoder.decode("AAEC").unwrap();
        let b = decoder.decode("AwQ=").unwrap();
        assert_eq!(&a[..], &[0, 1, 2]);
        assert_eq!(&b[..], &[3, 4]);
        // Unpadded input is accepted.
        assert_eq!(&decoder.decode("AwQ").unwrap()[..], &[3, 4]);
        assert!(decoder.decode("!!!!").is_err());
        assert_eq!(&decoder.decode("AAEC").unwrap()[..], &[0, 1, 2]);
        assert!(decoder.decode("").unwrap().is_empty());
    }
}
