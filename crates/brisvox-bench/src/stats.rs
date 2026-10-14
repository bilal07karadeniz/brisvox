//! Latency distribution summaries.

use std::fmt;
use std::time::Duration;

/// Nearest-rank percentiles of a set of durations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Number of samples.
    pub count: usize,
    /// Median.
    pub p50: Duration,
    /// 90th percentile.
    pub p90: Duration,
    /// 95th percentile.
    pub p95: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// Largest sample.
    pub max: Duration,
}

impl Summary {
    /// Summarizes `samples` (all zero if there are none).
    pub fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        // Nearest rank: the smallest sample with at least q of the samples at or below it.
        let pick = |permille: usize| {
            let rank = permille.saturating_mul(samples.len()).div_ceil(1000);
            samples
                .get(rank.saturating_sub(1))
                .copied()
                .unwrap_or_default()
        };
        Self {
            count: samples.len(),
            p50: pick(500),
            p90: pick(900),
            p95: pick(950),
            p99: pick(990),
            max: samples.last().copied().unwrap_or_default(),
        }
    }
}

/// Milliseconds with three decimals (microsecond resolution).
pub fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1_000.0)
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "p50 {} / p90 {} / p99 {} / max {} ms (n={})",
            ms(self.p50),
            ms(self.p90),
            ms(self.p99),
            ms(self.max),
            self.count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn nearest_rank_percentiles() {
        let s = Summary::of((1..=100).rev().map(|i| i * MS).collect());
        assert_eq!(s.count, 100);
        assert_eq!(
            (s.p50, s.p90, s.p95, s.p99, s.max),
            (50 * MS, 90 * MS, 95 * MS, 99 * MS, 100 * MS)
        );
        let one = Summary::of(vec![7 * MS]);
        assert_eq!((one.p50, one.p99, one.max), (7 * MS, 7 * MS, 7 * MS));
        // With 10 samples, p99 is the largest one (nearest rank rounds up).
        let ten = Summary::of((1..=10).map(|i| i * MS).collect());
        assert_eq!((ten.p50, ten.p90, ten.p99), (5 * MS, 9 * MS, 10 * MS));
        assert_eq!(Summary::of(Vec::new()), Summary::default());
    }

    #[test]
    fn formats_milliseconds() {
        assert_eq!(ms(Duration::from_micros(1_250)), "1.250");
        assert_eq!(ms(Duration::from_nanos(1_250_600)), "1.251");
    }
}
