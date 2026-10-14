//! The control protocol of the websocket transport, version 1 (see `docs/WEBSOCKET_PROTOCOL.md`).
//!
//! Binary frames carry audio. Text frames carry one JSON object each, tagged by `type`:
//!
//! | direction | `type` | fields |
//! |---|---|---|
//! | client to server | `start` | `version`, `sample_rate`, `encoding`, `call_id`, `metadata` |
//! | client to server | `dtmf` | `digit` |
//! | client to server | `mark` | `id` (echo: the audio before the mark has played) |
//! | client to server | `stop` | |
//! | server to client | `started` | `version`, `call_id`, `sample_rate`, `encoding` |
//! | server to client | `clear` | `epoch` |
//! | server to client | `mark` | `id` |
//! | server to client | `stop` | `reason` |
//! | server to client | `error` | `code`, `message`, `fatal` |
//!
//! Unknown fields are ignored. An unknown `type` is a non-fatal fault ([`ProtocolError::UnknownType`]);
//! every other violation is fatal ([`ProtocolError::is_fatal`]).

use std::collections::BTreeMap;

use brisvox_audio::Codec;
use brisvox_core::frame::{DtmfDigit, SampleRate};
use serde::{Deserialize, Serialize};

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Most metadata entries a `start` message may carry.
pub const MAX_METADATA_ENTRIES: usize = 32;
/// Longest metadata key, in bytes.
pub const MAX_METADATA_KEY: usize = 64;
/// Longest metadata value, in bytes.
pub const MAX_METADATA_VALUE: usize = 256;
/// Longest `call_id` the client may propose, in bytes.
pub const MAX_CALL_ID: usize = 128;
/// Longest mark id, in bytes. The client's echo of a longer one is a protocol violation, so the
/// server does not send marks with longer ids (`mark_id_too_long`).
pub const MAX_MARK_ID: usize = 256;
/// Longest error message the parser keeps from a serde error, in bytes.
const MAX_ERROR_DETAIL: usize = 160;

/// Error `code` values of the `error` message (and of [`ProtocolError::code`]).
pub mod code {
    /// The text frame is not valid JSON, or lacks a `type`.
    pub const INVALID_JSON: &str = "invalid_json";
    /// A known message has missing or invalid fields.
    pub const INVALID_MESSAGE: &str = "invalid_message";
    /// A message `type` this version does not know (non-fatal).
    pub const UNKNOWN_MESSAGE: &str = "unknown_message";
    /// `start` asked for another protocol version.
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    /// `start` asked for a sample rate the engine does not support.
    pub const UNSUPPORTED_SAMPLE_RATE: &str = "unsupported_sample_rate";
    /// A second `start`.
    pub const UNEXPECTED_START: &str = "unexpected_start";
    /// Audio before `start`.
    pub const AUDIO_BEFORE_START: &str = "audio_before_start";
    /// No `start` within the start timeout.
    pub const START_TIMEOUT: &str = "start_timeout";
    /// A binary frame that is not whole samples.
    pub const BAD_AUDIO: &str = "bad_audio";
    /// A frame or message above the configured limit.
    pub const FRAME_TOO_LARGE: &str = "frame_too_large";
    /// Too many non-fatal faults on one call.
    pub const TOO_MANY_ERRORS: &str = "too_many_errors";
    /// The engine asked for a mark whose id is longer than [`MAX_MARK_ID`](super::MAX_MARK_ID)
    /// (non-fatal; the mark is not sent).
    pub const MARK_ID_TOO_LONG: &str = "mark_id_too_long";
    /// Bot audio at another sample rate than the call's (non-fatal, once per call).
    pub const OUTPUT_RATE_MISMATCH: &str = "output_rate_mismatch";
    /// The server is at its call limit.
    pub const SERVER_BUSY: &str = "server_busy";
    /// The engine takes no input from the call.
    pub const OVERLOADED: &str = "overloaded";
    /// No frame from the client for the ping timeout.
    pub const PING_TIMEOUT: &str = "ping_timeout";
    /// The client does not take output (write timeout).
    pub const SLOW_CLIENT: &str = "slow_client";
}

/// `reason` values of the server's `stop` message.
pub mod stop {
    /// The call ended: the engine hung up, or the client's own `stop` was processed.
    pub const ENDED: &str = "ended";
    /// The server is shutting down.
    pub const SHUTDOWN: &str = "shutdown";
}

/// How audio bytes are encoded in binary frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// 16-bit signed little-endian PCM, mono.
    #[default]
    Pcm16,
    /// G.711 μ-law, one byte per sample, mono.
    Mulaw,
}

impl Encoding {
    /// The codec that converts this encoding from and to PCM16.
    pub const fn codec(self) -> Codec {
        match self {
            Self::Pcm16 => Codec::Pcm16,
            Self::Mulaw => Codec::Ulaw,
        }
    }

    /// The name used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pcm16 => "pcm16",
            Self::Mulaw => "mulaw",
        }
    }
}

/// The client's first message: what the audio looks like and who is calling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// Protocol version; must be [`PROTOCOL_VERSION`].
    pub version: u32,
    /// Sample rate of the audio in both directions.
    pub sample_rate: u32,
    /// Audio encoding in both directions. Default `pcm16`.
    #[serde(default)]
    pub encoding: Encoding,
    /// A call id of the client's own. Kept as metadata `client_call_id`; the server assigns the
    /// call's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Free-form string metadata, passed to the engine in the call's metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl Start {
    /// A version 1 start for `sample_rate` and `encoding`, with no metadata.
    pub fn new(sample_rate: u32, encoding: Encoding) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            sample_rate,
            encoding,
            call_id: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Checks the message against the protocol limits and returns the negotiated rate.
    ///
    /// # Errors
    /// [`ProtocolError::Invalid`] naming the first violated rule.
    pub fn validate(&self) -> Result<SampleRate, ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::invalid(
                code::UNSUPPORTED_VERSION,
                format!(
                    "protocol version {} is not supported, this server speaks {PROTOCOL_VERSION}",
                    self.version
                ),
            ));
        }
        let rate = SampleRate::try_from(self.sample_rate).map_err(|_| {
            ProtocolError::invalid(
                code::UNSUPPORTED_SAMPLE_RATE,
                format!("sample rate {} Hz is not supported", self.sample_rate),
            )
        })?;
        if self
            .call_id
            .as_ref()
            .is_some_and(|id| id.len() > MAX_CALL_ID)
        {
            return Err(ProtocolError::invalid(
                code::INVALID_MESSAGE,
                format!("call_id is longer than {MAX_CALL_ID} bytes"),
            ));
        }
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(ProtocolError::invalid(
                code::INVALID_MESSAGE,
                format!("more than {MAX_METADATA_ENTRIES} metadata entries"),
            ));
        }
        if self
            .metadata
            .iter()
            .any(|(k, v)| k.len() > MAX_METADATA_KEY || v.len() > MAX_METADATA_VALUE)
        {
            return Err(ProtocolError::invalid(
                code::INVALID_MESSAGE,
                format!(
                    "a metadata key is longer than {MAX_METADATA_KEY} bytes or a value longer \
                     than {MAX_METADATA_VALUE} bytes"
                ),
            ));
        }
        Ok(rate)
    }
}

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// Opens the call; must be the first message.
    Start(Start),
    /// A DTMF key press.
    Dtmf {
        /// The key.
        digit: DtmfDigit,
    },
    /// The client has played everything before the mark `id`.
    Mark {
        /// The id of a `mark` the server sent.
        id: String,
    },
    /// The client hangs up.
    Stop,
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    /// The call is open.
    Started {
        /// Protocol version.
        version: u32,
        /// The server's id for the call.
        call_id: String,
        /// Sample rate of the audio in both directions.
        sample_rate: u32,
        /// Audio encoding in both directions.
        encoding: Encoding,
    },
    /// Barge-in: drop all buffered bot audio now.
    Clear {
        /// The epoch now current.
        epoch: u32,
    },
    /// Echo this id as a `mark` once the audio before it has played.
    Mark {
        /// The mark id.
        id: String,
    },
    /// The server ends the call and closes the connection.
    Stop {
        /// Why (see [`stop`]).
        reason: String,
    },
    /// A protocol fault. A fatal one is followed by a close.
    Error {
        /// Machine-readable code (see [`code`]).
        code: String,
        /// Human-readable detail; never credentials.
        message: String,
        /// Whether the server closes the connection.
        fatal: bool,
    },
}

/// Why a text frame is not an acceptable message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// Not JSON, not an object, or no `type`.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// A well-formed message of a type this version does not know. Ignorable.
    #[error("unknown message type {0:?}")]
    UnknownType(String),
    /// A known message with bad fields.
    #[error("{message}")]
    Invalid {
        /// One of the [`code`] constants.
        code: &'static str,
        /// What is wrong.
        message: String,
    },
}

impl ProtocolError {
    pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            code,
            message: message.into(),
        }
    }

    /// The [`code`] to report to the client.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson(_) => code::INVALID_JSON,
            Self::UnknownType(_) => code::UNKNOWN_MESSAGE,
            Self::Invalid { code, .. } => code,
        }
    }

    /// Whether the connection must be closed after reporting it.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::UnknownType(_))
    }
}

/// Cuts a rendered serde error down, so a hostile payload cannot make us echo kilobytes back.
fn short(error: &serde_json::Error) -> String {
    let mut text = error.to_string();
    if text.len() > MAX_ERROR_DETAIL {
        let mut end = MAX_ERROR_DETAIL;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct DtmfBody {
    digit: char,
}

#[derive(Deserialize)]
struct MarkBody {
    id: String,
}

fn body<'a, T: Deserialize<'a>>(text: &'a str, kind: &str) -> Result<T, ProtocolError> {
    serde_json::from_str(text).map_err(|e| {
        ProtocolError::invalid(
            code::INVALID_MESSAGE,
            format!("bad {kind} message: {}", short(&e)),
        )
    })
}

impl ClientMessage {
    /// Parses one text frame.
    ///
    /// # Errors
    /// [`ProtocolError`]; see [`ProtocolError::is_fatal`].
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let envelope: Envelope =
            serde_json::from_str(text).map_err(|e| ProtocolError::InvalidJson(short(&e)))?;
        match envelope.kind.as_str() {
            "start" => Ok(Self::Start(body(text, "start")?)),
            "dtmf" => {
                let DtmfBody { digit } = body(text, "dtmf")?;
                let digit = DtmfDigit::try_from(digit).map_err(|_| {
                    ProtocolError::invalid(code::INVALID_MESSAGE, "dtmf digit is not a DTMF key")
                })?;
                Ok(Self::Dtmf { digit })
            }
            "mark" => {
                let MarkBody { id } = body(text, "mark")?;
                if id.len() > MAX_MARK_ID {
                    return Err(ProtocolError::invalid(
                        code::INVALID_MESSAGE,
                        format!("mark id is longer than {MAX_MARK_ID} bytes"),
                    ));
                }
                Ok(Self::Mark { id })
            }
            "stop" => Ok(Self::Stop),
            other => Err(ProtocolError::UnknownType(other.chars().take(32).collect())),
        }
    }

    /// The message as a JSON text frame.
    pub fn to_json(&self) -> String {
        let value = match self {
            Self::Start(start) => {
                let mut value = serde_json::to_value(start).unwrap_or_default();
                if let Some(object) = value.as_object_mut() {
                    object.insert("type".to_owned(), "start".into());
                }
                value
            }
            Self::Dtmf { digit } => {
                serde_json::json!({"type": "dtmf", "digit": digit.as_char().to_string()})
            }
            Self::Mark { id } => serde_json::json!({"type": "mark", "id": id}),
            Self::Stop => serde_json::json!({"type": "stop"}),
        };
        value.to_string()
    }
}

#[derive(Deserialize)]
struct StartedBody {
    version: u32,
    call_id: String,
    sample_rate: u32,
    #[serde(default)]
    encoding: Encoding,
}

#[derive(Deserialize)]
struct ClearBody {
    epoch: u32,
}

#[derive(Deserialize)]
struct StopBody {
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    fatal: bool,
}

impl ServerMessage {
    /// An `error` message.
    pub fn error(code: &str, message: impl Into<String>, fatal: bool) -> Self {
        Self::Error {
            code: code.to_owned(),
            message: message.into(),
            fatal,
        }
    }

    /// Parses one text frame (used by clients).
    ///
    /// # Errors
    /// [`ProtocolError`]; an unknown `type` is [`ProtocolError::UnknownType`], which a client
    /// should ignore.
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let envelope: Envelope =
            serde_json::from_str(text).map_err(|e| ProtocolError::InvalidJson(short(&e)))?;
        match envelope.kind.as_str() {
            "started" => {
                let b: StartedBody = body(text, "started")?;
                Ok(Self::Started {
                    version: b.version,
                    call_id: b.call_id,
                    sample_rate: b.sample_rate,
                    encoding: b.encoding,
                })
            }
            "clear" => Ok(Self::Clear {
                epoch: body::<ClearBody>(text, "clear")?.epoch,
            }),
            "mark" => Ok(Self::Mark {
                id: body::<MarkBody>(text, "mark")?.id,
            }),
            "stop" => Ok(Self::Stop {
                reason: body::<StopBody>(text, "stop")?.reason,
            }),
            "error" => {
                let b: ErrorBody = body(text, "error")?;
                Ok(Self::Error {
                    code: b.code,
                    message: b.message,
                    fatal: b.fatal,
                })
            }
            other => Err(ProtocolError::UnknownType(other.chars().take(32).collect())),
        }
    }

    /// The message as a JSON text frame.
    pub fn to_json(&self) -> String {
        match self {
            Self::Started {
                version,
                call_id,
                sample_rate,
                encoding,
            } => serde_json::json!({
                "type": "started",
                "version": version,
                "call_id": call_id,
                "sample_rate": sample_rate,
                "encoding": encoding.as_str(),
            }),
            Self::Clear { epoch } => serde_json::json!({"type": "clear", "epoch": epoch}),
            Self::Mark { id } => serde_json::json!({"type": "mark", "id": id}),
            Self::Stop { reason } => serde_json::json!({"type": "stop", "reason": reason}),
            Self::Error {
                code,
                message,
                fatal,
            } => serde_json::json!({
                "type": "error",
                "code": code,
                "message": message,
                "fatal": fatal,
            }),
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_round_trips_and_defaults() {
        let mut start = Start::new(16_000, Encoding::Mulaw);
        start.call_id = Some("abc".to_owned());
        start.metadata.insert("k".to_owned(), "v".to_owned());
        let json = ClientMessage::Start(start.clone()).to_json();
        assert_eq!(
            ClientMessage::parse(&json).unwrap(),
            ClientMessage::Start(start)
        );

        let minimal = r#"{"type":"start","version":1,"sample_rate":8000,"extra":true}"#;
        let ClientMessage::Start(parsed) = ClientMessage::parse(minimal).unwrap() else {
            panic!("not a start");
        };
        assert_eq!(parsed.encoding, Encoding::Pcm16);
        assert!(parsed.metadata.is_empty());
        assert_eq!(parsed.validate().unwrap(), SampleRate::Hz8000);
    }

    #[test]
    fn start_validation_names_the_rule() {
        let mut start = Start::new(16_000, Encoding::Pcm16);
        start.version = 2;
        assert_eq!(
            start.validate().unwrap_err().code(),
            code::UNSUPPORTED_VERSION
        );
        let start = Start::new(12_345, Encoding::Pcm16);
        assert_eq!(
            start.validate().unwrap_err().code(),
            code::UNSUPPORTED_SAMPLE_RATE
        );
        let mut start = Start::new(16_000, Encoding::Pcm16);
        for i in 0..=MAX_METADATA_ENTRIES {
            start.metadata.insert(format!("k{i}"), String::new());
        }
        assert_eq!(start.validate().unwrap_err().code(), code::INVALID_MESSAGE);
        let mut start = Start::new(16_000, Encoding::Pcm16);
        start
            .metadata
            .insert("k".to_owned(), "v".repeat(MAX_METADATA_VALUE + 1));
        assert!(start.validate().is_err());
        let mut start = Start::new(16_000, Encoding::Pcm16);
        start.call_id = Some("x".repeat(MAX_CALL_ID + 1));
        assert!(start.validate().is_err());
    }

    #[test]
    fn control_messages_parse() {
        assert_eq!(
            ClientMessage::parse(r##"{"type":"dtmf","digit":"#"}"##).unwrap(),
            ClientMessage::Dtmf {
                digit: DtmfDigit::Pound
            }
        );
        assert_eq!(
            ClientMessage::parse(r#"{"type":"mark","id":"m1"}"#).unwrap(),
            ClientMessage::Mark {
                id: "m1".to_owned()
            }
        );
        assert_eq!(
            ClientMessage::parse(r#"{"type":"stop"}"#).unwrap(),
            ClientMessage::Stop
        );
    }

    #[test]
    fn violations_are_classified() {
        for bad in ["", "nope", "[]", "{}", r#"{"type":5}"#] {
            let e = ClientMessage::parse(bad).unwrap_err();
            assert_eq!(e.code(), code::INVALID_JSON, "{bad}");
            assert!(e.is_fatal());
        }
        for bad in [
            r#"{"type":"dtmf"}"#,
            r#"{"type":"dtmf","digit":"x"}"#,
            r#"{"type":"dtmf","digit":"12"}"#,
            r#"{"type":"mark"}"#,
            r#"{"type":"start","version":"one"}"#,
        ] {
            let e = ClientMessage::parse(bad).unwrap_err();
            assert_eq!(e.code(), code::INVALID_MESSAGE, "{bad}");
            assert!(e.is_fatal());
        }
        let long_id = format!(
            r#"{{"type":"mark","id":"{}"}}"#,
            "x".repeat(MAX_MARK_ID + 1)
        );
        assert!(ClientMessage::parse(&long_id).unwrap_err().is_fatal());
        let unknown = ClientMessage::parse(r#"{"type":"rewind"}"#).unwrap_err();
        assert_eq!(unknown.code(), code::UNKNOWN_MESSAGE);
        assert!(!unknown.is_fatal());
    }

    #[test]
    fn server_messages_round_trip() {
        let messages = [
            ServerMessage::Started {
                version: 1,
                call_id: "ws-1".to_owned(),
                sample_rate: 16_000,
                encoding: Encoding::Pcm16,
            },
            ServerMessage::Clear { epoch: 7 },
            ServerMessage::Mark { id: "m".to_owned() },
            ServerMessage::Stop {
                reason: stop::ENDED.to_owned(),
            },
            ServerMessage::error(code::BAD_AUDIO, "odd", true),
        ];
        for message in messages {
            assert_eq!(ServerMessage::parse(&message.to_json()).unwrap(), message);
        }
        assert_eq!(
            ServerMessage::parse(r#"{"type":"future"}"#)
                .unwrap_err()
                .code(),
            code::UNKNOWN_MESSAGE
        );
    }
}
