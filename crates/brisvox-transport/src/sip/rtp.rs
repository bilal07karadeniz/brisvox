//! RTP (RFC 3550) packets for G.711 telephony: header parsing and writing, RFC 4733
//! `telephone-event` payloads, a bounded reorder buffer for the receive side and the sender state
//! (sequence numbers, timestamps, marker bits) for the send side.
//!
//! Everything here is plain data in, plain data out: no sockets and no clock reads (callers pass
//! `now`), so it is tested with synthetic instants. G.711 payloads are 1 byte per sample at
//! 8000 Hz, so a 20 ms packet carries 160 bytes and advances the RTP timestamp by 160
//! (RFC 3551 section 4.5.14).

use std::collections::VecDeque;
use std::ops::Range;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::frame::DtmfDigit;
use tokio::time::Instant;

/// Length of the fixed RTP header.
pub const HEADER_LEN: usize = 12;

/// Packetization time of every packet this endpoint sends (and the one it announces).
pub const PTIME: Duration = Duration::from_millis(20);

/// G.711 samples (= payload bytes) in one [`PTIME`] packet.
pub const SAMPLES_PER_PACKET: usize = 160;

/// RTP timestamp units per millisecond for 8 kHz audio.
const TICKS_PER_MS: u32 = 8;

/// A datagram larger than this is not read as RTP (a G.711 packet is 172 bytes).
pub const MAX_DATAGRAM: usize = 2048;

/// Payload types 72 to 76 (RTCP packet types 200 to 204 with the marker bit) identify RTCP when
/// RTP and RTCP share a port (RFC 5761 section 4).
const RTCP_PAYLOAD_TYPES: Range<u8> = 72..77;

/// The fixed part of an RTP header (RFC 3550 section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// The marker bit: for audio, the first packet of a talkspurt.
    pub marker: bool,
    /// The payload type.
    pub payload_type: u8,
    /// The 16-bit sequence number.
    pub sequence: u16,
    /// The sampling-clock timestamp of the first sample.
    pub timestamp: u32,
    /// The synchronization source.
    pub ssrc: u32,
}

/// Why a datagram is not a usable RTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtpError {
    /// Shorter than the header it announces.
    #[error("truncated RTP packet")]
    Truncated,
    /// The version field is not 2.
    #[error("not RTP version 2")]
    BadVersion,
    /// The packet is an RTCP packet on a muxed port.
    #[error("RTCP packet")]
    Rtcp,
    /// The padding length is impossible.
    #[error("bad RTP padding")]
    BadPadding,
}

impl RtpHeader {
    /// Parses a datagram: the header and the range of the payload (CSRC list, extension header and
    /// padding skipped).
    ///
    /// # Errors
    /// [`RtpError`] for a datagram that is not RTP version 2 or is malformed.
    pub fn parse(datagram: &[u8]) -> Result<(Self, Range<usize>), RtpError> {
        let &[first, second, s0, s1, t0, t1, t2, t3, c0, c1, c2, c3, ..] = datagram else {
            return Err(RtpError::Truncated);
        };
        if first >> 6 != 2 {
            return Err(RtpError::BadVersion);
        }
        let padding = first & 0x20 != 0;
        let extension = first & 0x10 != 0;
        let csrc_count = usize::from(first & 0x0f);
        let payload_type = second & 0x7f;
        if RTCP_PAYLOAD_TYPES.contains(&payload_type) {
            return Err(RtpError::Rtcp);
        }
        let mut start = HEADER_LEN + csrc_count * 4;
        if extension {
            // 16-bit profile id, 16-bit length in 32-bit words, then the words.
            let length = datagram
                .get(start + 2..start + 4)
                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                .map(u16::from_be_bytes)
                .ok_or(RtpError::Truncated)?;
            start += 4 + usize::from(length) * 4;
        }
        let mut end = datagram.len();
        if padding {
            let pad = usize::from(*datagram.last().ok_or(RtpError::Truncated)?);
            if pad == 0 || end < start + pad {
                return Err(RtpError::BadPadding);
            }
            end -= pad;
        }
        if start > end {
            return Err(RtpError::Truncated);
        }
        Ok((
            Self {
                marker: second & 0x80 != 0,
                payload_type,
                sequence: u16::from_be_bytes([s0, s1]),
                timestamp: u32::from_be_bytes([t0, t1, t2, t3]),
                ssrc: u32::from_be_bytes([c0, c1, c2, c3]),
            },
            start..end,
        ))
    }

    /// Appends the 12-byte header (version 2, no padding, extension or CSRC) to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(0x80);
        out.push(self.payload_type & 0x7f | if self.marker { 0x80 } else { 0 });
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// An RFC 4733 telephone-event payload (section 2.3): event code, end bit, volume, duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelephoneEvent {
    /// The event code (0 to 9 digits, 10 `*`, 11 `#`, 12 to 15 `A` to `D`).
    pub event: u8,
    /// The end bit: the last packets of the event.
    pub end: bool,
    /// Power level in -dBm0 (0 to 63).
    pub volume: u8,
    /// Duration so far in timestamp units (8000 per second).
    pub duration: u16,
}

impl TelephoneEvent {
    /// Parses the first event of a payload (a payload may carry several; the first is the one
    /// that belongs to this packet's timestamp). `None` for fewer than 4 bytes.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let &[event, flags, d0, d1, ..] = payload else {
            return None;
        };
        Some(Self {
            event,
            end: flags & 0x80 != 0,
            volume: flags & 0x3f,
            duration: u16::from_be_bytes([d0, d1]),
        })
    }

    /// The 4-byte payload.
    pub fn encode(self) -> [u8; 4] {
        let d = self.duration.to_be_bytes();
        [
            self.event,
            self.volume & 0x3f | if self.end { 0x80 } else { 0 },
            d[0],
            d[1],
        ]
    }

    /// The key this event stands for, `None` for events other than the 16 DTMF keys.
    pub fn digit(self) -> Option<DtmfDigit> {
        digit_from_event(self.event)
    }
}

/// The DTMF key of an RFC 4733 event code 0 to 15.
pub const fn digit_from_event(event: u8) -> Option<DtmfDigit> {
    Some(match event {
        0 => DtmfDigit::D0,
        1 => DtmfDigit::D1,
        2 => DtmfDigit::D2,
        3 => DtmfDigit::D3,
        4 => DtmfDigit::D4,
        5 => DtmfDigit::D5,
        6 => DtmfDigit::D6,
        7 => DtmfDigit::D7,
        8 => DtmfDigit::D8,
        9 => DtmfDigit::D9,
        10 => DtmfDigit::Star,
        11 => DtmfDigit::Pound,
        12 => DtmfDigit::A,
        13 => DtmfDigit::B,
        14 => DtmfDigit::C,
        15 => DtmfDigit::D,
        _ => return None,
    })
}

/// The RFC 4733 event code of a DTMF key.
pub const fn event_from_digit(digit: DtmfDigit) -> u8 {
    match digit {
        DtmfDigit::D0 => 0,
        DtmfDigit::D1 => 1,
        DtmfDigit::D2 => 2,
        DtmfDigit::D3 => 3,
        DtmfDigit::D4 => 4,
        DtmfDigit::D5 => 5,
        DtmfDigit::D6 => 6,
        DtmfDigit::D7 => 7,
        DtmfDigit::D8 => 8,
        DtmfDigit::D9 => 9,
        DtmfDigit::Star => 10,
        DtmfDigit::Pound => 11,
        DtmfDigit::A => 12,
        DtmfDigit::B => 13,
        DtmfDigit::C => 14,
        DtmfDigit::D => 15,
    }
}

/// Remembers the timestamps of the last few DTMF events so the repeated packets of one event
/// (every packet of an event carries the event's start timestamp, and the end packet is sent three
/// times, RFC 4733 section 2.5.1.4) report the key once.
#[derive(Debug, Default)]
pub struct DtmfDedupe {
    seen: VecDeque<u32>,
}

impl DtmfDedupe {
    const REMEMBER: usize = 8;

    /// Whether `timestamp` starts a new event (and remembers it).
    pub fn is_new(&mut self, timestamp: u32) -> bool {
        if self.seen.contains(&timestamp) {
            return false;
        }
        if self.seen.len() >= Self::REMEMBER {
            self.seen.pop_front();
        }
        self.seen.push_back(timestamp);
        true
    }

    /// Forgets everything (the stream restarted).
    pub fn reset(&mut self) {
        self.seen.clear();
    }
}

// ---------------------------------------------------------------------------------------------
// Reorder buffer

/// A packet leaving the [`ReorderBuffer`].
#[derive(Debug, PartialEq, Eq)]
pub enum Released<T> {
    /// The next packet, in sequence.
    Packet(T),
    /// This many packets were given up on (never arrived within the buffer's depth or wait).
    Lost(u16),
}

/// Counters of a [`ReorderBuffer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReorderStats {
    /// Packets accepted.
    pub received: u64,
    /// Packets that arrived out of order and were put back in place.
    pub reordered: u64,
    /// Packets that repeated one already held.
    pub duplicates: u64,
    /// Packets that arrived after their slot was given up or delivered.
    pub late: u64,
    /// Packets given up on.
    pub lost: u64,
    /// Times the sequence space jumped and the buffer started over.
    pub resyncs: u64,
}

struct Held<T> {
    ext: u64,
    item: T,
    arrived: Instant,
}

/// Puts RTP packets back in sequence order with bounded memory and bounded delay.
///
/// An in-order packet leaves at once: the buffer adds no latency to a clean stream. A packet that
/// arrives ahead of a gap is held until the gap fills, until more than `depth` packets are held,
/// or until the oldest held packet has waited `max_wait`; then the gap is given up
/// ([`Released::Lost`]) and the held packets are released. Sequence numbers are extended over
/// wrap-around; a jump of more than 3000 ahead or 100 behind is taken as a restarted stream
/// (RFC 3550 appendix A.1) and the buffer starts over.
pub struct ReorderBuffer<T> {
    depth: usize,
    max_wait: Duration,
    next: Option<u64>,
    held: VecDeque<Held<T>>,
    stats: ReorderStats,
}

/// A sequence number this far ahead of the expected one is a dropout, not a restart.
const MAX_DROPOUT: i64 = 3000;
/// A sequence number up to this far behind the expected one is a late or duplicate packet.
const MAX_MISORDER: i64 = 100;

impl<T> ReorderBuffer<T> {
    /// A buffer holding at most `depth` packets (at least 1) for at most `max_wait`.
    pub fn new(depth: usize, max_wait: Duration) -> Self {
        let depth = depth.max(1);
        Self {
            depth,
            max_wait,
            next: None,
            held: VecDeque::with_capacity(depth + 1),
            stats: ReorderStats::default(),
        }
    }

    /// The counters.
    pub const fn stats(&self) -> ReorderStats {
        self.stats
    }

    /// Packets currently held.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// When [`poll`](Self::poll) must be called next, `None` while nothing is held.
    pub fn deadline(&self) -> Option<Instant> {
        let oldest = self.held.iter().map(|h| h.arrived).min()?;
        Some(deadline_after(oldest, self.max_wait))
    }

    /// Starts over: releases every held packet (gaps ignored) and forgets the sequence position.
    pub fn flush(&mut self, out: &mut impl FnMut(Released<T>)) {
        while let Some(held) = self.held.pop_front() {
            out(Released::Packet(held.item));
        }
        self.next = None;
    }

    /// Adds the packet with sequence number `sequence`, releasing whatever became deliverable.
    pub fn push(
        &mut self,
        sequence: u16,
        item: T,
        now: Instant,
        out: &mut impl FnMut(Released<T>),
    ) {
        let Some(next) = self.next else {
            self.stats.received += 1;
            self.next = Some(u64::from(sequence).wrapping_add(1));
            out(Released::Packet(item));
            return;
        };
        // The distance from the expected sequence number, as a signed 16-bit value.
        let delta = i64::from(sequence.wrapping_sub(next as u16) as i16);
        if !(-MAX_MISORDER..MAX_DROPOUT).contains(&delta) {
            self.stats.resyncs += 1;
            self.flush(out);
            self.push(sequence, item, now, out);
            return;
        }
        if delta < 0 {
            self.stats.late += 1;
            return;
        }
        let ext = next + delta.unsigned_abs();
        if ext == next {
            self.stats.received += 1;
            self.next = Some(next + 1);
            out(Released::Packet(item));
            self.release_ready(out);
            return;
        }
        // Ahead of a gap: hold it, in order.
        let position = self.held.partition_point(|h| h.ext < ext);
        if self.held.get(position).is_some_and(|h| h.ext == ext) {
            self.stats.duplicates += 1;
            return;
        }
        self.stats.received += 1;
        self.stats.reordered += 1;
        self.held.insert(
            position,
            Held {
                ext,
                item,
                arrived: now,
            },
        );
        while self.held.len() > self.depth {
            self.give_up_gap(out);
        }
    }

    /// Gives up gaps whose oldest waiting packet has waited `max_wait` by `now`.
    pub fn poll(&mut self, now: Instant, out: &mut impl FnMut(Released<T>)) {
        while self.deadline().is_some_and(|due| now >= due) {
            self.give_up_gap(out);
        }
    }

    /// Releases the packets that directly follow the expected sequence number.
    fn release_ready(&mut self, out: &mut impl FnMut(Released<T>)) {
        while let (Some(next), Some(front)) = (self.next, self.held.front()) {
            if front.ext != next {
                break;
            }
            if let Some(held) = self.held.pop_front() {
                self.next = Some(next + 1);
                out(Released::Packet(held.item));
            }
        }
    }

    /// Gives up the gap in front of the first held packet and releases what follows it.
    fn give_up_gap(&mut self, out: &mut impl FnMut(Released<T>)) {
        let (Some(next), Some(front)) = (self.next, self.held.front()) else {
            return;
        };
        let gap = front.ext.saturating_sub(next);
        if gap > 0 {
            self.stats.lost += gap;
            out(Released::Lost(u16::try_from(gap).unwrap_or(u16::MAX)));
            self.next = Some(front.ext);
        }
        self.release_ready(out);
    }
}

// ---------------------------------------------------------------------------------------------
// Sender state

/// The send side of one RTP stream: SSRC, sequence number and timestamp, marker bits at talkspurt
/// starts, and timestamp jumps over silence.
///
/// RTP timestamps run on the sampling clock, also while nothing is sent (RFC 3550 section 5.1), so
/// when audio resumes after a pause of 40 ms or more the timestamp advances by the pause and the
/// marker bit is set (RFC 3551 section 4.1). The marker bit is also set on the first packet of a
/// call and on the first audio after [`note_clear`](Self::note_clear) (barge-in).
#[derive(Debug)]
pub struct RtpSender {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    last_sent: Option<Instant>,
    event_start: Option<u32>,
    marker_pending: bool,
}

impl RtpSender {
    /// A sender with the given initial values.
    pub const fn new(ssrc: u32, sequence: u16, timestamp: u32) -> Self {
        Self {
            ssrc,
            sequence,
            timestamp,
            last_sent: None,
            event_start: None,
            marker_pending: true,
        }
    }

    /// A sender with a random SSRC, initial sequence number and timestamp (RFC 3550 section 5.1
    /// asks for random starting values).
    ///
    /// # Errors
    /// [`ring::error::Unspecified`] if the system random source fails.
    pub fn random() -> Result<Self, ring::error::Unspecified> {
        use ring::rand::SecureRandom as _;
        let mut bytes = [0u8; 10];
        ring::rand::SystemRandom::new().fill(&mut bytes)?;
        let [a, b, c, d, e, f, g, h, i, j] = bytes;
        Ok(Self::new(
            u32::from_be_bytes([a, b, c, d]),
            u16::from_be_bytes([e, f]),
            u32::from_be_bytes([g, h, i, j]),
        ))
    }

    /// The synchronization source.
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The sequence number the next packet gets.
    pub const fn next_sequence(&self) -> u16 {
        self.sequence
    }

    /// The timestamp the next audio packet gets, before any pause is added.
    pub const fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    /// Marks the next audio packet as the start of a talkspurt (after queued audio was dropped).
    pub const fn note_clear(&mut self) {
        self.marker_pending = true;
    }

    /// Applies the pause rule for a packet sent at `now` and returns its marker bit.
    fn begin_packet(&mut self, now: Instant) -> bool {
        let mut marker = std::mem::take(&mut self.marker_pending);
        match self.last_sent {
            None => marker = true,
            Some(previous) => {
                let gap = now.saturating_duration_since(previous);
                if gap >= PTIME * 2 {
                    marker = true;
                    let extra = gap.saturating_sub(PTIME);
                    let ms = u32::try_from(extra.as_millis()).unwrap_or(u32::MAX);
                    self.timestamp = self.timestamp.wrapping_add(ms.wrapping_mul(TICKS_PER_MS));
                }
            }
        }
        marker
    }

    /// Writes the next audio packet (header and `payload`) into `out`, which is cleared first.
    pub fn audio_packet(
        &mut self,
        payload_type: u8,
        payload: &[u8],
        now: Instant,
        out: &mut Vec<u8>,
    ) {
        let marker = self.begin_packet(now);
        out.clear();
        RtpHeader {
            marker,
            payload_type,
            sequence: self.sequence,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
        }
        .write(out);
        out.extend_from_slice(payload);
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self
            .timestamp
            .wrapping_add(u32::try_from(payload.len()).unwrap_or(0));
        self.last_sent = Some(now);
    }

    /// Writes a telephone-event packet into `out` (cleared first). `start` is true for the first
    /// packet of an event: it takes the event's timestamp (all packets of the event share it) and
    /// the marker bit. The end packet moves the audio timestamp past the event.
    pub fn event_packet(
        &mut self,
        payload_type: u8,
        event: TelephoneEvent,
        start: bool,
        now: Instant,
        out: &mut Vec<u8>,
    ) {
        let mut marker = false;
        if start {
            self.begin_packet(now);
            marker = true;
            self.event_start = Some(self.timestamp);
        }
        let timestamp = self.event_start.unwrap_or(self.timestamp);
        out.clear();
        RtpHeader {
            marker,
            payload_type,
            sequence: self.sequence,
            timestamp,
            ssrc: self.ssrc,
        }
        .write(out);
        out.extend_from_slice(&event.encode());
        self.sequence = self.sequence.wrapping_add(1);
        if event.end {
            self.timestamp = timestamp.wrapping_add(u32::from(event.duration));
        }
        self.last_sent = Some(now);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // synthetic instants for a pure model test
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn drain(out: &mut Vec<Released<u16>>) -> Vec<String> {
        out.drain(..)
            .map(|r| match r {
                Released::Packet(p) => format!("p{p}"),
                Released::Lost(n) => format!("lost{n}"),
            })
            .collect()
    }

    #[test]
    fn header_round_trips_and_matches_the_rfc_layout() {
        let header = RtpHeader {
            marker: true,
            payload_type: 8,
            sequence: 0x1234,
            timestamp: 0xdead_beef,
            ssrc: 0x0102_0304,
        };
        let mut bytes = Vec::new();
        header.write(&mut bytes);
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            bytes,
            [
                0x80, 0x88, 0x12, 0x34, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4, 1, 2, 3
            ]
        );
        let (parsed, payload) = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(&bytes[payload], &[1, 2, 3]);
    }

    #[test]
    fn csrc_extension_and_padding_are_skipped() {
        // V=2, P=1, X=1, CC=1; PT 0; one CSRC; extension of one word; payload AB CD; 2 bytes padding.
        let mut packet = vec![0xb1, 0x00, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        packet.extend_from_slice(&[0, 0, 0, 9]); // CSRC
        packet.extend_from_slice(&[0xbe, 0xde, 0, 1, 7, 7, 7, 7]); // extension
        packet.extend_from_slice(&[0xab, 0xcd, 0, 2]); // payload + padding of 2
        let (_, payload) = RtpHeader::parse(&packet).unwrap();
        assert_eq!(&packet[payload], &[0xab, 0xcd]);
    }

    #[test]
    fn malformed_packets_are_errors_not_panics() {
        assert_eq!(RtpHeader::parse(&[]), Err(RtpError::Truncated));
        assert_eq!(RtpHeader::parse(&[0x80; 11]), Err(RtpError::Truncated));
        let mut bad_version = vec![0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(RtpHeader::parse(&bad_version), Err(RtpError::BadVersion));
        bad_version[0] = 0x80;
        bad_version[1] = 200 & 0x7f | 0x80; // RTCP sender report with the marker bit: PT 72
        assert_eq!(RtpHeader::parse(&bad_version), Err(RtpError::Rtcp));
        // CSRC count points past the end.
        assert_eq!(
            RtpHeader::parse(&[0x8f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).map(|_| ()),
            Err(RtpError::Truncated)
        );
        // Padding larger than the packet, and zero padding.
        assert_eq!(
            RtpHeader::parse(&[0xa0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]).map(|_| ()),
            Err(RtpError::BadPadding)
        );
        assert_eq!(
            RtpHeader::parse(&[0xa0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).map(|_| ()),
            Err(RtpError::BadPadding)
        );
        // Extension header cut short.
        assert_eq!(
            RtpHeader::parse(&[0x90, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).map(|_| ()),
            Err(RtpError::Truncated)
        );
    }

    #[test]
    fn telephone_event_matches_rfc_4733_vectors() {
        // Digit 5, end bit set, volume 10, duration 1600 (200 ms).
        let event = TelephoneEvent {
            event: 5,
            end: true,
            volume: 10,
            duration: 1600,
        };
        let bytes = event.encode();
        assert_eq!(bytes, [5, 0x8a, 0x06, 0x40]);
        assert_eq!(TelephoneEvent::parse(&bytes), Some(event));
        assert_eq!(event.digit(), Some(DtmfDigit::D5));
        assert_eq!(TelephoneEvent::parse(&[1, 2, 3]), None);
        // Trailing extra events are ignored.
        assert_eq!(
            TelephoneEvent::parse(&[11, 0, 0, 160, 12, 0, 0, 160]).map(|e| e.digit()),
            Some(Some(DtmfDigit::Pound))
        );
        for code in 0..16u8 {
            let digit = digit_from_event(code).unwrap();
            assert_eq!(event_from_digit(digit), code);
        }
        assert_eq!(digit_from_event(16), None);
        assert_eq!(digit_from_event(255), None);
    }

    #[test]
    fn dtmf_packets_of_one_event_report_once() {
        let mut dedupe = DtmfDedupe::default();
        assert!(dedupe.is_new(1000));
        assert!(!dedupe.is_new(1000));
        assert!(dedupe.is_new(2000));
        assert!(!dedupe.is_new(1000));
        for t in 0..20 {
            dedupe.is_new(5000 + t);
        }
        // The oldest were forgotten; the newest are still remembered.
        assert!(!dedupe.is_new(5019));
        dedupe.reset();
        assert!(dedupe.is_new(5019));
    }

    #[test]
    fn in_order_packets_leave_immediately() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(3, Duration::from_millis(60));
        let mut out = Vec::new();
        for seq in [10u16, 11, 12] {
            buffer.push(seq, seq, base, &mut |r| out.push(r));
        }
        assert_eq!(drain(&mut out), ["p10", "p11", "p12"]);
        assert_eq!(buffer.held(), 0);
        assert_eq!(buffer.stats().received, 3);
    }

    #[test]
    fn a_swapped_pair_is_put_back_in_order() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(3, Duration::from_millis(60));
        let mut out = Vec::new();
        buffer.push(1, 1u16, base, &mut |r| out.push(r));
        buffer.push(3, 3, at(base, 1), &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["p1"]);
        assert_eq!(buffer.held(), 1);
        buffer.push(2, 2, at(base, 2), &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["p2", "p3"]);
        assert_eq!(buffer.stats().reordered, 1);
        assert_eq!(buffer.stats().lost, 0);
    }

    #[test]
    fn a_gap_is_given_up_when_the_buffer_is_deeper_than_its_depth() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(2, Duration::from_millis(500));
        let mut out = Vec::new();
        buffer.push(1, 1u16, base, &mut |r| out.push(r));
        for seq in [3u16, 4, 5] {
            buffer.push(seq, seq, base, &mut |r| out.push(r));
        }
        assert_eq!(drain(&mut out), ["p1", "lost1", "p3", "p4", "p5"]);
        assert_eq!(buffer.stats().lost, 1);
        // The missing packet arrives after all: late, dropped.
        buffer.push(2, 2, base, &mut |r| out.push(r));
        assert!(out.is_empty());
        assert_eq!(buffer.stats().late, 1);
    }

    #[test]
    fn a_gap_is_given_up_after_the_wait() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(8, Duration::from_millis(60));
        let mut out = Vec::new();
        buffer.push(1, 1u16, base, &mut |r| out.push(r));
        buffer.push(4, 4, at(base, 20), &mut |r| out.push(r));
        buffer.push(5, 5, at(base, 40), &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["p1"]);
        assert_eq!(buffer.deadline(), Some(at(base, 80)));
        buffer.poll(at(base, 79), &mut |r| out.push(r));
        assert!(out.is_empty());
        buffer.poll(at(base, 80), &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["lost2", "p4", "p5"]);
        assert_eq!(buffer.deadline(), None);
        assert_eq!(buffer.stats().lost, 2);
    }

    #[test]
    fn duplicates_and_late_packets_are_dropped() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(4, Duration::from_millis(60));
        let mut out = Vec::new();
        buffer.push(1, 1u16, base, &mut |r| out.push(r));
        buffer.push(1, 1, base, &mut |r| out.push(r)); // late (already delivered)
        buffer.push(3, 3, base, &mut |r| out.push(r));
        buffer.push(3, 3, base, &mut |r| out.push(r)); // duplicate of a held packet
        assert_eq!(drain(&mut out), ["p1"]);
        let stats = buffer.stats();
        assert_eq!((stats.late, stats.duplicates), (1, 1));
    }

    #[test]
    fn sequence_wrap_around_is_handled() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(4, Duration::from_millis(60));
        let mut out = Vec::new();
        buffer.push(65534, 0u16, base, &mut |r| out.push(r));
        buffer.push(1, 3, base, &mut |r| out.push(r)); // ahead of the gap across the wrap
        buffer.push(65535, 1, base, &mut |r| out.push(r));
        buffer.push(0, 2, base, &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["p0", "p1", "p2", "p3"]);
    }

    #[test]
    fn a_jump_restarts_the_stream() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(4, Duration::from_millis(60));
        let mut out = Vec::new();
        buffer.push(100, 1u16, base, &mut |r| out.push(r));
        buffer.push(102, 2, base, &mut |r| out.push(r)); // held
        buffer.push(40_000, 3, base, &mut |r| out.push(r)); // far ahead: restart
        assert_eq!(drain(&mut out), ["p1", "p2", "p3"]);
        assert_eq!(buffer.stats().resyncs, 1);
        buffer.push(40_001, 4, base, &mut |r| out.push(r));
        assert_eq!(drain(&mut out), ["p4"]);
    }

    #[test]
    fn memory_stays_bounded_under_a_flood_of_future_packets() {
        let base = Instant::now();
        let mut buffer = ReorderBuffer::new(3, Duration::from_secs(10));
        let mut out = Vec::new();
        buffer.push(0, 0u16, base, &mut |r| out.push(r));
        for seq in (2..2000u16).step_by(2) {
            buffer.push(seq, seq, base, &mut |r| out.push(r));
            assert!(buffer.held() <= 3);
        }
    }

    fn parse(packet: &[u8]) -> (RtpHeader, Vec<u8>) {
        let (header, range) = RtpHeader::parse(packet).unwrap();
        (header, packet[range].to_vec())
    }

    #[test]
    fn the_sender_advances_sequence_and_timestamp_per_packet() {
        let base = Instant::now();
        let mut sender = RtpSender::new(7, 65535, 1000);
        let mut buf = Vec::new();
        let payload = [0x55u8; SAMPLES_PER_PACKET];
        sender.audio_packet(0, &payload, base, &mut buf);
        let (first, body) = parse(&buf);
        assert_eq!(body.len(), 160);
        assert!(first.marker, "the first packet starts a talkspurt");
        assert_eq!(
            (first.sequence, first.timestamp, first.ssrc),
            (65535, 1000, 7)
        );
        sender.audio_packet(0, &payload, at(base, 20), &mut buf);
        let (second, _) = parse(&buf);
        assert!(!second.marker);
        assert_eq!((second.sequence, second.timestamp), (0, 1160));
        // A burst (packets sent faster than real time) does not move the timestamp off its grid.
        sender.audio_packet(0, &payload, at(base, 22), &mut buf);
        assert_eq!(parse(&buf).0.timestamp, 1320);
    }

    #[test]
    fn a_pause_sets_the_marker_and_advances_the_timestamp_by_the_pause() {
        let base = Instant::now();
        let mut sender = RtpSender::new(1, 0, 0);
        let mut buf = Vec::new();
        let payload = [0u8; 160];
        sender.audio_packet(8, &payload, base, &mut buf);
        // Next packet one second later: the sent packet covered 20 ms, 980 ms of silence follow.
        sender.audio_packet(8, &payload, at(base, 1000), &mut buf);
        let (header, _) = parse(&buf);
        assert!(header.marker);
        assert_eq!(header.timestamp, 160 + 980 * 8);
    }

    #[test]
    fn a_clear_marks_the_next_packet_without_moving_the_timestamp() {
        let base = Instant::now();
        let mut sender = RtpSender::new(1, 0, 0);
        let mut buf = Vec::new();
        let payload = [0u8; 160];
        sender.audio_packet(0, &payload, base, &mut buf);
        sender.audio_packet(0, &payload, at(base, 20), &mut buf);
        sender.note_clear();
        sender.audio_packet(0, &payload, at(base, 40), &mut buf);
        let (header, _) = parse(&buf);
        assert!(header.marker);
        assert_eq!(header.timestamp, 320);
    }

    #[test]
    fn dtmf_packets_share_the_start_timestamp_and_move_the_audio_clock() {
        let base = Instant::now();
        let mut sender = RtpSender::new(1, 10, 5000);
        let mut buf = Vec::new();
        let payload = [0u8; 160];
        sender.audio_packet(0, &payload, base, &mut buf); // ts 5000, next 5160
        let mut sent = Vec::new();
        let packets = [
            (160u16, false),
            (320, false),
            (480, true),
            (480, true),
            (480, true),
        ];
        for (i, (duration, end)) in packets.into_iter().enumerate() {
            let event = TelephoneEvent {
                event: 9,
                end,
                volume: 10,
                duration,
            };
            sender.event_packet(101, event, i == 0, at(base, 20 * (i as u64 + 1)), &mut buf);
            sent.push(parse(&buf));
        }
        assert!(sent[0].0.marker && !sent[1].0.marker);
        assert!(
            sent.iter()
                .all(|(h, _)| h.payload_type == 101 && h.timestamp == 5160)
        );
        let sequences: Vec<u16> = sent.iter().map(|(h, _)| h.sequence).collect();
        assert_eq!(sequences, [11, 12, 13, 14, 15]);
        assert_eq!(sent[4].1, [9, 0x8a, 0x01, 0xe0]);
        // The next audio packet continues after the event: start + duration.
        sender.audio_packet(0, &payload, at(base, 120), &mut buf);
        let (audio, _) = parse(&buf);
        assert_eq!(audio.timestamp, 5160 + 480);
        assert_eq!(audio.sequence, 16);
    }

    #[test]
    fn random_senders_differ() {
        let a = RtpSender::random().unwrap();
        let b = RtpSender::random().unwrap();
        assert_ne!(
            (a.ssrc(), a.next_sequence(), a.next_timestamp()),
            (b.ssrc(), b.next_sequence(), b.next_timestamp())
        );
    }
}
