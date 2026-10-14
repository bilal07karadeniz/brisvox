//! Streaming sample-rate conversion.
//!
//! [`StreamResampler`] converts one mono PCM16 stream between any two of the engine's rates
//! (8, 16, 22.05, 24, 44.1, 48 kHz) chunk by chunk, keeping filter state across calls: one
//! instance per session and direction. Each pair uses one of two rubato filters (see
//! [`Backend`]):
//!
//! - [`Backend::Fft`]: rubato's `Fft` (exact rational ratios; see `docs/providers/rubato.md`),
//!   for every pair except downsampling to 8 kHz;
//! - [`Backend::Sinc`]: rubato's windowed-sinc `Async` filter, for downsampling to 8 kHz,
//!   where it has 4.3-6 ms of delay instead of the FFT's 10 ms, with the same passband bar.
//!
//! It adds:
//!
//! - PCM16 in and out, converted through preallocated `f32` scratch buffers;
//! - any input length per call: input is accumulated into the resampler's fixed chunk;
//! - [`StreamResampler::flush`] to drain the end of an utterance (partial chunk + filter tail);
//! - no allocation after construction (output goes into a caller buffer).
//!
//! **Latency** = [`StreamResampler::delay`], the filter's group delay. FFT pairs: half a block,
//! where a block is 160 samples of the lower rate (needed for a flat passband to ~0.85 x
//! Nyquist): 10 ms when the lower rate is 8 kHz, 5 ms at 16 kHz, 3.3 ms at 24 kHz (more where
//! the rates' exact rational step is longer, e.g. 22.05 kHz). Sinc pairs (to 8 kHz): 4.25 ms
//! (44.1 kHz) to 6.0 ms (16 kHz). Measured values: `docs/LATENCY.md`.
//!
//! Output appears per whole chunk: one FFT block, or 5 ms of input for the sinc backend. That
//! adds nothing for input aligned to the chunk and up to [`StreamResampler::buffering`]
//! otherwise.
//!
//! **Construction is not free**: FFT pairs plan their transforms (10-20 µs, release); sinc pairs
//! build a filter table of 98-262 KB per instance (rubato does not share tables between
//! instances) in 0.6-1.6 ms (release, this machine). Build instances at session setup, off the
//! async workers, never per frame.

use std::time::Duration;

use audioadapter_buffers::direct::InterleavedSlice;
use brisvox_core::frame::SampleRate;
use rubato::{
    Async, Fft, FixedAsync, FixedSync, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::error::{Error, Result};

/// Converts one mono PCM16 stream from one rate to another, with state across calls.
pub struct StreamResampler {
    from: SampleRate,
    to: SampleRate,
    /// `None` for equal rates (copy); FFT or sinc otherwise (see [`Backend`]).
    fft: Option<Box<dyn Resampler<f32> + Send>>,
    backend: Backend,
    /// Input frames per resampler call.
    chunk_in: usize,
    /// Accumulated input (`chunk_in` long; `filled` valid).
    input: Vec<f32>,
    filled: usize,
    /// Output scratch (`output_frames_max` long).
    output: Vec<f32>,
    /// Input frames accepted and output frames emitted since construction or reset.
    fed: u64,
    emitted: u64,
}

impl std::fmt::Debug for StreamResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("chunk_in", &self.chunk_in)
            .field("delay", &self.delay())
            .finish_non_exhaustive()
    }
}

// Moved into session tasks.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<StreamResampler>();
};

/// The filter converting one rate pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Equal rates: samples are copied.
    Copy,
    /// rubato's FFT resampler (exact rational ratio), one block = 160 samples of the lower rate.
    Fft,
    /// rubato's windowed-sinc resampler, used to downsample to 8 kHz: flat to 3.4 kHz,
    /// at most 2 dB down at 3.6 kHz, at least 55 dB rejection of 4.4 kHz, at 4.3-6 ms of delay
    /// instead of the FFT's 10 ms. Its filter table is per instance.
    Sinc {
        /// Filter length in input samples.
        sinc_len: usize,
    },
}

/// Sinc filter length for a pair that uses the sinc backend: downsampling to 8 kHz. Chosen by
/// the length sweep of `tests/sinc_design.rs` (`probe_sinc_length_sweep`, numbers in
/// `docs/LATENCY.md`) against: |gain| <= 0.2 dB at 3.4 kHz, >= -2 dB at 3.6 kHz and >= 55 dB
/// rejection of 4.4 kHz. The shortest passing length is 192 (16 kHz), 224 (22.05 and 24 kHz),
/// 384 (44.1 kHz). For 48 kHz the shortest is 384 too, but at -55.6 dB it has no margin, so 512
/// (-82 dB) is used; its delay is 5.25 ms.
fn sinc_len_for(from: SampleRate, to: SampleRate) -> Option<usize> {
    if to != SampleRate::Hz8000 {
        return None;
    }
    match from {
        SampleRate::Hz16000 => Some(192),
        SampleRate::Hz22050 | SampleRate::Hz24000 => Some(224),
        SampleRate::Hz44100 => Some(384),
        SampleRate::Hz48000 => Some(512),
        SampleRate::Hz8000 => None,
    }
}

/// Input chunk of the sinc backend: 5 ms (output follows each chunk; small chunks keep the
/// buffering of non-aligned input low).
fn sinc_chunk_frames(from: SampleRate) -> usize {
    usize::try_from(from.hz() / 200).unwrap_or(80).max(1)
}

/// FFT block length in samples at the LOWER of the two rates. rubato's FFT filter keeps the
/// passband flat (|gain| < 0.2 dB) to ~0.85 x the lower Nyquist only with blocks of about 160
/// samples there; shorter blocks widen the transition band (5 ms at 8 kHz: -2 dB at 2.4 kHz,
/// -10 dB at 2.8 kHz - telephony speech needs 3.4 kHz). Measured 2026-10-08.
/// The filter delay is half a block: 10 ms when the lower rate is 8 kHz, 5 ms at 16 kHz, 3.3 ms
/// at 24 kHz. Applies to FFT pairs only (the sinc pairs to 8 kHz are sized by `sinc_len_for`).
const BLOCK_AT_LOWER_RATE: u32 = 160;

/// Input frames of one block: `BLOCK_AT_LOWER_RATE` samples of the lower rate, at `from`.
fn block_frames(from: SampleRate, to: SampleRate) -> usize {
    let lower = from.hz().min(to.hz()).max(1);
    usize::try_from(u64::from(from.hz()) * u64::from(BLOCK_AT_LOWER_RATE) / u64::from(lower))
        .unwrap_or(960)
        .max(1)
}

fn resampler_error(e: impl std::fmt::Display) -> Error {
    Error::Resampler(e.to_string())
}

impl StreamResampler {
    /// A resampler from `from` to `to`.
    ///
    /// Costs 10-20 µs and 22-31 KB for FFT pairs, and 0.6-1.6 ms and 105-274 KB (of which the
    /// filter table is 98-262 KB) for sinc pairs (downsampling to 8 kHz). Release build, this
    /// machine; measured by `tests/sinc_design.rs` and `tests/resample_memory.rs`. Call it at
    /// session setup on a blocking or inference thread, not on an async worker, and not per
    /// frame.
    ///
    /// # Errors
    /// [`Error::Resampler`] if rubato rejects the configuration.
    pub fn new(from: SampleRate, to: SampleRate) -> Result<Self> {
        if from == to {
            return Ok(Self {
                from,
                to,
                fft: None,
                backend: Backend::Copy,
                chunk_in: 0,
                input: Vec::new(),
                filled: 0,
                output: Vec::new(),
                fed: 0,
                emitted: 0,
            });
        }
        let rate = |r: SampleRate| usize::try_from(r.hz()).unwrap_or(48_000);
        if let Some(sinc_len) = sinc_len_for(from, to) {
            let parameters =
                SincInterpolationParameters::new(sinc_len, WindowFunction::BlackmanHarris2)
                    .oversampling_factor(128)
                    .interpolation(SincInterpolationType::Cubic);
            let ratio = f64::from(to.hz()) / f64::from(from.hz());
            let sinc = Async::<f32>::new_sinc(
                ratio,
                1.0,
                &parameters,
                sinc_chunk_frames(from),
                1,
                FixedAsync::Input,
            )
            .map_err(resampler_error)?;
            let chunk_in = sinc.input_frames_next();
            let output_max = sinc.output_frames_max();
            return Ok(Self {
                from,
                to,
                fft: Some(Box::new(sinc)),
                backend: Backend::Sinc { sinc_len },
                chunk_in,
                input: vec![0.0; chunk_in],
                filled: 0,
                output: vec![0.0; output_max],
                fed: 0,
                emitted: 0,
            });
        }
        let build = |chunk: usize| {
            Fft::<f32>::new_custom(
                rate(from),
                rate(to),
                chunk,
                1,
                1,
                WindowFunction::BlackmanHarris2,
                FixedSync::Input,
            )
            .map_err(resampler_error)
        };
        let mut fft = build(block_frames(from, to))?;
        // rubato rounds its FFT block up to the rates' exact rational step (441 frames for
        // 22.05 kHz <-> 8/16 kHz) but keeps the requested chunk: make them equal, so output
        // follows every whole chunk and `buffering()` is exactly one chunk.
        if fft.fft_size_in() != fft.input_frames_next() {
            fft = build(fft.fft_size_in())?;
        }
        let chunk_in = fft.input_frames_next();
        let output_max = fft.output_frames_max();
        Ok(Self {
            from,
            to,
            fft: Some(Box::new(fft)),
            backend: Backend::Fft,
            chunk_in,
            input: vec![0.0; chunk_in],
            filled: 0,
            output: vec![0.0; output_max],
            fed: 0,
            emitted: 0,
        })
    }

    /// Which filter converts this pair.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Input rate.
    pub fn from_rate(&self) -> SampleRate {
        self.from
    }

    /// Output rate.
    pub fn to_rate(&self) -> SampleRate {
        self.to
    }

    /// Input frames the resampler consumes per internal call (0 for equal rates).
    pub fn chunk_frames(&self) -> usize {
        self.chunk_in
    }

    /// Delay the filter adds to the stream (group delay at the output rate). This is the
    /// resampler's latency for chunk-aligned input (the engine's 20 ms frames).
    pub fn delay(&self) -> Duration {
        self.fft.as_ref().map_or(Duration::ZERO, |fft| {
            frames_to_duration(fft.output_delay(), self.to)
        })
    }

    /// Worst-case input held back before it can come out: one chunk (one FFT block, or 5 ms
    /// for the sinc backend). Extra latency only for input not aligned to
    /// [`Self::chunk_frames`].
    pub fn buffering(&self) -> Duration {
        frames_to_duration(self.chunk_in, self.from)
    }

    /// The filter's group delay alone, in output frames (0 for equal rates).
    pub fn filter_delay_frames(&self) -> usize {
        self.fft.as_ref().map_or(0, |engine| engine.output_delay())
    }

    /// Output frames that `process` may produce for `input_frames` more input: size `out`
    /// with this.
    pub fn max_output_len(&self, input_frames: usize) -> usize {
        let Some(fft) = &self.fft else {
            return input_frames;
        };
        let chunks = (self.filled + input_frames) / self.chunk_in.max(1);
        chunks.saturating_mul(fft.output_frames_max())
    }

    /// Output frames [`Self::flush`] will write: the rest of the stream's real output.
    pub fn max_flush_len(&self) -> usize {
        usize::try_from(self.flush_target().saturating_sub(self.emitted)).unwrap_or(usize::MAX)
    }

    /// Total output frames the stream so far should produce once drained: the input at the
    /// output rate plus the filter delay (the output starts with `delay` frames of ramp-in).
    fn flush_target(&self) -> u64 {
        if self.fed == 0 || self.fft.is_none() {
            return self.emitted;
        }
        let delay = u64::try_from(self.filter_delay_frames()).unwrap_or(0);
        self.fed * u64::from(self.to.hz()) / u64::from(self.from.hz()) + delay
    }

    /// Feeds `input` (PCM16 samples at the input rate) and writes the resampled samples that
    /// became available into `out`; returns how many. Output comes in whole resampler chunks,
    /// so a call may return 0 (input buffered) or more than the input's share.
    ///
    /// # Errors
    /// [`Error::BufferTooSmall`] if `out` is shorter than [`Self::max_output_len`] (in that
    /// case nothing is consumed); [`Error::Resampler`] if rubato fails (not expected for valid
    /// buffers).
    pub fn process(&mut self, input: &[i16], out: &mut [i16]) -> Result<usize> {
        let needed = self.max_output_len(input.len());
        if out.len() < needed {
            return Err(Error::BufferTooSmall {
                needed: needed.saturating_mul(2),
                available: out.len().saturating_mul(2),
            });
        }
        if self.fft.is_none() {
            if let Some(dst) = out.get_mut(..input.len()) {
                dst.copy_from_slice(input);
            }
            return Ok(input.len());
        }
        let mut written = 0;
        let mut rest = input;
        while !rest.is_empty() {
            let space = self.chunk_in - self.filled;
            let (now, later) = rest.split_at(space.min(rest.len()));
            rest = later;
            let slot = self
                .input
                .get_mut(self.filled..self.filled + now.len())
                .ok_or_else(|| resampler_error("input accumulator overrun"))?;
            for (dst, src) in slot.iter_mut().zip(now) {
                *dst = f32::from(*src) / 32_768.0;
            }
            self.filled += now.len();
            self.fed += u64::try_from(now.len()).unwrap_or(0);
            if self.filled < self.chunk_in {
                break;
            }
            let target = out.get_mut(written..).unwrap_or_default();
            written += self.run_chunk(target, usize::MAX)?;
        }
        Ok(written)
    }

    /// Drains the end of the stream into `out` (at least [`Self::max_flush_len`] long): the
    /// buffered partial chunk and the filter tail, padded with silence and cut at the input's
    /// end. Then resets, ready for an unrelated stream. Returns the frames written.
    ///
    /// # Errors
    /// [`Error::BufferTooSmall`] if `out` is too short (nothing changes then);
    /// [`Error::Resampler`] if rubato fails.
    pub fn flush(&mut self, out: &mut [i16]) -> Result<usize> {
        let missing = self.max_flush_len();
        if out.len() < missing {
            return Err(Error::BufferTooSmall {
                needed: missing.saturating_mul(2),
                available: out.len().saturating_mul(2),
            });
        }
        let mut written = 0;
        // A padded chunk yields its nominal share of output (one rubato block may lag); the
        // bound only guards against a misbehaving resampler.
        let nominal = (self.chunk_in * usize::try_from(self.to.hz()).unwrap_or(1)
            / usize::try_from(self.from.hz()).unwrap_or(1).max(1))
        .max(1);
        let mut rounds = missing / nominal + 4;
        while written < missing && rounds > 0 {
            rounds -= 1;
            if let Some(pad) = self.input.get_mut(self.filled..) {
                pad.fill(0.0);
            }
            self.filled = self.chunk_in;
            let target = out.get_mut(written..missing).unwrap_or_default();
            written += self.run_chunk(target, missing - written)?;
        }
        self.reset();
        Ok(written)
    }

    /// Runs the resampler on the full input chunk and writes at most `limit` frames of its
    /// output to `out` (the rest is discarded: flush padding).
    fn run_chunk(&mut self, out: &mut [i16], limit: usize) -> Result<usize> {
        let Some(fft) = &mut self.fft else {
            return Ok(0);
        };
        let frames_out = self.output.len();
        let input_adapter =
            InterleavedSlice::new(&self.input, 1, self.chunk_in).map_err(resampler_error)?;
        let mut output_adapter =
            InterleavedSlice::new_mut(&mut self.output, 1, frames_out).map_err(resampler_error)?;
        let (_, produced) = fft
            .process_into_buffer(&input_adapter, &mut output_adapter, None)
            .map_err(resampler_error)?;
        self.filled = 0;
        let keep = produced.min(limit);
        // `max_output_len` bounds this (rubato: per call <= output_frames_max); a miss is
        // reported, never silently dropped.
        let target = out
            .get_mut(..keep)
            .ok_or_else(|| resampler_error("output exceeded max_output_len"))?;
        for (dst, src) in target.iter_mut().zip(&self.output) {
            *dst = to_i16(*src);
        }
        self.emitted += u64::try_from(keep).unwrap_or(0);
        Ok(keep)
    }

    /// Clears the filter state and buffered input (a new, unrelated stream follows). Buffered
    /// audio is dropped; use [`Self::flush`] to keep it.
    pub fn reset(&mut self) {
        if let Some(fft) = &mut self.fft {
            fft.reset();
        }
        self.filled = 0;
        self.fed = 0;
        self.emitted = 0;
    }
}

fn frames_to_duration(frames: usize, rate: SampleRate) -> Duration {
    let frames = u64::try_from(frames).unwrap_or(u64::MAX);
    Duration::from_micros(frames.saturating_mul(1_000_000) / u64::from(rate.hz()))
}

/// `f32` (-1..1) to PCM16, rounded to nearest (ties to even) and clamped. Avoids the `roundf`
/// library call on baseline x86-64 (~10x faster): adding 1.5 * 2^23 puts the rounded integer
/// in the low mantissa bits. A NaN input gives an arbitrary (but defined) sample.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bit extraction, see above
fn to_i16(x: f32) -> i16 {
    let shifted = (x * 32_768.0).clamp(-32_768.0, 32_767.0) + 12_582_912.0;
    shifted.to_bits() as u16 as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_rounding_matches_round_ties_even_on_the_whole_range() {
        for i in -70_000_i32..=70_000 {
            // Integers and halves from -35000 to 35000 (beyond the PCM16 range on both sides),
            // plus quarter steps around each: exercises ties, rounding and clamping.
            #[allow(clippy::cast_precision_loss)] // exact: |i| < 2^24
            let v = i as f32 / 2.0;
            for x in [v - 0.25, v, v + 0.25] {
                let expected = x.round_ties_even().clamp(-32_768.0, 32_767.0) as i16;
                assert_eq!(to_i16(x / 32_768.0), expected, "{x}");
            }
        }
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), i16::MIN);
    }
}
