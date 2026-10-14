//! [`OutputFramer`]: provider TTS audio to the call's output rate, cut into exact frames
//! (`docs/PIPELINE.md` section 6).
//!
//! The TTS provider delivers PCM16 chunks of any length at its own rate (24 kHz by default). The
//! transport accepts one rate only (the call's output rate) and the engine's granularity of
//! interruption, pacing and playback positions is the frame (20 ms). The framer converts and
//! cuts:
//!
//! ```text
//! provider chunk --bytes to i16--> StreamResampler --i16 to bytes--> pending --split--> 20 ms frames
//!                                  (skipped at equal rates)          (BytesMut)        (Bytes slices)
//! ```
//!
//! # Allocation
//! A provider chunk is copied once into a `BytesMut`, the whole frames are cut from it with
//! `split_to(..).freeze()` and handed out as `Bytes::slice`s of that one block: **no allocation
//! per frame**, whatever the chunk length. While frames of the previous chunk are still alive
//! (queued in the pipeline, buffered by the transport; always, in production) the chunk needs a
//! new block: two allocations per chunk (the block and the shared counter `bytes` creates when
//! the block is split), the same for a 20 ms chunk and a one-second chunk; when they are gone
//! the block is reused and the cost is zero. `tests/tts_alloc.rs` proves both with a counting
//! allocator. The remainder shorter than a frame stays in `pending` and is completed by the next
//! chunk. The scratch buffers for the sample conversion are reused for the framer's whole life.
//!
//! # Timeline
//! A resampler delays its output by its filter's group delay (`StreamResampler::delay`) and
//! starts with that many frames of ramp-in. The framer drops the ramp-in so the output starts
//! with the first sample of the input: word offsets the TTS reports (from the first audio
//! sample) stay valid on the output timeline, and the output of a whole utterance has exactly
//! the input's duration.
//!
//! # Ends of utterances
//! [`OutputFramer::finish`] drains the resampler's tail and emits the last frame, which may be
//! shorter than a frame (its audio is real; padding it with silence would make the playback
//! position run ahead of the words). [`OutputFramer::reset`] drops everything buffered (a
//! cancelled utterance).

use brisvox_audio::StreamResampler;
use brisvox_core::frame::SampleRate;
use bytes::{BufMut, Bytes, BytesMut};

use crate::error::Result;

/// Converts and frames one stream of TTS audio. See the [module docs](self).
pub struct OutputFramer {
    frame_bytes: usize,
    /// `None` when the TTS and output rates are equal.
    resampler: Option<StreamResampler>,
    to: SampleRate,
    /// The input chunk as samples.
    samples_in: Vec<i16>,
    /// The resampler's output.
    samples_out: Vec<i16>,
    /// Output not yet cut into frames (always shorter than a frame between calls).
    pending: BytesMut,
    /// Output samples of the resampler's ramp-in still to drop.
    skip: usize,
    /// The first byte of a sample that a chunk ended in the middle of.
    odd: Option<u8>,
}

impl std::fmt::Debug for OutputFramer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputFramer")
            .field("to", &self.to)
            .field("frame_bytes", &self.frame_bytes)
            .field("resampler", &self.resampler)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl OutputFramer {
    /// A framer from `from` to `to` cutting frames of `frame_samples` samples at `to`. Builds the
    /// resampler here: 10-20 us for most pairs, 0.6-1.6 ms for pairs down to 8 kHz (see
    /// `StreamResampler::new`), so call it off the async workers (or build the resampler on the
    /// [`Offload`](crate::Offload) and use [`Self::with_resampler`]).
    ///
    /// # Errors
    /// [`Error::Audio`](crate::Error::Audio) if the resampler cannot be built.
    pub fn new(from: SampleRate, to: SampleRate, frame_samples: usize) -> Result<Self> {
        let resampler = Self::build_resampler(from, to)?;
        Ok(Self::with_resampler(to, frame_samples, resampler))
    }

    /// The resampler for `from` to `to`, `None` when the rates are equal. Pure CPU work: run it
    /// on the pool in production.
    ///
    /// # Errors
    /// [`Error::Audio`](crate::Error::Audio) if the resampler cannot be built.
    pub fn build_resampler(from: SampleRate, to: SampleRate) -> Result<Option<StreamResampler>> {
        if from == to {
            return Ok(None);
        }
        Ok(Some(StreamResampler::new(from, to)?))
    }

    /// A framer around an already built resampler (see [`Self::build_resampler`]). A zero
    /// `frame_samples` is treated as 1.
    pub fn with_resampler(
        to: SampleRate,
        frame_samples: usize,
        resampler: Option<StreamResampler>,
    ) -> Self {
        let skip = resampler
            .as_ref()
            .map_or(0, StreamResampler::filter_delay_frames);
        Self {
            frame_bytes: frame_samples
                .max(1)
                .saturating_mul(crate::tts_audio::BYTES_PER_SAMPLE),
            resampler,
            to,
            samples_in: Vec::new(),
            samples_out: Vec::new(),
            pending: BytesMut::new(),
            skip,
            odd: None,
        }
    }

    /// The rate of the frames this framer produces.
    pub const fn output_rate(&self) -> SampleRate {
        self.to
    }

    /// Bytes of one full frame.
    pub const fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Whether the framer holds audio that has not been emitted yet.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.odd.is_none()
    }

    /// Feeds one provider chunk (PCM16 little-endian mono at the input rate) and calls `emit`
    /// with each whole frame that became available, in order. A trailing odd byte is kept for the
    /// next chunk.
    ///
    /// The cost is linear in the chunk (about 1 to 12 us per 20 ms of audio, release): feed very
    /// long chunks in pieces and yield between them.
    ///
    /// # Errors
    /// [`Error::Audio`](crate::Error::Audio) if the resampler fails (not expected for valid
    /// buffers). The chunk is then lost; the framer stays usable after [`Self::reset`].
    pub fn push(&mut self, pcm: &[u8], emit: &mut impl FnMut(Bytes)) -> Result<()> {
        self.samples_in.clear();
        let mut rest = pcm;
        if let Some(low) = self.odd.take() {
            if let Some((&high, tail)) = rest.split_first() {
                self.samples_in.push(i16::from_le_bytes([low, high]));
                rest = tail;
            } else {
                self.odd = Some(low);
                return Ok(());
            }
        }
        let (pairs, remainder) = rest.as_chunks::<2>();
        self.samples_in
            .extend(pairs.iter().map(|pair| i16::from_le_bytes(*pair)));
        self.odd = remainder.first().copied();
        if self.samples_in.is_empty() {
            return Ok(());
        }
        match &mut self.resampler {
            None => {
                self.pending
                    .reserve(self.samples_in.len().saturating_mul(2));
                for sample in &self.samples_in {
                    self.pending.put_i16_le(*sample);
                }
            }
            Some(resampler) => {
                let needed = resampler.max_output_len(self.samples_in.len());
                if self.samples_out.len() < needed {
                    self.samples_out.resize(needed, 0);
                }
                let produced = resampler.process(&self.samples_in, &mut self.samples_out)?;
                let out = self.samples_out.get(..produced).unwrap_or(&[]);
                let dropped = self.skip.min(out.len());
                self.skip -= dropped;
                let out = out.get(dropped..).unwrap_or(&[]);
                self.pending.reserve(out.len().saturating_mul(2));
                for sample in out {
                    self.pending.put_i16_le(*sample);
                }
            }
        }
        self.cut(emit);
        Ok(())
    }

    /// Ends the utterance: drains the resampler's buffered tail, emits every remaining whole
    /// frame and then the last partial frame (if any bytes remain), and leaves the framer ready
    /// for an unrelated stream.
    ///
    /// # Errors
    /// [`Error::Audio`](crate::Error::Audio) if the resampler fails.
    pub fn finish(&mut self, emit: &mut impl FnMut(Bytes)) -> Result<()> {
        self.odd = None;
        if let Some(resampler) = &mut self.resampler {
            let needed = resampler.max_flush_len();
            if self.samples_out.len() < needed {
                self.samples_out.resize(needed, 0);
            }
            let produced = resampler.flush(&mut self.samples_out)?;
            let out = self.samples_out.get(..produced).unwrap_or(&[]);
            let dropped = self.skip.min(out.len());
            let out = out.get(dropped..).unwrap_or(&[]);
            self.pending.reserve(out.len().saturating_mul(2));
            for sample in out {
                self.pending.put_i16_le(*sample);
            }
            self.skip = resampler.filter_delay_frames();
        }
        self.cut(emit);
        if !self.pending.is_empty() {
            emit(self.pending.split().freeze());
        }
        Ok(())
    }

    /// Drops everything buffered and starts a new stream (a cancelled utterance).
    pub fn reset(&mut self) {
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
            self.skip = resampler.filter_delay_frames();
        }
        self.pending.clear();
        self.odd = None;
    }

    /// Emits every whole frame of `pending` as slices of one block.
    fn cut(&mut self, emit: &mut impl FnMut(Bytes)) {
        let whole = self.pending.len() / self.frame_bytes * self.frame_bytes;
        if whole == 0 {
            return;
        }
        let block = self.pending.split_to(whole).freeze();
        let mut at = 0;
        while at < whole {
            let end = at.saturating_add(self.frame_bytes);
            emit(block.slice(at..end));
            at = end;
        }
    }
}

/// Bytes per PCM16 sample.
pub(crate) const BYTES_PER_SAMPLE: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_16K: usize = 320;

    fn sine(samples: usize, rate: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for n in 0..samples {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let v = (12_000.0
                * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / f64::from(rate)).sin())
                as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    fn collect(framer: &mut OutputFramer, chunk: &[u8]) -> Vec<Bytes> {
        let mut frames = Vec::new();
        framer.push(chunk, &mut |f| frames.push(f)).unwrap();
        frames
    }

    #[test]
    fn equal_rates_cut_exact_frames_and_carry_the_remainder() {
        let mut framer =
            OutputFramer::new(SampleRate::Hz16000, SampleRate::Hz16000, FRAME_16K).unwrap();
        // 500 samples: one frame of 320, 180 carried.
        let frames = collect(&mut framer, &sine(500, 16_000));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), FRAME_16K * 2);
        // 460 more: 180 + 460 = 640 = two frames, nothing left.
        let frames = collect(&mut framer, &sine(460, 16_000));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.len() == FRAME_16K * 2));
        assert!(framer.is_empty());
        // The pieces are the input, byte for byte.
        let input = sine(1_000, 16_000);
        let mut framer =
            OutputFramer::new(SampleRate::Hz16000, SampleRate::Hz16000, FRAME_16K).unwrap();
        let mut got = Vec::new();
        for piece in input.chunks(250) {
            got.extend(collect(&mut framer, piece).iter().flat_map(|f| f.to_vec()));
        }
        framer.finish(&mut |f| got.extend_from_slice(&f)).unwrap();
        assert_eq!(got, input);
    }

    #[test]
    fn the_last_partial_frame_is_emitted_at_finish_and_is_not_padded() {
        let mut framer =
            OutputFramer::new(SampleRate::Hz16000, SampleRate::Hz16000, FRAME_16K).unwrap();
        let frames = collect(&mut framer, &sine(400, 16_000));
        assert_eq!(frames.len(), 1);
        let mut tail = Vec::new();
        framer.finish(&mut |f| tail.push(f)).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].len(), (400 - 320) * 2, "80 samples, unpadded");
        // The framer is ready for the next stream.
        assert!(framer.is_empty());
        let frames = collect(&mut framer, &sine(320, 16_000));
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn an_odd_byte_at_the_end_of_a_chunk_is_completed_by_the_next() {
        let input = sine(640, 16_000);
        let mut framer =
            OutputFramer::new(SampleRate::Hz16000, SampleRate::Hz16000, FRAME_16K).unwrap();
        let mut got = Vec::new();
        // Chunk boundaries inside samples.
        for piece in [&input[..333], &input[333..334], &input[334..]] {
            got.extend(collect(&mut framer, piece).iter().flat_map(|f| f.to_vec()));
        }
        framer.finish(&mut |f| got.extend_from_slice(&f)).unwrap();
        assert_eq!(got, input);
    }

    #[test]
    fn a_resampled_utterance_has_the_duration_of_its_input_and_no_ramp_in() {
        for (from, to) in [
            (SampleRate::Hz24000, SampleRate::Hz16000),
            (SampleRate::Hz24000, SampleRate::Hz8000),
            (SampleRate::Hz24000, SampleRate::Hz48000),
            (SampleRate::Hz16000, SampleRate::Hz24000),
        ] {
            let frame = usize::try_from(to.hz() / 50).unwrap();
            let mut framer = OutputFramer::new(from, to, frame).unwrap();
            let seconds = 0.5;
            let samples = usize::try_from(from.hz()).unwrap() / 2;
            let mut total = 0usize;
            let mut whole_frames = 0usize;
            let mut first_peak = 0i16;
            let input = sine(samples, from.hz());
            for piece in input.chunks(usize::try_from(from.hz() / 25).unwrap() * 2) {
                let mut frames = Vec::new();
                framer.push(piece, &mut |f| frames.push(f)).unwrap();
                for f in frames {
                    assert_eq!(f.len(), frame * 2, "{from:?}->{to:?}: exact frames");
                    if whole_frames == 0 {
                        first_peak = f
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| i16::from_le_bytes(*b).abs())
                            .max()
                            .unwrap();
                    }
                    whole_frames += 1;
                    total += f.len() / 2;
                }
            }
            framer
                .finish(&mut |f| {
                    total += f.len() / 2;
                })
                .unwrap();
            #[allow(clippy::cast_precision_loss)]
            let expected = f64::from(to.hz()) * seconds;
            #[allow(clippy::cast_precision_loss)]
            let got = total as f64;
            assert!(
                (got - expected).abs() <= 2.0,
                "{from:?}->{to:?}: {total} samples, expected about {expected}"
            );
            assert!(
                first_peak > 2_000,
                "{from:?}->{to:?}: the first frame carries signal, not a silent ramp-in ({first_peak})"
            );
        }
    }

    /// One utterance through `framer`: `(samples out, peak of the first frame)`, finishing it.
    fn utterance(framer: &mut OutputFramer, from: SampleRate, seconds: f64) -> (usize, i16) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let samples = (f64::from(from.hz()) * seconds) as usize;
        let input = sine(samples, from.hz());
        let mut total = 0usize;
        let mut first_peak = None;
        let take = |frame: Bytes, total: &mut usize, first_peak: &mut Option<i16>| {
            if first_peak.is_none() {
                *first_peak = frame
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b).abs())
                    .max();
            }
            *total += frame.len() / 2;
        };
        for piece in input.chunks(usize::try_from(from.hz() / 25).unwrap() * 2) {
            let mut frames = Vec::new();
            framer.push(piece, &mut |f| frames.push(f)).unwrap();
            for frame in frames {
                take(frame, &mut total, &mut first_peak);
            }
        }
        let mut tail = Vec::new();
        framer.finish(&mut |f| tail.push(f)).unwrap();
        for frame in tail {
            take(frame, &mut total, &mut first_peak);
        }
        (total, first_peak.unwrap_or(0))
    }

    #[test]
    fn every_utterance_through_one_resampling_framer_has_its_exact_duration_and_no_ramp_in() {
        // The resampler is reused for every utterance of a call: finish and reset must re-arm
        // the ramp-in drop, or the second utterance would start with the filter's silence and
        // every word offset of it would be late by the group delay.
        for (from, to) in [
            (SampleRate::Hz24000, SampleRate::Hz16000),
            (SampleRate::Hz24000, SampleRate::Hz8000),
            (SampleRate::Hz16000, SampleRate::Hz24000),
        ] {
            let frame = usize::try_from(to.hz() / 50).unwrap();
            let mut framer = OutputFramer::new(from, to, frame).unwrap();
            for (index, seconds) in [0.5, 0.3, 0.7].into_iter().enumerate() {
                let (total, first_peak) = utterance(&mut framer, from, seconds);
                #[allow(clippy::cast_precision_loss)]
                let (got, expected) = (total as f64, f64::from(to.hz()) * seconds);
                assert!(
                    (got - expected).abs() <= 2.0,
                    "{from:?}->{to:?} utterance {index}: {total} samples, expected about {expected}"
                );
                assert!(
                    first_peak > 2_000,
                    "{from:?}->{to:?} utterance {index}: silent first frame ({first_peak})"
                );
                if index == 1 {
                    // A cancelled utterance in between: the next one starts clean as well.
                    let mut frames = Vec::new();
                    framer
                        .push(&sine(1_500, from.hz()), &mut |f| frames.push(f))
                        .unwrap();
                    framer.reset();
                }
            }
        }
    }

    #[test]
    fn reset_drops_buffered_audio_and_restarts_the_stream() {
        let mut framer =
            OutputFramer::new(SampleRate::Hz24000, SampleRate::Hz16000, FRAME_16K).unwrap();
        let before = collect(&mut framer, &sine(1_000, 24_000)).len();
        framer.reset();
        assert!(framer.is_empty());
        let after = collect(&mut framer, &sine(1_000, 24_000)).len();
        assert_eq!(before, after, "a reset stream behaves like a new one");
    }
}
