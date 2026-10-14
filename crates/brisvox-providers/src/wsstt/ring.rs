//! The bounded buffer of audio a session keeps for replay after a reconnect and for holding audio
//! back while the protocol cannot take it (a finalize that is being answered).
//!
//! Chunks are the frames the engine sent (after resampling to the wire rate), numbered with an
//! absolute index. Three positions move over them: `front` (the oldest retained chunk), `next`
//! (the first chunk not yet written to the socket) and the end. Sent chunks stay until the
//! protocol says the audio is settled ([`Ring::trim_to`], [`Ring::keep_last_sent`]) or the byte
//! cap pushes them out; unsent chunks are held back for as long as the gate is shut.
//!
//! Overflow policy: the oldest chunk is dropped first, whether sent or not (a held chunk that
//! is pushed out is audio that is lost, counted by the caller from [`Ring::push`]'s result).

use std::collections::VecDeque;

use bytes::Bytes;

/// What [`Ring::push`] did to make room.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pushed {
    /// Chunks dropped from the front to stay within the cap.
    pub(crate) dropped: usize,
    /// Of those, chunks that had not been written to the socket (audio that is lost).
    pub(crate) dropped_unsent: usize,
}

/// Retained audio chunks (see the module docs).
#[derive(Debug)]
pub(crate) struct Ring {
    chunks: VecDeque<Bytes>,
    /// Absolute index of `chunks[0]`.
    front: u64,
    /// Absolute index of the first chunk not yet written.
    next: u64,
    /// Bytes in all retained chunks.
    bytes: usize,
    /// Bytes in the chunks before `next`.
    sent_bytes: usize,
    /// Most bytes retained.
    cap: usize,
}

impl Ring {
    /// A ring that retains at most `cap` bytes (at least one chunk is always kept).
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            front: 0,
            next: 0,
            bytes: 0,
            sent_bytes: 0,
            cap,
        }
    }

    /// Absolute index one past the newest chunk (the position of the next one pushed).
    pub(crate) fn end(&self) -> u64 {
        self.front.saturating_add(self.chunks.len() as u64)
    }

    /// Absolute index of the first unsent chunk.
    pub(crate) fn next_index(&self) -> u64 {
        self.next
    }

    /// Appends a chunk, dropping the oldest ones beyond the cap.
    pub(crate) fn push(&mut self, chunk: Bytes) -> Pushed {
        let mut pushed = Pushed::default();
        self.bytes = self.bytes.saturating_add(chunk.len());
        self.chunks.push_back(chunk);
        while self.bytes > self.cap && self.chunks.len() > 1 {
            let was_sent = self.front < self.next;
            let Some(old) = self.chunks.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.len());
            self.front += 1;
            pushed.dropped += 1;
            if was_sent {
                self.sent_bytes = self.sent_bytes.saturating_sub(old.len());
            } else {
                pushed.dropped_unsent += 1;
                self.next = self.front;
            }
        }
        pushed
    }

    /// The first unsent chunk, if it lies before `limit` (an absolute index): audio past a
    /// pending finalize position is not to be sent before the finalize.
    pub(crate) fn peek_next(&self, limit: u64) -> Option<&Bytes> {
        if self.next >= limit {
            return None;
        }
        let at = usize::try_from(self.next.checked_sub(self.front)?).ok()?;
        self.chunks.get(at)
    }

    /// Whether any retained chunk has not been written yet.
    pub(crate) fn has_unsent(&self) -> bool {
        self.next < self.end()
    }

    /// Marks the chunk `peek_next` returned as written.
    pub(crate) fn advance(&mut self) {
        let Some(at) = self
            .next
            .checked_sub(self.front)
            .and_then(|d| usize::try_from(d).ok())
        else {
            return;
        };
        if let Some(chunk) = self.chunks.get(at) {
            self.sent_bytes = self.sent_bytes.saturating_add(chunk.len());
            self.next += 1;
        }
    }

    /// Makes every retained chunk unsent again: a new connection replays them.
    pub(crate) fn rewind(&mut self) {
        self.next = self.front;
        self.sent_bytes = 0;
    }

    /// Drops sent chunks before the absolute index `mark` (never an unsent one).
    pub(crate) fn trim_to(&mut self, mark: u64) {
        while self.front < mark && self.front < self.next {
            self.drop_front();
        }
    }

    /// Drops the oldest sent chunks until at most `keep` bytes of sent audio remain.
    pub(crate) fn keep_last_sent(&mut self, keep: usize) {
        while self.sent_bytes > keep && self.front < self.next {
            self.drop_front();
        }
    }

    fn drop_front(&mut self) {
        if let Some(old) = self.chunks.pop_front() {
            self.bytes = self.bytes.saturating_sub(old.len());
            self.sent_bytes = self.sent_bytes.saturating_sub(old.len());
            self.front += 1;
        }
    }

    /// Bytes retained (sent and unsent).
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Chunks that have not been written yet.
    #[cfg(test)]
    pub(crate) fn unsent(&self) -> u64 {
        self.end().saturating_sub(self.next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize) -> Bytes {
        Bytes::from(vec![0_u8; n])
    }

    #[test]
    fn chunks_are_sent_in_order_up_to_the_limit() {
        let mut ring = Ring::new(1000);
        for _ in 0..3 {
            ring.push(chunk(10));
        }
        assert_eq!(ring.end(), 3);
        // A limit of 2: only chunks 0 and 1 may go.
        assert!(ring.peek_next(2).is_some());
        ring.advance();
        assert!(ring.peek_next(2).is_some());
        ring.advance();
        assert!(ring.peek_next(2).is_none());
        assert!(ring.peek_next(u64::MAX).is_some());
        assert_eq!(ring.unsent(), 1);
    }

    #[test]
    fn the_cap_drops_the_oldest_and_counts_lost_unsent_audio() {
        let mut ring = Ring::new(25);
        assert_eq!(ring.push(chunk(10)), Pushed::default());
        assert_eq!(ring.push(chunk(10)), Pushed::default());
        // Third chunk: 30 bytes > 25, the oldest (unsent) goes.
        let pushed = ring.push(chunk(10));
        assert_eq!(
            pushed,
            Pushed {
                dropped: 1,
                dropped_unsent: 1
            }
        );
        assert_eq!(ring.bytes(), 20);
        assert_eq!(ring.next_index(), 1);
        // A sent chunk pushed out is not "lost".
        ring.advance();
        let pushed = ring.push(chunk(10));
        assert_eq!(
            pushed,
            Pushed {
                dropped: 1,
                dropped_unsent: 0
            }
        );
    }

    #[test]
    fn one_chunk_larger_than_the_cap_is_still_kept() {
        let mut ring = Ring::new(5);
        ring.push(chunk(50));
        assert_eq!(ring.bytes(), 50);
        assert!(ring.peek_next(u64::MAX).is_some());
    }

    #[test]
    fn trim_to_never_drops_unsent_audio() {
        let mut ring = Ring::new(1000);
        for _ in 0..4 {
            ring.push(chunk(10));
        }
        ring.advance();
        ring.advance();
        ring.trim_to(4);
        // Only the two sent chunks could go.
        assert_eq!(ring.bytes(), 20);
        assert_eq!(ring.unsent(), 2);
        assert_eq!(ring.next_index(), 2);
    }

    #[test]
    fn keep_last_sent_trims_sent_audio_down_to_the_tail() {
        let mut ring = Ring::new(1000);
        for _ in 0..5 {
            ring.push(chunk(10));
        }
        for _ in 0..4 {
            ring.advance();
        }
        ring.keep_last_sent(20);
        assert_eq!(ring.bytes(), 30);
        assert_eq!(ring.unsent(), 1);
        ring.keep_last_sent(0);
        assert_eq!(ring.bytes(), 10);
        assert_eq!(ring.unsent(), 1);
    }

    #[test]
    fn rewind_makes_everything_retained_unsent_again() {
        let mut ring = Ring::new(1000);
        ring.push(chunk(10));
        ring.push(chunk(10));
        ring.advance();
        ring.advance();
        ring.rewind();
        assert_eq!(ring.unsent(), 2);
        ring.advance();
        ring.keep_last_sent(0);
        assert_eq!(ring.unsent(), 1);
        assert_eq!(ring.bytes(), 10);
    }
}
