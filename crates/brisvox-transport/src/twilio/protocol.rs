//! The Twilio Media Streams messages (`docs/providers/twilio.md`, fetched 2026-10-08).
//!
//! Every frame is one JSON text frame tagged by `event`.
//!
//! | direction | `event` | fields used here |
//! |---|---|---|
//! | Twilio to brisvox | `connected` | `protocol`, `version` |
//! | Twilio to brisvox | `start` | `start.{streamSid, callSid, accountSid, tracks, mediaFormat, customParameters}` |
//! | Twilio to brisvox | `media` | `media.{track, chunk, timestamp, payload}`, `streamSid` |
//! | Twilio to brisvox | `dtmf` | `dtmf.digit` |
//! | Twilio to brisvox | `mark` | `mark.name` |
//! | Twilio to brisvox | `stop` | |
//! | brisvox to Twilio | `media` | `streamSid`, `media.payload` (base64 mu-law 8 kHz) |
//! | brisvox to Twilio | `mark` | `streamSid`, `mark.name` |
//! | brisvox to Twilio | `clear` | `streamSid` |
//!
//! Twilio documents `sequenceNumber`, `media.chunk` and `media.timestamp` as numbers and prints
//! them as JSON strings; [`Lenient`] reads both where the value is used (`start.mediaFormat`).
//! Fields the adapter only reads for statistics or not at all (`sequenceNumber`, `media.chunk`,
//! `media.timestamp`, `connected.protocol` / `version`) are read softly: a value of an
//! unexpected type (`"1.0"`, `""`, a float, an object) reads as absent and is never a reason to
//! drop the message that carries it. Unknown fields are ignored and an unknown
//! `event` parses to [`Inbound::Unknown`] (forward compatibility). Twilio accepts only `media`,
//! `mark` and `clear` from the server (error 31951 for anything else), so this module can build
//! exactly those three.

use std::borrow::Cow;
use std::collections::BTreeMap;

use base64::Engine as _;
use brisvox_core::frame::DtmfDigit;
use bytes::BytesMut;
use serde::Deserialize;

pub(crate) use crate::telephony::BASE64;
pub use crate::telephony::{Lenient, MarkName};
use crate::telephony::{Soft, SoftText, clipped_error};

/// The only encoding of a Media Stream (`start.mediaFormat.encoding`).
pub const MULAW_ENCODING: &str = "audio/x-mulaw";

/// The only sample rate of a Media Stream, in Hz.
pub const SAMPLE_RATE_HZ: u32 = 8000;

/// A violation of the message format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The frame is not JSON, or a known field has the wrong type.
    #[error("not a valid twilio message: {0}")]
    InvalidJson(String),
    /// The frame is JSON but lacks something its `event` needs.
    #[error("invalid {event} message: {what}")]
    Invalid {
        /// The `event` of the message.
        event: &'static str,
        /// What is wrong.
        what: &'static str,
    },
}

impl ProtocolError {
    fn json(error: &serde_json::Error) -> Self {
        Self::InvalidJson(clipped_error(error))
    }
}

/// `start.mediaFormat`.
#[derive(Debug, Default, Deserialize)]
struct MediaFormatBody {
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default, rename = "sampleRate")]
    sample_rate: Option<Lenient>,
    #[serde(default)]
    channels: Option<Lenient>,
}

/// The `start` object as Twilio sends it.
#[derive(Debug, Default, Deserialize)]
struct StartBody {
    #[serde(default, rename = "streamSid")]
    stream_sid: Option<String>,
    #[serde(default, rename = "callSid")]
    call_sid: Option<String>,
    #[serde(default, rename = "accountSid")]
    account_sid: Option<String>,
    #[serde(default)]
    tracks: Vec<String>,
    #[serde(default, rename = "mediaFormat")]
    media_format: Option<MediaFormatBody>,
    #[serde(default, rename = "customParameters")]
    custom_parameters: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MediaBody<'a> {
    #[serde(default, borrow)]
    track: Option<Cow<'a, str>>,
    #[serde(default)]
    chunk: Soft,
    #[serde(default)]
    timestamp: Soft,
    #[serde(default, borrow)]
    payload: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct MarkBody<'a> {
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct DtmfBody<'a> {
    #[serde(default, borrow)]
    digit: Option<Cow<'a, str>>,
}

/// The envelope fields every message has. The rest of a frame is read per `event`, so an
/// unknown `event` with any other content still parses.
#[derive(Debug, Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    event: Cow<'a, str>,
    #[serde(default, borrow, rename = "streamSid")]
    stream_sid: Option<Cow<'a, str>>,
    #[serde(default, rename = "sequenceNumber")]
    sequence_number: Soft,
}

#[derive(Debug, Default, Deserialize)]
struct ConnectedMsg {
    #[serde(default)]
    protocol: SoftText,
    #[serde(default)]
    version: SoftText,
}

#[derive(Debug, Deserialize)]
struct StartMsg {
    #[serde(default)]
    start: Option<StartBody>,
}

#[derive(Debug, Deserialize)]
struct MediaMsg<'a> {
    #[serde(default, borrow)]
    media: Option<MediaBody<'a>>,
}

#[derive(Debug, Deserialize)]
struct MarkMsg<'a> {
    #[serde(default, borrow)]
    mark: Option<MarkBody<'a>>,
}

#[derive(Debug, Deserialize)]
struct DtmfMsg<'a> {
    #[serde(default, borrow)]
    dtmf: Option<DtmfBody<'a>>,
}

fn body<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, ProtocolError> {
    serde_json::from_str(text).map_err(|e| ProtocolError::json(&e))
}

/// What `start` told us about the stream, validated for presence but not for content (the
/// connection checks the media format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    /// The stream id every later message and every reply carries (`start.streamSid`).
    pub stream_sid: String,
    /// The call id (`start.callSid`); empty when Twilio did not send one.
    pub call_sid: String,
    /// The account id (`start.accountSid`), if sent.
    pub account_sid: Option<String>,
    /// `start.tracks`.
    pub tracks: Vec<String>,
    /// `start.mediaFormat.encoding`, if sent.
    pub encoding: Option<String>,
    /// `start.mediaFormat.sampleRate`, if sent.
    pub sample_rate: Option<u64>,
    /// `start.mediaFormat.channels`, if sent.
    pub channels: Option<u64>,
    /// The `<Parameter>` values of the TwiML, as strings (other JSON values are rendered as
    /// JSON text).
    pub custom_parameters: BTreeMap<String, String>,
}

/// One message from Twilio.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound<'a> {
    /// `connected`: the first message.
    Connected {
        /// `protocol`, e.g. `Call`.
        protocol: Option<String>,
        /// `version`, e.g. `1.0.0`.
        version: Option<String>,
    },
    /// `start`: sent once, right after `connected`.
    Start(Box<StartInfo>),
    /// `media`: caller audio.
    Media {
        /// `media.track`; `inbound` for a bidirectional stream.
        track: Option<Cow<'a, str>>,
        /// `media.chunk`, counting from 1.
        chunk: Option<u64>,
        /// `media.timestamp`, in milliseconds since the stream started.
        timestamp_ms: Option<u64>,
        /// `media.payload`: base64 mu-law, 8 kHz.
        payload: Cow<'a, str>,
    },
    /// `dtmf`: a key press.
    Dtmf(DtmfDigit),
    /// `mark`: Twilio echoes a mark brisvox sent (played, or flushed by a `clear`).
    Mark {
        /// `mark.name`.
        name: Cow<'a, str>,
    },
    /// `stop`: the stream ended (the call ended).
    Stop,
    /// An `event` this version does not know; ignored.
    Unknown(Cow<'a, str>),
}

/// A parsed frame with its envelope fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<'a> {
    /// The top-level `streamSid`, if present.
    pub stream_sid: Option<Cow<'a, str>>,
    /// `sequenceNumber`, if present and numeric.
    pub sequence: Option<u64>,
    /// The message.
    pub message: Inbound<'a>,
}

/// Parses one text frame from Twilio.
///
/// # Errors
/// [`ProtocolError::InvalidJson`] if the text is not a JSON object with a string `event` (or a
/// field has the wrong type), [`ProtocolError::Invalid`] if a known `event` lacks a field it
/// needs (`start.streamSid`, `media.payload`, `mark.name`, a single-key `dtmf.digit`).
pub fn parse(text: &str) -> Result<Parsed<'_>, ProtocolError> {
    let Head {
        event,
        stream_sid,
        sequence_number,
    } = body(text)?;
    let message = match event.as_ref() {
        "connected" => {
            // `connected` is informational and optional: what cannot be read is left out.
            let ConnectedMsg { protocol, version } = body(text).unwrap_or_default();
            Inbound::Connected {
                protocol: protocol.0,
                version: version.0,
            }
        }
        "start" => {
            let StartMsg { start } = body(text)?;
            let start = start.ok_or(ProtocolError::Invalid {
                event: "start",
                what: "no start object",
            })?;
            let stream_sid = start
                .stream_sid
                .clone()
                .or_else(|| stream_sid.as_ref().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .ok_or(ProtocolError::Invalid {
                    event: "start",
                    what: "no streamSid",
                })?;
            let format = start.media_format.unwrap_or_default();
            Inbound::Start(Box::new(StartInfo {
                stream_sid,
                call_sid: start.call_sid.unwrap_or_default(),
                account_sid: start.account_sid,
                tracks: start.tracks,
                encoding: format.encoding,
                sample_rate: format.sample_rate.map(|n| n.0),
                channels: format.channels.map(|n| n.0),
                custom_parameters: start
                    .custom_parameters
                    .into_iter()
                    .map(|(name, value)| match value {
                        serde_json::Value::String(text) => (name, text),
                        other => (name, other.to_string()),
                    })
                    .collect(),
            }))
        }
        "media" => {
            let MediaMsg { media } = body(text)?;
            let media = media.ok_or(ProtocolError::Invalid {
                event: "media",
                what: "no media object",
            })?;
            let payload = media.payload.ok_or(ProtocolError::Invalid {
                event: "media",
                what: "no payload",
            })?;
            Inbound::Media {
                track: media.track,
                chunk: media.chunk.0,
                timestamp_ms: media.timestamp.0,
                payload,
            }
        }
        "dtmf" => {
            let DtmfMsg { dtmf } = body(text)?;
            let digit = dtmf
                .and_then(|dtmf| dtmf.digit)
                .ok_or(ProtocolError::Invalid {
                    event: "dtmf",
                    what: "no digit",
                })?;
            let mut chars = digit.chars();
            let key = match (chars.next(), chars.next()) {
                (Some(key), None) => DtmfDigit::try_from(key).ok(),
                _ => None,
            };
            Inbound::Dtmf(key.ok_or(ProtocolError::Invalid {
                event: "dtmf",
                what: "digit is not a single DTMF key",
            })?)
        }
        "mark" => {
            let MarkMsg { mark } = body(text)?;
            let name = mark
                .and_then(|mark| mark.name)
                .ok_or(ProtocolError::Invalid {
                    event: "mark",
                    what: "no name",
                })?;
            Inbound::Mark { name }
        }
        "stop" => Inbound::Stop,
        _ => Inbound::Unknown(event),
    };
    Ok(Parsed {
        stream_sid,
        sequence: sequence_number.0,
        message,
    })
}

/// Builds the messages brisvox sends to Twilio for one stream. The `streamSid` is escaped once
/// and kept in the fixed parts of each message, so building a `media` message is a copy plus a
/// base64 encode.
#[derive(Debug, Clone)]
pub struct Outbound {
    media_prefix: String,
    clear: String,
    mark_prefix: String,
}

/// End of a `media` message after the payload.
const MEDIA_SUFFIX: &str = "\"}}";

impl Outbound {
    /// Messages for the stream `stream_sid` (the id from `start`; Twilio rejects any other).
    pub fn new(stream_sid: &str) -> Self {
        let sid = serde_json::Value::String(stream_sid.to_owned()).to_string();
        Self {
            media_prefix: format!(r#"{{"event":"media","streamSid":{sid},"media":{{"payload":""#),
            clear: format!(r#"{{"event":"clear","streamSid":{sid}}}"#),
            mark_prefix: format!(r#"{{"event":"mark","streamSid":{sid},"mark":{{"name":""#),
        }
    }

    /// Appends a `media` message carrying `mulaw` (raw G.711 mu-law, 8 kHz, no header) to `out`.
    pub fn write_media(&self, mulaw: &[u8], out: &mut BytesMut) {
        let encoded = base64::encoded_len(mulaw.len(), true).unwrap_or(0);
        out.reserve(self.media_prefix.len() + encoded + MEDIA_SUFFIX.len());
        out.extend_from_slice(self.media_prefix.as_bytes());
        let start = out.len();
        out.resize(start + encoded, 0);
        if let Some(target) = out.get_mut(start..) {
            // The target is sized exactly, so the encode cannot run out of space.
            let _ = BASE64.encode_slice(mulaw, target);
        }
        out.extend_from_slice(MEDIA_SUFFIX.as_bytes());
    }

    /// The `clear` message.
    pub fn clear(&self) -> &str {
        &self.clear
    }

    /// The `mark` message for `name`.
    pub fn mark(&self, name: MarkName) -> String {
        format!("{}{name}\"}}}}", self.mark_prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Parsed<'_> {
        parse(text).unwrap()
    }

    #[test]
    fn the_doc_examples_parse() {
        let connected =
            parsed(r#"{ "event": "connected", "protocol": "Call", "version": "1.0.0" }"#);
        assert_eq!(
            connected.message,
            Inbound::Connected {
                protocol: Some("Call".to_owned()),
                version: Some("1.0.0".to_owned())
            }
        );

        let start = parsed(
            r#"{
              "event": "start",
              "sequenceNumber": "1",
              "start": {
                "accountSid": "ACXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "callSid": "CAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "tracks": [ "inbound" ],
                "mediaFormat": { "encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1 },
                "customParameters": { "FirstName": "Jane", "LastName": "Doe", "RemoteParty": "Bob" }
              },
              "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
            }"#,
        );
        assert_eq!(start.sequence, Some(1));
        let Inbound::Start(info) = start.message else {
            panic!("not a start");
        };
        assert_eq!(info.stream_sid, "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX");
        assert_eq!(info.call_sid, "CAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX");
        assert_eq!(
            info.account_sid.as_deref(),
            Some("ACXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX")
        );
        assert_eq!(info.tracks, ["inbound"]);
        assert_eq!(info.encoding.as_deref(), Some(MULAW_ENCODING));
        assert_eq!((info.sample_rate, info.channels), (Some(8000), Some(1)));
        assert_eq!(info.custom_parameters["FirstName"], "Jane");
        assert_eq!(info.custom_parameters.len(), 3);

        let media = parsed(
            r#"{
              "event": "media",
              "sequenceNumber": "4",
              "media": { "track": "inbound", "chunk": "2", "timestamp": "5", "payload": "no+JhoaJjpzS..." },
              "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
            }"#,
        );
        assert_eq!(media.sequence, Some(4));
        assert_eq!(
            media.message,
            Inbound::Media {
                track: Some("inbound".into()),
                chunk: Some(2),
                timestamp_ms: Some(5),
                payload: "no+JhoaJjpzS...".into()
            }
        );

        let stop = parsed(
            r#"{ "event": "stop", "sequenceNumber": "5",
                 "stop": { "accountSid": "AC1", "callSid": "CA1" }, "streamSid": "MZ1" }"#,
        );
        assert_eq!(stop.message, Inbound::Stop);
        assert_eq!(stop.stream_sid.as_deref(), Some("MZ1"));

        let dtmf = parsed(
            r#"{ "event": "dtmf", "streamSid": "MZ1", "sequenceNumber": "5",
                 "dtmf": { "track": "inbound_track", "digit": "1" } }"#,
        );
        assert_eq!(dtmf.message, Inbound::Dtmf(DtmfDigit::D1));

        let mark = parsed(
            r#"{ "event": "mark", "sequenceNumber": "4", "streamSid": "MZ1",
                 "mark": { "name": "my label" } }"#,
        );
        assert_eq!(
            mark.message,
            Inbound::Mark {
                name: "my label".into()
            }
        );
    }

    #[test]
    fn numbers_may_be_strings_or_numbers() {
        let a = parsed(
            r#"{"event":"media","sequenceNumber":7,"media":{"chunk":3,"timestamp":"60","payload":"AA=="}}"#,
        );
        assert_eq!(a.sequence, Some(7));
        assert!(matches!(
            a.message,
            Inbound::Media {
                chunk: Some(3),
                timestamp_ms: Some(60),
                ..
            }
        ));
        // Start with string-typed format numbers.
        let b = parsed(
            r#"{"event":"start","start":{"streamSid":"MZ1","mediaFormat":{"sampleRate":"8000","channels":"1"}}}"#,
        );
        let Inbound::Start(info) = b.message else {
            panic!("not a start");
        };
        assert_eq!((info.sample_rate, info.channels), (Some(8000), Some(1)));
        // The top-level streamSid is enough when start.streamSid is missing.
        let c = parsed(r#"{"event":"start","streamSid":"MZ2","start":{}}"#);
        let Inbound::Start(info) = c.message else {
            panic!("not a start");
        };
        assert_eq!(info.stream_sid, "MZ2");
        assert!(info.custom_parameters.is_empty());
    }

    #[test]
    fn fields_the_adapter_does_not_use_never_fail_a_message() {
        // Floats, empty strings, nulls, bools, negatives, arrays and objects where a number is
        // expected: the media message still parses and the value reads as absent.
        for odd in [
            r#""1.0""#,
            r#""5.5""#,
            r#""""#,
            "5.5",
            "null",
            "true",
            "-1",
            "[1,2]",
            r#"{"a":[1]}"#,
        ] {
            let text = format!(
                r#"{{"event":"media","sequenceNumber":{odd},"media":{{"chunk":{odd},"timestamp":{odd},"payload":"AA=="}}}}"#
            );
            let p = parse(&text).unwrap_or_else(|e| panic!("{odd}: {e}"));
            assert_eq!(p.sequence, None, "{odd}");
            assert!(
                matches!(
                    p.message,
                    Inbound::Media {
                        chunk: None,
                        timestamp_ms: None,
                        ..
                    }
                ),
                "{odd}"
            );
        }
    }

    #[test]
    fn a_connected_message_with_odd_types_is_never_an_error() {
        let p = parsed(r#"{"event":"connected","protocol":{"x":1},"version":1}"#);
        assert_eq!(
            p.message,
            Inbound::Connected {
                protocol: None,
                version: Some("1".to_owned())
            }
        );
        let q = parsed(
            r#"{"event":"connected","protocol":null,"version":["1"],"sequenceNumber":"1.0"}"#,
        );
        assert_eq!(
            q.message,
            Inbound::Connected {
                protocol: None,
                version: None
            }
        );
    }

    #[test]
    fn non_string_custom_parameters_are_rendered_as_text() {
        let p = parsed(
            r#"{"event":"start","start":{"streamSid":"MZ1","customParameters":{"n":5,"b":true,"s":"x"}}}"#,
        );
        let Inbound::Start(info) = p.message else {
            panic!("not a start");
        };
        assert_eq!(info.custom_parameters["n"], "5");
        assert_eq!(info.custom_parameters["b"], "true");
        assert_eq!(info.custom_parameters["s"], "x");
    }

    #[test]
    fn unknown_events_and_fields_are_ignored() {
        let p = parsed(r#"{"event":"future","streamSid":"MZ1","media":7,"extra":{"a":1}}"#);
        assert_eq!(p.message, Inbound::Unknown("future".into()));
        let q = parsed(r#"{"event":"mark","mark":{"name":"x","other":1},"new":true}"#);
        assert!(matches!(q.message, Inbound::Mark { .. }));
    }

    #[test]
    fn malformed_messages_are_errors() {
        for text in [
            "",
            "nope",
            "[]",
            r#"{"streamSid":"MZ1"}"#,
            r#"{"event":5}"#,
            r#"{"event":"media"}"#,
            r#"{"event":"media","media":{"track":"inbound"}}"#,
            r#"{"event":"mark"}"#,
            r#"{"event":"mark","mark":{}}"#,
            r#"{"event":"dtmf","dtmf":{"digit":"12"}}"#,
            r#"{"event":"dtmf","dtmf":{"digit":"x"}}"#,
            r#"{"event":"dtmf","dtmf":{}}"#,
            r#"{"event":"start"}"#,
            r#"{"event":"start","start":{"callSid":"CA1"}}"#,
            r#"{"event":"start","start":{"streamSid":""}}"#,
        ] {
            assert!(parse(text).is_err(), "{text:?} should not parse");
        }
    }

    #[test]
    fn all_dtmf_keys_parse() {
        for key in "0123456789*#ABCD".chars() {
            let text = format!(r#"{{"event":"dtmf","dtmf":{{"digit":"{key}"}}}}"#);
            let Inbound::Dtmf(digit) = parse(&text).unwrap().message else {
                panic!("not a dtmf");
            };
            assert_eq!(digit.as_char(), key);
        }
    }

    #[test]
    fn outbound_messages_have_the_documented_shape() {
        let out = Outbound::new("MZ123");
        let mut buf = BytesMut::new();
        out.write_media(&[0xFF, 0x7F, 0x00], &mut buf);
        let json: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "event": "media",
                "streamSid": "MZ123",
                "media": { "payload": "/38A" }
            })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(out.clear()).unwrap(),
            serde_json::json!({ "event": "clear", "streamSid": "MZ123" })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out.mark(MarkName::Engine(9))).unwrap(),
            serde_json::json!({ "event": "mark", "streamSid": "MZ123", "mark": { "name": "e9" } })
        );
    }

    #[test]
    fn a_stream_sid_is_escaped_so_the_json_stays_valid() {
        let out = Outbound::new("MZ\"\\\n1");
        let mut buf = BytesMut::new();
        out.write_media(&[1, 2, 3, 4], &mut buf);
        let json: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["streamSid"], "MZ\"\\\n1");
        let clear: serde_json::Value = serde_json::from_str(out.clear()).unwrap();
        assert_eq!(clear["streamSid"], "MZ\"\\\n1");
    }

    #[test]
    fn media_payloads_round_trip_for_every_length() {
        let out = Outbound::new("MZ1");
        for len in 0..=40usize {
            let data: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i * 7 % 256).unwrap())
                .collect();
            let mut buf = BytesMut::new();
            out.write_media(&data, &mut buf);
            let text = std::str::from_utf8(&buf).unwrap();
            let Inbound::Media { payload, .. } = parse(text).unwrap().message else {
                panic!("not a media message");
            };
            assert_eq!(
                BASE64.decode(payload.as_bytes()).unwrap(),
                data,
                "len {len}"
            );
        }
    }

    #[test]
    fn unpadded_base64_is_accepted_on_input() {
        assert_eq!(BASE64.decode("/38").unwrap(), [0xFF, 0x7F]);
        assert_eq!(BASE64.decode("/38=").unwrap(), [0xFF, 0x7F]);
    }
}
