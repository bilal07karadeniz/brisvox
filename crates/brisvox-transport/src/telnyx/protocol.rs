//! The Telnyx media streaming messages (`docs/providers/telnyx.md`, fetched 2026-10-08; the JSON
//! shapes are those of Telnyx's AsyncAPI spec).
//!
//! Every frame is one JSON text frame tagged by `event`. Field names are snake_case, the stream id
//! is `stream_id` (a UUID) and, unlike Twilio, brisvox's own messages carry no stream id.
//!
//! | direction | `event` | fields used here |
//! |---|---|---|
//! | Telnyx to brisvox | `connected` | `version`, `connected.x-telnyx-streaming-auth-token` |
//! | Telnyx to brisvox | `start` | `stream_id`, `start.{call_control_id, call_session_id, user_id, from, to, tags, client_state, custom_parameters, media_format}` |
//! | Telnyx to brisvox | `media` | `stream_id`, `media.{track, chunk, timestamp, payload}` |
//! | Telnyx to brisvox | `dtmf` | `stream_id`, `dtmf.digit` |
//! | Telnyx to brisvox | `mark` | `stream_id`, `mark.name` |
//! | Telnyx to brisvox | `stop` | `stream_id` |
//! | Telnyx to brisvox | `error` | `stream_id`, `payload.{code, title, detail}` |
//! | brisvox to Telnyx | `media` | `media.payload` (base64 G.711, RTP payload without header) |
//! | brisvox to Telnyx | `mark` | `mark.name` |
//! | brisvox to Telnyx | `clear` | |
//!
//! Telnyx documents `sequence_number`, `media.chunk` and `media.timestamp` as strings and the
//! numbers inside `media_format` as numbers; both are read leniently where the value is used
//! (`media.chunk`, `start.media_format`). Fields the adapter only reads for statistics or not at
//! all are read softly: a value of an unexpected type reads as absent and is never a reason to
//! drop the message that carries it. Unknown fields are ignored and an unknown `event` parses to
//! [`Inbound::Unknown`] (forward compatibility). The server-to-Telnyx schemas forbid extra
//! properties, so this module builds exactly the three documented messages and nothing else.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use brisvox_audio::Codec;
use brisvox_core::Secret;
use brisvox_core::frame::DtmfDigit;
use bytes::BytesMut;
use serde::Deserialize;

use crate::telephony::{BASE64, Lenient, MarkName, Soft, SoftText, clipped_error};

/// The only sample rate this adapter converts, in Hz (G.711 on the wire).
pub const SAMPLE_RATE_HZ: u32 = 8000;

/// The G.711 codecs the adapter speaks, as Telnyx names them (`start.media_format.encoding`,
/// `stream_bidirectional_codec`). `L16`, `G722`, `OPUS` and `AMR-WB` are not supported: L16's byte
/// order is not documented (UNVERIFIED) and the others need codecs that are not in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelnyxCodec {
    /// `PCMU`: G.711 mu-law, 8 kHz. Telnyx's default for bidirectional audio.
    #[default]
    Pcmu,
    /// `PCMA`: G.711 A-law, 8 kHz.
    Pcma,
}

impl TelnyxCodec {
    /// The name Telnyx uses (`PCMU` or `PCMA`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::Pcma => "PCMA",
        }
    }

    /// The name in call metadata (`encoding`): `mulaw` or `alaw`.
    pub const fn metadata_name(self) -> &'static str {
        match self {
            Self::Pcmu => "mulaw",
            Self::Pcma => "alaw",
        }
    }

    /// Parses a Telnyx encoding name (case-insensitive); `None` for any codec not supported.
    pub fn parse(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("PCMU") {
            Some(Self::Pcmu)
        } else if name.eq_ignore_ascii_case("PCMA") {
            Some(Self::Pcma)
        } else {
            None
        }
    }

    /// The G.711 codec of the audio layer.
    pub const fn g711(self) -> Codec {
        match self {
            Self::Pcmu => Codec::Ulaw,
            Self::Pcma => Codec::Alaw,
        }
    }
}

/// A violation of the message format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The frame is not JSON, or a known field has the wrong type.
    #[error("not a valid telnyx message: {0}")]
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

/// The stream auth token as `connected` carried it. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenText(String);

impl TokenText {
    /// The token, for a constant-time comparison only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TokenText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Secret::<String>::REDACTED)
    }
}

/// `start.media_format`.
#[derive(Debug, Default, Deserialize)]
struct MediaFormatBody {
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    sample_rate: Option<Lenient>,
    #[serde(default)]
    channels: Option<Lenient>,
}

/// The `start` object as Telnyx sends it.
#[derive(Debug, Default, Deserialize)]
struct StartBody {
    #[serde(default)]
    user_id: SoftText,
    #[serde(default)]
    call_control_id: SoftText,
    #[serde(default)]
    call_session_id: SoftText,
    #[serde(default)]
    from: SoftText,
    #[serde(default)]
    to: SoftText,
    #[serde(default)]
    tags: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    client_state: SoftText,
    #[serde(default)]
    custom_parameters: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    media_format: Option<MediaFormatBody>,
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

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Soft,
    #[serde(default)]
    title: SoftText,
    #[serde(default)]
    detail: SoftText,
}

/// The envelope fields every message has. The rest of a frame is read per `event`, so an
/// unknown `event` with any other content still parses.
#[derive(Debug, Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    event: Cow<'a, str>,
    #[serde(default, borrow)]
    stream_id: Option<Cow<'a, str>>,
    #[serde(default)]
    sequence_number: Soft,
}

#[derive(Debug, Default, Deserialize)]
struct ConnectedBody {
    #[serde(default, rename = "x-telnyx-streaming-auth-token")]
    token: SoftText,
}

#[derive(Debug, Default, Deserialize)]
struct ConnectedMsg {
    #[serde(default)]
    version: SoftText,
    #[serde(default)]
    connected: Option<ConnectedBody>,
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

#[derive(Debug, Deserialize)]
struct ErrorMsg {
    #[serde(default)]
    payload: Option<ErrorBody>,
}

fn body<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, ProtocolError> {
    serde_json::from_str(text).map_err(|e| ProtocolError::json(&e))
}

/// What `start` told us about the stream, validated for presence but not for content (the
/// connection checks the media format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    /// The stream id (`stream_id`, the top-level field of the `start` message).
    pub stream_id: String,
    /// The call id used by Telnyx's REST API (`start.call_control_id`); empty when not sent.
    pub call_control_id: String,
    /// `start.call_session_id`, if sent.
    pub call_session_id: Option<String>,
    /// `start.user_id`, if sent.
    pub user_id: Option<String>,
    /// `start.from` (caller number or SIP URI), if sent.
    pub from: Option<String>,
    /// `start.to` (callee number or SIP URI), if sent.
    pub to: Option<String>,
    /// `start.tags`.
    pub tags: Vec<String>,
    /// `start.client_state` (base64, set by whoever started the stream), if sent.
    pub client_state: Option<String>,
    /// `start.media_format.encoding`, if sent.
    pub encoding: Option<String>,
    /// `start.media_format.sample_rate`, if sent.
    pub sample_rate: Option<u64>,
    /// `start.media_format.channels`, if sent.
    pub channels: Option<u64>,
    /// `start.custom_parameters` as strings (other JSON values are rendered as JSON text).
    pub custom_parameters: BTreeMap<String, String>,
}

/// One message from Telnyx.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound<'a> {
    /// `connected`: the first message.
    Connected {
        /// `version`, e.g. `1.0.0`.
        version: Option<String>,
        /// `connected.x-telnyx-streaming-auth-token`, sent when a stream auth token is configured.
        token: Option<TokenText>,
    },
    /// `start`: sent once, after `connected`.
    Start(Box<StartInfo>),
    /// `media`: caller audio.
    Media {
        /// `media.track`; `inbound` for the caller.
        track: Option<Cow<'a, str>>,
        /// `media.chunk`, the per-stream chunk number frames are reordered by.
        chunk: Option<u64>,
        /// `media.timestamp`, an RTP timestamp (not milliseconds).
        rtp_timestamp: Option<u64>,
        /// `media.payload`: base64 G.711.
        payload: Cow<'a, str>,
    },
    /// `dtmf`: a key press.
    Dtmf(DtmfDigit),
    /// `mark`: Telnyx echoes a mark brisvox sent (played, or flushed by a `clear`).
    Mark {
        /// `mark.name`.
        name: Cow<'a, str>,
    },
    /// `stop`: Telnyx stopped streaming for the call.
    Stop,
    /// `error`: Telnyx rejected something brisvox sent (`100002` to `100005`).
    Error {
        /// `payload.code`.
        code: Option<u64>,
        /// `payload.title`, e.g. `rate_limit_reached`.
        title: Option<String>,
        /// `payload.detail`.
        detail: Option<String>,
    },
    /// An `event` this version does not know; ignored.
    Unknown(Cow<'a, str>),
}

/// A parsed frame with its envelope fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<'a> {
    /// The top-level `stream_id`, if present.
    pub stream_id: Option<Cow<'a, str>>,
    /// `sequence_number`, if present and numeric.
    pub sequence: Option<u64>,
    /// The message.
    pub message: Inbound<'a>,
}

fn tag_text(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    }
}

/// Parses one text frame from Telnyx.
///
/// # Errors
/// [`ProtocolError::InvalidJson`] if the text is not a JSON object with a string `event` (or a
/// field has the wrong type), [`ProtocolError::Invalid`] if a known `event` lacks a field it
/// needs (`start.stream_id`, `media.payload`, `mark.name`, a single-key `dtmf.digit`).
pub fn parse(text: &str) -> Result<Parsed<'_>, ProtocolError> {
    let Head {
        event,
        stream_id,
        sequence_number,
    } = body(text)?;
    let message = match event.as_ref() {
        "connected" => {
            // `connected` is informational and optional: what cannot be read is left out.
            let ConnectedMsg { version, connected } = body(text).unwrap_or_default();
            Inbound::Connected {
                version: version.0,
                token: connected
                    .and_then(|c| c.token.0)
                    .filter(|token| !token.is_empty())
                    .map(TokenText),
            }
        }
        "start" => {
            let StartMsg { start } = body(text)?;
            let start = start.ok_or(ProtocolError::Invalid {
                event: "start",
                what: "no start object",
            })?;
            let stream_id = stream_id
                .as_ref()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .ok_or(ProtocolError::Invalid {
                    event: "start",
                    what: "no stream_id",
                })?;
            let format = start.media_format.unwrap_or_default();
            Inbound::Start(Box::new(StartInfo {
                stream_id,
                call_control_id: start.call_control_id.0.unwrap_or_default(),
                call_session_id: start.call_session_id.0,
                user_id: start.user_id.0,
                from: start.from.0,
                to: start.to.0,
                tags: start
                    .tags
                    .unwrap_or_default()
                    .into_iter()
                    .map(tag_text)
                    .collect(),
                client_state: start.client_state.0,
                encoding: format.encoding,
                sample_rate: format.sample_rate.map(|n| n.0),
                channels: format.channels.map(|n| n.0),
                custom_parameters: start
                    .custom_parameters
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, value)| (name, tag_text(value)))
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
                rtp_timestamp: media.timestamp.0,
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
        "error" => {
            // The details are for the engine's log: what cannot be read is left out.
            let ErrorMsg { payload } = body(text).unwrap_or(ErrorMsg { payload: None });
            let payload = payload.unwrap_or_default();
            Inbound::Error {
                code: payload.code.0,
                title: payload.title.0,
                detail: payload.detail.0,
            }
        }
        _ => Inbound::Unknown(event),
    };
    Ok(Parsed {
        stream_id,
        sequence: sequence_number.0,
        message,
    })
}

/// Start of a `media` message up to the payload.
const MEDIA_PREFIX: &str = r#"{"event":"media","media":{"payload":""#;

/// End of a `media` message after the payload.
const MEDIA_SUFFIX: &str = "\"}}";

/// The `clear` message (Telnyx stops the media playing and clears its queue).
pub const CLEAR: &str = r#"{"event":"clear"}"#;

/// Appends a `media` message carrying `g711` (raw G.711, 8 kHz, no header) to `out`.
pub fn write_media(g711: &[u8], out: &mut BytesMut) {
    let encoded = base64::encoded_len(g711.len(), true).unwrap_or(0);
    out.reserve(MEDIA_PREFIX.len() + encoded + MEDIA_SUFFIX.len());
    out.extend_from_slice(MEDIA_PREFIX.as_bytes());
    let start = out.len();
    out.resize(start + encoded, 0);
    if let Some(target) = out.get_mut(start..) {
        // The target is sized exactly, so the encode cannot run out of space.
        let _ = BASE64.encode_slice(g711, target);
    }
    out.extend_from_slice(MEDIA_SUFFIX.as_bytes());
}

/// The `mark` message for `name`.
pub fn mark(name: MarkName) -> String {
    format!(r#"{{"event":"mark","mark":{{"name":"{name}"}}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Parsed<'_> {
        parse(text).unwrap()
    }

    #[test]
    fn the_doc_examples_parse() {
        let connected = parsed(r#"{ "event": "connected", "version": "1.0.0" }"#);
        assert_eq!(
            connected.message,
            Inbound::Connected {
                version: Some("1.0.0".to_owned()),
                token: None
            }
        );
        let with_token = parsed(
            r#"{ "event": "connected", "connected": { "x-telnyx-streaming-auth-token": "3c8b06a4" }, "version": "1.0.0" }"#,
        );
        let Inbound::Connected { token, .. } = with_token.message else {
            panic!("not connected");
        };
        assert_eq!(token.unwrap().expose(), "3c8b06a4");

        let start = parsed(
            r#"{
              "event": "start",
              "sequence_number": "1",
              "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
              "start": {
                "user_id": "3E6F995F-85F7-4705-9741-53B116D28237",
                "call_control_id": "v2:T02llQxIyaRkhfRKxgAP8nY511EhFLizdvdUKJiSw8d6A9BborherQ",
                "call_session_id": "ff55a038-6f5d-11ef-9692-02420aeffb1f",
                "from": "+13122010094",
                "to": "+13122123456",
                "tags": [ "TAG1", "TAG2" ],
                "client_state": "aGF2ZSBhIG5pY2UgZGF5ID1d",
                "custom_parameters": { "customer_id": "customer_123" },
                "media_format": { "encoding": "PCMU", "sample_rate": 8000, "channels": 1 }
              }
            }"#,
        );
        assert_eq!(start.sequence, Some(1));
        let Inbound::Start(info) = start.message else {
            panic!("not a start");
        };
        assert_eq!(info.stream_id, "32DE0DEA-53CB-4B21-89A4-9E1819C043BC");
        assert_eq!(
            info.call_control_id,
            "v2:T02llQxIyaRkhfRKxgAP8nY511EhFLizdvdUKJiSw8d6A9BborherQ"
        );
        assert_eq!(
            info.call_session_id.as_deref(),
            Some("ff55a038-6f5d-11ef-9692-02420aeffb1f")
        );
        assert_eq!(info.from.as_deref(), Some("+13122010094"));
        assert_eq!(info.to.as_deref(), Some("+13122123456"));
        assert_eq!(info.tags, ["TAG1", "TAG2"]);
        assert_eq!(
            info.client_state.as_deref(),
            Some("aGF2ZSBhIG5pY2UgZGF5ID1d")
        );
        assert_eq!(info.encoding.as_deref(), Some("PCMU"));
        assert_eq!((info.sample_rate, info.channels), (Some(8000), Some(1)));
        assert_eq!(info.custom_parameters["customer_id"], "customer_123");

        let media = parsed(
            r#"{
              "event": "media",
              "sequence_number": "4",
              "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
              "media": { "track": "inbound", "chunk": "2", "timestamp": "5", "payload": "no+JhoaJjpzS" }
            }"#,
        );
        assert_eq!(media.sequence, Some(4));
        assert_eq!(
            media.stream_id.as_deref(),
            Some("32DE0DEA-53CB-4B21-89A4-9E1819C043BC")
        );
        assert_eq!(
            media.message,
            Inbound::Media {
                track: Some("inbound".into()),
                chunk: Some(2),
                rtp_timestamp: Some(5),
                payload: "no+JhoaJjpzS".into()
            }
        );

        let dtmf = parsed(
            r#"{ "event": "dtmf", "stream_id": "S", "occurred_at": "2025-06-05T08:54:19.698408Z",
                 "sequence_number": "5", "dtmf": { "digit": "1" } }"#,
        );
        assert_eq!(dtmf.message, Inbound::Dtmf(DtmfDigit::D1));

        let mark = parsed(
            r#"{ "event": "mark", "sequence_number": "5", "stream_id": "S",
                 "mark": { "name": "some_mark_name" } }"#,
        );
        assert_eq!(
            mark.message,
            Inbound::Mark {
                name: "some_mark_name".into()
            }
        );

        let stop = parsed(
            r#"{ "event": "stop", "sequence_number": "5", "stream_id": "S",
                 "stop": { "user_id": "U", "call_control_id": "C" } }"#,
        );
        assert_eq!(stop.message, Inbound::Stop);

        let error = parsed(
            r#"{ "event": "error", "stream_id": "S",
                 "payload": { "code": 100004, "title": "invalid_media",
                              "detail": "Media provided was not base64 encoded" } }"#,
        );
        assert_eq!(
            error.message,
            Inbound::Error {
                code: Some(100_004),
                title: Some("invalid_media".to_owned()),
                detail: Some("Media provided was not base64 encoded".to_owned())
            }
        );
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let p = parsed(
            r#"{"event":"connected","connected":{"x-telnyx-streaming-auth-token":"hunter2-secret"}}"#,
        );
        assert!(!format!("{p:?}").contains("hunter2-secret"));
        assert!(format!("{p:?}").contains("REDACTED"));
    }

    #[test]
    fn numbers_may_be_strings_or_numbers() {
        let a = parsed(
            r#"{"event":"media","sequence_number":7,"media":{"chunk":3,"timestamp":60,"payload":"AA=="}}"#,
        );
        assert_eq!(a.sequence, Some(7));
        assert!(matches!(
            a.message,
            Inbound::Media {
                chunk: Some(3),
                rtp_timestamp: Some(60),
                ..
            }
        ));
        let b = parsed(
            r#"{"event":"start","stream_id":"S","start":{"media_format":{"sample_rate":"8000","channels":"1"}}}"#,
        );
        let Inbound::Start(info) = b.message else {
            panic!("not a start");
        };
        assert_eq!((info.sample_rate, info.channels), (Some(8000), Some(1)));
        assert_eq!(info.call_control_id, "");
        assert!(info.custom_parameters.is_empty());
    }

    #[test]
    fn fields_the_adapter_does_not_use_never_fail_a_message() {
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
                r#"{{"event":"media","sequence_number":{odd},"media":{{"chunk":{odd},"timestamp":{odd},"payload":"AA=="}}}}"#
            );
            let p = parse(&text).unwrap_or_else(|e| panic!("{odd}: {e}"));
            assert_eq!(p.sequence, None, "{odd}");
            assert!(
                matches!(
                    p.message,
                    Inbound::Media {
                        chunk: None,
                        rtp_timestamp: None,
                        ..
                    }
                ),
                "{odd}"
            );
        }
    }

    #[test]
    fn a_connected_message_with_odd_types_is_never_an_error() {
        let p = parsed(r#"{"event":"connected","version":1,"connected":"x"}"#);
        assert_eq!(
            p.message,
            Inbound::Connected {
                version: None,
                token: None
            }
        );
        let q = parsed(
            r#"{"event":"connected","version":["1"],"connected":{"x-telnyx-streaming-auth-token":5}}"#,
        );
        assert!(matches!(
            q.message,
            Inbound::Connected {
                version: None,
                token: Some(_)
            }
        ));
    }

    #[test]
    fn non_string_start_fields_are_rendered_as_text() {
        let p = parsed(
            r#"{"event":"start","stream_id":"S","start":{"tags":["a",1],"custom_parameters":{"n":5,"b":true,"s":"x"},"from":null}}"#,
        );
        let Inbound::Start(info) = p.message else {
            panic!("not a start");
        };
        assert_eq!(info.tags, ["a", "1"]);
        assert_eq!(info.custom_parameters["n"], "5");
        assert_eq!(info.custom_parameters["b"], "true");
        assert_eq!(info.custom_parameters["s"], "x");
        assert_eq!(info.from, None);
    }

    #[test]
    fn unknown_events_and_fields_are_ignored() {
        let p = parsed(r#"{"event":"future","stream_id":"S","media":7,"extra":{"a":1}}"#);
        assert_eq!(p.message, Inbound::Unknown("future".into()));
        let q = parsed(r#"{"event":"mark","mark":{"name":"x","other":1},"new":true}"#);
        assert!(matches!(q.message, Inbound::Mark { .. }));
        // An error frame whose payload is not an object still reports as an error.
        let r = parsed(r#"{"event":"error","payload":"boom"}"#);
        assert_eq!(
            r.message,
            Inbound::Error {
                code: None,
                title: None,
                detail: None
            }
        );
    }

    #[test]
    fn malformed_messages_are_errors() {
        for text in [
            "",
            "nope",
            "[]",
            r#"{"stream_id":"S"}"#,
            r#"{"event":5}"#,
            r#"{"event":"media"}"#,
            r#"{"event":"media","media":{"track":"inbound"}}"#,
            r#"{"event":"mark"}"#,
            r#"{"event":"mark","mark":{}}"#,
            r#"{"event":"dtmf","dtmf":{"digit":"12"}}"#,
            r#"{"event":"dtmf","dtmf":{"digit":"x"}}"#,
            r#"{"event":"dtmf","dtmf":{}}"#,
            r#"{"event":"start"}"#,
            r#"{"event":"start","start":{"call_control_id":"C"}}"#,
            r#"{"event":"start","stream_id":"","start":{}}"#,
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
    fn codecs_parse_by_telnyx_name() {
        assert_eq!(TelnyxCodec::parse("PCMU"), Some(TelnyxCodec::Pcmu));
        assert_eq!(TelnyxCodec::parse("pcma"), Some(TelnyxCodec::Pcma));
        for unsupported in ["L16", "G722", "OPUS", "AMR-WB", "", "audio/x-mulaw"] {
            assert_eq!(TelnyxCodec::parse(unsupported), None, "{unsupported:?}");
        }
        assert_eq!(TelnyxCodec::Pcmu.name(), "PCMU");
        assert_eq!(TelnyxCodec::Pcma.metadata_name(), "alaw");
        assert_eq!(TelnyxCodec::default(), TelnyxCodec::Pcmu);
    }

    #[test]
    fn outbound_messages_have_the_documented_shape_and_no_stream_id() {
        let mut buf = BytesMut::new();
        write_media(&[0xFF, 0x7F, 0x00], &mut buf);
        let json: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "event": "media", "media": { "payload": "/38A" } })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(CLEAR).unwrap(),
            serde_json::json!({ "event": "clear" })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&mark(MarkName::Engine(9))).unwrap(),
            serde_json::json!({ "event": "mark", "mark": { "name": "e9" } })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&mark(MarkName::Progress(3))).unwrap(),
            serde_json::json!({ "event": "mark", "mark": { "name": "p3" } })
        );
    }

    #[test]
    fn media_payloads_round_trip_for_every_length() {
        for len in 0..=40usize {
            let data: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i * 7 % 256).unwrap())
                .collect();
            let mut buf = BytesMut::new();
            write_media(&data, &mut buf);
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
}
