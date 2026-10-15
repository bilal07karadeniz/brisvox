//! The metric primitives and the Prometheus text writer.
//!
//! Everything here is a lock-free atomic: [`Counter`] and [`Histogram`] are bumped once per turn
//! or per event, never per frame. [`Prom`] writes the text exposition format 0.0.4
//! (`text/plain; version=0.0.4`), which every Prometheus-compatible scraper reads.

use std::fmt::{self, Display, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A monotonic counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// A counter at zero.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Adds one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Adds `n` (saturating at `u64::MAX`).
    pub fn add(&self, n: u64) {
        if n == 0 {
            return;
        }
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(n))
            });
    }

    /// The current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Upper bounds of the latency buckets, in microseconds (the exposed `le` labels are seconds).
///
/// Tuned for voice turns: fine below 100 ms where the engine's own hops live, coarse towards the
/// seconds a slow provider takes.
pub const LATENCY_BOUNDS_US: [u64; 18] = [
    1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 150_000, 200_000, 300_000, 400_000,
    500_000, 750_000, 1_000_000, 1_500_000, 2_000_000, 3_000_000, 5_000_000,
];

/// Upper bounds of the call-duration buckets, in microseconds: from a failed start to a
/// multi-hour call.
pub const CALL_DURATION_BOUNDS_US: [u64; 11] = [
    5_000_000,
    10_000_000,
    30_000_000,
    60_000_000,
    120_000_000,
    180_000_000,
    300_000_000,
    600_000_000,
    900_000_000,
    1_800_000_000,
    3_600_000_000,
];

/// A fixed-bucket histogram of durations (plus the `+Inf` bucket): [`Histogram::new`] over
/// [`LATENCY_BOUNDS_US`], [`Histogram::with_bounds`] over other bounds.
#[derive(Debug)]
pub struct Histogram {
    /// Upper bounds in microseconds, ascending.
    bounds: &'static [u64],
    /// Observations per bucket (not cumulative); the last slot is the `+Inf` overflow.
    buckets: Box<[AtomicU64]>,
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// A consistent-enough copy of a [`Histogram`] (each field is read atomically; a scrape racing
/// an observation can see the count one ahead of the buckets, which Prometheus tolerates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistogramSnapshot {
    /// Cumulative counts, one per bound of the histogram.
    pub cumulative: Vec<u64>,
    /// Observations in all.
    pub count: u64,
    /// Sum of the observations, in microseconds.
    pub sum_us: u64,
}

impl Histogram {
    /// An empty latency histogram over [`LATENCY_BOUNDS_US`].
    pub fn new() -> Self {
        Self::with_bounds(&LATENCY_BOUNDS_US)
    }

    /// An empty histogram over `bounds` (microseconds, ascending).
    pub fn with_bounds(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            buckets: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Records one observation.
    pub fn observe(&self, value: Duration) {
        let us = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        let slot = self.bounds.partition_point(|bound| *bound < us);
        if let Some(bucket) = self.buckets.get(slot) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self
            .sum_us
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(us))
            });
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Observations so far.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// A copy for rendering.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut running = 0u64;
        let cumulative = self
            .buckets
            .iter()
            .take(self.bounds.len())
            .map(|bucket| {
                running = running.saturating_add(bucket.load(Ordering::Relaxed));
                running
            })
            .collect();
        HistogramSnapshot {
            cumulative,
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
        }
    }
}

/// The kind of a metric family, as written on its `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Monotonic.
    Counter,
    /// Goes up and down.
    Gauge,
    /// Buckets, sum and count.
    Histogram,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// Writer of the Prometheus text exposition format 0.0.4.
#[derive(Debug, Default)]
pub struct Prom {
    out: String,
}

/// Escapes a label value (`\`, `"` and newline).
fn escape_label(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Escapes a `# HELP` text (`\` and newline).
fn escape_help(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn write_labels(out: &mut String, labels: &[(&str, &str)], extra: Option<(&str, &str)>) {
    if labels.is_empty() && extra.is_none() {
        return;
    }
    out.push('{');
    for (i, (name, value)) in labels.iter().copied().chain(extra).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        escape_label(out, value);
        out.push('"');
    }
    out.push('}');
}

impl Prom {
    /// An empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a family: its `# HELP` and `# TYPE` lines. Write all samples of a family right
    /// after (a scraper rejects a family that is split up).
    pub fn family(&mut self, name: &str, help: &str, kind: Kind) {
        self.out.push_str("# HELP ");
        self.out.push_str(name);
        self.out.push(' ');
        escape_help(&mut self.out, help);
        self.out.push('\n');
        let _ = writeln!(self.out, "# TYPE {name} {}", kind.as_str());
    }

    /// One sample of the current family.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl Display) {
        self.out.push_str(name);
        write_labels(&mut self.out, labels, None);
        let _ = writeln!(self.out, " {value}");
    }

    /// The buckets, `_sum` and `_count` of `histogram` as one series of the current family
    /// `name` (seconds).
    pub fn histogram(&mut self, name: &str, labels: &[(&str, &str)], histogram: &Histogram) {
        let snap = histogram.snapshot();
        for (bound_us, cumulative) in histogram.bounds.iter().zip(&snap.cumulative) {
            let le = Seconds(*bound_us).to_string();
            self.out.push_str(name);
            self.out.push_str("_bucket");
            write_labels(&mut self.out, labels, Some(("le", &le)));
            let _ = writeln!(self.out, " {cumulative}");
        }
        self.out.push_str(name);
        self.out.push_str("_bucket");
        write_labels(&mut self.out, labels, Some(("le", "+Inf")));
        let _ = writeln!(self.out, " {}", snap.count);
        self.out.push_str(name);
        self.out.push_str("_sum");
        write_labels(&mut self.out, labels, None);
        let _ = writeln!(self.out, " {}", Seconds(snap.sum_us));
        self.out.push_str(name);
        self.out.push_str("_count");
        write_labels(&mut self.out, labels, None);
        let _ = writeln!(self.out, " {}", snap.count);
    }

    /// The document.
    pub fn finish(self) -> String {
        self.out
    }
}

/// Microseconds printed as decimal seconds without float rounding (`1500` -> `0.0015`).
pub(super) struct Seconds(pub(super) u64);

impl Display for Seconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 1_000_000;
        let micros = self.0 % 1_000_000;
        if micros == 0 {
            write!(f, "{whole}")
        } else {
            let frac = format!("{micros:06}");
            write!(f, "{whole}.{}", frac.trim_end_matches('0'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative_and_inclusive() {
        let h = Histogram::new();
        h.observe(Duration::from_micros(1_000)); // exactly the first bound: inside it
        h.observe(Duration::from_micros(1_001));
        h.observe(Duration::from_secs(60)); // beyond the last bound: only +Inf
        let snap = h.snapshot();
        assert_eq!(snap.cumulative.first(), Some(&1));
        assert_eq!(snap.cumulative.get(1), Some(&2));
        assert_eq!(snap.cumulative.last(), Some(&2));
        assert_eq!(snap.count, 3);
        assert_eq!(snap.sum_us, 60_002_001);
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let c = Counter::new();
        c.add(u64::MAX);
        c.inc();
        assert_eq!(c.get(), u64::MAX);
    }

    #[test]
    fn seconds_print_exactly() {
        assert_eq!(Seconds(1_000).to_string(), "0.001");
        assert_eq!(Seconds(2_500).to_string(), "0.0025");
        assert_eq!(Seconds(1_500_000).to_string(), "1.5");
        assert_eq!(Seconds(5_000_000).to_string(), "5");
        assert_eq!(Seconds(0).to_string(), "0");
    }

    #[test]
    fn labels_are_escaped() {
        let mut p = Prom::new();
        p.family("x_total", "help \\ with\nnewline", Kind::Counter);
        p.sample("x_total", &[("a", "q\"uote\\back\nline")], 1);
        let out = p.finish();
        assert!(
            out.contains("# HELP x_total help \\\\ with\\nnewline\n"),
            "{out}"
        );
        assert!(
            out.contains("x_total{a=\"q\\\"uote\\\\back\\nline\"} 1\n"),
            "{out}"
        );
    }
}
