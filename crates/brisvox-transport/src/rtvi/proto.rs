//! The protobuf framing of the Pipecat websocket serializer: `pipecat.Frame`.
//!
//! Every websocket message of an RTVI call is ONE binary frame holding ONE serialized [`Frame`]
//! (`docs/providers/rtvi.md`, "Pipecat WebSocket serializer"). The messages are derived by hand
//! with `#[derive(prost::Message)]` from the schema of `pipecat/frames/frames.proto` (Pipecat
//! v1.12.0), so no `protoc` and no build script is needed. Field numbers and types are exactly
//! those of the schema; the schema text itself is not copied.
//!
//! ```proto
//! message TextFrame          { uint64 id = 1; string name = 2; string text = 3; }
//! message AudioRawFrame      { uint64 id = 1; string name = 2; bytes audio = 3;
//!                              uint32 sample_rate = 4; uint32 num_channels = 5;
//!                              optional uint64 pts = 6; }
//! message TranscriptionFrame { uint64 id = 1; string name = 2; string text = 3;
//!                              string user_id = 4; string timestamp = 5; }
//! message MessageFrame       { string data = 1; }
//! message InterruptionFrame  { uint64 id = 1; string name = 2; }
//! message Frame { oneof frame { TextFrame text = 1; AudioRawFrame audio = 2;
//!                 TranscriptionFrame transcription = 3; MessageFrame message = 4;
//!                 InterruptionFrame interruption = 5; } }
//! ```
//!
//! The `@pipecat-ai/websocket-transport` JS client has no `InterruptionFrame` (no field 5 in its
//! `Frame`), so this crate decodes field 5 from a client but never encodes it.

use bytes::{Bytes, BytesMut};
use prost::Message;

/// `pipecat.TextFrame`. Decoded and ignored.
#[derive(Clone, PartialEq, Message)]
pub struct TextFrame {
    /// Frame id (zero is omitted on the wire).
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Frame name.
    #[prost(string, tag = "2")]
    pub name: String,
    /// The text.
    #[prost(string, tag = "3")]
    pub text: String,
}

/// `pipecat.AudioRawFrame`: raw PCM16 little-endian audio.
#[derive(Clone, PartialEq, Message)]
pub struct AudioRawFrame {
    /// Frame id (the JS client sends zero, which is omitted on the wire).
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Frame name (the JS client sends `"audio"`). A `string` on the wire; held as raw bytes
    /// (wire-compatible, UTF-8 is not checked) so that decoding a frame needs no allocation.
    #[prost(bytes = "bytes", tag = "2")]
    pub name: Bytes,
    /// PCM16 LE samples. No WAV header.
    #[prost(bytes = "bytes", tag = "3")]
    pub audio: Bytes,
    /// Sample rate in Hz. For client audio it is authoritative; the JS client ignores it for bot
    /// audio and plays at its own configured rate.
    #[prost(uint32, tag = "4")]
    pub sample_rate: u32,
    /// Channel count; the JS client always sends 1.
    #[prost(uint32, tag = "5")]
    pub num_channels: u32,
    /// Presentation timestamp, unused.
    #[prost(uint64, optional, tag = "6")]
    pub pts: Option<u64>,
}

/// `pipecat.TranscriptionFrame`. Decoded and ignored.
#[derive(Clone, PartialEq, Message)]
pub struct TranscriptionFrame {
    /// Frame id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Frame name.
    #[prost(string, tag = "2")]
    pub name: String,
    /// The text.
    #[prost(string, tag = "3")]
    pub text: String,
    /// Speaker id.
    #[prost(string, tag = "4")]
    pub user_id: String,
    /// Timestamp string (format unspecified).
    #[prost(string, tag = "5")]
    pub timestamp: String,
}

/// `pipecat.MessageFrame`: carries one RTVI JSON message as a string.
#[derive(Clone, PartialEq, Message)]
pub struct MessageFrame {
    /// The RTVI envelope as a JSON string.
    #[prost(string, tag = "1")]
    pub data: String,
}

/// `pipecat.InterruptionFrame`. Decoded and ignored; never sent (the JS client's schema lacks it).
#[derive(Clone, PartialEq, Message)]
pub struct InterruptionFrame {
    /// Frame id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Frame name.
    #[prost(string, tag = "2")]
    pub name: String,
}

/// `pipecat.Frame`: the one message of a websocket binary frame.
#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    /// Which frame it is; `None` for an empty message.
    #[prost(oneof = "frame::Kind", tags = "1, 2, 3, 4, 5")]
    pub kind: Option<frame::Kind>,
}

/// The `oneof frame` of [`Frame`].
pub mod frame {
    use prost::Oneof;

    /// The variants of `Frame.frame`, with the schema's field numbers.
    #[derive(Clone, PartialEq, Oneof)]
    pub enum Kind {
        /// `text = 1`
        #[prost(message, tag = "1")]
        Text(super::TextFrame),
        /// `audio = 2`
        #[prost(message, tag = "2")]
        Audio(super::AudioRawFrame),
        /// `transcription = 3`
        #[prost(message, tag = "3")]
        Transcription(super::TranscriptionFrame),
        /// `message = 4`
        #[prost(message, tag = "4")]
        Message(super::MessageFrame),
        /// `interruption = 5`
        #[prost(message, tag = "5")]
        Interruption(super::InterruptionFrame),
    }
}

impl Frame {
    /// A frame carrying one RTVI JSON message.
    pub const fn message(json: String) -> Self {
        Self {
            kind: Some(frame::Kind::Message(MessageFrame { data: json })),
        }
    }

    /// A frame carrying PCM16 mono audio at `sample_rate`, shaped as the Pipecat serializer
    /// sends it (an empty `name` and a zero `id` are omitted on the wire, as Python omits falsy
    /// fields).
    pub const fn audio(pcm: Bytes, sample_rate: u32) -> Self {
        Self {
            kind: Some(frame::Kind::Audio(AudioRawFrame {
                id: 0,
                name: Bytes::new(),
                audio: pcm,
                sample_rate,
                num_channels: 1,
                pts: None,
            })),
        }
    }

    /// Serializes the frame into `out` (appending) and returns its encoded length.
    ///
    /// # Errors
    /// [`prost::EncodeError`] if `out` cannot grow by the encoded size (it can, for a `BytesMut`).
    pub fn encode_into(&self, out: &mut BytesMut) -> Result<usize, prost::EncodeError> {
        let len = self.encoded_len();
        out.reserve(len);
        self.encode(out)?;
        Ok(len)
    }

    /// Parses one websocket binary message.
    ///
    /// # Errors
    /// [`prost::DecodeError`] when the bytes are not a valid `pipecat.Frame`.
    pub fn parse(bytes: Bytes) -> Result<Self, prost::DecodeError> {
        <Self as Message>::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_audio_frame_has_the_exact_wire_bytes_of_the_schema() {
        // Frame{audio(2): AudioRawFrame{audio(3): [1,2], sample_rate(4): 16000, num_channels(5): 1}}
        let frame = Frame::audio(Bytes::from_static(&[1, 2]), 16_000);
        let mut out = BytesMut::new();
        frame.encode_into(&mut out).unwrap();
        // 0x12 = field 2, length-delimited, 9 bytes: 0x1a 0x02 1 2 | 0x20 0x80 0x7d | 0x28 0x01
        assert_eq!(
            out.as_ref(),
            &[
                0x12, 0x09, 0x1a, 0x02, 0x01, 0x02, 0x20, 0x80, 0x7d, 0x28, 0x01
            ]
        );
        assert_eq!(Frame::parse(out.freeze()).unwrap(), frame);
    }

    #[test]
    fn a_message_frame_is_field_four_with_a_string() {
        let frame = Frame::message("{}".to_owned());
        let mut out = BytesMut::new();
        frame.encode_into(&mut out).unwrap();
        // 0x22 = field 4, length-delimited; inner: 0x0a 0x02 '{' '}'
        assert_eq!(out.as_ref(), &[0x22, 0x04, 0x0a, 0x02, b'{', b'}']);
        assert_eq!(Frame::parse(out.freeze()).unwrap(), frame);
    }

    #[test]
    fn the_js_clients_audio_frame_decodes_with_its_name_and_without_copying() {
        // name "audio" (field 2), audio (3), sample_rate 16000 (4), num_channels 1 (5).
        let wire = Bytes::from_static(&[
            0x12, 0x10, 0x12, 0x05, b'a', b'u', b'd', b'i', b'o', 0x1a, 0x02, 0x01, 0x02, 0x20,
            0x80, 0x7d, 0x28, 0x01,
        ]);
        let Some(frame::Kind::Audio(audio)) = Frame::parse(wire).unwrap().kind else {
            panic!("an audio frame");
        };
        assert_eq!(audio.name.as_ref(), b"audio");
        assert_eq!(audio.audio.as_ref(), &[1, 2]);
        assert_eq!((audio.sample_rate, audio.num_channels), (16_000, 1));
        assert_eq!(audio.id, 0);
        assert_eq!(audio.pts, None);
    }

    #[test]
    fn field_five_decodes_as_an_interruption() {
        // 0x2a = field 5, length-delimited, empty message.
        let frame = Frame::parse(Bytes::from_static(&[0x2a, 0x00])).unwrap();
        assert!(matches!(frame.kind, Some(frame::Kind::Interruption(_))));
    }

    #[test]
    fn garbage_is_a_decode_error_and_empty_is_an_empty_frame() {
        assert!(Frame::parse(Bytes::from_static(&[0xff, 0xff, 0xff])).is_err());
        assert_eq!(Frame::parse(Bytes::new()).unwrap().kind, None);
    }

    #[test]
    fn unknown_fields_are_skipped() {
        // Field 15 varint, then a valid message frame.
        let frame = Frame::parse(Bytes::from_static(&[
            0x78, 0x01, 0x22, 0x04, 0x0a, 0x02, b'h', b'i',
        ]))
        .unwrap();
        assert_eq!(frame, Frame::message("hi".to_owned()));
    }
}
