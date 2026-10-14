//! The bounded ring of audio kept for replay after a reconnect.
//!
//! Every chunk the [`Batcher`](super::batch::Batcher) produces is appended here (as a refcounted
//! [`Bytes`], never copied) with its absolute position in the session's audio stream. Chunks
//! leave the ring when the server has finished the turn they belong to ([`ReplayRing::trim_to`])
//! or, as a hard bound on memory, when the ring holds more than its capacity (oldest first).
//!
//! Unlike a byte-addressed ring, this one only ever drops **whole chunks**: AssemblyAI rejects
//! chunks shorter than 50 ms, so a replay must be made of chunks that are valid on their own.
//! After a reconnect, the session replays from [`ReplayRing::front`], so the new connection hears
//! the audio that was not yet turned into a final, plus at most the rest of one chunk.

use std::collections::VecDeque;

use bytes::Bytes;

/// Bounded buffer of recent audio chunks, addressed by absolute stream position in bytes.
#[derive(Debug)]
pub(crate) struct ReplayRing {
    /// `(absolute start position, data)`, contiguous and ordered.
    chunks: VecDeque<(u64, Bytes)>,
    /// Absolute position of the first stored byte.
    front: u64,
    /// Absolute position after the last stored byte (= total bytes pushed).
    total: u64,
    /// Most bytes kept; older chunks are dropped beyond this.
    capacity: u64,
}

impl ReplayRing {
    /// An empty ring holding at most about `capacity` bytes (whole chunks are dropped, so it may
    /// exceed this by less than one chunk).
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            front: 0,
            total: 0,
            capacity,
        }
    }

    /// Absolute position of the first stored byte (a chunk boundary).
    pub(crate) fn front(&self) -> u64 {
        self.front
    }

    /// Absolute position after the last byte pushed so far.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Bytes currently stored.
    pub(crate) fn len(&self) -> u64 {
        self.total.saturating_sub(self.front)
    }

    /// Appends a chunk; evicts the oldest chunks while the ring is over capacity (the chunk just
    /// pushed always stays).
    pub(crate) fn push(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }
        let start = self.total;
        self.total = self.total.saturating_add(data.len() as u64);
        self.chunks.push_back((start, data));
        while self.len() > self.capacity && self.chunks.len() > 1 {
            self.pop_front();
        }
    }

    /// Drops the chunks that end at or before absolute position `pos`. A chunk that `pos` cuts
    /// through stays whole.
    pub(crate) fn trim_to(&mut self, pos: u64) {
        while let Some((start, data)) = self.chunks.front() {
            if start.saturating_add(data.len() as u64) <= pos {
                self.pop_front();
            } else {
                break;
            }
        }
    }

    fn pop_front(&mut self) {
        if let Some((_, dropped)) = self.chunks.pop_front() {
            self.front = self.front.saturating_add(dropped.len() as u64);
        }
    }

    /// The stored chunk that holds absolute position `from` (a refcounted view, no copy), with
    /// its start position. `None` if nothing at or after `from` is stored. A `from` before the
    /// front yields the first chunk.
    pub(crate) fn chunk_from(&self, from: u64) -> Option<(u64, Bytes)> {
        let from = from.max(self.front);
        let idx = self
            .chunks
            .partition_point(|(start, data)| start.saturating_add(data.len() as u64) <= from);
        self.chunks.get(idx).map(|(s, d)| (*s, d.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize, fill: u8) -> Bytes {
        Bytes::from(vec![fill; n])
    }

    #[test]
    fn pushing_tracks_positions_without_copying() {
        let mut ring = ReplayRing::new(1000);
        let data = chunk(100, 1);
        let ptr = data.as_ptr();
        ring.push(data);
        ring.push(chunk(50, 2));
        assert_eq!((ring.front(), ring.total(), ring.len()), (0, 150, 150));
        let (start, first) = ring.chunk_from(0).unwrap();
        assert_eq!((start, first.as_ptr()), (0, ptr));
        assert_eq!(ring.chunk_from(100).unwrap().0, 100);
        assert_eq!(ring.chunk_from(150), None);
    }

    #[test]
    fn the_oldest_chunks_are_evicted_beyond_capacity_but_the_newest_stays() {
        let mut ring = ReplayRing::new(250);
        for i in 0..5_u8 {
            ring.push(chunk(100, i));
        }
        assert_eq!((ring.front(), ring.total()), (300, 500));
        assert_eq!(ring.chunk_from(0).unwrap().1[0], 3);
        let mut tiny = ReplayRing::new(10);
        tiny.push(chunk(100, 9));
        assert_eq!(tiny.len(), 100, "a single oversized chunk is kept");
    }

    #[test]
    fn trimming_drops_whole_chunks_only() {
        let mut ring = ReplayRing::new(10_000);
        ring.push(chunk(100, 1));
        ring.push(chunk(100, 2));
        ring.push(chunk(100, 3));
        ring.trim_to(150);
        assert_eq!(ring.front(), 100, "the cut chunk stays whole");
        assert_eq!(ring.chunk_from(0).unwrap().1.len(), 100);
        ring.trim_to(200);
        assert_eq!(ring.front(), 200);
        ring.trim_to(10);
        assert_eq!(ring.front(), 200, "trimming backwards is a no-op");
        ring.trim_to(10_000);
        assert_eq!((ring.front(), ring.len()), (300, 0));
        assert_eq!(ring.chunk_from(0), None);
        ring.push(chunk(60, 4));
        assert_eq!((ring.front(), ring.total()), (300, 360));
    }

    #[test]
    fn a_position_inside_a_chunk_returns_that_whole_chunk() {
        let mut ring = ReplayRing::new(10_000);
        ring.push(chunk(100, 1));
        ring.push(chunk(100, 2));
        let (start, data) = ring.chunk_from(130).unwrap();
        assert_eq!((start, data.len(), data[0]), (100, 100, 2));
    }

    #[test]
    fn empty_chunks_are_ignored() {
        let mut ring = ReplayRing::new(10);
        ring.push(Bytes::new());
        assert_eq!((ring.total(), ring.len()), (0, 0));
    }
}
