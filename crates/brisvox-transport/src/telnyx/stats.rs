//! The counters of the Telnyx adapter ([`TelnyxServer::stats`](super::TelnyxServer::stats)).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Counters shared by every call of a [`TelnyxServer`](super::TelnyxServer). All relaxed atomics:
/// they are statistics, not synchronization.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) calls: AtomicU64,
    pub(crate) media_in: AtomicU64,
    pub(crate) media_out: AtomicU64,
    pub(crate) media_out_bytes: AtomicU64,
    pub(crate) inbound_chunks_lost: AtomicU64,
    pub(crate) media_reordered: AtomicU64,
    pub(crate) media_late: AtomicU64,
    pub(crate) media_before_start: AtomicU64,
    pub(crate) marks_sent: AtomicU64,
    pub(crate) marks_echoed: AtomicU64,
    pub(crate) marks_cut: AtomicU64,
    pub(crate) marks_unmatched: AtomicU64,
    pub(crate) marks_evicted: AtomicU64,
    pub(crate) clears_sent: AtomicU64,
    pub(crate) foreign_stream_messages: AtomicU64,
    pub(crate) faults: AtomicU64,
    pub(crate) telnyx_errors: AtomicU64,
    pub(crate) duplicate_calls_refused: AtomicU64,
    pub(crate) hangups_requested: AtomicU64,
    pub(crate) hangups_failed: AtomicU64,
    pub(crate) hangups_skipped: AtomicU64,
    pub(crate) chunk_resyncs: AtomicU64,
    pub(crate) dtmf_reordered: AtomicU64,
    pub(crate) max_echo_latency_us: AtomicU64,
    pub(crate) max_unplayed_ms: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn max(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> TelnyxStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        TelnyxStats {
            calls: get(&self.calls),
            media_in: get(&self.media_in),
            media_out: get(&self.media_out),
            media_out_bytes: get(&self.media_out_bytes),
            inbound_chunks_lost: get(&self.inbound_chunks_lost),
            media_reordered: get(&self.media_reordered),
            media_late: get(&self.media_late),
            media_before_start: get(&self.media_before_start),
            marks_sent: get(&self.marks_sent),
            marks_echoed: get(&self.marks_echoed),
            marks_cut: get(&self.marks_cut),
            marks_unmatched: get(&self.marks_unmatched),
            marks_evicted: get(&self.marks_evicted),
            clears_sent: get(&self.clears_sent),
            foreign_stream_messages: get(&self.foreign_stream_messages),
            faults: get(&self.faults),
            telnyx_errors: get(&self.telnyx_errors),
            duplicate_calls_refused: get(&self.duplicate_calls_refused),
            hangups_requested: get(&self.hangups_requested),
            hangups_failed: get(&self.hangups_failed),
            hangups_skipped: get(&self.hangups_skipped),
            chunk_resyncs: get(&self.chunk_resyncs),
            dtmf_reordered: get(&self.dtmf_reordered),
            max_echo_latency: Duration::from_micros(get(&self.max_echo_latency_us)),
            max_unplayed: Duration::from_millis(get(&self.max_unplayed_ms)),
        }
    }
}

/// A snapshot of the adapter's counters, summed over every call since the server started
/// ([`TelnyxServer::stats`](super::TelnyxServer::stats)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TelnyxStats {
    /// Calls that completed `start` and were handed to the engine.
    pub calls: u64,
    /// Caller `media` messages handed to the engine (reordered ones included).
    pub media_in: u64,
    /// `media` messages sent to Telnyx.
    pub media_out: u64,
    /// G.711 bytes sent to Telnyx (8000 per second of audio).
    pub media_out_bytes: u64,
    /// Inbound `media.chunk` numbers that never arrived in time: that audio is lost.
    pub inbound_chunks_lost: u64,
    /// Inbound `media` messages that took the reorder buffer's path: they arrived ahead of
    /// their turn, or while earlier frames were still held (a frame that arrives in order with
    /// nothing held never touches the buffer).
    pub media_reordered: u64,
    /// Inbound `media` messages dropped because their turn had passed, their chunk number was
    /// implausible, or the chunk was a duplicate.
    pub media_late: u64,
    /// `media` (and other) messages that arrived before `start` and were dropped.
    pub media_before_start: u64,
    /// Marks sent (engine marks and progress marks).
    pub marks_sent: u64,
    /// Marks Telnyx echoed that were in flight.
    pub marks_echoed: u64,
    /// Marks forgotten because a `clear` flushed them.
    pub marks_cut: u64,
    /// Echoes of marks that were not in flight: flushed by a `clear`, forgotten, or foreign.
    pub marks_unmatched: u64,
    /// Marks forgotten to bound the book (a stream that does not echo marks).
    pub marks_evicted: u64,
    /// `clear` messages sent.
    pub clears_sent: u64,
    /// Messages dropped because their `stream_id` was not the stream's.
    pub foreign_stream_messages: u64,
    /// Non-fatal protocol faults reported to the engine.
    pub faults: u64,
    /// `error` frames Telnyx sent (`100002` to `100005`).
    pub telnyx_errors: u64,
    /// Connections refused because their `call_control_id` was streaming or had streamed already.
    pub duplicate_calls_refused: u64,
    /// Times the host's hang-up hook was called.
    pub hangups_requested: u64,
    /// Hang-up hook calls that failed or timed out.
    pub hangups_failed: u64,
    /// Hang-ups that were due but could not be asked for because `start` carried no
    /// `call_control_id` to name the call.
    pub hangups_skipped: u64,
    /// Times the inbound `media.chunk` numbering was taken to have changed (a restarted counter, a
    /// corrupt number) and the reorder buffer re-anchored on it.
    pub chunk_resyncs: u64,
    /// `dtmf` events delivered in a different order than they arrived in (put right by
    /// `sequence_number`).
    pub dtmf_reordered: u64,
    /// The longest time Telnyx took from a mark we sent to its echo, over every echoed mark:
    /// playout delay plus the audio buffered ahead of the mark. Feeds `docs/LATENCY.md`.
    pub max_echo_latency: Duration,
    /// The most audio that was sent but not yet known played, on any stream.
    pub max_unplayed: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_snapshot() {
        let counters = Counters::default();
        Counters::add(&counters.calls, 2);
        Counters::add(&counters.media_late, 3);
        Counters::max(&counters.max_echo_latency_us, 1500);
        Counters::max(&counters.max_echo_latency_us, 900);
        let stats = counters.snapshot();
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.media_late, 3);
        assert_eq!(stats.max_echo_latency, Duration::from_micros(1500));
    }
}
