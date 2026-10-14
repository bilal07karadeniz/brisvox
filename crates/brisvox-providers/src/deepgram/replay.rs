//! The bounded ring of audio kept for replay after a reconnect.
//!
//! Every chunk the engine sends is appended here (as a refcounted [`Bytes`], never copied) with
//! its absolute position in the session's audio stream. Chunks leave the ring when the server has
//! finalized the audio they hold ([`ReplayRing::trim_to`]) or, as a hard bound on memory, when
//! the ring holds more than its capacity (oldest first). After a reconnect, the session replays
//! from [`ReplayRing::front`], so the new connection hears exactly the audio that was not yet
//! turned into final text.

use std::collections::VecDeque;

use bytes::Bytes;

/// Bounded buffer of recent audio, addressed by absolute stream position in bytes.
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
    /// Bytes per sample: trims land on sample boundaries.
    align: u64,
}

impl ReplayRing {
    /// An empty ring holding at most about `capacity` bytes (whole chunks are dropped, so it may
    /// exceed this by less than one chunk).
    pub(crate) fn new(capacity: u64, align: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            front: 0,
            total: 0,
            capacity,
            align: align.max(1),
        }
    }

    /// Absolute position of the first stored byte.
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
    /// pushed always stays). Eviction leaves the front on a sample boundary, so a replay never
    /// starts in the middle of a sample even when the caller's chunks have odd sizes.
    pub(crate) fn push(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }
        let start = self.total;
        self.total = self.total.saturating_add(data.len() as u64);
        self.chunks.push_back((start, data));
        let mut evicted = false;
        while self.len() > self.capacity && self.chunks.len() > 1 {
            if let Some((_, dropped)) = self.chunks.pop_front() {
                self.front = self.front.saturating_add(dropped.len() as u64);
                evicted = true;
            }
        }
        if evicted {
            // Round up: the partial sample at the cut belongs to audio that is gone.
            let aligned = self.front.div_ceil(self.align).saturating_mul(self.align);
            self.drop_before(aligned.min(self.total));
        }
    }

    /// Drops everything before absolute position `pos` (clamped to what is stored, rounded down
    /// to a sample boundary).
    pub(crate) fn trim_to(&mut self, pos: u64) {
        let pos = (pos / self.align) * self.align;
        self.drop_before(pos.min(self.total));
    }

    /// Removes the stored bytes before `pos` (`pos <= total`) and moves the front there.
    fn drop_before(&mut self, pos: u64) {
        if pos <= self.front {
            return;
        }
        while let Some((start, data)) = self.chunks.front() {
            let end = start.saturating_add(data.len() as u64);
            if end <= pos {
                self.chunks.pop_front();
            } else {
                if *start < pos {
                    let cut = usize::try_from(pos - *start).unwrap_or(usize::MAX);
                    if let Some((s, d)) = self.chunks.front_mut() {
                        *d = d.slice(cut.min(d.len())..);
                        *s = pos;
                    }
                }
                break;
            }
        }
        self.front = pos;
    }

    /// The stored bytes from absolute position `from` on, up to the end of the chunk that holds
    /// `from` (a refcounted view, no copy). `None` if nothing at or after `from` is stored.
    pub(crate) fn chunk_from(&self, from: u64) -> Option<Bytes> {
        let from = from.max(self.front);
        let idx = self
            .chunks
            .partition_point(|(start, data)| start.saturating_add(data.len() as u64) <= from);
        let (start, data) = self.chunks.get(idx)?;
        let skip = usize::try_from(from.saturating_sub(*start)).unwrap_or(usize::MAX);
        Some(data.slice(skip.min(data.len())..))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize, fill: u8) -> Bytes {
        Bytes::from(vec![fill; n])
    }

    fn collect(ring: &ReplayRing, from: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pos = from.max(ring.front());
        while let Some(c) = ring.chunk_from(pos) {
            pos += c.len() as u64;
            out.extend_from_slice(&c);
        }
        out
    }

    #[test]
    fn pushing_tracks_positions_without_copying() {
        let mut ring = ReplayRing::new(1000, 2);
        let data = chunk(100, 1);
        let ptr = data.as_ptr();
        ring.push(data);
        ring.push(chunk(50, 2));
        assert_eq!((ring.front(), ring.total(), ring.len()), (0, 150, 150));
        assert_eq!(ring.chunk_from(0).map(|c| c.as_ptr()), Some(ptr));
        assert_eq!(collect(&ring, 0).len(), 150);
    }

    #[test]
    fn the_oldest_chunks_are_evicted_beyond_capacity_but_the_newest_stays() {
        let mut ring = ReplayRing::new(250, 2);
        for i in 0..5_u8 {
            ring.push(chunk(100, i));
        }
        // 5 x 100 pushed, at most 250 kept: the first three chunks are gone.
        assert_eq!((ring.front(), ring.total()), (300, 500));
        assert_eq!(
            collect(&ring, 0),
            [vec![3_u8; 100], vec![4_u8; 100]].concat()
        );
        let mut tiny = ReplayRing::new(10, 2);
        tiny.push(chunk(100, 9));
        assert_eq!(tiny.len(), 100, "a single oversized chunk is kept");
    }

    #[test]
    fn trimming_cuts_inside_a_chunk_on_a_sample_boundary() {
        let mut ring = ReplayRing::new(10_000, 2);
        ring.push(chunk(100, 1));
        ring.push(chunk(100, 2));
        ring.trim_to(51);
        assert_eq!(ring.front(), 50, "rounded down to a whole sample");
        assert_eq!(collect(&ring, 0).len(), 150);
        ring.trim_to(100);
        assert_eq!(collect(&ring, 0), vec![2_u8; 100]);
        ring.trim_to(10_000);
        assert_eq!((ring.front(), ring.len()), (200, 0));
        assert_eq!(ring.chunk_from(0), None);
        ring.push(chunk(40, 3));
        assert_eq!((ring.front(), ring.total()), (200, 240));
    }

    #[test]
    fn trimming_backwards_is_a_no_op_and_positions_before_the_front_clamp() {
        let mut ring = ReplayRing::new(10_000, 2);
        ring.push(chunk(100, 1));
        ring.trim_to(60);
        ring.trim_to(10);
        assert_eq!(ring.front(), 60);
        assert_eq!(ring.chunk_from(0).map(|c| c.len()), Some(40));
    }

    #[test]
    fn chunk_from_a_middle_position_returns_the_rest_of_that_chunk() {
        let mut ring = ReplayRing::new(10_000, 2);
        ring.push(chunk(100, 1));
        ring.push(chunk(100, 2));
        assert_eq!(ring.chunk_from(30).map(|c| c.len()), Some(70));
        assert_eq!(ring.chunk_from(100).map(|c| c.len()), Some(100));
        assert_eq!(ring.chunk_from(200), None);
    }

    #[test]
    fn eviction_never_leaves_the_front_inside_a_sample() {
        // Odd-sized chunks of 16-bit audio (a frame split mid-sample is possible on a socket).
        let mut ring = ReplayRing::new(250, 2);
        for i in 0..6_u8 {
            ring.push(chunk(101, i));
        }
        assert_eq!(ring.front() % 2, 0, "front {}", ring.front());
        assert!(ring.len() <= 251);
        // What is stored is exactly the bytes from the front on.
        let bytes = collect(&ring, 0);
        assert_eq!(bytes.len() as u64, ring.len());
        // Mu-law (one byte per sample) is never rounded.
        let mut mulaw = ReplayRing::new(250, 1);
        for i in 0..6_u8 {
            mulaw.push(chunk(101, i));
        }
        assert_eq!(mulaw.front(), 404);
    }

    #[test]
    fn empty_chunks_are_ignored() {
        let mut ring = ReplayRing::new(10, 2);
        ring.push(Bytes::new());
        assert_eq!((ring.total(), ring.len()), (0, 0));
    }
}
