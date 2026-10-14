//! Seeded, deterministic latency profiles for mock providers.

use std::time::Duration;

/// SplitMix64: tiny, fast, good enough for simulation; identical sequence for identical seeds.
#[derive(Debug, Clone)]
pub struct SimRng {
    state: u64,
}

impl SimRng {
    /// A generator seeded with `seed`.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        // 53 random mantissa bits.
        #[allow(clippy::cast_precision_loss)] // exact: values < 2^53
        let bits = (self.next_u64() >> 11) as f64;
        bits / (1u64 << 53) as f64
    }

    /// Standard normal sample (Box–Muller).
    pub fn next_standard_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// How long a mock provider takes for one step (connect, first token, next token, ...).
#[derive(Debug, Clone, PartialEq)]
pub enum LatencyProfile {
    /// Always the same.
    Fixed(Duration),
    /// Normal distribution, clamped at zero.
    Normal {
        /// Mean.
        mean: Duration,
        /// Standard deviation.
        std_dev: Duration,
    },
    /// Log-normal distribution (long right tail, like real network latency).
    LogNormal {
        /// Median (`e^mu`).
        median: Duration,
        /// Shape (standard deviation of the underlying normal).
        sigma: f64,
    },
    /// The k-th sample is `script[k]`; the last value repeats once the script is exhausted
    /// (empty script: zero).
    Scripted(Vec<Duration>),
}

impl LatencyProfile {
    /// No latency.
    pub const ZERO: Self = Self::Fixed(Duration::ZERO);
}

/// Draws samples from a [`LatencyProfile`] deterministically.
#[derive(Debug, Clone)]
pub struct LatencySampler {
    profile: LatencyProfile,
    rng: SimRng,
    drawn: usize,
}

impl LatencySampler {
    /// A sampler for `profile`; `seed` makes random profiles reproducible.
    pub fn new(profile: LatencyProfile, seed: u64) -> Self {
        Self {
            profile,
            rng: SimRng::new(seed),
            drawn: 0,
        }
    }

    /// The next latency.
    pub fn sample(&mut self) -> Duration {
        let k = self.drawn;
        self.drawn = self.drawn.saturating_add(1);
        match &self.profile {
            LatencyProfile::Fixed(d) => *d,
            LatencyProfile::Normal { mean, std_dev } => {
                let z = self.rng.next_standard_normal();
                secs(z.mul_add(std_dev.as_secs_f64(), mean.as_secs_f64()))
            }
            LatencyProfile::LogNormal { median, sigma } => {
                let z = self.rng.next_standard_normal();
                secs(median.as_secs_f64() * (sigma * z).exp())
            }
            LatencyProfile::Scripted(script) => script
                .get(k)
                .or_else(|| script.last())
                .copied()
                .unwrap_or(Duration::ZERO),
        }
    }
}

/// Seconds to a duration, clamping negatives/NaN to zero and huge values to one hour.
fn secs(value: f64) -> Duration {
    const MAX: Duration = Duration::from_secs(3_600);
    if value.is_nan() || value <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(value).map_or(MAX, |d| d.min(MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn fixed_and_scripted() {
        let mut fixed = LatencySampler::new(LatencyProfile::Fixed(5 * MS), 0);
        assert_eq!((fixed.sample(), fixed.sample()), (5 * MS, 5 * MS));
        let mut scripted =
            LatencySampler::new(LatencyProfile::Scripted(vec![MS, 2 * MS, 3 * MS]), 0);
        let got: Vec<Duration> = (0..5).map(|_| scripted.sample()).collect();
        assert_eq!(got, [MS, 2 * MS, 3 * MS, 3 * MS, 3 * MS]);
        let mut empty = LatencySampler::new(LatencyProfile::Scripted(Vec::new()), 0);
        assert_eq!(empty.sample(), Duration::ZERO);
    }

    #[test]
    fn seeded_profiles_are_reproducible() {
        let profile = LatencyProfile::LogNormal {
            median: 300 * MS,
            sigma: 0.4,
        };
        let run = |seed| {
            let mut s = LatencySampler::new(profile.clone(), seed);
            (0..100).map(|_| s.sample()).collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn distributions_have_the_requested_shape() {
        const N: u32 = 20_000;
        let mut normal = LatencySampler::new(
            LatencyProfile::Normal {
                mean: 200 * MS,
                std_dev: 20 * MS,
            },
            1,
        );
        let mean: Duration = (0..N).map(|_| normal.sample()).sum::<Duration>() / N;
        assert!((195 * MS..205 * MS).contains(&mean), "{mean:?}");

        let mut lognormal = LatencySampler::new(
            LatencyProfile::LogNormal {
                median: 300 * MS,
                sigma: 0.5,
            },
            2,
        );
        let mut samples: Vec<Duration> = (0..N).map(|_| lognormal.sample()).collect();
        samples.sort();
        let median = samples[samples.len() / 2];
        assert!((290 * MS..310 * MS).contains(&median), "{median:?}");
        // Right tail: p99 well above the median.
        let p99 = samples[samples.len() * 99 / 100];
        assert!(p99 > 2 * median, "{p99:?}");

        let mut clamped = LatencySampler::new(
            LatencyProfile::Normal {
                mean: MS,
                std_dev: 100 * MS,
            },
            3,
        );
        let zeros = (0..1_000)
            .filter(|_| clamped.sample() == Duration::ZERO)
            .count();
        assert!(zeros > 300, "negative samples must clamp to zero ({zeros})");
    }
}
