//! Dry/wet blend of any denoiser.

use std::collections::VecDeque;
use std::time::Duration;

use super::{BoxedDenoiser, Denoiser};
use crate::error::Result;

/// A denoiser mixed with its own input: `out = dry * input + (1 - dry) * enhanced`, the input
/// delayed to line up with the output. `dry = 0` is the plain denoiser, `dry = 0.25` the "12 dB cap"
/// of DeepFilterNet's `atten_lim_db` (weight 0.251 on the noisy signal). The blend keeps the artifacts
/// and the over-suppression of a model smaller, at the price of leaving noise in.
pub struct Blended {
    inner: BoxedDenoiser,
    dry: f32,
    /// Input samples from index `base` on, enough to line up with the next output.
    history: VecDeque<i16>,
    base: u64,
    produced: u64,
    /// The inner denoiser's delay in samples.
    lag: u64,
    scratch: Vec<i16>,
}

impl std::fmt::Debug for Blended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blended")
            .field("dry", &self.dry)
            .finish_non_exhaustive()
    }
}

impl Blended {
    /// Wraps `inner` with `dry` (clamped to 0..=1) of the input in the output.
    pub fn new(inner: BoxedDenoiser, dry: f32) -> Self {
        let micros = inner.delay().as_micros() * u128::from(inner.sample_rate());
        let lag = u64::try_from(micros / 1_000_000).unwrap_or(0);
        Self {
            inner,
            dry: dry.clamp(0.0, 1.0),
            history: VecDeque::with_capacity(4096),
            base: 0,
            produced: 0,
            lag,
            scratch: Vec::new(),
        }
    }
}

impl Denoiser for Blended {
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn delay(&self) -> Duration {
        self.inner.delay()
    }

    #[allow(clippy::cast_possible_truncation)] // rounded and clamped to the i16 range
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
        self.history.extend(input.iter().copied());
        self.scratch.clear();
        let result = self.inner.process(input, &mut self.scratch);
        for wet in &self.scratch {
            // Output sample `produced` belongs to input sample `produced - lag`.
            let dry = self
                .produced
                .checked_sub(self.lag)
                .and_then(|i| i.checked_sub(self.base))
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| self.history.get(i))
                .copied()
                .unwrap_or(0);
            let mixed = self.dry * f32::from(dry) + (1.0 - self.dry) * f32::from(*wet);
            output.push(mixed.round().clamp(-32_768.0, 32_767.0) as i16);
            self.produced += 1;
        }
        // Inputs older than the next dry sample are not needed any more.
        let keep_from = self.produced.saturating_sub(self.lag);
        while self.base < keep_from && self.history.pop_front().is_some() {
            self.base += 1;
        }
        result
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.history.clear();
        self.base = 0;
        self.produced = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Delays by `lag` samples and halves the amplitude.
    struct HalfDelayed {
        lag: usize,
        line: VecDeque<i16>,
    }

    impl Denoiser for HalfDelayed {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn delay(&self) -> Duration {
            Duration::from_micros(self.lag as u64 * 1_000_000 / 16_000)
        }
        fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
            for s in input {
                self.line.push_back(*s);
                if self.line.len() > self.lag {
                    output.push(self.line.pop_front().unwrap_or(0) / 2);
                } else {
                    output.push(0);
                }
            }
            Ok(())
        }
        fn reset(&mut self) {
            self.line.clear();
        }
    }

    fn run(dry: f32) -> Vec<i16> {
        let inner = Box::new(HalfDelayed {
            lag: 160,
            line: VecDeque::new(),
        });
        let mut d = Blended::new(inner, dry);
        let input: Vec<i16> = (0..1000).map(|i| (i * 20 % 10_000) as i16).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            d.process(chunk, &mut out).unwrap();
        }
        out
    }

    #[test]
    fn dry_zero_is_the_plain_denoiser_and_dry_one_is_the_delayed_input() {
        let (wet, dry) = (run(0.0), run(1.0));
        assert_eq!(wet.len(), 1000);
        for k in 200..1000 {
            let delayed = ((k - 160) * 20 % 10_000) as i16;
            assert_eq!(dry[k], delayed, "dry at {k}");
            assert_eq!(wet[k], delayed / 2, "wet at {k}");
        }
    }

    #[test]
    fn a_blend_lies_between_the_two() {
        let mixed = run(0.25);
        let (wet, dry) = (run(0.0), run(1.0));
        for k in 200..1000 {
            let expect = 0.25 * f32::from(dry[k]) + 0.75 * f32::from(wet[k]);
            assert!((f32::from(mixed[k]) - expect).abs() <= 1.0);
        }
    }
}
