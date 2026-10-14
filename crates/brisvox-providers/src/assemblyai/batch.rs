//! Turns the caller's audio frames into chunks the server accepts.
//!
//! AssemblyAI closes a session that receives a chunk shorter than 50 ms or longer than 1000 ms
//! (close code 3007). The engine sends 10-20 ms frames, so frames are held until 50 ms are there.
//! A caller chunk that is already long enough is passed on as it is (a refcounted [`Bytes`], no
//! copy); only short frames are copied into the holding buffer. Every chunk leaving the batcher
//! is a whole number of samples between `min` and `max` bytes, so the session's replay ring
//! stores chunks that can be sent again as they are.

use bytes::{Bytes, BytesMut};

/// Collects audio into chunks of `min..=max` bytes.
#[derive(Debug)]
pub(crate) struct Batcher {
    /// Audio held back: shorter than `min`, or a trailing partial sample.
    held: BytesMut,
    /// Smallest chunk (a whole number of samples).
    min: usize,
    /// Largest chunk (a whole number of samples, at least `2 * min`).
    max: usize,
    /// Bytes per sample.
    align: usize,
    /// The byte of a silent sample, used to pad a short tail.
    silence: u8,
}

impl Batcher {
    /// A batcher for chunks of `min..=max` bytes. `min` is rounded up and `max` down to whole
    /// samples; `max` is raised to at least `2 * min` so that every split leaves valid pieces.
    pub(crate) fn new(min: usize, max: usize, align: usize, silence: u8) -> Self {
        let align = align.max(1);
        let min = min.max(1).div_ceil(align) * align;
        let max = (max / align * align).max(min.saturating_mul(2));
        Self {
            held: BytesMut::new(),
            min,
            max,
            align,
            silence,
        }
    }

    /// Whether nothing is held back.
    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Adds `data`; appends every chunk that is now complete to `out`.
    pub(crate) fn push(&mut self, data: Bytes, out: &mut Vec<Bytes>) {
        if data.is_empty() {
            return;
        }
        if self.held.is_empty() && data.len() >= self.min {
            self.emit(data, out);
            return;
        }
        self.held.extend_from_slice(&data);
        if self.held.len() >= self.min {
            let all = self.held.split().freeze();
            self.emit(all, out);
        }
    }

    /// Pads what is held with silence up to the smallest chunk and appends it to `out`. Does
    /// nothing when nothing is held.
    pub(crate) fn flush_padded(&mut self, out: &mut Vec<Bytes>) {
        if self.held.is_empty() {
            return;
        }
        if self.held.len() < self.min {
            self.held.resize(self.min, self.silence);
        }
        let all = self.held.split().freeze();
        self.emit(all, out);
    }

    /// Splits `data` (at least `min` bytes) into valid chunks; a trailing partial sample goes
    /// back into the holding buffer.
    fn emit(&mut self, data: Bytes, out: &mut Vec<Bytes>) {
        let whole = data.len() - data.len() % self.align;
        if whole < data.len() {
            self.held
                .extend_from_slice(data.get(whole..).unwrap_or_default());
        }
        let mut rest = data.slice(..whole);
        while !rest.is_empty() {
            let remaining = rest.len();
            let take = if remaining <= self.max {
                remaining
            } else if remaining - self.max < self.min {
                // Leave a tail of exactly `min` rather than one that is too short.
                remaining - self.min
            } else {
                self.max
            };
            out.push(rest.split_to(take));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 16 kHz PCM16: 32 bytes per ms. min 50 ms, max 500 ms.
    const MIN: usize = 1600;
    const MAX: usize = 16_000;

    fn batcher() -> Batcher {
        Batcher::new(MIN, MAX, 2, 0)
    }

    fn frame(n: usize, fill: u8) -> Bytes {
        Bytes::from(vec![fill; n])
    }

    fn sizes(out: &[Bytes]) -> Vec<usize> {
        out.iter().map(Bytes::len).collect()
    }

    #[test]
    fn short_frames_are_held_until_fifty_milliseconds_are_there() {
        let mut b = batcher();
        let mut out = Vec::new();
        b.push(frame(640, 1), &mut out); // 20 ms
        b.push(frame(640, 2), &mut out); // 40 ms
        assert!(out.is_empty() && !b.is_empty());
        b.push(frame(640, 3), &mut out); // 60 ms
        assert_eq!(sizes(&out), [1920]);
        assert!(b.is_empty());
        assert_eq!(out[0][0], 1);
        assert_eq!(out[0][1919], 3);
    }

    #[test]
    fn a_long_enough_chunk_goes_through_without_a_copy() {
        let mut b = batcher();
        let data = frame(3200, 7);
        let ptr = data.as_ptr();
        let mut out = Vec::new();
        b.push(data, &mut out);
        assert_eq!(sizes(&out), [3200]);
        assert_eq!(out[0].as_ptr(), ptr);
    }

    #[test]
    fn oversized_chunks_are_split_and_no_piece_is_too_short() {
        let mut b = batcher();
        let mut out = Vec::new();
        // 16_000 + 100 bytes: a plain split would leave 100 bytes.
        b.push(frame(MAX + 100, 1), &mut out);
        assert_eq!(sizes(&out), [MAX + 100 - MIN, MIN]);
        out.clear();
        b.push(frame(3 * MAX + 1000, 1), &mut out);
        assert!(
            out.iter().all(|c| (MIN..=MAX).contains(&c.len())),
            "{:?}",
            sizes(&out)
        );
        assert_eq!(out.iter().map(Bytes::len).sum::<usize>(), 3 * MAX + 1000);
    }

    #[test]
    fn chunks_hold_whole_samples_and_the_odd_byte_waits() {
        let mut b = batcher();
        let mut out = Vec::new();
        b.push(frame(1601, 1), &mut out);
        assert_eq!(sizes(&out), [1600]);
        assert!(!b.is_empty(), "the odd byte is held");
        b.push(frame(1, 2), &mut out);
        assert_eq!(sizes(&out), [1600], "one sample is far below the minimum");
        b.flush_padded(&mut out);
        assert_eq!(sizes(&out), [1600, 1600]);
        assert!(out.iter().all(|c| c.len() % 2 == 0));
    }

    #[test]
    fn flush_pads_the_tail_with_silence_to_the_minimum() {
        let mut b = Batcher::new(400, 4000, 1, 0xFF);
        let mut out = Vec::new();
        b.push(frame(100, 0x10), &mut out);
        b.flush_padded(&mut out);
        assert_eq!(sizes(&out), [400]);
        assert_eq!(out[0][99], 0x10);
        assert_eq!(out[0][100], 0xFF);
        assert!(b.is_empty());
        out.clear();
        b.flush_padded(&mut out);
        assert!(out.is_empty(), "nothing held, nothing sent");
    }

    #[test]
    fn empty_frames_and_degenerate_parameters_are_safe() {
        let mut b = Batcher::new(0, 0, 0, 0);
        let mut out = Vec::new();
        b.push(Bytes::new(), &mut out);
        assert!(out.is_empty());
        b.push(frame(5, 1), &mut out);
        assert_eq!(out.iter().map(Bytes::len).sum::<usize>(), 5);
    }
}
