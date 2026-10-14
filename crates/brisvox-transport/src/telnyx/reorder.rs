//! The reorder buffer for inbound `media`: Telnyx does not guarantee frame order ("the order of
//! events is not guaranteed and the chunk number can be used to reorder the events"), so caller
//! audio is released in `media.chunk` order.
//!
//! Sans-IO and bounded. The common case, the next chunk arriving with nothing held, never touches
//! the buffer (the caller delivers it directly, see [`Reorder::in_order`]); only a frame that
//! arrives ahead of its turn is copied into the buffer. At most [`Reorder::window`] frames are
//! held: when one more arrives, the smallest held chunk is released and the missing chunks before
//! it are counted as lost. The caller also flushes the buffer when the oldest held frame has waited
//! too long (a lost chunk must not stall the audio that follows it), using the injected clock: each
//! held frame carries the instant it arrived, and a missing chunk is given up on `hold` after the
//! first frame behind it arrived ([`Reorder::due_at`], [`Reorder::pop_due`]). A frame that arrived
//! later has not waited that long yet and stays put, so a gap further on still gets its full wait.
//!
//! A frame whose number is far from the one due cannot be reordered, only distrusted: one that is
//! behind it (late, or the counter restarted) or more than [`MAX_FORWARD_JUMP`] ahead (a corrupt
//! number) is dropped, and when more than [`Reorder::resync_after`] of them arrive in a row the
//! buffer concludes the numbering itself changed and re-anchors on the newest one
//! ([`Offer::Resynced`]). Without that, one wild chunk number would put every genuine frame behind
//! it for the rest of the call and the caller would go deaf with nothing reported.
//!
//! A chunk whose payload was unusable (empty, not base64) is still a chunk that arrived: the
//! caller offers it with no data ([`Reorder::offer`] with an empty `Vec`), which fills its place so
//! the frames behind it do not wait for it.

use std::collections::BTreeMap;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use tokio::time::Instant;

/// The furthest ahead of the chunk due a frame may be and still count as a (very) late arrival
/// of the audio before it: 1500 chunks is 30 s of 20 ms frames. Further than that is a corrupt
/// number or a numbering that changed.
pub(crate) const MAX_FORWARD_JUMP: u64 = 1500;

/// A frame released by the buffer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Released {
    /// The G.711 bytes; empty for a chunk that arrived without usable audio.
    pub(crate) data: Vec<u8>,
    /// Chunks skipped between the previous released frame and this one (lost audio).
    pub(crate) lost: u64,
}

/// What became of an offered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Held until its predecessors arrive (or are given up on).
    Held,
    /// Its turn has passed (a late frame), it is implausibly far ahead, or the chunk is already
    /// held (a duplicate): dropped.
    Dropped,
    /// So many frames in a row were out of range that the numbering is taken to have changed:
    /// the buffer dropped what it held (`discarded` frames) and now expects this very chunk, so
    /// the caller delivers the frame directly (`in_order` is true) and reports a fault.
    Resynced {
        /// Frames that were held and are gone.
        discarded: usize,
    },
}

/// A held frame and when it arrived.
#[derive(Debug)]
struct Held {
    data: Vec<u8>,
    at: Instant,
}

/// The buffer.
#[derive(Debug)]
pub(crate) struct Reorder {
    window: usize,
    /// The chunk number due next; `None` until the first frame.
    next: Option<u64>,
    pending: BTreeMap<u64, Held>,
    /// Frames in a row that were out of range (see the module docs).
    out_of_range: usize,
}

impl Reorder {
    /// A buffer that holds at most `window` frames.
    pub(crate) const fn new(window: usize) -> Self {
        Self {
            window,
            next: None,
            pending: BTreeMap::new(),
            out_of_range: 0,
        }
    }

    /// Out-of-range frames in a row after which the buffer re-anchors: never fewer than 4, so a
    /// tiny window does not make it twitchy.
    pub(crate) fn resync_after(&self) -> usize {
        self.window.max(4)
    }

    /// Frames held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing is held.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// When the hold timer must fire for the gap in front of the held frames: `hold` after the
    /// first frame behind it arrived. `None` when nothing is held.
    pub(crate) fn due_at(&self, hold: Duration) -> Option<Instant> {
        let first = self.pending.values().map(|held| held.at).min()?;
        Some(deadline_after(first, hold))
    }

    /// Whether `chunk` can be delivered right now without touching the buffer: nothing is held and
    /// it is the chunk due (or the first one ever seen). The caller then delivers it and calls
    /// [`advance`](Self::advance).
    pub(crate) fn in_order(&self, chunk: u64) -> bool {
        self.pending.is_empty() && self.next.is_none_or(|next| next == chunk)
    }

    /// Records that `chunk` was delivered directly (or consumed without audio).
    pub(crate) fn advance(&mut self, chunk: u64) {
        self.next = Some(chunk.saturating_add(1));
        self.out_of_range = 0;
    }

    /// Offers a frame that is not in order, arriving `at`. A late frame (its chunk was already
    /// released or given up on), an implausible one and a duplicate are dropped; see
    /// [`Offer::Resynced`] for a numbering that changed.
    pub(crate) fn offer(&mut self, chunk: u64, data: Vec<u8>, at: Instant) -> Offer {
        let next = *self.next.get_or_insert(chunk);
        if chunk < next || chunk - next > MAX_FORWARD_JUMP {
            self.out_of_range += 1;
            if self.out_of_range > self.resync_after() {
                let discarded = self.pending.len();
                self.pending.clear();
                self.next = Some(chunk);
                self.out_of_range = 0;
                return Offer::Resynced { discarded };
            }
            return Offer::Dropped;
        }
        if self.pending.contains_key(&chunk) {
            return Offer::Dropped;
        }
        self.out_of_range = 0;
        self.pending.insert(chunk, Held { data, at });
        Offer::Held
    }

    /// The next frame that may leave: the one due, or, when the buffer is over its window, the
    /// smallest held (the gap before it is given up on). Call until `None` after each offer.
    pub(crate) fn pop_ready(&mut self) -> Option<Released> {
        let (&chunk, _) = self.pending.first_key_value()?;
        let next = self.next.unwrap_or(chunk);
        if chunk == next || self.pending.len() > self.window {
            return self.release(chunk, next);
        }
        None
    }

    /// Releases the smallest held frame if the gap in front of it has been waited on for `hold`
    /// (see [`due_at`](Self::due_at)) at `now`, or if there is no gap. Call until `None` when the
    /// hold timer fires.
    pub(crate) fn pop_due(&mut self, now: Instant, hold: Duration) -> Option<Released> {
        let (&chunk, _) = self.pending.first_key_value()?;
        let next = self.next.unwrap_or(chunk);
        if chunk != next && self.due_at(hold)? > now {
            return None;
        }
        self.release(chunk, next)
    }

    /// Releases the smallest held frame whatever the window says (the stream ended).
    pub(crate) fn pop_oldest(&mut self) -> Option<Released> {
        let (&chunk, _) = self.pending.first_key_value()?;
        let next = self.next.unwrap_or(chunk);
        self.release(chunk, next)
    }

    fn release(&mut self, chunk: u64, next: u64) -> Option<Released> {
        let held = self.pending.remove(&chunk)?;
        self.next = Some(chunk.saturating_add(1));
        Some(Released {
            data: held.data,
            lost: chunk.saturating_sub(next),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn now() -> Instant {
        brisvox_core::SystemClock::shared().now()
    }

    fn frame(tag: u8) -> Vec<u8> {
        vec![tag; 4]
    }

    /// Feeds `chunks` (tagged with their own number) and returns the released tags in order, with
    /// the total lost.
    fn run(window: usize, chunks: &[u64]) -> (Vec<u8>, u64) {
        let mut buffer = Reorder::new(window);
        let mut out = Vec::new();
        let mut lost = 0;
        for &chunk in chunks {
            let tag = u8::try_from(chunk).unwrap();
            if buffer.in_order(chunk) {
                buffer.advance(chunk);
                out.push(tag);
                continue;
            }
            buffer.offer(chunk, frame(tag), now());
            while let Some(released) = buffer.pop_ready() {
                lost += released.lost;
                out.push(released.data[0]);
            }
        }
        while let Some(released) = buffer.pop_oldest() {
            lost += released.lost;
            out.push(released.data[0]);
        }
        (out, lost)
    }

    #[test]
    fn frames_in_order_never_touch_the_buffer() {
        let mut buffer = Reorder::new(4);
        for chunk in 1..=10 {
            assert!(buffer.in_order(chunk));
            buffer.advance(chunk);
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn swapped_frames_are_put_right() {
        assert_eq!(run(4, &[1, 3, 2, 4]), (vec![1, 2, 3, 4], 0));
        assert_eq!(run(4, &[1, 4, 3, 2, 5]), (vec![1, 2, 3, 4, 5], 0));
    }

    #[test]
    fn the_first_frame_anchors_the_numbering_whatever_its_number() {
        assert_eq!(run(4, &[7, 9, 8, 10]), (vec![7, 8, 9, 10], 0));
    }

    #[test]
    fn a_gap_is_given_up_on_when_the_window_overflows_and_counted_as_lost() {
        // Chunk 2 never arrives; with a window of 2 it is abandoned when 5 arrives.
        let (out, lost) = run(2, &[1, 3, 4, 5, 6]);
        assert_eq!(out, vec![1, 3, 4, 5, 6]);
        assert_eq!(lost, 1);
    }

    #[test]
    fn a_late_frame_is_dropped_after_its_gap_was_given_up() {
        let mut buffer = Reorder::new(1);
        buffer.advance(1);
        assert_eq!(buffer.offer(3, frame(3), now()), Offer::Held);
        assert_eq!(buffer.offer(4, frame(4), now()), Offer::Held);
        // Over the window: 3 leaves, 2 is lost.
        assert_eq!(
            buffer.pop_ready(),
            Some(Released {
                data: frame(3),
                lost: 1
            })
        );
        assert_eq!(buffer.pop_ready().map(|r| r.data), Some(frame(4)));
        // 2 shows up too late.
        assert_eq!(buffer.offer(2, frame(2), now()), Offer::Dropped);
        assert!(buffer.is_empty());
    }

    #[test]
    fn duplicates_are_dropped() {
        let mut buffer = Reorder::new(4);
        buffer.advance(1);
        assert_eq!(buffer.offer(3, frame(3), now()), Offer::Held);
        assert_eq!(buffer.offer(3, frame(3), now()), Offer::Dropped);
        assert_eq!(buffer.len(), 1);
        // An already delivered chunk repeated is late.
        assert_eq!(buffer.offer(1, frame(1), now()), Offer::Dropped);
    }

    #[test]
    fn the_hold_timer_releases_everything_in_order() {
        let mut buffer = Reorder::new(8);
        buffer.advance(1);
        buffer.offer(4, frame(4), now());
        buffer.offer(3, frame(3), now());
        assert_eq!(buffer.pop_ready(), None);
        assert_eq!(
            buffer.pop_oldest().map(|r| (r.data, r.lost)),
            Some((frame(3), 1))
        );
        assert_eq!(
            buffer.pop_oldest().map(|r| (r.data, r.lost)),
            Some((frame(4), 0))
        );
        assert_eq!(buffer.pop_oldest(), None);
    }

    #[test]
    fn the_buffer_never_holds_more_than_window_plus_one() {
        let mut buffer = Reorder::new(3);
        buffer.advance(0);
        let mut peak = 0;
        // Every other chunk, so nothing is ever contiguous.
        for chunk in (2..200).step_by(2) {
            buffer.offer(chunk, frame(1), now());
            peak = peak.max(buffer.len());
            while buffer.pop_ready().is_some() {}
        }
        assert!(peak <= 4, "peak {peak}");
    }

    #[test]
    fn chunk_numbers_at_the_top_of_the_range_do_not_overflow() {
        let mut buffer = Reorder::new(2);
        buffer.advance(u64::MAX);
        assert!(buffer.in_order(u64::MAX));
        assert_eq!(buffer.offer(u64::MAX - 1, frame(1), now()), Offer::Dropped);
    }

    #[test]
    fn a_wild_forward_chunk_is_dropped_and_the_stream_goes_on() {
        let mut buffer = Reorder::new(4);
        buffer.advance(100);
        assert_eq!(buffer.offer(4_000_000_000, frame(1), now()), Offer::Dropped);
        assert!(buffer.is_empty(), "never held");
        // The genuine frames behind it are not late.
        assert!(buffer.in_order(101));
        buffer.advance(101);
        assert!(buffer.in_order(102));
    }

    #[test]
    fn a_numbering_that_restarts_is_followed_after_a_few_late_frames() {
        let mut buffer = Reorder::new(4);
        buffer.advance(500);
        // The counter restarts at 1: the first frames look late.
        for chunk in 1..=4 {
            assert_eq!(
                buffer.offer(chunk, frame(1), now()),
                Offer::Dropped,
                "{chunk}"
            );
        }
        assert_eq!(
            buffer.offer(5, frame(5), now()),
            Offer::Resynced { discarded: 0 }
        );
        // The frame that triggered it is delivered directly and the new numbering flows.
        assert!(buffer.in_order(5));
        buffer.advance(5);
        assert!(buffer.in_order(6));
    }

    #[test]
    fn a_resync_drops_what_was_held_from_the_old_numbering() {
        let mut buffer = Reorder::new(2);
        buffer.advance(500);
        assert_eq!(buffer.offer(502, frame(2), now()), Offer::Held);
        // Late frames in a row (the window is 2, so the limit is 4).
        for chunk in 1..=4 {
            assert_eq!(buffer.offer(chunk, frame(1), now()), Offer::Dropped);
        }
        assert_eq!(
            buffer.offer(5, frame(5), now()),
            Offer::Resynced { discarded: 1 }
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn one_in_range_frame_resets_the_out_of_range_count() {
        let mut buffer = Reorder::new(4);
        buffer.advance(100);
        for _ in 0..3 {
            assert_eq!(buffer.offer(1, frame(1), now()), Offer::Dropped);
        }
        buffer.advance(101);
        for _ in 0..3 {
            assert_eq!(buffer.offer(1, frame(1), now()), Offer::Dropped);
        }
    }

    #[test]
    fn an_empty_chunk_fills_its_place_and_is_released_with_no_audio() {
        let mut buffer = Reorder::new(4);
        buffer.advance(1);
        // Chunk 2 arrived without usable audio, 3 behind it.
        assert_eq!(buffer.offer(3, frame(3), now()), Offer::Held);
        assert_eq!(buffer.offer(2, Vec::new(), now()), Offer::Held);
        let first = buffer.pop_ready().unwrap();
        assert!(first.data.is_empty());
        assert_eq!(first.lost, 0);
        assert_eq!(buffer.pop_ready().map(|r| r.data), Some(frame(3)));
    }

    #[test]
    fn a_gap_is_given_up_hold_after_the_first_frame_behind_it_arrived() {
        let hold = Duration::from_millis(60);
        let mut buffer = Reorder::new(8);
        buffer.advance(1);
        assert_eq!(buffer.due_at(hold), None);
        let t0 = now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // 5 arrived first (gap at 2..4), then 3.
        buffer.offer(5, frame(5), at(0));
        buffer.offer(3, frame(3), at(30));
        assert_eq!(buffer.due_at(hold), Some(at(60)));
        assert!(buffer.pop_due(at(59), hold).is_none());
        // Gap 2 is given up at 60 ms and 3 leaves; 5 follows only after ITS gap (4) has been
        // waited on: the first frame behind it is 5 itself, which arrived at 0.
        assert_eq!(buffer.pop_due(at(60), hold).map(|r| r.lost), Some(1));
        assert_eq!(buffer.pop_due(at(60), hold).map(|r| r.lost), Some(1));
        assert!(buffer.pop_due(at(60), hold).is_none());
    }

    #[test]
    fn a_frame_that_arrived_later_keeps_its_own_full_wait() {
        let hold = Duration::from_millis(60);
        let mut buffer = Reorder::new(8);
        buffer.advance(1);
        let t0 = now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        buffer.offer(3, frame(3), at(0));
        buffer.offer(2, frame(2), at(50));
        buffer.offer(5, frame(5), at(55));
        // 2 and 3 leave on arrival of 2; 5 (gap 4) arrived at 55 and is due at 115, not at 60.
        while buffer.pop_ready().is_some() {}
        assert_eq!(buffer.due_at(hold), Some(at(115)));
        assert!(buffer.pop_due(at(100), hold).is_none());
        assert_eq!(buffer.pop_due(at(115), hold).map(|r| r.lost), Some(1));
    }
}
