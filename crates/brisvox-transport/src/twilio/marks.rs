//! The marks brisvox has in flight on one Twilio stream, and the counters of the adapter.
//!
//! Twilio plays the audio it was sent in order and echoes a `mark` when the audio before it has
//! played, or when a `clear` flushed it (the echo of a flushed mark looks exactly like the echo of
//! a played one on the wire). The [`MarkBook`] is what tells them apart: [`MarkBook::clear`]
//! forgets every mark in flight, so an echo that arrives afterwards finds nothing and is dropped.
//! The book is shared with the other telephony adapters and lives in the crate-private `telephony` module; the
//! counters here are Twilio's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use crate::telephony::{Echo, MAX_MARKS_IN_FLIGHT, MarkBook};

/// Counters shared by every call of a [`TwilioServer`](super::TwilioServer). All relaxed atomics:
/// they are statistics, not synchronization.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) calls: AtomicU64,
    pub(crate) media_in: AtomicU64,
    pub(crate) media_out: AtomicU64,
    pub(crate) media_out_bytes: AtomicU64,
    pub(crate) inbound_chunks_lost: AtomicU64,
    pub(crate) marks_sent: AtomicU64,
    pub(crate) marks_echoed: AtomicU64,
    pub(crate) marks_cut: AtomicU64,
    pub(crate) marks_unmatched: AtomicU64,
    pub(crate) marks_evicted: AtomicU64,
    pub(crate) clears_sent: AtomicU64,
    pub(crate) foreign_stream_messages: AtomicU64,
    pub(crate) faults: AtomicU64,
    pub(crate) max_echo_latency_us: AtomicU64,
    pub(crate) max_unplayed_ms: AtomicU64,
    pub(crate) sessions_parked: AtomicU64,
    pub(crate) streams_resumed: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn max(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> TwilioStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        TwilioStats {
            calls: get(&self.calls),
            media_in: get(&self.media_in),
            media_out: get(&self.media_out),
            media_out_bytes: get(&self.media_out_bytes),
            inbound_chunks_lost: get(&self.inbound_chunks_lost),
            marks_sent: get(&self.marks_sent),
            marks_echoed: get(&self.marks_echoed),
            marks_cut: get(&self.marks_cut),
            marks_unmatched: get(&self.marks_unmatched),
            marks_evicted: get(&self.marks_evicted),
            clears_sent: get(&self.clears_sent),
            foreign_stream_messages: get(&self.foreign_stream_messages),
            faults: get(&self.faults),
            max_echo_latency: Duration::from_micros(get(&self.max_echo_latency_us)),
            max_unplayed: Duration::from_millis(get(&self.max_unplayed_ms)),
            sessions_parked: get(&self.sessions_parked),
            streams_resumed: get(&self.streams_resumed),
        }
    }
}

/// A snapshot of the adapter's counters, summed over every call since the server started
/// ([`TwilioServer::stats`](super::TwilioServer::stats)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TwilioStats {
    /// Calls that completed `start` and were handed to the engine.
    pub calls: u64,
    /// Caller `media` messages accepted.
    pub media_in: u64,
    /// `media` messages sent to Twilio.
    pub media_out: u64,
    /// Mu-law bytes sent to Twilio (8000 per second of audio).
    pub media_out_bytes: u64,
    /// Inbound `media.chunk` numbers that were skipped: Twilio dropped that audio (error 31930).
    pub inbound_chunks_lost: u64,
    /// Marks sent (engine marks and progress marks).
    pub marks_sent: u64,
    /// Marks Twilio echoed that were in flight.
    pub marks_echoed: u64,
    /// Marks forgotten because a `clear` flushed them.
    pub marks_cut: u64,
    /// Echoes of marks that were not in flight: flushed by a `clear`, forgotten, or foreign.
    pub marks_unmatched: u64,
    /// Marks forgotten to bound the book (a stream that does not echo marks).
    pub marks_evicted: u64,
    /// `clear` messages sent.
    pub clears_sent: u64,
    /// Messages dropped because their `streamSid` was not the stream's.
    pub foreign_stream_messages: u64,
    /// Non-fatal protocol faults reported to the engine.
    pub faults: u64,
    /// The longest time Twilio took from a mark we sent to its echo, over every echoed mark:
    /// playout delay plus the audio buffered ahead of the mark. Feeds `docs/LATENCY.md`.
    pub max_echo_latency: Duration,
    /// The most audio that was sent but not yet known played, on any stream.
    pub max_unplayed: Duration,
    /// Streams that ended on purpose (announced by the host) with the engine's session kept for
    /// the call's next stream.
    pub sessions_parked: u64,
    /// Parked sessions that went on with a new stream (`resume`).
    pub streams_resumed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_snapshot() {
        let counters = Counters::default();
        Counters::add(&counters.calls, 2);
        Counters::max(&counters.max_echo_latency_us, 1500);
        Counters::max(&counters.max_echo_latency_us, 900);
        let stats = counters.snapshot();
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.max_echo_latency, Duration::from_micros(1500));
    }
}
