//! RNNoise through the pure-Rust `nnnoiseless` crate (BSD-3-Clause; the model is embedded).
//!
//! RNNoise works on 10 ms frames (480 samples) of 48 kHz, 16-bit-scaled `f32` audio, so a call
//! at 16 kHz or 8 kHz goes up to 48 kHz, through the model and back down
//! ([`super::rate::RateConverter`]). The model is a small GRU stack (about 60 K
//! parameters), 40 to 100 microseconds per 10 ms frame on a modern core.

use std::time::Duration;

use nnnoiseless::DenoiseState;

use super::rate::RateConverter;
use super::{Denoiser, push_f32, to_i16};
use crate::error::Result;

/// Sample rate RNNoise works at.
const NATIVE_HZ: u32 = 48_000;

/// Samples per RNNoise frame.
const FRAME: usize = DenoiseState::FRAME_SIZE;

/// Algorithmic delay of the model itself: its synthesis window overlaps the previous frame, so
/// the output is one frame (10 ms) behind the input.
const MODEL_DELAY: Duration = Duration::from_millis(10);

/// One caller's RNNoise state.
pub struct RnnoiseDenoiser {
    rate: RateConverter,
    state: Box<DenoiseState<'static>>,
    /// Model-rate input not yet forming a whole frame (`f32`, 16-bit scale).
    pending: Vec<f32>,
    frame_out: Vec<f32>,
    /// Converted-up input, and the model's output before conversion back.
    up: Vec<i16>,
    native_out: Vec<i16>,
}

impl std::fmt::Debug for RnnoiseDenoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RnnoiseDenoiser")
            .field("rate", &self.rate)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl RnnoiseDenoiser {
    /// A denoiser for audio at `rate_hz`.
    ///
    /// # Errors
    /// [`Error::Model`](crate::Error::Model) for a rate the resampler does not handle.
    pub fn new(rate_hz: u32) -> Result<Self> {
        Ok(Self {
            rate: RateConverter::new(rate_hz, NATIVE_HZ)?,
            state: DenoiseState::new(),
            pending: Vec::with_capacity(2 * FRAME),
            frame_out: vec![0.0; FRAME],
            up: Vec::new(),
            native_out: Vec::new(),
        })
    }
}

impl Denoiser for RnnoiseDenoiser {
    fn sample_rate(&self) -> u32 {
        self.rate.outer_hz()
    }

    fn delay(&self) -> Duration {
        self.rate.delay() + MODEL_DELAY
    }

    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
        self.up.clear();
        self.rate.to_native(input, &mut self.up)?;
        push_f32(&self.up, &mut self.pending);
        self.native_out.clear();
        let mut used = 0;
        while let Some(frame) = self.pending.get(used..used + FRAME) {
            self.state.process_frame(&mut self.frame_out, frame);
            self.native_out
                .extend(self.frame_out.iter().map(|s| to_i16(*s)));
            used += FRAME;
        }
        self.pending.drain(..used);
        self.rate.from_native(&self.native_out, output)
    }

    fn reset(&mut self) {
        self.rate.reset();
        self.state = DenoiseState::new();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed noise-like signal (xorshift), `n` samples, peak about `amp`.
    fn noise(n: usize, amp: f32, seed: u32) -> Vec<i16> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                {
                    ((x as f32 / u32::MAX as f32 - 0.5) * 2.0 * amp) as i16
                }
            })
            .collect()
    }

    #[test]
    fn output_length_follows_input_less_what_is_buffered() {
        for rate in [8_000, 16_000, 48_000] {
            let mut d = RnnoiseDenoiser::new(rate).unwrap();
            let mut out = Vec::new();
            let chunk = usize::try_from(rate / 50).unwrap();
            let total = chunk * 100;
            for _ in 0..100 {
                d.process(&noise(chunk, 3000.0, 7), &mut out).unwrap();
            }
            // Whatever is buffered is at most a model frame plus the resamplers' blocks (20 ms).
            let lag = total - out.len();
            assert!(lag <= chunk * 3, "rate {rate}: {lag} samples buffered");
            assert!(out.len() > total / 2, "rate {rate}: {} out", out.len());
        }
    }

    #[test]
    fn chunking_does_not_change_the_samples() {
        let input = noise(16_000, 4000.0, 11);
        let mut whole = Vec::new();
        RnnoiseDenoiser::new(16_000)
            .unwrap()
            .process(&input, &mut whole)
            .unwrap();
        let mut parts = Vec::new();
        let mut d = RnnoiseDenoiser::new(16_000).unwrap();
        for chunk in input.chunks(333) {
            d.process(chunk, &mut parts).unwrap();
        }
        let n = whole.len().min(parts.len());
        assert!(n > 14_000);
        assert_eq!(whole.get(..n), parts.get(..n));
    }

    #[test]
    fn reset_restarts_the_stream() {
        let input = noise(8_000, 4000.0, 3);
        let mut d = RnnoiseDenoiser::new(16_000).unwrap();
        let mut first = Vec::new();
        d.process(&input, &mut first).unwrap();
        d.reset();
        let mut second = Vec::new();
        d.process(&input, &mut second).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn steady_noise_is_attenuated() {
        // Stationary noise is what RNNoise removes (white noise only by a few dB: its gain floor).
        let input = noise(32_000, 3000.0, 5);
        let mut d = RnnoiseDenoiser::new(16_000).unwrap();
        let mut out = Vec::new();
        d.process(&input, &mut out).unwrap();
        let rms = |s: &[i16]| {
            (s.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt()
        };
        let tail_in = rms(input.get(16_000..).unwrap());
        let tail_out = rms(out.get(16_000..).unwrap());
        assert!(tail_out < tail_in * 0.75, "in {tail_in} out {tail_out}");
    }

    #[test]
    fn unsupported_rate_is_an_error() {
        assert!(RnnoiseDenoiser::new(12_345).is_err());
    }
}
