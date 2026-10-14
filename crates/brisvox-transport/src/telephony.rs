//! What the telephony adapters ([`twilio`](crate::twilio), [`telnyx`](crate::telnyx)) share: the
//! mark bookkeeping, mark names, lenient number parsing, base64 and the small helpers of the
//! websocket upgrade and refusal paths. Nothing here is provider specific.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::error::ProtocolError as WsProtocolError;
use tokio_tungstenite::tungstenite::handshake::server::ErrorResponse;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::websocket::{Shared, timeout};

/// The upgraded connection the telephony adapters serve.
pub(crate) type Ws = WebSocketStream<TcpStream>;

/// Base64 as the providers write it (padded). Decoding accepts missing padding too, so a payload
/// that was cut at a base64 boundary by some proxy is not rejected for that alone.
pub(crate) const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Longest serde error text kept (it echoes input fragments).
const MAX_ERROR_DETAIL: usize = 120;

/// A serde error rendered for a [`ProtocolError`](crate::twilio::protocol::ProtocolError)-like
/// value: cut to [`MAX_ERROR_DETAIL`] bytes at a character boundary.
pub(crate) fn clipped_error(error: &serde_json::Error) -> String {
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

/// An unsigned number that a provider prints either as a JSON number or as a JSON string (`"5"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lenient(pub u64);

impl<'de> Deserialize<'de> for Lenient {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Lenient;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an unsigned integer or a string holding one")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Lenient, E> {
                Ok(Lenient(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Lenient, E> {
                u64::try_from(v)
                    .map(Lenient)
                    .map_err(|_| E::custom("negative number"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Lenient, E> {
                v.trim()
                    .parse()
                    .map(Lenient)
                    .map_err(|_| E::custom("not an unsigned integer"))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// A number the adapter does not depend on: an unsigned integer or a string holding one reads as
/// `Some`, any other JSON value (a float, an empty string, `null`, a bool, an array, an object)
/// as `None`. Reading it never fails, so a field a provider changes the type of cannot cost audio.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Soft(pub(crate) Option<u64>);

impl<'de> Deserialize<'de> for Soft {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Soft;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any value")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Soft, E> {
                Ok(Soft(Some(v)))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Soft, E> {
                Ok(Soft(u64::try_from(v).ok()))
            }

            fn visit_i128<E: de::Error>(self, _: i128) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_u128<E: de::Error>(self, _: u128) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Soft, E> {
                Ok(Soft(v.trim().parse().ok()))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_none<E: de::Error>(self) -> Result<Soft, E> {
                Ok(Soft(None))
            }

            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Soft, D2::Error> {
                d.deserialize_any(self)
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Soft, A::Error> {
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(Soft(None))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Soft, A::Error> {
                while map
                    .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(Soft(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Like [`Soft`] for a text field: a string is kept, a number is rendered, anything else reads
/// as `None`.
#[derive(Debug, Clone, Default)]
pub(crate) struct SoftText(pub(crate) Option<String>);

impl<'de> Deserialize<'de> for SoftText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self(match value {
            serde_json::Value::String(text) => Some(text),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        }))
    }
}

/// The kind of a mark brisvox sends. Marks never carry the engine's ids on the wire (the
/// providers document no limit on a name's length or characters): the name is a letter and a
/// counter, and the connection remembers which id the counter stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkName {
    /// A mark the engine asked for (`TransportCommand::Mark`): `e<seq>`.
    Engine(u64),
    /// A progress mark the adapter sends by itself while audio plays: `p<seq>`.
    Progress(u64),
}

impl MarkName {
    /// Parses a name brisvox sent; `None` for any other name.
    pub fn parse(name: &str) -> Option<Self> {
        let (kind, number) = name.split_at_checked(1)?;
        let seq: u64 = number.parse().ok()?;
        match kind {
            "e" => Some(Self::Engine(seq)),
            "p" => Some(Self::Progress(seq)),
            _ => None,
        }
    }
}

impl fmt::Display for MarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(seq) => write!(f, "e{seq}"),
            Self::Progress(seq) => write!(f, "p{seq}"),
        }
    }
}

/// Most marks in flight per stream. Past it the oldest is forgotten (its echo, if it ever comes,
/// is then dropped as unknown). Audio is paced in real time and marks are sent every few hundred
/// milliseconds, so a healthy stream has a handful in flight; a stream that never echoes marks
/// must not grow the book.
pub const MAX_MARKS_IN_FLIGHT: usize = 512;

#[derive(Debug)]
struct Entry {
    seq: u64,
    /// The engine's id for an engine mark; `None` for a progress mark.
    id: Option<String>,
    sent_at: Instant,
    /// Audio bytes sent on the stream up to and including this mark's position.
    audio_bytes: u64,
}

/// What an echoed mark meant.
#[derive(Debug, PartialEq, Eq)]
pub enum Echo {
    /// The mark was the engine's: the audio before it has played.
    Engine {
        /// The id of the engine's `TransportCommand::Mark`.
        id: String,
        /// How long the provider took from our send to the echo.
        latency: Duration,
    },
    /// A progress mark: the audio before it has played.
    Progress {
        /// How long the provider took from our send to the echo.
        latency: Duration,
    },
    /// Not in flight: flushed by a `clear`, forgotten, repeated, or never sent by us.
    Unknown,
}

/// The marks in flight and the audio counters of one stream.
///
/// The provider plays the audio it was sent in order and echoes a `mark` when the audio before it
/// has played, or when a `clear` flushed it (the echo of a flushed mark looks exactly like the
/// echo of a played one on the wire). The book is what tells them apart: [`MarkBook::clear`]
/// forgets every mark in flight, so an echo that arrives afterwards finds nothing and is dropped.
/// The core reports the engine's own marks as `MarkCleared` at the same moment
/// (`TransportCommand::Clear`).
///
/// The writer (marks go out, audio is counted) and the reader (echoes come in) share the book
/// behind a `std::sync::Mutex` that is only held for these small bookkeeping operations, never
/// across an await.
#[derive(Debug, Default)]
pub struct MarkBook {
    in_flight: VecDeque<Entry>,
    next_seq: u64,
    /// Audio bytes (G.711, one per sample) sent since the stream started.
    sent_bytes: u64,
    /// Audio bytes known played (up to the last echoed mark).
    played_bytes: u64,
    /// Audio bytes sent since the last mark of any kind.
    unmarked_bytes: u64,
}

impl MarkBook {
    /// Counts `bytes` of audio sent.
    pub fn audio_sent(&mut self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.sent_bytes = self.sent_bytes.saturating_add(bytes);
        self.unmarked_bytes = self.unmarked_bytes.saturating_add(bytes);
    }

    /// Audio bytes sent since the last mark.
    pub const fn unmarked_bytes(&self) -> u64 {
        self.unmarked_bytes
    }

    /// Audio bytes sent but not yet known to have played.
    pub const fn unplayed_bytes(&self) -> u64 {
        self.sent_bytes.saturating_sub(self.played_bytes)
    }

    /// Marks in flight.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    fn push(&mut self, id: Option<String>, now: Instant) -> (u64, bool) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let evicted = if self.in_flight.len() >= MAX_MARKS_IN_FLIGHT {
            self.in_flight.pop_front().is_some()
        } else {
            false
        };
        self.in_flight.push_back(Entry {
            seq,
            id,
            sent_at: now,
            audio_bytes: self.sent_bytes,
        });
        self.unmarked_bytes = 0;
        (seq, evicted)
    }

    /// Registers an engine mark about to be sent; returns its wire name and whether the oldest
    /// mark in flight was forgotten to make room.
    pub fn engine_mark(&mut self, id: String, now: Instant) -> (MarkName, bool) {
        let (seq, evicted) = self.push(Some(id), now);
        (MarkName::Engine(seq), evicted)
    }

    /// Registers a progress mark about to be sent; returns its wire name and whether the oldest
    /// mark in flight was forgotten to make room.
    pub fn progress_mark(&mut self, now: Instant) -> (MarkName, bool) {
        let (seq, evicted) = self.push(None, now);
        (MarkName::Progress(seq), evicted)
    }

    /// Resolves an echoed mark. A known mark leaves the book and advances the played position.
    pub fn echo(&mut self, name: MarkName, now: Instant) -> Echo {
        let (MarkName::Engine(seq) | MarkName::Progress(seq)) = name;
        let Some(at) = self.in_flight.iter().position(|e| e.seq == seq) else {
            return Echo::Unknown;
        };
        let Some(entry) = self.in_flight.remove(at) else {
            return Echo::Unknown;
        };
        self.played_bytes = self.played_bytes.max(entry.audio_bytes);
        let latency = now.saturating_duration_since(entry.sent_at);
        match (name, entry.id) {
            (MarkName::Engine(_), Some(id)) => Echo::Engine { id, latency },
            (MarkName::Progress(_), None) => Echo::Progress { latency },
            // The kind letter and the entry disagree: not a mark we sent under that name.
            _ => Echo::Unknown,
        }
    }

    /// Barge-in: every mark in flight is forgotten (the provider flushes its buffer and echoes
    /// them; those echoes are dropped). The unplayed audio is gone too, so the played position
    /// catches up to the sent one. Returns the number of marks forgotten.
    pub fn clear(&mut self) -> usize {
        let cut = self.in_flight.len();
        self.in_flight.clear();
        self.played_bytes = self.sent_bytes;
        self.unmarked_bytes = 0;
        cut
    }
}

/// Longest close reason a close frame may carry (125 bytes of payload minus the status code).
const MAX_CLOSE_REASON: usize = 120;

/// A close reason that fits a close frame.
pub(crate) fn close_reason(reason: &str) -> Utf8Bytes {
    let mut end = reason.len().min(MAX_CLOSE_REASON);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Utf8Bytes::from(reason.get(..end).unwrap_or_default())
}

/// An HTTP refusal of a websocket upgrade.
pub(crate) fn refusal(status: StatusCode, body: &str) -> Box<ErrorResponse> {
    let mut response = ErrorResponse::new(Some(body.to_owned()));
    *response.status_mut() = status;
    Box::new(response)
}

/// Maps a websocket read error to the close to answer with, if the peer is to blame (a socket that
/// vanishes is nobody's fault).
pub(crate) fn blame(error: &WsError) -> Option<(&'static str, CloseCode)> {
    match error {
        WsError::Protocol(WsProtocolError::ResetWithoutClosingHandshake) | WsError::Io(_) => None,
        WsError::Capacity(_) => Some(("message exceeds the frame limit", CloseCode::Size)),
        WsError::Protocol(_) | WsError::Utf8(_) | WsError::AttackAttempt => {
            Some(("websocket protocol violation", CloseCode::Protocol))
        }
        _ => None,
    }
}

/// Closes a connection that is not split yet and waits briefly for the peer's close frame (so the
/// peer sees a clean close, not a reset).
pub(crate) async fn refuse(ws: &mut Ws, shared: &Shared, reason: &str, close: CloseCode) {
    let write = ws.send(Message::Close(Some(CloseFrame {
        code: close,
        reason: close_reason(reason),
    })));
    let clock = &*shared.clock;
    if !matches!(
        timeout(clock, shared.config.write_timeout, write).await,
        Ok(Ok(()))
    ) {
        return;
    }
    let drain = async { while let Some(Ok(_)) = ws.next().await {} };
    tokio::select! {
        biased;
        () = shared.cancel.cancelled() => {}
        _ = timeout(clock, shared.config.close_grace, drain) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        brisvox_core::SystemClock::shared().now()
    }

    #[test]
    fn an_engine_mark_is_echoed_with_its_id() {
        let now = t0();
        let mut book = MarkBook::default();
        book.audio_sent(1600);
        let (name, evicted) = book.engine_mark("utt-1".to_owned(), now);
        assert!(!evicted);
        assert_eq!(name, MarkName::Engine(0));
        assert_eq!(book.unmarked_bytes(), 0);
        assert_eq!(book.unplayed_bytes(), 1600);
        let echo = book.echo(name, now + Duration::from_millis(300));
        assert_eq!(
            echo,
            Echo::Engine {
                id: "utt-1".to_owned(),
                latency: Duration::from_millis(300)
            }
        );
        assert_eq!(book.unplayed_bytes(), 0);
        // A second echo of the same mark is unknown.
        assert_eq!(book.echo(name, now), Echo::Unknown);
    }

    #[test]
    fn progress_marks_advance_the_played_position() {
        let now = t0();
        let mut book = MarkBook::default();
        book.audio_sent(1600);
        let (a, _) = book.progress_mark(now);
        book.audio_sent(1600);
        let (b, _) = book.progress_mark(now);
        assert_eq!(book.unplayed_bytes(), 3200);
        assert!(matches!(book.echo(a, now), Echo::Progress { .. }));
        assert_eq!(book.unplayed_bytes(), 1600);
        assert!(matches!(book.echo(b, now), Echo::Progress { .. }));
        assert_eq!(book.unplayed_bytes(), 0);
    }

    #[test]
    fn clear_forgets_every_mark_so_their_echoes_are_dropped() {
        let now = t0();
        let mut book = MarkBook::default();
        book.audio_sent(800);
        let (e, _) = book.engine_mark("a".to_owned(), now);
        let (p, _) = book.progress_mark(now);
        assert_eq!(book.clear(), 2);
        assert_eq!(book.in_flight(), 0);
        assert_eq!(book.unplayed_bytes(), 0);
        // The provider returns the flushed marks; none of them counts.
        assert_eq!(book.echo(e, now), Echo::Unknown);
        assert_eq!(book.echo(p, now), Echo::Unknown);
        // Marks sent after the clear work as before and get fresh names.
        let (again, _) = book.engine_mark("a".to_owned(), now);
        assert_ne!(again, e);
        assert!(matches!(book.echo(again, now), Echo::Engine { .. }));
    }

    #[test]
    fn the_book_is_bounded() {
        let now = t0();
        let mut book = MarkBook::default();
        let mut first = None;
        for i in 0..MAX_MARKS_IN_FLIGHT + 10 {
            let (name, evicted) = book.progress_mark(now);
            first.get_or_insert(name);
            assert_eq!(evicted, i >= MAX_MARKS_IN_FLIGHT);
        }
        assert_eq!(book.in_flight(), MAX_MARKS_IN_FLIGHT);
        // The oldest was forgotten.
        assert_eq!(book.echo(first.unwrap(), now), Echo::Unknown);
    }

    #[test]
    fn a_name_whose_kind_disagrees_with_the_entry_is_unknown() {
        let now = t0();
        let mut book = MarkBook::default();
        let (name, _) = book.engine_mark("x".to_owned(), now);
        let MarkName::Engine(seq) = name else {
            panic!("not an engine mark");
        };
        assert_eq!(book.echo(MarkName::Progress(seq), now), Echo::Unknown);
    }

    #[test]
    fn mark_names_round_trip_and_foreign_names_do_not_parse() {
        for name in [
            MarkName::Engine(0),
            MarkName::Engine(42),
            MarkName::Progress(7),
        ] {
            assert_eq!(MarkName::parse(&name.to_string()), Some(name));
        }
        assert_eq!(MarkName::Engine(5).to_string(), "e5");
        assert_eq!(MarkName::Progress(5).to_string(), "p5");
        for foreign in [
            "", "e", "p", "x5", "e-1", "e5x", "my label", "é5", "E5", "e 5",
        ] {
            assert_eq!(MarkName::parse(foreign), None, "{foreign:?}");
        }
    }

    #[test]
    fn a_close_reason_is_cut_at_a_character_boundary() {
        let long = "é".repeat(100);
        let cut = close_reason(&long);
        assert!(cut.len() <= MAX_CLOSE_REASON);
        assert!(long.starts_with(cut.as_str()));
    }
}
